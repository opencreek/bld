//! The scheduler: walks the selected part of the task graph in dependency
//! order, keeping at most `concurrency` tasks running.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::cache::Cache;
use crate::env::EnvSnapshot;
use crate::graph::{Selection, TaskGraph};
use crate::persistent::{Exit, Persistent};
use crate::printer::PrinterHandle;
use crate::task_exec::{Executed, TaskCtx, execute};
use crate::walk::{FileHashCache, Walks};
use crate::workspace::{TaskIdx, Workspace};

/// State that outlives a single run: the workspace, the graph and any
/// persistent tasks. In watch mode one session spans every rebuild.
pub struct Session {
  pub ws: Arc<Workspace>,
  pub graph: Arc<TaskGraph>,
  pub ctx: Arc<TaskCtx>,
  pub persistent: Persistent,
  pub walks: Walks,
  exits: mpsc::UnboundedReceiver<Exit>,
}

impl Session {
  pub fn new(
    ws: Arc<Workspace>,
    graph: Arc<TaskGraph>,
    printer: PrinterHandle,
    env: Arc<EnvSnapshot>,
    opts: RunOpts,
  ) -> Self {
    let locks = crate::locks::Locks::new(ws.settings.lock_dir.clone());
    let (persistent, exits) = Persistent::new(locks.clone());
    let walks = Walks::new(ws.clone(), FileHashCache::new());
    let ctx = Arc::new(TaskCtx {
      ws: ws.clone(),
      env,
      printer,
      persistent: persistent.clone(),
      walks: walks.clone(),
      cache: Arc::new(Cache::new(ws.settings.cache_dir.clone())),
      locks,
      memo: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
      opts,
    });
    Self {
      ws,
      graph,
      ctx,
      persistent,
      walks,
      exits,
    }
  }

  pub async fn run(&mut self, selection: &Selection, cancel: &CancellationToken) -> RunReport {
    run(&self.ws, &self.graph, self.ctx.clone(), selection, cancel).await
  }

  /// Blocks while persistent tasks keep running. Returns the first one that
  /// exited on its own, which ends the run, or `None` when cancelled or when
  /// there is nothing persistent to wait for.
  pub async fn wait_for_persistent(&mut self, cancel: &CancellationToken) -> Option<Exit> {
    if self.persistent.is_empty().await {
      return None;
    }
    tokio::select! {
      exit = self.exits.recv() => exit,
      _ = cancel.cancelled() => None,
    }
  }

  /// The next persistent task to exit on its own. Pends forever while they
  /// all keep running, which makes it safe to `select!` on.
  pub async fn wait_for_persistent_exit(&mut self) -> Option<Exit> {
    self.exits.recv().await
  }

  pub async fn shutdown(&self) {
    self.persistent.shutdown().await;
  }
}

#[derive(Debug, Clone, Copy)]
pub struct RunOpts {
  pub concurrency: usize,
  pub force: bool,
  pub continue_on_fail: bool,
}

#[derive(Debug, Clone)]
pub enum SuccessKind {
  /// A task with no command; it only orders other tasks.
  NoOp,
  /// Outputs and logs came from the cache.
  CacheHit,
  /// The command ran.
  Ran,
  /// Watch mode: nothing this task depends on has changed since it last ran.
  UpToDate,
  /// A persistent task is running; it will not finish on its own.
  PersistentRunning,
}

#[derive(Debug, Clone)]
pub enum FailReason {
  Exit(String),
  Spawn(String),
  Internal(String),
  PersistentExited(String),
  /// Another bld process holds this task, and waiting is not an option:
  /// a dev server cannot queue behind the one already running.
  Busy(String),
}

impl fmt::Display for FailReason {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Exit(what) => write!(f, "{what}"),
      Self::Spawn(what) => write!(f, "could not start the command: {what}"),
      Self::Internal(what) => write!(f, "{what}"),
      Self::PersistentExited(what) => write!(f, "persistent task exited ({what})"),
      Self::Busy(what) => write!(f, "{what}"),
    }
  }
}

#[derive(Debug, Clone)]
pub enum Outcome {
  Success(SuccessKind),
  Failed(FailReason),
  /// The run was cancelled before or while this task ran.
  Cancelled,
  /// Something this task depends on failed, so it never started.
  Skipped,
}

impl Outcome {
  pub fn is_failure(&self) -> bool {
    matches!(self, Self::Failed(_))
  }
}

/// What happened to every task of one run, indexed by [`TaskIdx`].
pub struct RunReport {
  pub outcomes: Vec<Option<Outcome>>,
  /// How many tasks the run set out to execute.
  pub selected: usize,
  pub elapsed: Duration,
}

impl RunReport {
  pub fn failed(&self) -> bool {
    self.outcomes.iter().flatten().any(Outcome::is_failure)
  }

  pub fn cancelled(&self) -> bool {
    self
      .outcomes
      .iter()
      .flatten()
      .any(|o| matches!(o, Outcome::Cancelled))
  }

