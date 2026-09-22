//! Watch mode.
//!
//! After the first run, bld watches the workspace and re-runs whatever a
//! change could affect. Correctness comes from hashing rather than from
//! bookkeeping: every iteration recomputes each task's hash and skips the
//! ones that match their last success, so a run cancelled half way through
//! simply picks up where it left off. Filesystem events only decide which
//! packages are worth re-walking, and whether a running build is worth
//! interrupting.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ignore::gitignore::Gitignore;
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::env::EnvSnapshot;
use crate::globs::Globs;
use crate::graph::{Selection, Selector, TaskGraph};
use crate::printer::PrinterHandle;
use crate::process::describe_status;
use crate::runner::{RunOpts, Session};
use crate::walk::{self, WalkOpts};
use crate::workspace::{PkgIdx, TaskIdx, Workspace};

/// How long the workspace must be quiet before a rebuild starts.
const QUIET: Duration = Duration::from_millis(150);
/// Upper bound on debouncing, so a steady trickle of writes still rebuilds.
const MAX_WAIT: Duration = Duration::from_secs(1);

/// What a batch of filesystem events means for the next run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Touch {
  #[default]
  Nothing,
  /// These tasks have changed inputs; their packages need re-walking.
  Tasks(HashSet<TaskIdx>),
  /// Something global changed; re-walk everything.
  Everything,
  /// A `bld.toml` changed; reload the workspace.
  Reload,
}

impl Touch {
  /// Combines two batches, keeping the more far-reaching one.
  pub fn merge(self, other: Self) -> Self {
    match (self, other) {
      (Self::Reload, _) | (_, Self::Reload) => Self::Reload,
      (Self::Everything, _) | (_, Self::Everything) => Self::Everything,
      (Self::Tasks(mut a), Self::Tasks(b)) => {
        a.extend(b);
        Self::Tasks(a)
      }
      (Self::Tasks(a), Self::Nothing) | (Self::Nothing, Self::Tasks(a)) => Self::Tasks(a),
      (Self::Nothing, Self::Nothing) => Self::Nothing,
    }
  }

  pub fn is_nothing(&self) -> bool {
    matches!(self, Self::Nothing)
  }
}

/// Decides which tasks a changed path affects.
pub struct Classifier {
  root: PathBuf,
  /// The directories bld keeps for itself; events under them mean nothing.
  skip: Vec<PathBuf>,
  /// Every package directory with the tasks defined there. A path can belong
  /// to two of them: the root package contains all the others.
  packages: Vec<(PathBuf, PkgIdx)>,
  gitignores: Vec<(PathBuf, Gitignore)>,
  global_inputs: Globs,
  /// Only tasks in the run are worth reacting to, indexed by [`TaskIdx`].
  relevant: Vec<bool>,
  ws: Arc<Workspace>,
}

/// The ignore files that decide whether a changed path is worth a rebuild.
/// Read from disk, so editing one has to re-read them.
fn load_gitignores(ws: &Workspace) -> Vec<(PathBuf, Gitignore)> {
  let mut gitignores = Vec::new();
  for dir in std::iter::once(&ws.root).chain(ws.packages.iter().map(|p| &p.dir)) {
    let file = dir.join(".gitignore");
    if file.is_file() {
      let (gi, _) = Gitignore::new(&file);
      gitignores.push((dir.clone(), gi));
    }
  }
  gitignores
}

impl Classifier {
  pub fn new(ws: Arc<Workspace>, selection: &Selection) -> Self {
    let gitignores = load_gitignores(&ws);
    let packages = ws
      .packages
      .iter()
      .enumerate()
      .map(|(i, p)| (p.dir.clone(), PkgIdx(i as u32)))
      .collect();
    Self {
      root: ws.root.clone(),
      skip: ws.settings.skip_dirs(),
      packages,
      gitignores,
      global_inputs: ws.settings.global_inputs.clone(),
      relevant: selection.included.clone(),
      ws,
    }
  }

  pub fn classify_event(&self, event: &notify::Event) -> Touch {
    // The backend lost track; assume everything moved.
    if event.need_rescan() {
      return Touch::Everything;
    }
    // Reads never change inputs.
    if matches!(event.kind, EventKind::Access(_)) {
      return Touch::Nothing;
    }
    event.paths.iter().fold(Touch::Nothing, |acc, path| {
      acc.merge(self.classify_path(path))
    })
  }

