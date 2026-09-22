//! Discovery, resolution and validation of the workspace: the root
//! `bld.toml`, every package `bld.toml`, and the tasks they define.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use globset::{GlobBuilder, GlobSetBuilder};

use crate::config::{PackageConfig, RootConfig, TaskConfig};
use crate::env::{self, EnvPattern};
use crate::globs::{self, Globs};

/// Name of the implicit package formed by the workspace root.
pub const ROOT_PACKAGE: &str = "//";

/// Index into [`Workspace::packages`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PkgIdx(pub u32);

/// Index into [`Workspace::tasks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskIdx(pub u32);

impl PkgIdx {
  pub fn i(self) -> usize {
    self.0 as usize
  }
}

impl TaskIdx {
  pub fn i(self) -> usize {
    self.0 as usize
  }
}

/// Workspace-wide settings from the root `bld.toml`.
#[derive(Debug)]
pub struct Settings {
  pub concurrency: usize,
  pub shell: Vec<String>,
  /// Absolute path to the cache directory.
  pub cache_dir: PathBuf,
  /// Globs relative to the root, hashed into every task.
  pub global_inputs: Globs,
  /// Raw global input patterns, used to pick walk roots.
  pub global_input_patterns: Vec<String>,
  /// Directories watch mode never registers, on top of the ignore rules.
  /// Relative to the root; nothing to do with what gets hashed.
  pub watch_exclude: Globs,
  /// Absolute path to the task lock directory. Always inside the workspace:
  /// locks stop two bld processes writing one output directory, and two
  /// workspaces sharing a cache have no such conflict.
  pub lock_dir: PathBuf,
}

impl Settings {
  /// Directories bld keeps for itself. Never hashed, never watched: a lock
  /// file is not an input, and a cache write is not a change.
  pub fn skip_dirs(&self) -> Vec<PathBuf> {
    vec![self.cache_dir.clone(), self.lock_dir.clone()]
  }
}

#[derive(Debug)]
pub struct Package {
  pub name: String,
  /// Absolute path to the package directory.
  pub dir: PathBuf,
  /// Path relative to the workspace root; empty for the root package.
  pub rel: PathBuf,
  pub deps: Vec<PkgIdx>,
  pub tasks: Vec<TaskIdx>,
  /// Every input pattern of every task here, so one walk serves them all.
  pub input_union: Globs,
}

#[derive(Debug)]
pub struct TaskDef {
  pub pkg: PkgIdx,
  pub name: String,
  /// `pkg#task`, used in selectors, log prefixes and cache metadata.
  pub label: String,
  pub command: Option<String>,
  pub inputs: Globs,
  /// Raw input patterns, used to build the package-wide walk filter.
  pub input_patterns: Vec<String>,
  pub outputs: Globs,
  /// Raw output patterns, sorted: hashed, and used to pick walk roots.
  pub output_patterns: Vec<String>,
  /// Unresolved dependency specs; resolved into edges by [`crate::graph`].
  pub depends_on: Vec<String>,
  /// Global + task `env`, whose values are hashed and passed through.
  pub env: Vec<EnvPattern>,
  /// Global + task `pass_through_env`, passed but not hashed.
  pub pass_through_env: Vec<EnvPattern>,
  pub cache: bool,
  pub show_cached_logs: bool,
  pub persistent: bool,
  pub interruptible: bool,
}

impl TaskDef {
  /// Whether `rel` (relative to the package dir) is an input. A task's own
  /// outputs never count as its inputs, so producing them cannot invalidate
  /// the task that produced them.
  pub fn is_input(&self, rel: &Path) -> bool {
    self.inputs.is_match(rel) && !self.outputs.is_match(rel)
  }
}

#[derive(Debug)]
pub struct Workspace {
  /// Canonicalized workspace root.
  pub root: PathBuf,
  pub settings: Settings,
  pub packages: Vec<Package>,
  pub tasks: Vec<TaskDef>,
}

