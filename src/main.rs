mod cache;
mod cli;
mod config;
mod env;
mod globs;
mod graph;
mod hash;
mod locks;
mod persistent;
mod printer;
mod process;
mod runner;
mod task_exec;
#[cfg(test)]
mod testutil;
mod toolchain;
mod walk;
mod watch;
mod workspace;

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::cache::Cache;
use crate::cli::{Cli, Command, HashArgs, RunArgs, WatchArgs};
use crate::config::UserConfig;
use crate::env::EnvSnapshot;
use crate::graph::{Selection, TaskGraph};
use crate::printer::{Printer, PrinterHandle, use_color};
use crate::runner::{FailReason, Outcome, RunOpts, RunReport, Session};
use crate::walk::{FileHashCache, Walks};
use crate::watch::WatchSetup;
use crate::workspace::Workspace;

/// A task failed.
const EXIT_FAILED: u8 = 1;
/// The workspace or the command line is wrong; nothing ran.
const EXIT_CONFIG: u8 = 2;
/// The run was interrupted.
const EXIT_INTERRUPTED: u8 = 130;

fn main() -> ExitCode {
  let cli = Cli::parse();
  let runtime = match tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
  {
    Ok(rt) => rt,
    Err(e) => {
      eprintln!("bld: could not start the async runtime: {e}");
      return ExitCode::from(EXIT_CONFIG);
    }
  };
  let code = match runtime.block_on(dispatch(cli)) {
    Ok(code) => code,
    Err(err) => {
      eprintln!("bld: {err:#}");
      ExitCode::from(EXIT_CONFIG)
    }
  };
  // Dropping the runtime last lets every child be reaped before we exit.
  drop(runtime);
  code
}

async fn dispatch(cli: Cli) -> Result<ExitCode> {
  let root = Workspace::find_root(&std::env::current_dir()?)?;
  match cli.command {
    Command::Run(args) => run_command(&root, args).await,
    Command::Watch(args) => watch_command(&root, args).await,
    Command::Hash(args) => hash_command(&root, args).await,
    Command::Clean => {
      let ws = Workspace::load(&root)?;
      if ws.settings.cache_dir.exists() {
        std::fs::remove_dir_all(&ws.settings.cache_dir)
          .with_context(|| format!("removing {}", ws.settings.cache_dir.display()))?;
      }
      println!("removed {}", ws.settings.cache_dir.display());
      Ok(ExitCode::SUCCESS)
    }
  }
}

async fn run_command(root: &std::path::Path, args: RunArgs) -> Result<ExitCode> {
  let ws = Arc::new(Workspace::load(root)?);
  if args.targets.is_empty() {
    print_targets(&ws);
    return Ok(ExitCode::SUCCESS);
  }
  let graph = Arc::new(TaskGraph::build(&ws)?);
  let selection = graph.select(&ws, &args.targets.selectors()?, &args.targets.filter()?)?;
  let task_args = crate::graph::TaskArgs::new(args.args.clone(), &selection);
  task_args.check(&ws)?;

  if args.dry_run {
    print_plan(&ws, &graph, &selection);
    return Ok(ExitCode::SUCCESS);
  }

  let opts = RunOpts {
    concurrency: args
      .common
      .concurrency
      .unwrap_or(ws.settings.concurrency)
      .max(1),
    force: args.force,
    continue_on_fail: args.continue_on_fail,
  };
  let align = args.common.align_output(UserConfig::load()?.align_output);
  Cache::new(ws.settings.cache_dir.clone()).prepare()?;
  crate::locks::Locks::new(ws.settings.lock_dir.clone()).prepare()?;
  let printer = Printer::start(
    ws.tasks.iter().map(|t| t.label.clone()).collect(),
    label_width(&ws, &selection, align),
    use_color(args.common.color),
  );
  let cancel = CancellationToken::new();
  let signals = install_signal_handler(cancel.clone(), printer.handle());

  let code = {
    let mut session = Session::new(
      ws.clone(),
      graph.clone(),
      printer.handle(),
      Arc::new(EnvSnapshot::capture()),
      opts,
      task_args,
    );
    let mut report = session.run(&selection, &cancel).await;

    // Persistent tasks outlive the graph walk: wait until one exits or the
    // user interrupts.
    if !report.failed()
      && !cancel.is_cancelled()
      && let Some((task, status)) = session.wait_for_persistent(&cancel).await
    {
      let reason = FailReason::PersistentExited(process::describe_status(status));
      printer
        .handle()
        .status(task, format!("failed: {reason}"))
        .await;
      report.outcomes[task.i()] = Some(Outcome::Failed(reason));
    }
    session.shutdown().await;

    report_failures(&ws, &report, &printer.handle()).await;
    printer.handle().note(report.summary()).await;
    printer.handle().barrier().await;

    if report.failed() {
      ExitCode::from(EXIT_FAILED)
    } else if cancel.is_cancelled() || report.cancelled() {
      ExitCode::from(EXIT_INTERRUPTED)
    } else {
      ExitCode::SUCCESS
    }
  };
  // The signal task holds a printer handle, so it has to go first.
  signals.abort();
  drop(printer);
  Ok(code)
}