  pub fn classify_path(&self, path: &Path) -> Touch {
    let Ok(rel) = path.strip_prefix(&self.root) else {
      return Touch::Nothing; // outside the workspace
    };
    if self.skip.iter().any(|dir| path.starts_with(dir))
      || rel.components().any(|c| c.as_os_str() == ".git")
    {
      return Touch::Nothing;
    }
    if path.file_name().is_some_and(|n| n == "bld.toml") {
      return Touch::Reload;
    }
    let is_dir = path.is_dir();
    if self.is_gitignored(path, is_dir) {
      return Touch::Nothing;
    }
    if self.global_inputs.is_match(rel) {
      return Touch::Everything;
    }

    let mut tasks = HashSet::new();
    for (dir, pkg) in &self.packages {
      let Ok(pkg_rel) = path.strip_prefix(dir) else {
        continue;
      };
      for &task in &self.ws.pkg(*pkg).tasks {
        // Changes to a task nobody asked to run are not worth a rebuild.
        if !self.relevant.get(task.i()).copied().unwrap_or(false) {
          continue;
        }
        // A directory event says nothing about which file changed, so every
        // task of that package has to look again.
        if is_dir || self.ws.task(task).is_input(pkg_rel) {
          tasks.insert(task);
        }
      }
    }
    if tasks.is_empty() {
      Touch::Nothing
    } else {
      Touch::Tasks(tasks)
    }
  }

  /// Re-reads the ignore files. A `.gitignore` edit changes which paths are
  /// worth reacting to without changing any `bld.toml`, so nothing else
  /// would notice.
  pub fn refresh_gitignores(&mut self) {
    self.gitignores = load_gitignores(&self.ws);
  }

  fn is_gitignored(&self, path: &Path, is_dir: bool) -> bool {
    self.gitignores.iter().any(|(dir, gi)| {
      // `matched_path_or_any_parents` panics on a path outside its root.
      path.starts_with(dir) && gi.matched_path_or_any_parents(path, is_dir).is_ignore()
    })
  }
}

/// Watching many more directories than this usually means a large tree is not
/// ignored. The budget is per user and shared with every other watcher on the
/// machine, so it is worth saying something before it runs out.
const WATCH_WARN: usize = 10_000;

/// Keeps the kernel's watch registrations in step with the directories bld
/// would hash.
///
/// On Linux every directory is registered on its own, because inotify charges
/// a watch descriptor per directory out of a budget shared with every other
/// tool running as the same user. Walking the ignore rules first is what keeps
/// a monorepo's dependency directories, its build outputs and anything a
/// symlink points at out of that budget. macOS keeps one recursive watch:
/// FSEvents streams a whole subtree per registration, so splitting it up would
/// only cost more.
struct WatchSet {
  watcher: notify::RecommendedWatcher,
  root: PathBuf,
  opts: WalkOpts,
  exclude: Globs,
  /// Registered directories. Empty where the backend watches recursively.
  watched: HashSet<PathBuf>,
}

impl WatchSet {
  fn new(
    root: PathBuf,
    opts: WalkOpts,
    exclude: Globs,
    handler: impl notify::EventHandler,
  ) -> Result<Self> {
    // notify's default follows symlinks while it walks a subtree to register
    // it. The walk that hashes never does, so following them here can only
    // register directories whose contents bld will never look at.
    let config = notify::Config::default().with_follow_symlinks(false);
    let watcher = notify::RecommendedWatcher::new(handler, config)
      .context("starting the filesystem watcher")?;
    let mut set = Self {
      watcher,
      root,
      opts,
      exclude,
      watched: HashSet::new(),
    };
    set.start()?;
    Ok(set)
  }

  /// Points the next reconcile at a reloaded configuration.
  fn retarget(&mut self, opts: WalkOpts, exclude: Globs) {
    self.opts = opts;
    self.exclude = exclude;
  }

  /// A warning worth printing, if the set came out surprisingly large.
  fn oversized(&self) -> Option<String> {
    oversized(&self.root, &self.watched)
  }
}

