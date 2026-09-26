//! Command line surface.

use std::ffi::OsString;

use anyhow::Result;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::graph::Selector;

#[derive(Debug, Parser)]
#[command(name = "bld", version, about = "A fast task runner for monorepos")]
pub struct Cli {
  #[command(subcommand)]
  pub command: Command,
}

impl Cli {
  /// Parses the command line, treating a first argument that is not one of
  /// our commands as the start of a `run`, so `bld build` means
  /// `bld run build`. A task named like a command still needs the `run`.
  pub fn parse_with_implicit_run() -> Self {
    Self::parse_from(implicit_run(std::env::args_os().collect()))
  }
}

/// Inserts `run` after the program name, unless the first argument is a
/// flag (`--help`, `--version`) or names a command or one of its aliases.
fn implicit_run(mut argv: Vec<OsString>) -> Vec<OsString> {
  let Some(first) = argv.get(1).and_then(|arg| arg.to_str()) else {
    return argv;
  };
  if first.starts_with('-') || first == "help" {
    return argv;
  }
  let cmd = Cli::command();
  let builtin = cmd
    .get_subcommands()
    .any(|sub| sub.get_name() == first || sub.get_all_aliases().any(|alias| alias == first));
  if !builtin {
    argv.insert(1, "run".into());
  }
  argv
}

#[derive(Debug, Subcommand)]
pub enum Command {
  /// Run tasks and their dependencies.
  #[command(visible_alias = "r")]
  Run(RunArgs),
  /// Print the input hash of each task, to see why something re-ran.
  Hash(HashArgs),
  /// Delete the local cache.
  Clean,
}

/// What to act on: which tasks, and in which packages.
///
/// Two positional arguments rather than a list and a flag, because the pair
/// is what a person means: `bld run lint,check frontend,backend` is "these
/// tasks, those packages".
#[derive(Debug, Args)]
pub struct Targets {
  /// Tasks, comma separated: `build`, `lint,check`, or `web#build` to name
  /// one package directly. Lists the available tasks, if left out.
  #[arg(value_name = "TASKS")]
  pub tasks: Option<String>,
  /// Packages the bare task names apply to, comma separated. Every package
  /// that defines the task, if left out.
  #[arg(value_name = "PACKAGES")]
  pub packages: Option<String>,
}

impl Targets {
  /// Whether any tasks were named; without them there is nothing to select,
  /// only a list of what could be.
  pub fn is_empty(&self) -> bool {
    self.tasks.is_none()
  }

  pub fn selectors(&self) -> Result<Vec<Selector>> {
    let Some(tasks) = &self.tasks else {
      anyhow::bail!("no task given");
    };
    split(tasks, "task")?
      .iter()
      .map(|entry| entry.parse())
      .collect()
  }

  pub fn filter(&self) -> Result<Vec<String>> {
    match &self.packages {
      Some(list) => {
        let names = split(list, "package")?;
        for name in &names {
          crate::workspace::validate_name("package", name)?;
        }
        Ok(names)
      }
      None => Ok(Vec::new()),
    }
  }
}

/// Splits a comma separated list, rejecting the empty entries that a stray
/// comma leaves behind rather than quietly ignoring them.
fn split(list: &str, kind: &str) -> Result<Vec<String>> {
  if list.is_empty() {
    anyhow::bail!("no {kind} given");
  }
  list
    .split(',')
    .map(|entry| {
      if entry.is_empty() {
        anyhow::bail!("empty {kind} in `{list}`");
      }
      Ok(entry.to_string())
    })
    .collect()
}

