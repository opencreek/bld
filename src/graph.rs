//! The task dependency graph: resolving `depends_on` into edges, detecting
//! cycles, ordering tasks, and turning command line selectors into the set of
//! tasks a run will execute.

use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};

use crate::workspace::{TaskIdx, Workspace};

/// A task named on the command line: `build`, `web#build` or `//#lint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
  pub package: Option<String>,
  pub task: String,
}

impl FromStr for Selector {
  type Err = anyhow::Error;

  fn from_str(s: &str) -> Result<Self> {
    let (package, task) = match s.split_once('#') {
      Some((pkg, task)) => (Some(pkg.to_string()), task.to_string()),
      None => (None, s.to_string()),
    };
    if task.is_empty() || package.as_deref().is_some_and(str::is_empty) {
      bail!("invalid selector `{s}`; expected `task`, `package#task` or `//#task`");
    }
    crate::workspace::validate_name("task", &task).with_context(|| format!("in selector `{s}`"))?;
    // `//` is the workspace root, which no package may be called.
    if let Some(pkg) = &package
      && pkg != crate::workspace::ROOT_PACKAGE
    {
      crate::workspace::validate_name("package", pkg)
        .with_context(|| format!("in selector `{s}`"))?;
    }
    Ok(Self { package, task })
  }
}

impl fmt::Display for Selector {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match &self.package {
      Some(p) => write!(f, "{p}#{}", self.task),
      None => write!(f, "{}", self.task),
    }
  }
}

/// Direct dependencies and dependents of every task in the workspace, plus a
/// global topological order (dependencies before dependents).
#[derive(Debug)]
pub struct TaskGraph {
  pub deps: Vec<Vec<TaskIdx>>,
  pub rdeps: Vec<Vec<TaskIdx>>,
  pub topo: Vec<TaskIdx>,
}

impl TaskGraph {
  pub fn build(ws: &Workspace) -> Result<Self> {
    let n = ws.tasks.len();
    let mut deps: Vec<Vec<TaskIdx>> = vec![Vec::new(); n];

    for (i, task) in ws.tasks.iter().enumerate() {
      let me = TaskIdx(i as u32);
      for spec in &task.depends_on {
        let targets =
          resolve_dep(ws, me, spec).map_err(|e| anyhow!("task `{}`: {e}", task.label))?;
        for target in targets {
          if target == me {
            bail!("task `{}` depends on itself", task.label);
          }
          if ws.task(target).persistent {
            bail!(
              "task `{}` depends on `{}`, which is persistent; persistent tasks never \
               complete, so nothing can wait for them",
              task.label,
              ws.task(target).label
            );
          }
          if !deps[i].contains(&target) {
            deps[i].push(target);
          }
        }
      }
    }

    let mut rdeps: Vec<Vec<TaskIdx>> = vec![Vec::new(); n];
    for (i, list) in deps.iter().enumerate() {
      for &d in list {
        rdeps[d.i()].push(TaskIdx(i as u32));
      }
    }

    let topo = topo_order(ws, &deps)?;
    Ok(Self { deps, rdeps, topo })
  }