/// Names the subtrees a large watch set is mostly made of, so the fix is
/// obvious without going looking for it.
fn oversized(root: &Path, watched: &HashSet<PathBuf>) -> Option<String> {
  if watched.len() < WATCH_WARN {
    return None;
  }
  let mut counts: HashMap<PathBuf, usize> = HashMap::new();
  for dir in watched {
    let Ok(rel) = dir.strip_prefix(root) else {
      continue;
    };
    let head: PathBuf = rel.components().take(2).collect();
    if !head.as_os_str().is_empty() {
      *counts.entry(head).or_default() += 1;
    }
  }
  let mut top: Vec<(PathBuf, usize)> = counts.into_iter().collect();
  top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
  top.truncate(3);
  let worst = top
    .iter()
    .map(|(dir, n)| format!("{} ({n})", dir.display()))
    .collect::<Vec<_>>()
    .join(", ");
  Some(format!(
    "watching {} directories, mostly {worst}; gitignore what you do not \
     build, or add `watch_exclude` to bld.toml",
    watched.len()
  ))
}

#[cfg(not(target_os = "macos"))]
impl WatchSet {
  fn start(&mut self) -> Result<()> {
    self.reconcile().map(|_| ())
  }

  /// Registers directories that appeared and drops those that are gone or
  /// newly ignored. Returns the directories that are newly watched, which the
  /// caller has to treat as changed: anything created inside one of them
  /// before the watch landed produced no event.
  fn reconcile(&mut self) -> Result<Vec<PathBuf>> {
    let desired: HashSet<PathBuf> = walk::watch_dirs(&self.root, &self.opts, &self.exclude)
      .into_iter()
      .collect();
    let mut added = Vec::new();
    for dir in &desired {
      if self.watched.contains(dir) {
        continue;
      }
      match self.watcher.watch(dir, RecursiveMode::NonRecursive) {
        Ok(()) => {
          self.watched.insert(dir.clone());
          added.push(dir.clone());
        }
        // Gone between the walk and the registration. The event saying so is
        // already on its way.
        Err(e) if is_missing(&e) => {}
        Err(e) => return Err(describe_watch_error(e, desired.len())),
      }
    }
    let stale: Vec<PathBuf> = self.watched.difference(&desired).cloned().collect();
    for dir in stale {
      // The kernel drops watches on deleted directories by itself, so a
      // failure here only means it got there first.
      let _ = self.watcher.unwatch(&dir);
      self.watched.remove(&dir);
    }
    Ok(added)
  }
}

#[cfg(target_os = "macos")]
impl WatchSet {
  fn start(&mut self) -> Result<()> {
    self
      .watcher
      .watch(&self.root, RecursiveMode::Recursive)
      .map_err(|e| describe_watch_error(e, 1))
      .with_context(|| format!("watching {}", self.root.display()))
  }

  /// FSEvents covers the subtree from the one registration on the root, so
  /// there is nothing to keep in step.
  fn reconcile(&mut self) -> Result<Vec<PathBuf>> {
    Ok(Vec::new())
  }
}

/// Whether an event may have changed which directories exist, or which of
/// them are ignored. Watches are registered per directory, so both have to
/// reach the kernel before the next build.
fn touches_watch_set(event: &notify::Event) -> bool {
  use notify::event::{CreateKind, ModifyKind, RemoveKind};
  if event.need_rescan() {
    return true;
  }
  let structural = match event.kind {
    // inotify labels directory events, so these need no filesystem check.
    EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => true,
    // A rename that landed on a directory moved a subtree. Asking the
    // filesystem keeps an editor's write-temp-then-rename from re-walking the
    // workspace on every save.
    EventKind::Create(CreateKind::Any)
    | EventKind::Remove(RemoveKind::Any)
    | EventKind::Modify(ModifyKind::Name(_)) => event.paths.iter().any(|p| p.is_dir()),
    _ => false,
  };
  structural
    || event
      .paths
      .iter()
      .any(|p| p.file_name().is_some_and(|n| n == ".gitignore"))
}

fn is_missing(err: &notify::Error) -> bool {
  match &err.kind {
    notify::ErrorKind::PathNotFound => true,
    notify::ErrorKind::Io(e) => e.kind() == std::io::ErrorKind::NotFound,
    _ => false,
  }
}