#[derive(Debug, Args)]
pub struct HashArgs {
  #[command(flatten)]
  pub targets: Targets,
  /// Arguments passed to the tasks named above, after `--`.
  #[arg(last = true, value_name = "ARG")]
  pub args: Vec<String>,
  /// List every input file with its hash.
  #[arg(long)]
  pub files: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
  #[command(flatten)]
  pub targets: Targets,
  #[command(flatten)]
  pub common: CommonArgs,
  /// Ignore cached results and run every task.
  #[arg(long)]
  pub force: bool,
  /// Keep going after a task fails instead of stopping the run. Watching
  /// always does.
  #[arg(long = "continue")]
  pub continue_on_fail: bool,
  /// Keep running, and re-run tasks whenever their inputs change. Implied
  /// when a persistent task is selected.
  #[arg(long, short = 'w')]
  pub watch: bool,
  /// Print the tasks that would run, in order, and exit.
  #[arg(long)]
  pub dry_run: bool,
  /// Arguments passed to the tasks named above, after `--`.
  #[arg(last = true, value_name = "ARG")]
  pub args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct CommonArgs {
  /// Maximum number of tasks running at once.
  #[arg(long, short = 'c', value_name = "N")]
  pub concurrency: Option<usize>,
  /// When to colorize output.
  #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
  pub color: ColorChoice,
  /// Pad task labels to the longest one, so output lines up in a column.
  #[arg(long, overrides_with = "no_align_output")]
  pub align_output: bool,
  /// Don't pad task labels, even if the user config asks for it.
  #[arg(long, overrides_with = "align_output")]
  pub no_align_output: bool,
}

impl CommonArgs {
  /// Whether to align output, with the flags taking precedence over the
  /// user config's `default`.
  pub fn align_output(&self, default: bool) -> bool {
    match (self.align_output, self.no_align_output) {
      (true, _) => true,
      (_, true) => false,
      _ => default,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
  Auto,
  Always,
  Never,
}

#[cfg(test)]
mod tests {
  use clap::CommandFactory;

  use super::*;

  #[test]
  fn cli_is_well_formed() {
    Cli::command().debug_assert();
  }

  fn run_args(argv: &[&str]) -> RunArgs {
    let cli = Cli::parse_from([&["bld", "run"], argv].concat());
    let Command::Run(args) = cli.command else {
      panic!("expected run")
    };
    args
  }

  #[test]
  fn parses_a_run_invocation() {
    let args = run_args(&["lint", "--force", "-c", "3"]);
    assert!(args.force && !args.continue_on_fail && !args.watch);
    assert_eq!(args.common.concurrency, Some(3));
    assert_eq!(
      args.targets.selectors().unwrap(),
      vec!["lint".parse::<Selector>().unwrap()]
    );
    assert!(args.targets.filter().unwrap().is_empty());
  }

  #[test]
  fn parses_lists_of_tasks_and_packages() {
    let args = run_args(&["lint,check", "frontend,backend"]);
    let tasks: Vec<String> = args
      .targets
      .selectors()
      .unwrap()
      .iter()
      .map(|s| s.to_string())
      .collect();
    assert_eq!(tasks, ["lint", "check"]);
    assert_eq!(args.targets.filter().unwrap(), ["frontend", "backend"]);
  }

  #[test]
  fn a_package_qualified_task_still_works() {
    let args = run_args(&["web#build,//#lint"]);
    let selectors = args.targets.selectors().unwrap();
    assert_eq!(selectors[0].package.as_deref(), Some("web"));
    assert_eq!(selectors[1].package.as_deref(), Some("//"));
  }

  #[test]
  fn align_flags_override_the_config() {
    assert!(!run_args(&["lint"]).common.align_output(false));
    assert!(run_args(&["lint"]).common.align_output(true));
    assert!(
      run_args(&["lint", "--align-output"])
        .common
        .align_output(false)
    );
    assert!(
      !run_args(&["lint", "--no-align-output"])
        .common
        .align_output(true)
    );
    // The last one given wins.
    let args = run_args(&["lint", "--align-output", "--no-align-output"]);
    assert!(!args.common.align_output(true));
  }

  #[test]
  fn short_aliases_and_no_targets() {
    let cli = Cli::parse_from(["bld", "r", "lint"]);
    assert!(matches!(cli.command, Command::Run(_)));
    let args = run_args(&["-w"]);
    assert!(args.watch && args.targets.is_empty());
    assert!(run_args(&["lint", "--watch"]).watch);
  }

  fn implicit(argv: &[&str]) -> Vec<String> {
    implicit_run(argv.iter().map(OsString::from).collect())
      .into_iter()
      .map(|arg| arg.into_string().unwrap())
      .collect()
  }

  #[test]
  fn a_bare_task_implies_run() {
    assert_eq!(
      implicit(&["bld", "lint", "web", "--force"]),
      ["bld", "run", "lint", "web", "--force"]
    );
    assert_eq!(implicit(&["bld", "web#build"]), ["bld", "run", "web#build"]);
    for argv in [
      &["bld"][..],
      &["bld", "run", "lint"],
      &["bld", "r", "lint"],
      &["bld", "hash", "lint"],
      &["bld", "clean"],
      &["bld", "help"],
      &["bld", "--help"],
      &["bld", "-V"],
    ] {
      assert_eq!(implicit(argv), argv, "{argv:?}");
    }
  }

  #[test]
  fn rejects_malformed_lists() {
    let err = |argv: &[&str]| run_args(argv);
    assert!(err(&["lint,"]).targets.selectors().is_err());
    assert!(err(&[",lint"]).targets.selectors().is_err());
    assert!(err(&["web#"]).targets.selectors().is_err());
    assert!(err(&["lint", "front end"]).targets.filter().is_err());
    // A package list is not the place for a task selector.
    assert!(err(&["lint", "web#build"]).targets.filter().is_err());
  }
}
