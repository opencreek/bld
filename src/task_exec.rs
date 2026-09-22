//! What running one task means: decide whether it needs to run at all, then
//! run it and record the result. The scheduler in [`crate::runner`] only
//! decides *when* this happens.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::cache::{Cache, Meta};
use crate::env::EnvSnapshot;
use crate::hash;
use crate::persistent::{Persistent, Started};
use crate::printer::PrinterHandle;
use crate::process::{self, SpawnSpec, describe_status};
use crate::runner::{FailReason, Outcome, RunOpts, SuccessKind};
use crate::walk::{self, Walks};
use crate::workspace::{TaskIdx, Workspace};

/// How long a cancelled task gets to stop before it is killed.
const CANCEL_GRACE: Duration = Duration::from_secs(5);
/// How long to wait for a finished task's pipes to drain.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// The hash covering a task's inputs, command, environment and dependencies.
async fn compute_hash(
  ctx: &TaskCtx,
  task: TaskIdx,
  dep_hashes: &[(String, u64)],
) -> anyhow::Result<u64> {
  let def = ctx.ws.task(task);
  let package_files = ctx.walks.package(def.pkg).await?;
  let global_files = ctx.walks.globals().await?;
  let inputs = hash::inputs_hash(
    &ctx.ws,
    task,
    &package_files,
    &global_files,
    &ctx.env,
    ctx.args.for_task(task),
  );
  Ok(hash::task_hash(inputs, dep_hashes))
}

/// Everything one task needs, shared by all of them.
pub struct TaskCtx {
  pub ws: Arc<Workspace>,
  pub env: Arc<EnvSnapshot>,
  pub printer: PrinterHandle,
  pub persistent: Persistent,
  pub walks: Walks,
  pub cache: Arc<Cache>,
  /// Keeps a second bld from running this task at the same time.
  pub locks: crate::locks::Locks,
  /// Puts a package's own tools on PATH, the way a package manager would.
  pub toolchain: crate::toolchain::Toolchain,
  /// Arguments after `--`, for the tasks the command line named.
  pub args: crate::graph::TaskArgs,
  /// Hash of each task's last success in this session. In watch mode this is
  /// what makes an unaffected task a no-op instead of a cache lookup.
  pub memo: Arc<Mutex<HashMap<TaskIdx, u64>>>,
  pub opts: RunOpts,
}

impl TaskCtx {
  fn already_current(&self, task: TaskIdx, hash: u64) -> bool {
    self.memo.lock().expect("task memo").get(&task) == Some(&hash)
  }

  fn mark_current(&self, task: TaskIdx, hash: u64) {
    self.memo.lock().expect("task memo").insert(task, hash);
  }
}

/// The result of one task, reported back to the scheduler.
pub struct Executed {
  pub task: TaskIdx,
  pub outcome: Outcome,
  /// The task's hash, passed to dependents so it becomes part of their hash.
  pub hash: Option<u64>,
}