/// Events seen but not yet acted on.
#[derive(Default)]
struct Pending {
  touch: Touch,
  /// Whether the set of watched directories may be out of date.
  reconcile: bool,
}

impl Pending {
  fn absorb(&mut self, event: notify::Result<notify::Event>, classifier: &Classifier) {
    if let Ok(event) = &event {
      self.reconcile |= touches_watch_set(event);
    }
    self.touch = std::mem::take(&mut self.touch).merge(classify(event, classifier));
  }

  /// Whether anything at all is outstanding. A reconcile on its own updates
  /// the watch set without rebuilding, so it must not cancel a running build.
  fn is_nothing(&self) -> bool {
    self.touch.is_nothing() && !self.reconcile
  }
}

/// Everything watch mode needs to build, and rebuild, a session.
pub struct WatchSetup {
  pub root: PathBuf,
  pub selectors: Vec<Selector>,
  pub filter: Vec<String>,
  pub concurrency: Option<usize>,
  pub env: Arc<EnvSnapshot>,
}

struct Loaded {
  ws: Arc<Workspace>,
  selection: Selection,
  classifier: Classifier,
  session: Session,
}

impl WatchSetup {
  fn load(&self, printer: &PrinterHandle) -> Result<Loaded> {
    let ws = Arc::new(Workspace::load(&self.root)?);
    let graph = Arc::new(TaskGraph::build(&ws)?);
    let selection = graph.select(&ws, &self.selectors, &self.filter)?;
    let opts = RunOpts {
      concurrency: self.concurrency.unwrap_or(ws.settings.concurrency).max(1),
      force: false,
      continue_on_fail: true, // a failure must not end the watch
    };
    crate::cache::Cache::new(ws.settings.cache_dir.clone()).prepare()?;
    crate::locks::Locks::new(ws.settings.lock_dir.clone()).prepare()?;
    let session = Session::new(ws.clone(), graph, printer.clone(), self.env.clone(), opts);
    let classifier = Classifier::new(ws.clone(), &selection);
    Ok(Loaded {
      ws,
      selection,
      classifier,
      session,
    })
  }
}

/// Runs the selected tasks, then keeps re-running them as inputs change,
/// until `cancel` fires.
pub async fn watch(
  setup: WatchSetup,
  printer: PrinterHandle,
  cancel: CancellationToken,
) -> Result<()> {
  // What is worth watching comes out of the configuration, so it has to be
  // read first: a broken `bld.toml` should fail before any watch exists.
  let mut loaded = setup.load(&printer)?;
  let (tx, mut events) = mpsc::unbounded_channel();
  let mut watch_set = WatchSet::new(
    loaded.ws.root.clone(),
    watch_opts(&loaded),
    loaded.ws.settings.watch_exclude.clone(),
    move |res| {
      // The callback runs on the watcher's own thread; an unbounded send
      // never blocks it.
      let _ = tx.send(res);
    },
  )?;
  if let Some(warning) = watch_set.oversized() {
    printer.note(format!("bld: {warning}")).await;
  }
  announce(&loaded, &printer).await;

  let mut pending = Pending::default();
  loop {
    // One build, interruptible by a change that matters.
    let run_cancel = cancel.child_token();
    let report = {
      let run = loaded.session.run(&loaded.selection, &run_cancel);
      let mut run = std::pin::pin!(run);
      loop {
        tokio::select! {
          report = &mut run => break report,
          Some(event) = events.recv() => {
            pending.absorb(event, &loaded.classifier);
            // A watch-set change on its own is not worth interrupting a build.
            if !pending.touch.is_nothing() {
              run_cancel.cancel();
            }
          }
          _ = cancel.cancelled() => run_cancel.cancel(),
        }
      }
    };
    if cancel.is_cancelled() {
      break;
    }
    // Only a queued rebuild suppresses the summary. A build that created an
    // output directory leaves a reconcile outstanding, and that is not news.
    if pending.touch.is_nothing() {
      printer.note(report.summary()).await;
      printer.note("watching for changes".to_string()).await;
    }

    // Wait for something worth rebuilding, keeping the watch set in step
    // with the tree while we wait.
    let batch = 'wait: loop {
      let taken = std::mem::take(&mut pending);
      let Some(mut batch) =
        wait_for_changes(&mut events, &mut loaded, &printer, &cancel, taken).await
      else {
        break 'wait None;
      };
      if batch.reconcile {
        batch.touch = batch
          .touch
          .merge(reconcile(&mut watch_set, &mut loaded, &printer).await);
      }
      if !batch.touch.is_nothing() {
        break 'wait Some(batch.touch);
      }
    };
    let Some(batch) = batch else { break };
    printer
      .note("change detected, rebuilding".to_string())
      .await;

    match batch {
      Touch::Reload => match setup.load(&printer) {
        Ok(fresh) => {
          // The old session's dev servers belong to the old config.
          loaded.session.shutdown().await;
          loaded = fresh;
          // A reloaded configuration can move the cache directory, add a
          // package or change `watch_exclude`.
          watch_set.retarget(
            watch_opts(&loaded),
            loaded.ws.settings.watch_exclude.clone(),
          );
          reconcile(&mut watch_set, &mut loaded, &printer).await;
          announce(&loaded, &printer).await;
        }
        Err(e) => {
          printer.note(format!("bld: {e:#}")).await;
          printer
            .note("keeping the previous configuration".to_string())
            .await;
        }
      },
      Touch::Everything => loaded.session.walks.invalidate_all(),
      Touch::Tasks(tasks) => {
        for task in tasks {
          loaded
            .session
            .walks
            .invalidate_package(loaded.ws.task(task).pkg);
        }
      }
      Touch::Nothing => {}
    }
  }

  loaded.session.shutdown().await;
  Ok(())
}