impl Workspace {
  pub fn pkg(&self, i: PkgIdx) -> &Package {
    &self.packages[i.i()]
  }

  pub fn task(&self, i: TaskIdx) -> &TaskDef {
    &self.tasks[i.i()]
  }

  /// The absolute directory a task runs in.
  pub fn task_dir(&self, i: TaskIdx) -> &Path {
    &self.pkg(self.task(i).pkg).dir
  }

  pub fn package_by_name(&self, name: &str) -> Option<PkgIdx> {
    self
      .packages
      .iter()
      .position(|p| p.name == name)
      .map(|i| PkgIdx(i as u32))
  }

  pub fn task_in(&self, pkg: PkgIdx, name: &str) -> Option<TaskIdx> {
    self
      .pkg(pkg)
      .tasks
      .iter()
      .copied()
      .find(|&t| self.task(t).name == name)
  }

  /// Every task with the given name, in package order.
  pub fn tasks_named(&self, name: &str) -> Vec<TaskIdx> {
    self
      .tasks
      .iter()
      .enumerate()
      .filter(|(_, t)| t.name == name)
      .map(|(i, _)| TaskIdx(i as u32))
      .collect()
  }

  /// Loads and validates the workspace rooted at `root`.
  pub fn load(root: &Path) -> Result<Self> {
    let root = root
      .canonicalize()
      .with_context(|| format!("workspace root {} is not readable", root.display()))?;
    let root_file = root.join("bld.toml");
    let text = std::fs::read_to_string(&root_file)
      .with_context(|| format!("reading {}", root_file.display()))?;
    let cfg: RootConfig =
      toml::from_str(&text).with_context(|| format!("parsing {}", root_file.display()))?;

    let cache_dir = root.join(cfg.cache_dir.as_deref().unwrap_or(".bld/cache"));
    let settings = Settings {
      concurrency: match cfg.concurrency {
        Some(0) => bail!("concurrency must be at least 1"),
        Some(n) => n,
        None => std::thread::available_parallelism().map_or(4, |n| n.get()),
      },
      shell: match cfg.shell.clone() {
        Some(s) if s.is_empty() => bail!("shell must not be empty"),
        Some(s) => s,
        None => vec!["sh".into(), "-c".into()],
      },
      cache_dir,
      global_inputs: Globs::new(&cfg.inputs).context("root `inputs`")?,
      global_input_patterns: cfg.inputs.clone(),
      watch_exclude: Globs::includes_only(&cfg.watch_exclude).context("root `watch_exclude`")?,
      lock_dir: root.join(".bld/locks"),
    };

    let global_env = env::parse(&cfg.env).context("root `env`")?;
    let global_pass = env::parse(&cfg.pass_through_env).context("root `pass_through_env`")?;
    let show_cached_logs = cfg.show_cached_logs.unwrap_or(true);

    // The root package comes first so that `//` is always PkgIdx(0).
    let mut sources: Vec<(PathBuf, String, PackageConfig)> = vec![(
      root.clone(),
      ROOT_PACKAGE.to_string(),
      PackageConfig {
        name: Some(ROOT_PACKAGE.to_string()),
        depends_on: cfg.depends_on.clone(),
        tasks: cfg.tasks.clone(),
      },
    )];
    for dir in discover_packages(&root, &cfg.packages, &settings.cache_dir)? {
      let file = dir.join("bld.toml");
      let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
      let pkg_cfg: PackageConfig =
        toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
      let name = match &pkg_cfg.name {
        Some(n) => n.clone(),
        None => dir
          .file_name()
          .and_then(|n| n.to_str())
          .ok_or_else(|| anyhow!("package directory {} has no usable name", dir.display()))?
          .to_string(),
      };
      sources.push((dir, name, pkg_cfg));
    }

    check_unique_names(&sources)?;
    check_not_nested(&sources)?;

    let mut packages = Vec::with_capacity(sources.len());
    let mut tasks: Vec<TaskDef> = Vec::new();
    for (idx, (dir, name, pkg_cfg)) in sources.iter().enumerate() {
      let pkg = PkgIdx(idx as u32);
      let mut task_idxs = Vec::with_capacity(pkg_cfg.tasks.len());
      for (task_name, task_cfg) in &pkg_cfg.tasks {
        let label = format!("{name}#{task_name}");
        let def = build_task(
          pkg,
          task_name,
          &label,
          task_cfg,
          &global_env,
          &global_pass,
          show_cached_logs,
        )
        .with_context(|| format!("task `{label}`"))?;
        task_idxs.push(TaskIdx(tasks.len() as u32));
        tasks.push(def);
      }
      let input_union = Globs::union(
        task_idxs
          .iter()
          .map(|t| tasks[t.i()].input_patterns.as_slice()),
      )
      .with_context(|| format!("package `{name}`"))?;
      packages.push(Package {
        name: name.clone(),
        rel: dir
          .strip_prefix(&root)
          .unwrap_or(Path::new(""))
          .to_path_buf(),
        dir: dir.clone(),
        deps: Vec::new(),
        tasks: task_idxs,
        input_union,
      });
    }

    // Package-level dependencies, used to expand `^task`.
    let by_name: HashMap<&str, PkgIdx> = packages
      .iter()
      .enumerate()
      .map(|(i, p)| (p.name.as_str(), PkgIdx(i as u32)))
      .collect();
    let mut resolved: Vec<Vec<PkgIdx>> = Vec::with_capacity(packages.len());
    for (idx, (_, name, pkg_cfg)) in sources.iter().enumerate() {
      let mut deps = Vec::with_capacity(pkg_cfg.depends_on.len());
      for dep in &pkg_cfg.depends_on {
        let target = *by_name
          .get(dep.as_str())
          .ok_or_else(|| anyhow!("package `{name}` depends on unknown package `{dep}`"))?;
        if target.i() == idx {
          bail!("package `{name}` depends on itself");
        }
        if !deps.contains(&target) {
          deps.push(target);
        }
      }
      resolved.push(deps);
    }
    for (p, deps) in packages.iter_mut().zip(resolved) {
      p.deps = deps;
    }
    check_package_cycles(&packages)?;

    Ok(Self {
      root,
      settings,
      packages,
      tasks,
    })
  }