pub async fn execute(
  ctx: Arc<TaskCtx>,
  task: TaskIdx,
  dep_hashes: Vec<(String, u64)>,
  cancel: CancellationToken,
) -> Executed {
  let def = ctx.ws.task(task);

  // The hash is needed even for tasks that will not run: dependents fold it
  // into their own hash.
  let task_hash = match compute_hash(&ctx, task, &dep_hashes).await {
    Ok(hash) => hash,
    Err(e) => {
      return Executed {
        task,
        outcome: Outcome::Failed(FailReason::Internal(format!("{e:#}"))),
        hash: None,
      };
    }
  };
  let done = |outcome| Executed {
    task,
    outcome,
    hash: Some(task_hash),
  };

  // A task without a command only orders other tasks.
  let Some(command) = def
    .command
    .as_deref()
    .map(|cmd| process::with_args(cmd, ctx.args.for_task(task)))
  else {
    ctx.mark_current(task, task_hash);
    return done(Outcome::Success(SuccessKind::NoOp));
  };

  // Nothing this task depends on has changed since it last succeeded here.
  if !def.persistent && !ctx.opts.force && ctx.already_current(task, task_hash) {
    return done(Outcome::Success(SuccessKind::UpToDate));
  }

  let mut env = ctx.env.child_env(&def.env, &def.pass_through_env);
  env.push((OsString::from("BLD"), OsString::from("1")));
  env.push((OsString::from("BLD_TASK"), OsString::from(&def.label)));
  let cwd = ctx.ws.task_dir(task).to_path_buf();
  // After the allowlist, so the inherited PATH is what these go in front of.
  ctx.toolchain.apply(&cwd, &mut env);
  let spec = SpawnSpec {
    shell: &ctx.ws.settings.shell,
    command: &command,
    cwd: &cwd,
    env: &env,
  };

  if def.persistent {
    return match ctx
      .persistent
      .ensure(
        task,
        &def.label,
        task_hash,
        def.interruptible,
        &spec,
        &ctx.printer,
      )
      .await
    {
      Ok(_) => done(Outcome::Success(SuccessKind::PersistentRunning)),
      Err(Started::Busy(who)) => done(Outcome::Failed(FailReason::Busy(who))),
      Err(Started::Failed(e)) => done(Outcome::Failed(FailReason::Spawn(e))),
    };
  }

  // Everything below reads or writes this task's outputs, so no other bld may
  // be doing the same. The lock is on the task rather than on its hash: two
  // processes that disagree about the inputs still share the directory. By the
  // time a waiter gets in, the work is usually in the cache and the lookup
  // below turns into a hit.
  let _lock = match ctx
    .locks
    .acquire(&def.label, task, &ctx.printer, &cancel)
    .await
  {
    Ok(Some(guard)) => guard,
    Ok(None) => return done(Outcome::Cancelled),
    Err(e) => {
      return done(Outcome::Failed(FailReason::Internal(format!("{e:#}"))));
    }
  };

  if def.cache
    && !ctx.opts.force
    && let Some(entry) = ctx.cache.lookup(task_hash)
  {
    ctx.printer.status(task, "cache hit").await;
    if def.show_cached_logs {
      ctx.printer.begin(task, false).await;
      if let Err(e) = ctx.cache.replay_log(&entry, task, &ctx.printer).await {
        ctx
          .printer
          .status(task, format!("could not replay the cached log: {e:#}"))
          .await;
      }
      ctx.printer.end(task).await;
    }
    return match ctx.cache.restore(&entry, cwd.clone()).await {
      Ok(()) => {
        ctx.mark_current(task, task_hash);
        done(Outcome::Success(SuccessKind::CacheHit))
      }
      Err(e) => done(Outcome::Failed(FailReason::Internal(format!(
        "restoring cached outputs: {e:#}"
      )))),
    };
  }

  let started = Instant::now();
  let mut pg = match process::spawn(&spec) {
    Ok(pg) => pg,
    Err(e) => return done(Outcome::Failed(FailReason::Spawn(e.to_string()))),
  };
  let readers = pg.pump(task, ctx.printer.clone());
  // Only a cacheable task needs its output kept in memory.
  ctx.printer.begin(task, def.cache).await;

  // Scope the borrow so the process group is usable again afterwards.
  let finished = {
    let mut wait = std::pin::pin!(pg.wait());
    tokio::select! {
      status = &mut wait => Some(status),
      _ = cancel.cancelled() => None,
    }
  };

  let outcome = match finished {
    Some(Ok(status)) if status.success() => Outcome::Success(SuccessKind::Ran),
    Some(Ok(status)) => Outcome::Failed(FailReason::Exit(describe_status(status))),
    Some(Err(e)) => Outcome::Failed(FailReason::Internal(e.to_string())),
    None => {
      let _ = pg.terminate(CANCEL_GRACE).await;
      Outcome::Cancelled
    }
  };

  // Strays would keep the pipes open and stall the readers.
  pg.kill_strays();
  readers.drain(DRAIN_GRACE).await;
  let captured = ctx.printer.end(task).await;

  match &outcome {
    Outcome::Failed(reason) => {
      ctx.printer.status(task, format!("failed: {reason}")).await;
    }
    Outcome::Success(_) => {
      ctx.mark_current(task, task_hash);
      if def.cache {
        // A cache that cannot be written costs time later, but the task did
        // succeed, so the run is not failed over it.
        if let Err(e) = store(&ctx, task, task_hash, started.elapsed(), captured, &cwd).await {
          ctx
            .printer
            .status(task, format!("could not cache the result: {e:#}"))
            .await;
        }
      }
    }
    _ => {}
  }
  done(outcome)
}

/// Collects a successful task's outputs and stores them with its log.
async fn store(
  ctx: &TaskCtx,
  task: TaskIdx,
  hash: u64,
  duration: Duration,
  captured: crate::printer::Captured,
  package_dir: &std::path::Path,
) -> anyhow::Result<()> {
  let def = ctx.ws.task(task);
  let outputs = {
    let dir = package_dir.to_path_buf();
    let patterns = def.output_patterns.clone();
    let globs = def.outputs.clone();
    tokio::task::spawn_blocking(move || walk::walk_outputs(&dir, &patterns, &globs)).await??
  };
  let meta = Meta::new(
    &def.label,
    hash,
    duration,
    !outputs.is_empty(),
    captured.truncated,
  );
  ctx
    .cache
    .save(
      hash,
      meta,
      captured.bytes,
      outputs,
      package_dir.to_path_buf(),
    )
    .await
}