  /// Expands selectors into the tasks to run, including their transitive
  /// dependencies, in topological order.
  pub fn select(
    &self,
    ws: &Workspace,
    selectors: &[Selector],
    filter: &[String],
  ) -> Result<Selection> {
    for name in filter {
      if ws.package_by_name(name).is_none() {
        bail!("unknown package `{name}`");
      }
    }

    let mut requested: Vec<TaskIdx> = Vec::new();
    for sel in selectors {
      let found = match &sel.package {
        Some(pkg_name) => {
          let pkg = ws
            .package_by_name(pkg_name)
            .ok_or_else(|| anyhow!("selector `{sel}` names unknown package `{pkg_name}`"))?;
          let task = ws
            .task_in(pkg, &sel.task)
            .ok_or_else(|| anyhow!("package `{pkg_name}` has no task `{}`", sel.task))?;
          vec![task]
        }
        None => {
          let mut tasks = ws.tasks_named(&sel.task);
          // A name that matches nothing anywhere is a typo, and worth saying
          // so before the package list gets a chance to explain it away.
          if tasks.is_empty() {
            bail!("no package defines task `{}`", sel.task);
          }
          if !filter.is_empty() {
            tasks.retain(|&t| filter.iter().any(|f| f == &ws.pkg(ws.task(t).pkg).name));
          }
          // Empty after filtering is fine on its own: `lint,check` over two
          // packages must not fail because only one of them lints.
          tasks
        }
      };
      for t in found {
        if !requested.contains(&t) {
          requested.push(t);
        }
      }
    }

    // Every entry was defined somewhere, so an empty selection means the
    // package list ruled all of them out.
    if requested.is_empty() {
      let tasks: Vec<String> = selectors.iter().map(Selector::to_string).collect();
      bail!(
        "nothing to run: no task matching `{}` in `{}`",
        tasks.join(","),
        filter.join(",")
      );
    }

    let mut included = vec![false; ws.tasks.len()];
    let mut asked_for = vec![false; ws.tasks.len()];
    let mut queue: VecDeque<TaskIdx> = requested.iter().copied().collect();
    for &t in &requested {
      included[t.i()] = true;
      asked_for[t.i()] = true;
    }
    while let Some(t) = queue.pop_front() {
      for &d in &self.deps[t.i()] {
        if !included[d.i()] {
          included[d.i()] = true;
          queue.push_back(d);
        }
      }
    }

    let tasks: Vec<TaskIdx> = self
      .topo
      .iter()
      .copied()
      .filter(|t| included[t.i()])
      .collect();
    Ok(Selection {
      tasks,
      included,
      requested: asked_for,
    })
  }
}

/// The tasks of one run, topologically ordered.
#[derive(Debug, Clone)]
pub struct Selection {
  pub tasks: Vec<TaskIdx>,
  /// Membership by task index, for O(1) tests during scheduling.
  pub included: Vec<bool>,
  /// Which of those the command line actually named, as opposed to pulled in
  /// as a dependency. Only these take the arguments after `--`.
  pub requested: Vec<bool>,
}

/// The arguments after `--`, and the tasks they reach.
///
/// They go to the tasks the command line named, never to the dependencies
/// those tasks pulled in. `bld run build frontend -- --verbose` is a request
/// about frontend's build; handing `--verbose` to the schema build it happens
/// to need is as likely to break it as to help. turbo matches on the task
/// name instead, so there it would reach both.
///
/// They are hashed, because a task run with different arguments is a task
/// that produced something different.
#[derive(Debug, Clone, Default)]
pub struct TaskArgs {
  args: Vec<String>,
  /// By task index. Empty when there is nothing to place.
  requested: Vec<bool>,
}

impl TaskArgs {
  pub fn new(args: Vec<String>, selection: &Selection) -> Self {
    let requested = if args.is_empty() {
      Vec::new()
    } else {
      selection.requested.clone()
    };
    Self { args, requested }
  }

  pub fn for_task(&self, task: TaskIdx) -> &[String] {
    if self.requested.get(task.i()).copied().unwrap_or(false) {
      &self.args
    } else {
      &[]
    }
  }

  /// Rejects arguments that would reach nothing. A task without a command
  /// cannot take them, and silently dropping them hides a mistyped task name
  /// until someone wonders why the flag did nothing.
  pub fn check(&self, ws: &Workspace) -> Result<()> {
    if self.args.is_empty() {
      return Ok(());
    }
    let reaches = self
      .requested
      .iter()
      .enumerate()
      .any(|(i, &asked)| asked && ws.tasks[i].command.is_some());
    if !reaches {
      bail!(
        "the arguments after `--` reach no command: none of the tasks named \
         on the command line have one"
      );
    }
    Ok(())
  }
}

impl Selection {
  pub fn contains(&self, t: TaskIdx) -> bool {
    self.included[t.i()]
  }
}