  /// Finds the workspace root by walking up from `start`. The highest
  /// ancestor holding a `bld.toml` wins, bounded by the enclosing git
  /// repository when there is one.
  pub fn find_root(start: &Path) -> Result<PathBuf> {
    let start = start
      .canonicalize()
      .with_context(|| format!("{} is not readable", start.display()))?;
    let mut best = None;
    for dir in start.ancestors() {
      if dir.join("bld.toml").is_file() {
        best = Some(dir.to_path_buf());
      }
      if dir.join(".git").exists() {
        break;
      }
    }
    best.ok_or_else(|| {
      anyhow!(
        "no bld.toml found in {} or any parent directory",
        start.display()
      )
    })
  }
}

#[allow(clippy::too_many_arguments)]
fn build_task(
  pkg: PkgIdx,
  name: &str,
  label: &str,
  cfg: &TaskConfig,
  global_env: &[EnvPattern],
  global_pass: &[EnvPattern],
  default_show_logs: bool,
) -> Result<TaskDef> {
  if cfg.persistent && cfg.command.is_none() {
    bail!("a persistent task needs a `command`");
  }
  if cfg.interruptible && !cfg.persistent {
    bail!("`interruptible` only applies to a `persistent` task");
  }
  let input_patterns = cfg.inputs.clone().unwrap_or_else(|| vec!["**".to_string()]);
  let mut output_patterns = cfg.outputs.clone();
  output_patterns.sort();
  output_patterns.dedup();

  let mut env_pats = global_env.to_vec();
  env_pats.extend(env::parse(&cfg.env).context("`env`")?);
  let mut pass_pats = global_pass.to_vec();
  pass_pats.extend(env::parse(&cfg.pass_through_env).context("`pass_through_env`")?);

  Ok(TaskDef {
    pkg,
    name: name.to_string(),
    label: label.to_string(),
    command: cfg.command.clone(),
    inputs: Globs::new(&input_patterns).context("`inputs`")?,
    input_patterns,
    outputs: Globs::includes_only(&output_patterns).context("`outputs`")?,
    output_patterns,
    depends_on: cfg.depends_on.clone(),
    env: env_pats,
    pass_through_env: pass_pats,
    cache: cfg.cache.unwrap_or(true),
    show_cached_logs: cfg.show_cached_logs.unwrap_or(default_show_logs),
    persistent: cfg.persistent,
    interruptible: cfg.interruptible,
  })
}

