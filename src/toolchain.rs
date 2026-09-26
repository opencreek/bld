//! Environment inferred from what is on disk.
//!
//! A task's command is run by a plain shell, not by a package manager, so
//! `tsc` or `vite` are not on PATH the way they are under `npm run`. Rather
//! than making every config route through `pnpm run`, bld looks for the
//! directories a toolchain keeps its executables in and prepends them, which
//! is what a package manager does for its own scripts.
//!
//! Directories are searched from the package up to the workspace root, nearest
//! first, so a package's own copy of a tool wins over the one at the root.
//! That is npm's rule, and matching it is the point: a command that works
//! under `npm run` should work here.
//!
//! None of this is hashed. Which tools happen to be installed describes the
//! machine a task runs on, exactly as PATH itself does; what a task depends on
//! is its declared inputs, its command and its `env`. Pin your toolchain by
//! putting the lockfile in the root `inputs`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// One thing bld knows how to find. Adding another is a line in [`PROBES`].
struct Probe {
  /// What this looks for, named in `bld hash --files`.
  name: &'static str,
  /// Directory of executables, relative to a package or one of its ancestors.
  bin: &'static str,
}

/// Tried in order in every directory from the package to the workspace root.
const PROBES: &[Probe] = &[Probe {
  name: "node_modules",
  bin: "node_modules/.bin",
}];

/// A directory of executables a task can reach by name, and what found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinDir {
  pub dir: PathBuf,
  pub probe: &'static str,
}

/// Discovery rooted at one workspace.
///
/// Nothing is cached: a probe is a handful of `stat` calls per package, and
/// paying them every time means an install that happens while bld is watching
/// is picked up by the next task rather than the next session.
#[derive(Clone)]
pub struct Toolchain {
  root: PathBuf,
}

impl Toolchain {
  pub fn new(root: PathBuf) -> Self {
    Self { root }
  }

  /// The directories a task running in `dir` should find executables in,
  /// nearest first.
  pub fn bin_dirs(&self, dir: &Path) -> Vec<BinDir> {
    let mut found = Vec::new();
    let mut at = Some(dir);
    while let Some(cur) = at {
      // A task always runs inside the workspace; stopping at the root keeps a
      // stray path from walking up to `/`.
      if !cur.starts_with(&self.root) {
        break;
      }
      for probe in PROBES {
        let bin = cur.join(probe.bin);
        if bin.is_dir() {
          found.push(BinDir {
            dir: bin,
            probe: probe.name,
          });
        }
      }
      if cur == self.root {
        break;
      }
      at = cur.parent();
    }
    found
  }

  /// Prepends those directories to PATH in a child environment.
  pub fn apply(&self, dir: &Path, env: &mut Vec<(OsString, OsString)>) {
    let dirs = self.bin_dirs(dir);
    if dirs.is_empty() {
      return;
    }
    let mut path = OsString::new();
    for found in &dirs {
      if !path.is_empty() {
        path.push(":");
      }
      path.push(&found.dir);
    }
    match env
      .iter_mut()
      .find(|(name, _)| name.to_str() == Some("PATH"))
    {
      Some((_, value)) => {
        if !value.is_empty() {
          path.push(":");
          path.push(&*value);
        }
        *value = path;
      }
      // Nothing inherited a PATH, so what bld found is the whole of it.
      None => env.push((OsString::from("PATH"), path)),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::Fixture;

  fn dirs(fx: &Fixture, rel: &str) -> Vec<String> {
    Toolchain::new(fx.root.clone())
      .bin_dirs(&fx.path(rel))
      .iter()
      .map(|found| {
        found
          .dir
          .strip_prefix(&fx.root)
          .unwrap()
          .to_string_lossy()
          .into_owned()
      })
      .collect()
  }

  #[test]
  fn a_package_finds_its_own_tools_before_the_workspace_ones() {
    let fx = Fixture::new();
    fx.mkdir("node_modules/.bin");
    fx.mkdir("apps/web/node_modules/.bin");
    fx.mkdir("apps/web/src");
    assert_eq!(
      dirs(&fx, "apps/web"),
      ["apps/web/node_modules/.bin", "node_modules/.bin"]
    );
    // A package without its own falls back to the ones above it, the way npm
    // does.
    fx.mkdir("apps/api");
    assert_eq!(dirs(&fx, "apps/api"), ["node_modules/.bin"]);
    // The root package is the workspace root.
    assert_eq!(dirs(&fx, ""), ["node_modules/.bin"]);
  }

  #[test]
  fn nothing_installed_means_nothing_added() {
    let fx = Fixture::new();
    fx.mkdir("apps/web");
    assert!(dirs(&fx, "apps/web").is_empty());

    let mut env = vec![(OsString::from("PATH"), OsString::from("/usr/bin"))];
    Toolchain::new(fx.root.clone()).apply(&fx.path("apps/web"), &mut env);
    assert_eq!(env[0].1, OsString::from("/usr/bin"));
  }

  #[test]
  fn discovered_directories_go_in_front_of_the_inherited_path() {
    let fx = Fixture::new();
    fx.mkdir("node_modules/.bin");
    fx.mkdir("apps/web/node_modules/.bin");
    let tc = Toolchain::new(fx.root.clone());

    let mut env = vec![
      (OsString::from("HOME"), OsString::from("/home/u")),
      (OsString::from("PATH"), OsString::from("/usr/bin")),
    ];
    tc.apply(&fx.path("apps/web"), &mut env);
    let root = fx.root.display();
    assert_eq!(
      env[1].1.to_str().unwrap(),
      format!("{root}/apps/web/node_modules/.bin:{root}/node_modules/.bin:/usr/bin")
    );
    assert_eq!(env[0].1, OsString::from("/home/u"), "nothing else moves");

    // An environment with no PATH at all still gets one.
    let mut env = Vec::new();
    tc.apply(&fx.path("apps/web"), &mut env);
    assert_eq!(env.len(), 1);
    assert!(env[0].1.to_str().unwrap().ends_with("node_modules/.bin"));
  }
}