/// Runs the selected tasks, then keeps them up to date until interrupted.
async fn watch_command(root: &std::path::Path, args: WatchArgs) -> Result<ExitCode> {
  if args.targets.is_empty() {
    print_targets(&Workspace::load(root)?);
    return Ok(ExitCode::SUCCESS);
  }
  let align_output = args.common.align_output(UserConfig::load()?.align_output);
  // The task table is filled in once the workspace is loaded, and replaced
  // again whenever a `bld.toml` change reloads it.
  let printer = Printer::start(Vec::new(), 0, use_color(args.common.color));
  let cancel = CancellationToken::new();
  let signals = install_signal_handler(cancel.clone(), printer.handle());
  let setup = WatchSetup {
    root: root.to_path_buf(),
    selectors: args.targets.selectors()?,
    filter: args.targets.filter()?,
    args: args.args,
    concurrency: args.common.concurrency,
    align_output,
    env: Arc::new(EnvSnapshot::capture()),
  };

  let result = watch::watch(setup, printer.handle(), cancel).await;
  signals.abort();
  drop(printer);
  result?;
  Ok(ExitCode::from(EXIT_INTERRUPTED))
}

/// Prints the hash of each selected task, and optionally the files behind
/// it. This is the tool for answering "why did that run again?".
async fn hash_command(root: &std::path::Path, args: HashArgs) -> Result<ExitCode> {
  let ws = Arc::new(Workspace::load(root)?);
  if args.targets.is_empty() {
    print_targets(&ws);
    return Ok(ExitCode::SUCCESS);
  }
  let graph = TaskGraph::build(&ws)?;
  let selection = graph.select(&ws, &args.targets.selectors()?, &args.targets.filter()?)?;
  let task_args = crate::graph::TaskArgs::new(args.args.clone(), &selection);
  task_args.check(&ws)?;
  let walks = Walks::new(ws.clone(), FileHashCache::new());
  let env = EnvSnapshot::capture();
  let toolchain = crate::toolchain::Toolchain::new(ws.root.clone());

  // Dependencies come first, so their hashes are known when they are needed.
  let mut hashes: Vec<Option<u64>> = vec![None; ws.tasks.len()];
  for &t in &selection.tasks {
    let def = ws.task(t);
    let package_files = walks.package(def.pkg).await?;
    let global_files = walks.globals().await?;
    let inputs = hash::inputs_hash(
      &ws,
      t,
      &package_files,
      &global_files,
      &env,
      task_args.for_task(t),
    );
    let deps: Vec<(String, u64)> = graph.deps[t.i()]
      .iter()
      .filter_map(|&d| hashes[d.i()].map(|h| (ws.task(d).label.clone(), h)))
      .collect();
    let task_hash = hash::task_hash(inputs, &deps);
    hashes[t.i()] = Some(task_hash);

    println!("{}  {}", hash::hex(task_hash), def.label);
    if args.files {
      for (path, entry) in &global_files.entries {
        println!("    {}  {} (global)", hash::hex(entry.hash), path.display());
      }
      for (path, entry) in &package_files.entries {
        if def.is_input(path) {
          println!("    {}  {}", hash::hex(entry.hash), path.display());
        }
      }
      for (name, value) in env.matching(&def.env) {
        println!("    env {name}={}", value.to_string_lossy());
      }
      for (label, dep) in &deps {
        println!("    {}  {label} (dependency)", hash::hex(*dep));
      }
      // Not part of the hash, but part of how the command will run, and this
      // is where someone looks to find out.
      for found in toolchain.bin_dirs(ws.task_dir(t)) {
        println!(
          "    path {} ({}, not hashed)",
          found.dir.display(),
          found.probe
        );
      }
    }
  }
  Ok(ExitCode::SUCCESS)
}

