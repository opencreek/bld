//! Watch mode.
//!
//! After the first run, bld watches the workspace and re-runs whatever a
//! change could affect. Correctness comes from hashing rather than from
//! bookkeeping: every iteration recomputes each task's hash and skips the
//! ones that match their last success, so a run cancelled half way through
//! simply picks up where it left off. Filesystem events only decide which
//! packages are worth re-walking, and whether a running build is worth
//! interrupting.

use std::collections::HashSet;
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
use crate::workspace::{PkgIdx, TaskIdx, Workspace};

/// How long the workspace must be quiet before a rebuild starts.
const QUIET: Duration = Duration::from_millis(150);
/// Upper bound on debouncing, so a steady trickle of writes still rebuilds.
const MAX_WAIT: Duration = Duration::from_secs(1);

/// What a batch of filesystem events means for the next run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touch {
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
  cache_dir: PathBuf,
  /// Every package directory with the tasks defined there. A path can belong
  /// to two of them: the root package contains all the others.
  packages: Vec<(PathBuf, PkgIdx)>,
  gitignores: Vec<(PathBuf, Gitignore)>,
  global_inputs: Globs,
  /// Only tasks in the run are worth reacting to, indexed by [`TaskIdx`].
  relevant: Vec<bool>,
  ws: Arc<Workspace>,
}

impl Classifier {
  pub fn new(ws: Arc<Workspace>, selection: &Selection) -> Self {
    let mut gitignores = Vec::new();
    for dir in std::iter::once(&ws.root).chain(ws.packages.iter().map(|p| &p.dir)) {
      let file = dir.join(".gitignore");
      if file.is_file() {
        let (gi, _) = Gitignore::new(&file);
        gitignores.push((dir.clone(), gi));
      }
    }
    let packages = ws
      .packages
      .iter()
      .enumerate()
      .map(|(i, p)| (p.dir.clone(), PkgIdx(i as u32)))
      .collect();
    Self {
      root: ws.root.clone(),
      cache_dir: ws.settings.cache_dir.clone(),
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
    if path.starts_with(&self.cache_dir) || rel.components().any(|c| c.as_os_str() == ".git") {
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

  fn is_gitignored(&self, path: &Path, is_dir: bool) -> bool {
    self.gitignores.iter().any(|(dir, gi)| {
      // `matched_path_or_any_parents` panics on a path outside its root.
      path.starts_with(dir) && gi.matched_path_or_any_parents(path, is_dir).is_ignore()
    })
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
  let (tx, mut events) = mpsc::unbounded_channel();
  let mut watcher = notify::recommended_watcher(move |res| {
    // The callback runs on the watcher's own thread; an unbounded send never
    // blocks it.
    let _ = tx.send(res);
  })
  .context("starting the filesystem watcher")?;
  watcher
    .watch(&setup.root, RecursiveMode::Recursive)
    .map_err(describe_watch_error)
    .with_context(|| format!("watching {}", setup.root.display()))?;

  let mut loaded = setup.load(&printer)?;
  announce(&loaded, &printer).await;

  let mut pending = Touch::Nothing;
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
            pending = pending.merge(classify(event, &loaded.classifier));
            if !pending.is_nothing() {
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
    if pending.is_nothing() {
      printer.note(report.summary()).await;
      printer.note("watching for changes".to_string()).await;
    }

    // Wait for the workspace to go quiet.
    let batch = match wait_for_changes(&mut events, &mut loaded, &printer, &cancel, pending).await {
      Some(batch) => batch,
      None => break,
    };
    pending = Touch::Nothing;
    printer
      .note("change detected, rebuilding".to_string())
      .await;

    match batch {
      Touch::Reload => match setup.load(&printer) {
        Ok(fresh) => {
          // The old session's dev servers belong to the old config.
          loaded.session.shutdown().await;
          loaded = fresh;
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
  mut pending: Touch,
) -> Option<Touch> {
  let now = Instant::now();
  let mut first_seen = (!pending.is_nothing()).then_some(now);
  loop {
    let deadline = first_seen.map(|first| (first + MAX_WAIT).min(Instant::now() + QUIET));
    tokio::select! {
      event = events.recv() => {
        let event = event?;
        pending = pending.merge(classify(event, &loaded.classifier));
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

/// inotify's per-user watch limit is the usual reason watching a large
/// repository fails, and the error alone does not say so.
fn describe_watch_error(err: notify::Error) -> anyhow::Error {
  let hint = matches!(
    err.kind,
    notify::ErrorKind::Io(ref e) if e.raw_os_error() == Some(libc::ENOSPC)
  );
  let err = anyhow::Error::new(err);
  if hint {
    err.context(
      "the system ran out of inotify watches; raise fs.inotify.max_user_watches \
       or narrow the workspace",
    )
  } else {
    err
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::Fixture;

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