/// Blocks until there is something to rebuild, folding in anything that
/// arrived while the previous build was still running.
async fn wait_for_changes(
  events: &mut mpsc::UnboundedReceiver<notify::Result<notify::Event>>,
  loaded: &mut Loaded,
  printer: &PrinterHandle,
  cancel: &CancellationToken,
  mut pending: Pending,
) -> Option<Pending> {
  let now = Instant::now();
  let mut first_seen = (!pending.is_nothing()).then_some(now);
  loop {
    let deadline = first_seen.map(|first| (first + MAX_WAIT).min(Instant::now() + QUIET));
    tokio::select! {
      event = events.recv() => {
        let event = event?;
        pending.absorb(event, &loaded.classifier);
        if !pending.is_nothing() {
          first_seen.get_or_insert_with(Instant::now);
        }
      }
      _ = async { tokio::time::sleep_until(deadline.unwrap().into()).await }, if deadline.is_some() => {
        return Some(pending);
      }
      // A dev server that dies on its own is worth reporting, but watch mode
      // keeps going: the next rebuild starts it again.
      Some((task, status)) = loaded.session.wait_for_persistent_exit() => {
        printer
          .status(task, format!("exited on its own ({})", describe_status(status)))
          .await;
      }
      _ = cancel.cancelled() => return None,
    }
  }
}

fn classify(event: notify::Result<notify::Event>, classifier: &Classifier) -> Touch {
  match event {
    Ok(event) => classifier.classify_event(&event),
    // A dropped or failed event means the view of the tree is incomplete.
    Err(_) => Touch::Everything,
  }
}

async fn announce(loaded: &Loaded, printer: &PrinterHandle) {
  let labels: Vec<String> = loaded.ws.tasks.iter().map(|t| t.label.clone()).collect();
  let width = loaded
    .selection
    .tasks
    .iter()
    .map(|&t| loaded.ws.task(t).label.chars().count())
    .max()
    .unwrap_or(0);
  printer.set_labels(labels, width).await;
}

fn watch_opts(loaded: &Loaded) -> WalkOpts {
  WalkOpts {
    skip: loaded.ws.settings.skip_dirs(),
  }
}

/// Brings the watch set and the ignore rules up to date, reporting a failure
/// rather than ending the watch. Returns what the newly watched directories
/// mean for the next run.
async fn reconcile(set: &mut WatchSet, loaded: &mut Loaded, printer: &PrinterHandle) -> Touch {
  loaded.classifier.refresh_gitignores();
  match set.reconcile() {
    Ok(added) => added.iter().fold(Touch::Nothing, |acc, dir| {
      acc.merge(loaded.classifier.classify_path(dir))
    }),
    Err(e) => {
      printer.note(format!("bld: {e:#}")).await;
      Touch::Nothing
    }
  }
}

