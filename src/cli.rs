//! Command line surface.

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

#[derive(Debug, Args)]
pub struct HashArgs {
  /// Tasks to hash: `build`, `web#build` or `//#lint`.
  #[arg(required = true, value_name = "TASK")]
  pub selectors: Vec<Selector>,
  /// Only consider these packages for bare task selectors.
  #[arg(long, value_name = "PACKAGE")]
  pub filter: Vec<String>,
  /// List every input file with its hash.
  #[arg(long)]
  pub files: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
  /// Tasks to run: `build`, `web#build` or `//#lint`.
  #[arg(required = true, value_name = "TASK")]
  pub selectors: Vec<Selector>,
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
  /// Tasks to run: `build`, `web#build` or `//#lint`.
  #[arg(required = true, value_name = "TASK")]
  pub selectors: Vec<Selector>,
  #[command(flatten)]
  pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct CommonArgs {
  /// Maximum number of tasks running at once.
  #[arg(long, short = 'c', value_name = "N")]
  pub concurrency: Option<usize>,
  /// Only run bare task selectors in these packages.
  #[arg(long, value_name = "PACKAGE")]
  pub filter: Vec<String>,
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

  #[test]
  fn parses_a_run_invocation() {
    let cli = Cli::parse_from(["bld", "run", "build", "web#test", "--force", "-c", "3"]);
    let Command::Run(args) = cli.command else {
      panic!("expected run")
    };
    assert!(args.force && !args.continue_on_fail);
    assert_eq!(args.common.concurrency, Some(3));
    assert_eq!(args.selectors.len(), 2);
    assert_eq!(args.selectors[1].package.as_deref(), Some("web"));
  }

  #[test]
  fn rejects_a_malformed_selector() {
    assert!(Cli::try_parse_from(["bld", "run", "web#"]).is_err());
    assert!(Cli::try_parse_from(["bld", "run"]).is_err());
  }
}