/// Finds directories matching the `packages` globs that contain a `bld.toml`.
fn discover_packages(root: &Path, patterns: &[String], cache_dir: &Path) -> Result<Vec<PathBuf>> {
  if patterns.is_empty() {
    return Ok(Vec::new());
  }
  let mut set = GlobSetBuilder::new();
  let mut max_depth = Some(0usize);
  for pat in patterns {
    globs::validate(pat).context("root `packages`")?;
    set.add(
      GlobBuilder::new(pat.trim_end_matches('/'))
        .literal_separator(true)
        .build()
        .with_context(|| format!("invalid package glob `{pat}`"))?,
    );
    let depth = pat.trim_end_matches('/').split('/').count();
    max_depth = match max_depth {
      Some(d) if !pat.contains("**") => Some(d.max(depth)),
      _ => None,
    };
  }
  let set = set.build()?;

  let mut walker = ignore::WalkBuilder::new(root);
  walker
    .hidden(false)
    .follow_links(false)
    .max_depth(max_depth)
    .filter_entry({
      let cache_dir = cache_dir.to_path_buf();
      move |e| e.file_name() != ".git" && e.path() != cache_dir
    });

  let mut found = Vec::new();
  for entry in walker.build() {
    let entry = entry.context("scanning for packages")?;
    if entry.depth() == 0 || !entry.file_type().is_some_and(|t| t.is_dir()) {
      continue;
    }
    let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
    if set.is_match(rel) && entry.path().join("bld.toml").is_file() {
      found.push(entry.into_path());
    }
  }
  found.sort();
  Ok(found)
}

fn check_unique_names(sources: &[(PathBuf, String, PackageConfig)]) -> Result<()> {
  let mut seen: HashMap<&str, &Path> = HashMap::new();
  for (dir, name, _) in sources {
    if let Some(prev) = seen.insert(name, dir) {
      bail!(
        "duplicate package name `{name}` in {} and {}",
        prev.display(),
        dir.display()
      );
    }
  }
  Ok(())
}

/// Packages may not contain one another: a task's input walk covers its whole
/// directory subtree, so nesting would make ownership of a file ambiguous.
fn check_not_nested(sources: &[(PathBuf, String, PackageConfig)]) -> Result<()> {
  // Skip the root package, which contains every other package by definition.
  let pkgs = &sources[1..];
  for (i, (a, a_name, _)) in pkgs.iter().enumerate() {
    for (b, b_name, _) in &pkgs[i + 1..] {
      let (outer, inner, outer_name, inner_name) = if b.starts_with(a) {
        (a, b, a_name, b_name)
      } else if a.starts_with(b) {
        (b, a, b_name, a_name)
      } else {
        continue;
      };
      bail!(
        "package `{inner_name}` ({}) is nested inside package `{outer_name}` ({})",
        inner.display(),
        outer.display()
      );
    }
  }
  Ok(())
}