/// The prefix column width: the longest selected label when aligning,
/// otherwise no padding at all.
pub(crate) fn label_width(ws: &Workspace, selection: &Selection, align: bool) -> usize {
  if !align {
    return 0;
  }
  selection
    .tasks
    .iter()
    .map(|&t| ws.task(t).label.chars().count())
    .max()
    .unwrap_or(0)
}

async fn report_failures(ws: &Workspace, report: &RunReport, printer: &PrinterHandle) {
  for (i, outcome) in report.outcomes.iter().enumerate() {
    if let Some(Outcome::Failed(reason)) = outcome {
      printer
        .note(format!("bld: {} failed: {reason}", ws.tasks[i].label))
        .await;
    }
  }
}

/// Interrupts cancel the run; a second one stops waiting for children to
/// shut down politely.
fn install_signal_handler(
  cancel: CancellationToken,
  printer: PrinterHandle,
) -> tokio::task::JoinHandle<()> {
  tokio::spawn(async move {
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut hangup = signal(SignalKind::hangup()).expect("SIGHUP handler");
    loop {
      tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
        _ = hangup.recv() => {}
      }
      if cancel.is_cancelled() {
        process::request_hard_kill();
        printer
          .note("bld: interrupted again, killing tasks now")
          .await;
      } else {
        printer.note("bld: interrupted, stopping tasks").await;
        cancel.cancel();
      }
    }
  })
}

/// Lists every task name with the packages that define it, which is the
/// shape of the `TASKS PACKAGES` arguments.
fn print_targets(ws: &Workspace) {
  let mut by_name: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
  for task in &ws.tasks {
    by_name
      .entry(task.name.as_str())
      .or_default()
      .push(ws.pkg(task.pkg).name.as_str());
  }
  if by_name.is_empty() {
    println!("no tasks defined");
    return;
  }
  let width = by_name.keys().map(|n| n.chars().count()).max().unwrap_or(0);
  println!("available tasks:");
  for (name, packages) in by_name {
    println!("  {name:<width$}  {}", packages.join(", "));
  }
}

fn print_plan(ws: &Workspace, graph: &TaskGraph, selection: &Selection) {
  println!("{} tasks in dependency order:", selection.tasks.len());
  for &t in &selection.tasks {
    let task = ws.task(t);
    let deps: Vec<&str> = graph.deps[t.i()]
      .iter()
      .filter(|&&d| selection.contains(d))
      .map(|&d| ws.task(d).label.as_str())
      .collect();
    let mut notes = Vec::new();
    if task.command.is_none() {
      notes.push("no command".to_string());
    }
    if task.persistent {
      notes.push(if task.interruptible {
        "persistent, interruptible".to_string()
      } else {
        "persistent".to_string()
      });
    }
    if !task.cache {
      notes.push("uncached".to_string());
    }
    if !deps.is_empty() {
      notes.push(format!("after {}", deps.join(", ")));
    }
    let suffix = if notes.is_empty() {
      String::new()
    } else {
      format!("  ({})", notes.join("; "))
    };
    println!("  {}{suffix}", task.label);
  }
}