/// Resolves one `depends_on` entry into zero or more tasks.
fn resolve_dep(ws: &Workspace, from: TaskIdx, spec: &str) -> Result<Vec<TaskIdx>> {
  let pkg = ws.task(from).pkg;
  if let Some(name) = spec.strip_prefix('^') {
    if name.is_empty() {
      bail!("`^` must be followed by a task name");
    }
    if name.contains('#') {
      bail!("`{spec}` is not valid; `^task` already means \"in each package dependency\"");
    }
    // Package dependencies that do not define the task are simply skipped,
    // so `^build` works in a workspace where only some packages build.
    return Ok(
      ws.pkg(pkg)
        .deps
        .iter()
        .filter_map(|&dep| ws.task_in(dep, name))
        .collect(),
    );
  }
  if let Some((pkg_name, task_name)) = spec.rsplit_once('#') {
    if pkg_name.is_empty() || task_name.is_empty() {
      bail!("`{spec}` is not a valid dependency; expected `package#task`");
    }
    let target_pkg = ws
      .package_by_name(pkg_name)
      .ok_or_else(|| anyhow!("dependency `{spec}` names unknown package `{pkg_name}`"))?;
    let task = ws
      .task_in(target_pkg, task_name)
      .ok_or_else(|| anyhow!("package `{pkg_name}` has no task `{task_name}`"))?;
    return Ok(vec![task]);
  }
  let task = ws.task_in(pkg, spec).ok_or_else(|| {
    anyhow!(
      "package `{}` has no task `{spec}`; use `package#{spec}` for another package",
      ws.pkg(pkg).name
    )
  })?;
  Ok(vec![task])
}

