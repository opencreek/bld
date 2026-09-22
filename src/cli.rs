//! Command line surface.

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::graph::Selector;

#[derive(Debug, Parser)]
#[command(name = "bld", version, about = "A fast task runner for monorepos")]
pub struct Cli {
  #[command(subcommand)]
  pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
  /// Run tasks and their dependencies.
  Run(RunArgs),
  /// Run tasks, then re-run them whenever their inputs change.
  Watch(WatchArgs),
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
  /// one package directly.
  #[arg(value_name = "TASKS")]
  pub tasks: String,
  /// Packages the bare task names apply to, comma separated. Every package
  /// that defines the task, if left out.
  #[arg(value_name = "PACKAGES")]
  pub packages: Option<String>,
}

impl Targets {
  pub fn selectors(&self) -> Result<Vec<Selector>> {
    split(&self.tasks, "task")?
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
  /// Keep going after a task fails instead of stopping the run.
  #[arg(long = "continue")]
  pub continue_on_fail: bool,
  /// Print the tasks that would run, in order, and exit.
  #[arg(long)]
  pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct WatchArgs {
  #[command(flatten)]
  pub targets: Targets,
  #[command(flatten)]
  pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct CommonArgs {
  /// Maximum number of tasks running at once.
  #[arg(long, short = 'c', value_name = "N")]
  pub concurrency: Option<usize>,
  /// When to colorize output.
  #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
  pub color: ColorChoice,
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
    assert!(args.force && !args.continue_on_fail);
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
  fn rejects_malformed_lists() {
    assert!(Cli::try_parse_from(["bld", "run"]).is_err());
    let err = |argv: &[&str]| run_args(argv);
    assert!(err(&["lint,"]).targets.selectors().is_err());
    assert!(err(&[",lint"]).targets.selectors().is_err());
    assert!(err(&["web#"]).targets.selectors().is_err());
    assert!(err(&["lint", "front end"]).targets.filter().is_err());
    // A package list is not the place for a task selector.
    assert!(err(&["lint", "web#build"]).targets.filter().is_err());
  }
}