/// The per-user watch limit is the usual reason watching a large repository
/// fails. The error alone does not say so, and the budget is shared, so the
/// tool that exhausts it is often not the one that reports the failure.
fn describe_watch_error(err: notify::Error, wanted: usize) -> anyhow::Error {
  // notify turns ENOSPC into a variant of its own, so matching the io error
  // would never fire.
  if !matches!(err.kind, notify::ErrorKind::MaxFilesWatch) {
    return anyhow::Error::new(err);
  }
  let mut msg = format!("bld wanted to watch {wanted} directories");
  if let Some(budget) = watch_budget() {
    msg.push_str(&format!("; {budget}"));
  }
  msg.push_str(
    ". Raise fs.inotify.max_user_watches, stop whatever else is holding \
     watches, or add `watch_exclude` to bld.toml",
  );
  anyhow::Error::new(err).context(msg)
}

/// How many watches the kernel allows and who is already using them up.
#[cfg(target_os = "linux")]
fn watch_budget() -> Option<String> {
  let limit = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
    .ok()?
    .trim()
    .parse::<usize>()
    .ok()?;
  let mut total = 0usize;
  let mut worst: Option<(String, usize)> = None;
  for proc in std::fs::read_dir("/proc").ok()?.flatten() {
    if !proc
      .file_name()
      .as_encoded_bytes()
      .iter()
      .all(u8::is_ascii_digit)
    {
      continue;
    }
    let Ok(fds) = std::fs::read_dir(proc.path().join("fdinfo")) else {
      continue; // another user's process, or one that just exited
    };
    let held: usize = fds
      .flatten()
      .filter_map(|fd| std::fs::read_to_string(fd.path()).ok())
      .map(|text| text.lines().filter(|l| l.starts_with("inotify ")).count())
      .sum();
    if held == 0 {
      continue;
    }
    total += held;
    if worst.as_ref().is_none_or(|(_, most)| held > *most) {
      let name = std::fs::read_to_string(proc.path().join("comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| proc.file_name().to_string_lossy().into_owned());
      worst = Some((name, held));
    }
  }
  let mut msg = format!("{total} of {limit} are already in use");
  if let Some((name, held)) = worst {
    msg.push_str(&format!(", {held} of them by {name}"));
  }
  Some(msg)
}

#[cfg(not(target_os = "linux"))]
fn watch_budget() -> Option<String> {
  None
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::{Fixture, assert_contains};

  fn event(kind: EventKind, paths: &[PathBuf]) -> notify::Event {
    notify::Event {
      kind,
      paths: paths.to_vec(),
      attrs: notify::event::EventAttributes::default(),
    }
  }

  #[test]
  fn structural_changes_and_ignore_files_move_the_watch_set() {
    use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode};
    let fx = Fixture::new();
    let dir = fx.mkdir("src/new");
    let file = fx.write("src/a.ts", "a");

    assert!(touches_watch_set(&event(
      EventKind::Create(CreateKind::Folder),
      std::slice::from_ref(&dir)
    )));
    assert!(touches_watch_set(&event(
      EventKind::Remove(RemoveKind::Folder),
      std::slice::from_ref(&dir)
    )));
    // A directory that moved in takes a subtree with it.
    assert!(touches_watch_set(&event(
      EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
      &[fx.path("src/old"), dir]
    )));
    // An editor's write-temp-then-rename must not re-walk the workspace.
    assert!(!touches_watch_set(&event(
      EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
      &[fx.path("src/a.ts.tmp"), file.clone()]
    )));
    // An ignore file decides which directories belong in the set at all.
    assert!(touches_watch_set(&event(
      EventKind::Modify(ModifyKind::Data(DataChange::Content)),
      &[fx.path(".gitignore")]
    )));
    assert!(!touches_watch_set(&event(
      EventKind::Modify(ModifyKind::Data(DataChange::Content)),
      &[file]
    )));
  }

  /// Writing an output must not queue a rebuild, but it may well have created
  /// a directory, and the watch set has to hear about that.
  #[test]
  fn a_reconcile_alone_does_not_ask_for_a_rebuild() {
    use notify::event::CreateKind;
    let fx = Fixture::new();
    fx.write("bld.toml", "[tasks.build]\ncommand = \"true\"\n");
    fx.write(".gitignore", "out/\n");
    let out = fx.mkdir("out/nested");
    let ws = Arc::new(fx.load().unwrap());
    let mut pending = Pending::default();
    pending.absorb(
      Ok(event(EventKind::Create(CreateKind::Folder), &[out])),
      &classifier_for(ws),
    );
    assert!(pending.reconcile, "a new directory has to be registered");
    assert!(
      pending.touch.is_nothing(),
      "an ignored directory is not worth rebuilding for"
    );
    assert!(!pending.is_nothing());
  }

  #[test]
  fn a_watch_limit_failure_says_what_ran_out() {
    let err = describe_watch_error(notify::Error::new(notify::ErrorKind::MaxFilesWatch), 1234);
    let msg = format!("{err:#}");
    assert_contains(&msg, "1234 directories");
    assert_contains(&msg, "watch_exclude");
    // Anything else is passed through as it came.
    let other = describe_watch_error(notify::Error::new(notify::ErrorKind::WatchNotFound), 7);
    assert!(!format!("{other:#}").contains("watch_exclude"));
  }

  #[test]
  fn an_oversized_watch_set_names_the_worst_subtrees() {
    let root = PathBuf::from("/repo");
    let mut watched: HashSet<PathBuf> = HashSet::new();
    for i in 0..WATCH_WARN {
      watched.insert(root.join(format!("vendor/dep{i}")));
    }
    watched.insert(root.join("src"));
    let msg = oversized(&root, &watched).expect("above the threshold");
    assert_contains(&msg, "vendor");
    assert_contains(&msg, "watch_exclude");
    // A small set says nothing at all.
    assert!(oversized(&root, &HashSet::from([root.join("src")])).is_none());
  }

  /// A classifier over a selection naming every task, which is what the
  /// path tests care about.
  fn classifier_for(ws: Arc<Workspace>) -> Classifier {
    let graph = TaskGraph::build(&ws).unwrap();
    let selectors: Vec<Selector> = ws
      .tasks
      .iter()
      .map(|t| t.label.parse::<Selector>().unwrap())
      .collect();
    let selection = graph.select(&ws, &selectors, &[]).unwrap();
    Classifier::new(ws, &selection)
  }

  fn fixture() -> (Fixture, Classifier, Arc<Workspace>) {
    let fx = Fixture::new();
    fx.write(
      "bld.toml",
      r#"
        packages = ["packages/*"]
        inputs = ["bld.lock"]
        [tasks.lint]
        command = "lint"
        inputs = ["**/*.ts"]
      "#,
    );
    fx.write(".gitignore", "ignored/\n*.log\n");
    fx.write(
      "packages/core/bld.toml",
      r#"
        [tasks.build]
        command = "b"
        inputs = ["src/**"]
        outputs = ["dist/**"]
        [tasks.test]
        command = "t"
        inputs = ["tests/**"]
      "#,
    );
    fx.write("packages/core/src/a.ts", "");
    let ws = Arc::new(fx.load().unwrap());
    let classifier = classifier_for(ws.clone());
    (fx, classifier, ws)
  }

  fn tasks(touch: &Touch, ws: &Workspace) -> Vec<String> {
    match touch {
      Touch::Tasks(set) => {
        let mut names: Vec<String> = set.iter().map(|&t| ws.task(t).label.clone()).collect();
        names.sort();
        names
      }
      other => panic!("expected task set, got {other:?}"),
    }
  }

  #[test]
  fn a_source_file_touches_its_task_and_the_root_task() {
    let (fx, c, ws) = fixture();
    let touch = c.classify_path(&fx.path("packages/core/src/a.ts"));
    assert_eq!(tasks(&touch, &ws), ["//#lint", "core#build"]);
  }

  #[test]
  fn a_file_outside_every_input_glob_changes_nothing() {
    let (fx, c, _) = fixture();
    assert_eq!(
      c.classify_path(&fx.path("packages/core/README.md")),
      Touch::Nothing
    );
  }

  #[test]
  fn a_task_is_not_disturbed_by_its_own_outputs() {
    let (fx, c, ws) = fixture();
    let touch = c.classify_path(&fx.path("packages/core/dist/out.js"));
    // The root lint task only matches .ts files, so nothing is left.
    assert_eq!(touch, Touch::Nothing, "{:?}", tasks(&touch, &ws));
  }

  #[test]
  fn gitignored_paths_are_ignored() {
    let (fx, c, _) = fixture();
    assert_eq!(c.classify_path(&fx.path("ignored/a.ts")), Touch::Nothing);
    assert_eq!(c.classify_path(&fx.path("debug.log")), Touch::Nothing);
    assert_eq!(
      c.classify_path(&fx.path("packages/core/src/deep.log")),
      Touch::Nothing
    );
  }

  #[test]
  fn the_cache_and_git_directories_are_ignored() {
    let (fx, c, _) = fixture();
    assert_eq!(
      c.classify_path(&fx.path(".bld/cache/abc/meta.toml")),
      Touch::Nothing
    );
    assert_eq!(c.classify_path(&fx.path(".git/index")), Touch::Nothing);
  }

  #[test]
  fn a_config_change_asks_for_a_reload() {
    let (fx, c, _) = fixture();
    assert_eq!(c.classify_path(&fx.path("bld.toml")), Touch::Reload);
    assert_eq!(
      c.classify_path(&fx.path("packages/core/bld.toml")),
      Touch::Reload
    );
  }

  #[test]
  fn a_global_input_touches_everything() {
    let (fx, c, _) = fixture();
    assert_eq!(c.classify_path(&fx.path("bld.lock")), Touch::Everything);
  }

  #[test]
  fn paths_outside_the_workspace_are_ignored() {
    let (_fx, c, _) = fixture();
    assert_eq!(c.classify_path(Path::new("/etc/passwd")), Touch::Nothing);
  }

  #[test]
  fn a_directory_event_touches_every_task_of_its_package() {
    let (fx, c, ws) = fixture();
    let touch = c.classify_path(&fx.path("packages/core/src"));
    assert_eq!(tasks(&touch, &ws), ["//#lint", "core#build", "core#test"]);
  }

  #[test]
  fn rename_events_use_both_paths() {
    let (fx, c, ws) = fixture();
    let event = notify::Event {
      kind: EventKind::Modify(notify::event::ModifyKind::Name(
        notify::event::RenameMode::Both,
      )),
      paths: vec![
        fx.path("packages/core/src/a.ts"),
        fx.path("packages/core/tests/a.ts"),
      ],
      attrs: Default::default(),
    };
    assert_eq!(
      tasks(&c.classify_event(&event), &ws),
      ["//#lint", "core#build", "core#test"]
    );
  }

  #[test]
  fn reads_are_not_changes() {
    let (fx, c, _) = fixture();
    let event = notify::Event {
      kind: EventKind::Access(notify::event::AccessKind::Read),
      paths: vec![fx.path("packages/core/src/a.ts")],
      attrs: Default::default(),
    };
    assert_eq!(c.classify_event(&event), Touch::Nothing);
  }

  #[test]
  fn tasks_outside_the_run_are_ignored() {
    let (fx, _, ws) = fixture();
    let graph = TaskGraph::build(&ws).unwrap();
    // Only core#build is selected, so a file that merely matches the root
    // lint task must not trigger anything.
    let selection = graph
      .select(&ws, &["core#build".parse().unwrap()], &[])
      .unwrap();
    let c = Classifier::new(ws.clone(), &selection);
    assert_eq!(
      c.classify_path(&fx.path("packages/core/README.md")),
      Touch::Nothing
    );
    assert_eq!(
      tasks(&c.classify_path(&fx.path("packages/core/src/a.ts")), &ws),
      ["core#build"]
    );
  }

  #[test]
  fn merging_keeps_the_broader_batch() {
    let a = Touch::Tasks(HashSet::from([TaskIdx(0)]));
    let b = Touch::Tasks(HashSet::from([TaskIdx(1)]));
    assert_eq!(
      a.clone().merge(b),
      Touch::Tasks(HashSet::from([TaskIdx(0), TaskIdx(1)]))
    );
    assert_eq!(a.clone().merge(Touch::Nothing), a);
    assert_eq!(a.clone().merge(Touch::Everything), Touch::Everything);
    assert_eq!(Touch::Everything.merge(Touch::Reload), Touch::Reload);
    assert_eq!(Touch::Nothing.merge(Touch::Nothing), Touch::Nothing);
  }
}
