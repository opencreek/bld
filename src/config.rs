//! Serde schema for `bld.toml` files. Parsing only; validation lives in
//! [`crate::workspace`].

use std::collections::BTreeMap;

use serde::Deserialize;

/// The `bld.toml` at the workspace root: workspace-wide settings plus the tasks
/// of the root package (named `//`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RootConfig {
  /// Directory globs selecting packages, e.g. `["apps/*", "packages/*"]`.
  #[serde(default)]
  pub packages: Vec<String>,
  /// Max number of non-persistent tasks running at once.
  pub concurrency: Option<usize>,
  /// Shell used to run commands. Defaults to `["sh", "-c"]`.
  pub shell: Option<Vec<String>>,
  /// Cache directory, relative to the root. Defaults to `.bld/cache`.
  pub cache_dir: Option<String>,
  /// Default for the per-task `show_cached_logs`. Defaults to false: a
  /// cached task is one whose output you have already read.
  pub show_cached_logs: Option<bool>,
  /// Globs relative to the root hashed into *every* task, e.g. a lockfile.
  #[serde(default)]
  pub inputs: Vec<String>,
  /// Directory globs watch mode never registers, on top of the ignore rules.
  #[serde(default)]
  pub watch_exclude: Vec<String>,
  /// Env vars passed to every task and hashed into every task's inputs.
  #[serde(default)]
  pub env: Vec<String>,
  /// Env vars passed to every task but not hashed.
  #[serde(default)]
  pub pass_through_env: Vec<String>,
  /// Packages the root package depends on, for `^task` in root tasks.
  #[serde(default)]
  pub depends_on: Vec<String>,
  /// Tasks that run in the root package.
  #[serde(default)]
  pub tasks: BTreeMap<String, TaskConfig>,
}

/// A `bld.toml` inside a package directory.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct PackageConfig {
  /// Package name. Defaults to the directory basename.
  pub name: Option<String>,
  /// Other packages this one depends on; `^task` expands over these.
  #[serde(default)]
  pub depends_on: Vec<String>,
  #[serde(default)]
  pub tasks: BTreeMap<String, TaskConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct TaskConfig {
  /// Shell command. A task without one is a no-op node used for ordering.
  pub command: Option<String>,
  /// Input globs relative to the package dir. `!glob` excludes. Default `**`.
  pub inputs: Option<Vec<String>>,
  /// Output globs relative to the package dir, cached after a successful run.
  #[serde(default)]
  pub outputs: Vec<String>,
  /// `task` (same package), `pkg#task`, or `^task` (task in package deps).
  #[serde(default)]
  pub depends_on: Vec<String>,
  /// Env vars passed to the command and hashed into the inputs.
  #[serde(default)]
  pub env: Vec<String>,
  /// Env vars passed to the command but not hashed.
  #[serde(default)]
  pub pass_through_env: Vec<String>,
  /// Whether outputs and logs are cached. Default true.
  pub cache: Option<bool>,
  /// Whether cached logs are replayed on a cache hit. Defaults to the root
  /// setting, which itself defaults to false.
  pub show_cached_logs: Option<bool>,
  /// Long-running task (dev server) that never completes.
  #[serde(default)]
  pub persistent: bool,
  /// Persistent task that is restarted when its inputs change in watch mode.
  #[serde(default)]
  pub interruptible: bool,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_root_config() {
    let cfg: RootConfig = toml::from_str(
      r#"
        packages = ["apps/*"]
        concurrency = 4
        inputs = ["pnpm-lock.yaml"]
        env = ["CI"]

        [tasks.lint]
        command = "biome check ."
      "#,
    )
    .unwrap();
    assert_eq!(cfg.packages, ["apps/*"]);
    assert_eq!(cfg.concurrency, Some(4));
    assert_eq!(cfg.tasks["lint"].command.as_deref(), Some("biome check ."));
    assert!(cfg.tasks["lint"].inputs.is_none());
  }

  #[test]
  fn rejects_unknown_fields() {
    let err = toml::from_str::<RootConfig>("concurency = 4").unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
    let err = toml::from_str::<PackageConfig>("[tasks.build]\noutput = []").unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
  }

  #[test]
  fn task_defaults() {
    let cfg: PackageConfig = toml::from_str("[tasks.build]").unwrap();
    let t = &cfg.tasks["build"];
    assert!(t.command.is_none() && t.outputs.is_empty() && !t.persistent);
    assert!(t.cache.is_none() && !t.interruptible);
  }
}