fn check_package_cycles(packages: &[Package]) -> Result<()> {
  let mut state = vec![0u8; packages.len()]; // 0 unvisited, 1 on stack, 2 done
  for start in 0..packages.len() {
    if state[start] != 0 {
      continue;
    }
    // Iterative DFS keeping the current path, so a cycle can be reported.
    let mut path: Vec<(PkgIdx, usize)> = vec![(PkgIdx(start as u32), 0)];
    state[start] = 1;
    while let Some(&mut (node, ref mut next)) = path.last_mut() {
      if *next < packages[node.i()].deps.len() {
        let dep = packages[node.i()].deps[*next];
        *next += 1;
        match state[dep.i()] {
          0 => {
            state[dep.i()] = 1;
            path.push((dep, 0));
          }
          1 => {
            let start_at = path.iter().position(|(n, _)| *n == dep).unwrap_or(0);
            let mut cycle: Vec<&str> = path[start_at..]
              .iter()
              .map(|(n, _)| packages[n.i()].name.as_str())
              .collect();
            cycle.push(packages[dep.i()].name.as_str());
            bail!("package dependency cycle: {}", cycle.join(" -> "));
          }
          _ => {}
        }
      } else {
        state[node.i()] = 2;
        path.pop();
      }
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::{Fixture, assert_contains, err};

  fn base_root(extra: &str) -> String {
    format!("packages = [\"packages/*\"]\n{extra}")
  }

  #[test]
  fn loads_root_and_packages() {
    let fx = Fixture::new();
    fx.write(
      "bld.toml",
      &base_root("[tasks.lint]\ncommand = \"echo lint\"\n"),
    );
    fx.write(
      "packages/core/bld.toml",
      "[tasks.build]\ncommand = \"echo core\"\n",
    );
    fx.write(
      "packages/web/bld.toml",
      "name = \"webapp\"\ndepends_on = [\"core\"]\n[tasks.build]\ncommand = \"echo web\"\n",
    );
    let ws = fx.load().unwrap();

    assert_eq!(ws.packages[0].name, ROOT_PACKAGE);
    assert_eq!(ws.packages[0].rel, Path::new(""));
    let names: Vec<&str> = ws.packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["//", "core", "webapp"]);
    assert_eq!(ws.packages[2].rel, Path::new("packages/web"));
    assert_eq!(ws.packages[2].deps, vec![PkgIdx(1)]);

    let labels: Vec<&str> = ws.tasks.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["//#lint", "core#build", "webapp#build"]);
    assert_eq!(ws.settings.shell, ["sh", "-c"]);
    assert_eq!(ws.settings.cache_dir, fx.path(".bld/cache"));
  }

  #[test]
  fn directories_without_a_config_are_not_packages() {
    let fx = Fixture::new();
    fx.write("bld.toml", &base_root(""));
    fx.mkdir("packages/not-a-package");
    fx.write("packages/core/bld.toml", "");
    let ws = fx.load().unwrap();
    assert_eq!(ws.packages.len(), 2);
  }

  #[test]
  fn task_defaults_and_overrides() {
    let fx = Fixture::new();
    fx.write(
      "bld.toml",
      &base_root("show_cached_logs = false\nenv = [\"CI\"]\npass_through_env = [\"TOKEN\"]\n"),
    );
    fx.write(
      "packages/core/bld.toml",
      r#"
        [tasks.build]
        command = "x"
        outputs = ["dist/**"]
        env = ["NODE_ENV"]

        [tasks.test]
        command = "y"
        cache = false
        show_cached_logs = true
      "#,
    );
    let ws = fx.load().unwrap();
    let build = ws.task(ws.task_in(PkgIdx(1), "build").unwrap());
    assert!(build.cache);
    assert!(!build.show_cached_logs, "inherits the root default");
    assert!(build.is_input(Path::new("src/a.ts")));
    assert!(
      !build.is_input(Path::new("dist/a.js")),
      "outputs are never inputs"
    );
    assert_eq!(
      build.env,
      vec![
        crate::env::EnvPattern::Exact("CI".into()),
        crate::env::EnvPattern::Exact("NODE_ENV".into())
      ]
    );
    assert_eq!(
      build.pass_through_env,
      vec![crate::env::EnvPattern::Exact("TOKEN".into())]
    );

    let test = ws.task(ws.task_in(PkgIdx(1), "test").unwrap());
    assert!(!test.cache);
    assert!(test.show_cached_logs);
  }

  #[test]
  fn rejects_duplicate_package_names() {
    let fx = Fixture::new();
    fx.write("bld.toml", &base_root(""));
    fx.write("packages/a/bld.toml", "name = \"same\"\n");
    fx.write("packages/b/bld.toml", "name = \"same\"\n");
    assert_contains(&err(fx.load()), "duplicate package name `same`");
  }

  #[test]
  fn rejects_nested_packages() {
    let fx = Fixture::new();
    fx.write(
      "bld.toml",
      "packages = [\"packages/*\", \"packages/*/sub\"]\n",
    );
    fx.write("packages/a/bld.toml", "");
    fx.write("packages/a/sub/bld.toml", "");
    assert_contains(&err(fx.load()), "nested inside package");
  }

  #[test]
  fn rejects_unknown_package_dependency() {
    let fx = Fixture::new();
    fx.write("bld.toml", &base_root(""));
    fx.write("packages/a/bld.toml", "depends_on = [\"ghost\"]\n");
    assert_contains(&err(fx.load()), "depends on unknown package `ghost`");
  }

  #[test]
  fn rejects_package_cycles() {
    let fx = Fixture::new();
    fx.write("bld.toml", &base_root(""));
    fx.write("packages/a/bld.toml", "depends_on = [\"b\"]\n");
    fx.write("packages/b/bld.toml", "depends_on = [\"a\"]\n");
    let msg = err(fx.load());
    assert_contains(&msg, "package dependency cycle");
    assert_contains(&msg, "a -> b -> a");
  }

  #[test]
  fn rejects_bad_task_options() {
    let fx = Fixture::new();
    fx.write("bld.toml", &base_root(""));

    fx.write("packages/a/bld.toml", "[tasks.dev]\npersistent = true\n");
    assert_contains(&err(fx.load()), "persistent task needs a `command`");

    fx.write(
      "packages/a/bld.toml",
      "[tasks.dev]\ncommand = \"x\"\ninterruptible = true\n",
    );
    assert_contains(&err(fx.load()), "only applies to a `persistent` task");

    fx.write(
      "packages/a/bld.toml",
      "[tasks.build]\ninputs = [\"../other/**\"]\n",
    );
    assert_contains(&err(fx.load()), "must stay inside the package directory");

    fx.write(
      "packages/a/bld.toml",
      "[tasks.build]\noutputs = [\"!dist\"]\n",
    );
    assert_contains(&err(fx.load()), "exclusion patterns are not allowed");

    fx.write("packages/a/bld.toml", "[tasks.build]\nenv = [\"A*B\"]\n");
    assert_contains(&err(fx.load()), "trailing wildcard");
  }

  #[test]
  fn reports_the_offending_task() {
    let fx = Fixture::new();
    fx.write("bld.toml", &base_root(""));
    fx.write(
      "packages/a/bld.toml",
      "[tasks.build]\ninputs = [\"/abs\"]\n",
    );
    assert_contains(&err(fx.load()), "task `a#build`");
  }

  #[test]
  fn finds_root_from_a_nested_directory() {
    let fx = Fixture::new();
    fx.write("bld.toml", &base_root(""));
    fx.write("packages/core/bld.toml", "");
    fx.mkdir("packages/core/src/deep");
    assert_eq!(
      Workspace::find_root(&fx.path("packages/core/src/deep")).unwrap(),
      fx.root
    );
    assert_eq!(Workspace::find_root(&fx.root).unwrap(), fx.root);
  }

  #[test]
  fn find_root_stops_at_the_repository_boundary() {
    let fx = Fixture::new();
    // A stray config above the repository must not be picked up.
    let outer = fx.root.parent().unwrap().join("outer-bld.toml");
    let _ = std::fs::write(&outer, "");
    fx.write("bld.toml", "");
    fx.mkdir("sub");
    assert_eq!(Workspace::find_root(&fx.path("sub")).unwrap(), fx.root);
    let _ = std::fs::remove_file(outer);
  }
}