/// Depth-first post-order: every task lands after all of its dependencies.
fn topo_order(ws: &Workspace, deps: &[Vec<TaskIdx>]) -> Result<Vec<TaskIdx>> {
  const WHITE: u8 = 0;
  const GRAY: u8 = 1;
  const BLACK: u8 = 2;

  let n = deps.len();
  let mut color = vec![WHITE; n];
  let mut order = Vec::with_capacity(n);
  for start in 0..n {
    if color[start] != WHITE {
      continue;
    }
    let mut path: Vec<(TaskIdx, usize)> = vec![(TaskIdx(start as u32), 0)];
    color[start] = GRAY;
    while let Some(&mut (node, ref mut next)) = path.last_mut() {
      if *next < deps[node.i()].len() {
        let dep = deps[node.i()][*next];
        *next += 1;
        match color[dep.i()] {
          WHITE => {
            color[dep.i()] = GRAY;
            path.push((dep, 0));
          }
          GRAY => {
            let at = path.iter().position(|(n, _)| *n == dep).unwrap_or(0);
            let mut labels: Vec<&str> = path[at..]
              .iter()
              .map(|(n, _)| ws.task(*n).label.as_str())
              .collect();
            labels.push(ws.task(dep).label.as_str());
            bail!("task dependency cycle: {}", labels.join(" -> "));
          }
          _ => {}
        }
      } else {
        color[node.i()] = BLACK;
        order.push(node);
        path.pop();
      }
    }
  }
  Ok(order)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::{Fixture, assert_before, assert_contains, err, labels};

  /// core <- ui <- web, plus a root lint task and a persistent dev server.
  fn monorepo() -> Fixture {
    let fx = Fixture::new();
    fx.write(
      "bld.toml",
      "packages = [\"packages/*\"]\n[tasks.lint]\ncommand = \"lint\"\n",
    );
    fx.write("packages/core/bld.toml", "[tasks.build]\ncommand = \"b\"\n");
    fx.write(
      "packages/ui/bld.toml",
      r#"
        depends_on = ["core"]
        [tasks.build]
        command = "b"
        depends_on = ["^build"]
      "#,
    );
    fx.write(
      "packages/web/bld.toml",
      r#"
        depends_on = ["ui"]
        [tasks.build]
        command = "b"
        depends_on = ["^build", "codegen"]
        [tasks.codegen]
        command = "c"
        depends_on = ["core#build"]
        [tasks.dev]
        command = "d"
        persistent = true
        interruptible = true
        depends_on = ["^build"]
      "#,
    );
    fx
  }

  fn select(fx: &Fixture, sels: &[&str], filter: &[&str]) -> anyhow::Result<Vec<String>> {
    let (ws, graph) = fx.graph()?;
    let selectors: Vec<Selector> = sels.iter().map(|s| s.parse().unwrap()).collect();
    let filter: Vec<String> = filter.iter().map(|s| s.to_string()).collect();
    let sel = graph.select(&ws, &selectors, &filter)?;
    Ok(
      labels(&ws, &sel.tasks)
        .iter()
        .map(|s| s.to_string())
        .collect(),
    )
  }

  #[test]
  fn resolves_dependency_forms() {
    let fx = monorepo();
    let (ws, graph) = fx.graph().unwrap();
    let web_build = ws
      .task_in(ws.package_by_name("web").unwrap(), "build")
      .unwrap();
    let mut deps = labels(&ws, &graph.deps[web_build.i()]);
    deps.sort();
    assert_eq!(
      deps,
      ["ui#build", "web#codegen"],
      "^build and a same-package task"
    );

    let ui_build = ws
      .task_in(ws.package_by_name("ui").unwrap(), "build")
      .unwrap();
    assert_eq!(labels(&ws, &graph.deps[ui_build.i()]), ["core#build"]);

    let codegen = ws
      .task_in(ws.package_by_name("web").unwrap(), "codegen")
      .unwrap();
    assert_eq!(
      labels(&ws, &graph.deps[codegen.i()]),
      ["core#build"],
      "explicit pkg#task"
    );

    let core_build = ws
      .task_in(ws.package_by_name("core").unwrap(), "build")
      .unwrap();
    let mut rdeps = labels(&ws, &graph.rdeps[core_build.i()]);
    rdeps.sort();
    assert_eq!(rdeps, ["ui#build", "web#codegen"]);
  }

  #[test]
  fn caret_expands_to_nothing_without_package_deps() {
    let fx = Fixture::new();
    fx.write("bld.toml", "packages = [\"packages/*\"]\n");
    fx.write(
      "packages/core/bld.toml",
      "[tasks.build]\ncommand = \"b\"\ndepends_on = [\"^build\"]\n",
    );
    let (ws, graph) = fx.graph().unwrap();
    let build = ws
      .task_in(ws.package_by_name("core").unwrap(), "build")
      .unwrap();
    assert!(graph.deps[build.i()].is_empty());
  }

  #[test]
  fn caret_skips_package_deps_without_that_task() {
    let fx = Fixture::new();
    fx.write("bld.toml", "packages = [\"packages/*\"]\n");
    fx.write("packages/core/bld.toml", "[tasks.test]\ncommand = \"t\"\n");
    fx.write(
      "packages/web/bld.toml",
      "depends_on = [\"core\"]\n[tasks.build]\ncommand = \"b\"\ndepends_on = [\"^build\"]\n",
    );
    let (ws, graph) = fx.graph().unwrap();
    let build = ws
      .task_in(ws.package_by_name("web").unwrap(), "build")
      .unwrap();
    assert!(graph.deps[build.i()].is_empty());
  }

  #[test]
  fn topological_order_puts_dependencies_first() {
    let fx = monorepo();
    let order = select(&fx, &["build"], &[]).unwrap();
    assert_before(
      &order.iter().map(String::as_str).collect::<Vec<_>>(),
      "core#build",
      "ui#build",
    );
    assert_before(
      &order.iter().map(String::as_str).collect::<Vec<_>>(),
      "ui#build",
      "web#build",
    );
    assert_before(
      &order.iter().map(String::as_str).collect::<Vec<_>>(),
      "web#codegen",
      "web#build",
    );
  }

  #[test]
  fn selection_closes_over_dependencies_of_other_names() {
    let fx = monorepo();
    let mut order = select(&fx, &["web#build"], &[]).unwrap();
    order.sort();
    assert_eq!(
      order,
      ["core#build", "ui#build", "web#build", "web#codegen"]
    );
  }

  #[test]
  fn bare_selector_picks_every_package_defining_the_task() {
    let fx = monorepo();
    let mut order = select(&fx, &["build"], &[]).unwrap();
    order.sort();
    assert_eq!(
      order,
      ["core#build", "ui#build", "web#build", "web#codegen"]
    );
  }

  #[test]
  fn filter_restricts_bare_selectors_but_keeps_dependencies() {
    let fx = monorepo();
    let mut order = select(&fx, &["build"], &["ui"]).unwrap();
    order.sort();
    assert_eq!(order, ["core#build", "ui#build"]);
    assert_contains(
      &err(select(&fx, &["build"], &["ghost"])),
      "unknown package `ghost`",
    );
  }

  #[test]
  fn root_task_is_selectable() {
    let fx = monorepo();
    assert_eq!(select(&fx, &["//#lint"], &[]).unwrap(), ["//#lint"]);
  }

  #[test]
  fn unknown_selectors_are_errors() {
    let fx = monorepo();
    assert_contains(
      &err(select(&fx, &["ghost"], &[])),
      "no package defines task `ghost`",
    );
    assert_contains(
      &err(select(&fx, &["core#ghost"], &[])),
      "package `core` has no task `ghost`",
    );
    assert_contains(
      &err(select(&fx, &["ghost#build"], &[])),
      "unknown package `ghost`",
    );
  }

  #[test]
  fn persistent_tasks_may_depend_but_not_be_depended_on() {
    let fx = monorepo();
    // The dev server itself depends on builds, which is fine.
    let mut order = select(&fx, &["web#dev"], &[]).unwrap();
    order.sort();
    assert_eq!(order, ["core#build", "ui#build", "web#dev"]);

    fx.write(
      "packages/web/bld.toml",
      r#"
        [tasks.dev]
        command = "d"
        persistent = true
        [tasks.e2e]
        command = "e"
        depends_on = ["dev"]
      "#,
    );
    let msg = err(fx.graph());
    assert_contains(
      &msg,
      "task `web#e2e` depends on `web#dev`, which is persistent",
    );
  }

  #[test]
  fn rejects_task_cycles() {
    let fx = Fixture::new();
    fx.write("bld.toml", "packages = [\"packages/*\"]\n");
    fx.write(
      "packages/a/bld.toml",
      r#"
        [tasks.one]
        command = "x"
        depends_on = ["two"]
        [tasks.two]
        command = "x"
        depends_on = ["one"]
      "#,
    );
    let msg = err(fx.graph());
    assert_contains(&msg, "task dependency cycle");
    assert_contains(&msg, "a#one");
    assert_contains(&msg, "a#two");
  }

  #[test]
  fn rejects_self_dependency() {
    let fx = Fixture::new();
    fx.write("bld.toml", "packages = [\"packages/*\"]\n");
    fx.write(
      "packages/a/bld.toml",
      "[tasks.build]\ncommand = \"x\"\ndepends_on = [\"build\"]\n",
    );
    assert_contains(&err(fx.graph()), "depends on itself");
  }

  #[test]
  fn rejects_unknown_dependencies() {
    let fx = Fixture::new();
    fx.write("bld.toml", "packages = [\"packages/*\"]\n");
    fx.write(
      "packages/a/bld.toml",
      "[tasks.build]\ncommand = \"x\"\ndepends_on = [\"ghost\"]\n",
    );
    assert_contains(&err(fx.graph()), "package `a` has no task `ghost`");
    fx.write(
      "packages/a/bld.toml",
      "[tasks.build]\ncommand = \"x\"\ndepends_on = [\"ghost#build\"]\n",
    );
    assert_contains(&err(fx.graph()), "unknown package `ghost`");
    fx.write(
      "packages/a/bld.toml",
      "[tasks.build]\ncommand = \"x\"\ndepends_on = [\"^\"]\n",
    );
    assert_contains(&err(fx.graph()), "`^` must be followed by a task name");
  }

  #[test]
  fn parses_selectors() {
    assert_eq!(
      "build".parse::<Selector>().unwrap(),
      Selector {
        package: None,
        task: "build".into()
      }
    );
    assert_eq!(
      "web#build".parse::<Selector>().unwrap(),
      Selector {
        package: Some("web".into()),
        task: "build".into()
      }
    );
    assert_eq!(
      "//#lint".parse::<Selector>().unwrap(),
      Selector {
        package: Some("//".into()),
        task: "lint".into()
      }
    );
    assert!("web#".parse::<Selector>().is_err());
    assert!("".parse::<Selector>().is_err());
  }
}
