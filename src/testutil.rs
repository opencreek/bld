//! Helpers for building throwaway workspaces on disk in unit tests.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::graph::TaskGraph;
use crate::workspace::Workspace;

pub struct Fixture {
  _dir: TempDir,
  pub root: PathBuf,
}

impl Fixture {
  /// A workspace root containing an empty `.git` directory, so that gitignore
  /// handling and root discovery behave as they do in a real repository.
  pub fn new() -> Self {
    let dir = TempDir::new().unwrap();
    // The workspace sits one level down, so that a test writing above the
    // root touches only its own temporary directory.
    // Canonicalize: on macOS the temp dir is reached through a symlink.
    let root = dir.path().canonicalize().unwrap().join("workspace");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    Self { _dir: dir, root }
  }

  pub fn write(&self, rel: &str, contents: &str) -> PathBuf {
    let path = self.root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, contents).unwrap();
    path
  }

  pub fn path(&self, rel: &str) -> PathBuf {
    self.root.join(rel)
  }

  pub fn mkdir(&self, rel: &str) -> PathBuf {
    let path = self.root.join(rel);
    std::fs::create_dir_all(&path).unwrap();
    path
  }

  pub fn load(&self) -> anyhow::Result<Workspace> {
    Workspace::load(&self.root)
  }

  pub fn graph(&self) -> anyhow::Result<(Workspace, TaskGraph)> {
    let ws = self.load()?;
    let graph = TaskGraph::build(&ws)?;
    Ok((ws, graph))
  }
}

/// The error message of a `Result`, for assertions.
pub fn err(r: anyhow::Result<impl std::fmt::Debug>) -> String {
  match r {
    Ok(v) => panic!("expected an error, got {v:?}"),
    Err(e) => format!("{e:#}"),
  }
}

pub fn labels<'a>(ws: &'a Workspace, tasks: &[crate::workspace::TaskIdx]) -> Vec<&'a str> {
  tasks.iter().map(|&t| ws.task(t).label.as_str()).collect()
}

/// Asserts that `a` comes before `b` in `order`.
pub fn assert_before(order: &[&str], a: &str, b: &str) {
  let ia = order
    .iter()
    .position(|x| *x == a)
    .unwrap_or_else(|| panic!("{a} missing from {order:?}"));
  let ib = order
    .iter()
    .position(|x| *x == b)
    .unwrap_or_else(|| panic!("{b} missing from {order:?}"));
  assert!(ia < ib, "expected {a} before {b} in {order:?}");
}

pub fn assert_contains(haystack: &str, needle: &str) {
  assert!(
    haystack.contains(needle),
    "expected {needle:?} in {haystack:?}"
  );
}

pub fn _unused(_: &Path) {}