  /// One line for the end of a run. Tasks that never started because the run
  /// stopped early are counted too, so the numbers add up to the selection.
  pub fn summary(&self) -> String {
    let (mut ran, mut cached, mut fresh, mut failed, mut noop, mut persistent) =
      (0usize, 0, 0, 0, 0, 0);
    for outcome in self.outcomes.iter().flatten() {
      match outcome {
        Outcome::Success(SuccessKind::Ran) => ran += 1,
        Outcome::Success(SuccessKind::CacheHit) => cached += 1,
        Outcome::Success(SuccessKind::UpToDate) => fresh += 1,
        Outcome::Success(SuccessKind::NoOp) => noop += 1,
        Outcome::Success(SuccessKind::PersistentRunning) => persistent += 1,
        Outcome::Failed(_) => failed += 1,
        Outcome::Skipped | Outcome::Cancelled => {}
      }
    }
    let accounted = ran + cached + fresh + noop + persistent + failed;
    let not_run = self.selected.saturating_sub(accounted);

    let mut parts = vec![format!("{ran} ran")];
    if cached > 0 {
      parts.push(format!("{cached} cached"));
    }
    if fresh > 0 {
      parts.push(format!("{fresh} up to date"));
    }
    if persistent > 0 {
      parts.push(format!("{persistent} running"));
    }
    if failed > 0 {
      parts.push(format!("{failed} failed"));
    }
    if not_run > 0 {
      parts.push(format!("{not_run} not run"));
    }
    format!("{} in {}", parts.join(", "), format_duration(self.elapsed))
  }
}

/// Human-readable elapsed time: milliseconds below a second, then seconds.
pub fn format_duration(d: Duration) -> String {
  if d < Duration::from_secs(1) {
    format!("{}ms", d.as_millis())
  } else if d < Duration::from_secs(60) {
    format!("{:.2}s", d.as_secs_f64())
  } else {
    format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
  }
}

/// Runs `selection` to completion, or until `cancel` fires.
pub async fn run(
  ws: &Workspace,
  graph: &TaskGraph,
  ctx: Arc<TaskCtx>,
  selection: &Selection,
  cancel: &CancellationToken,
) -> RunReport {
  let started = Instant::now();
  let n = ws.tasks.len();
  let mut outcomes: Vec<Option<Outcome>> = vec![None; n];
  let mut hashes: Vec<Option<u64>> = vec![None; n];
  let mut remaining = vec![0usize; n];
  let mut ready: VecDeque<TaskIdx> = VecDeque::new();

  // `selection.tasks` is topologically ordered, so the ready queue starts out
  // in a deterministic, dependency-respecting order.
  for &t in &selection.tasks {
    let count = graph.deps[t.i()]
      .iter()
      .filter(|&&d| selection.contains(d))
      .count();
    remaining[t.i()] = count;
    if count == 0 {
      ready.push_back(t);
    }
  }

  let mut join: JoinSet<Executed> = JoinSet::new();
  let mut running = 0usize;
  loop {
    while let Some(&t) = ready.front() {
      let persistent = ws.task(t).persistent;
      if !persistent && running >= ctx.opts.concurrency {
        break;
      }
      ready.pop_front();
      if cancel.is_cancelled() {
        outcomes[t.i()] = Some(Outcome::Cancelled);
        continue;
      }
      let dep_hashes: Vec<(String, u64)> = graph.deps[t.i()]
        .iter()
        .filter_map(|&d| hashes[d.i()].map(|h| (ws.task(d).label.clone(), h)))
        .collect();
      join.spawn(execute(ctx.clone(), t, dep_hashes, cancel.clone()));
      if !persistent {
        running += 1;
      }
    }

    let Some(joined) = join.join_next().await else {
      break; // nothing running and nothing ready
    };
    let done = match joined {
      Ok(done) => done,
      Err(e) => {
        // A panicking task must not take the whole run down silently.
        ctx
          .printer
          .note(format!("bld: internal task failure: {e}"))
          .await;
        cancel.cancel();
        continue;
      }
    };
    if !ws.task(done.task).persistent {
      running -= 1;
    }
    hashes[done.task.i()] = done.hash;

    match &done.outcome {
      Outcome::Success(_) => {
        for &dep in &graph.rdeps[done.task.i()] {
          if !selection.contains(dep) {
            continue;
          }
          remaining[dep.i()] -= 1;
          if remaining[dep.i()] == 0 {
            ready.push_back(dep);
          }
        }
      }
      Outcome::Failed(_) | Outcome::Cancelled | Outcome::Skipped => {
        if ctx.opts.continue_on_fail {
          skip_dependents(graph, selection, done.task, &mut outcomes);
        } else {
          cancel.cancel();
        }
      }
    }
    outcomes[done.task.i()] = Some(done.outcome);
  }

  RunReport {
    outcomes,
    selected: selection.tasks.len(),
    elapsed: started.elapsed(),
  }
}

/// Marks everything downstream of a failed task as not run.
fn skip_dependents(
  graph: &TaskGraph,
  selection: &Selection,
  from: TaskIdx,
  outcomes: &mut [Option<Outcome>],
) {
  let mut stack = vec![from];
  while let Some(t) = stack.pop() {
    for &dep in &graph.rdeps[t.i()] {
      if !selection.contains(dep) || outcomes[dep.i()].is_some() {
        continue;
      }
      outcomes[dep.i()] = Some(Outcome::Skipped);
      stack.push(dep);
    }
  }
}
