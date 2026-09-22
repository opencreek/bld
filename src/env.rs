//! Environment variable filtering.
//!
//! Only three groups of variables reach a task's command: a small base
//! allowlist, the `env` vars (whose values are also hashed into the task's
//! input hash), and the `pass_through_env` vars (not hashed). Everything else
//! is dropped, so a task cannot accidentally depend on ambient state.

use std::ffi::{OsStr, OsString};

use anyhow::{Result, bail};

/// Variables always passed through, whatever a task declares.
///
/// None of these are hashed. They describe the machine a build runs on --
/// where its tools live, how to reach its local daemons -- not what is being
/// built, so two machines that differ only here should still share a cache.
/// The list follows turbo's builtin passthrough, minus what is specific to
/// Windows or to one vendor's hosting; see `BASE_PREFIX` for the wildcards.
///
/// Credentials are deliberately absent. A task that needs a token, or the SSH
/// agent, declares it in `pass_through_env`: handing every task an ambient
/// credential is not something a build tool should do silently.
const BASE_EXACT: &[&str] = &[
  // Who and where we are.
  "HOME",
  "USER",
  "LOGNAME",
  "SHELL",
  "TZ",
  "LANG",
  // Where the machine keeps things.
  "PATH",
  "TMPDIR",
  "TMP",
  "TEMP",
  // Dynamic linking. On NixOS these are what lets a binary start at all.
  "LD_LIBRARY_PATH",
  "LD_PRELOAD",
  "DYLD_FALLBACK_LIBRARY_PATH",
  "DYLD_INSERT_LIBRARIES",
  "LIBPATH",
  // Terminal capabilities, so a task colours its output as bld does.
  "TERM",
  "TERM_PROGRAM",
  "COLORTERM",
  "NO_COLOR",
  "FORCE_COLOR",
  "CLICOLOR_FORCE",
  // The desktop session, for anything that talks to it: a browser under test,
  // a keyring, a notifier.
  "DISPLAY",
  "WAYLAND_DISPLAY",
  "XAUTHORITY",
  "DBUS_SESSION_BUS_ADDRESS",
  // Where a package manager keeps its store. Not its credentials.
  "PNPM_HOME",
  "NPM_CONFIG_PREFIX",
  "NPM_CONFIG_STORE_DIR",
  "NPM_CONFIG_CACHE",
  // Changes how a runtime starts, not what it produces. Passed for the same
  // reason turbo passes it: a memory limit set in a shell has to survive.
  "NODE_OPTIONS",
];

/// Prefixes always passed through. Same rules as [`BASE_EXACT`].
const BASE_PREFIX: &[&str] = &[
  // Locale.
  "LC_",
  // Session directories: runtime (where a rootless daemon puts its socket),
  // data, config, cache, state.
  "XDG_",
  // Nix and NixOS put compiler and loader configuration here.
  "NIX_",
  "__NIXOS_",
  // Local container daemons. DOCKER_HOST is how a rootless install says its
  // socket is not in /var/run.
  "DOCKER_",
  "BUILDKIT_",
  "BUILDX_",
  "COMPOSE_",
  // Corepack decides which package manager version runs.
  "COREPACK_",
];

fn is_base(name: &str) -> bool {
  BASE_EXACT.contains(&name) || BASE_PREFIX.iter().any(|p| name.starts_with(p))
}

/// An env var name matcher: an exact name or a trailing-`*` prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvPattern {
  Exact(String),
  Prefix(String),
}

impl EnvPattern {
  pub fn matches(&self, name: &str) -> bool {
    match self {
      Self::Exact(n) => n == name,
      Self::Prefix(p) => name.starts_with(p.as_str()),
    }
  }
}

/// Parses env patterns, rejecting `*` anywhere but at the end.
pub fn parse(patterns: &[String]) -> Result<Vec<EnvPattern>> {
  patterns
    .iter()
    .map(|p| match p.strip_suffix('*') {
      Some(prefix) if !prefix.contains('*') => Ok(EnvPattern::Prefix(prefix.to_string())),
      None if !p.contains('*') => Ok(EnvPattern::Exact(p.clone())),
      _ => bail!("env pattern `{p}` may only use `*` as a trailing wildcard"),
    })
    .collect()
}

/// A snapshot of the process environment, taken once per run so every task
/// sees the same values.
#[derive(Debug, Clone, Default)]
pub struct EnvSnapshot {
  /// Sorted by name. Names that are not valid UTF-8 are dropped.
  vars: Vec<(String, OsString)>,
}

impl EnvSnapshot {
  pub fn capture() -> Self {
    Self::from_iter(std::env::vars_os())
  }

  fn from_iter(it: impl Iterator<Item = (OsString, OsString)>) -> Self {
    let mut vars: Vec<(String, OsString)> = it
      .filter_map(|(k, v)| k.into_string().ok().map(|k| (k, v)))
      .collect();
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    Self { vars }
  }

  /// The `(name, value)` pairs matched by `patterns`, sorted by name. These
  /// go into the task's input hash and are passed to the command.
  pub fn matching(&self, patterns: &[EnvPattern]) -> Vec<(&str, &OsStr)> {
    self
      .vars
      .iter()
      .filter(|(name, _)| patterns.iter().any(|p| p.matches(name)))
      .map(|(name, value)| (name.as_str(), value.as_os_str()))
      .collect()
  }

  /// The full environment for a task: base allowlist plus hashed `env` plus
  /// `pass_through_env`, and nothing else.
  pub fn child_env(
    &self,
    hashed: &[EnvPattern],
    passed: &[EnvPattern],
  ) -> Vec<(OsString, OsString)> {
    self
      .vars
      .iter()
      .filter(|(name, _)| {
        is_base(name)
          || hashed.iter().any(|p| p.matches(name))
          || passed.iter().any(|p| p.matches(name))
      })
      .map(|(name, value)| (OsString::from(name), value.clone()))
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn pats(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
  }

  fn child_names(snap: &EnvSnapshot, hashed: &[EnvPattern], passed: &[EnvPattern]) -> Vec<String> {
    snap
      .child_env(hashed, passed)
      .into_iter()
      .map(|(k, _)| k.into_string().unwrap())
      .collect()
  }

  fn snapshot(pairs: &[(&str, &str)]) -> EnvSnapshot {
    EnvSnapshot::from_iter(
      pairs
        .iter()
        .map(|(k, v)| (OsString::from(*k), OsString::from(*v))),
    )
  }

  #[test]
  fn parses_patterns() {
    let parsed = parse(&pats(&["CI", "VITE_*"])).unwrap();
    assert_eq!(
      parsed,
      vec![
        EnvPattern::Exact("CI".into()),
        EnvPattern::Prefix("VITE_".into())
      ]
    );
  }

  #[test]
  fn rejects_inner_wildcards() {
    assert!(parse(&pats(&["A*B"])).is_err());
    assert!(parse(&pats(&["*A"])).is_err());
    assert!(parse(&pats(&["A**"])).is_err());
    assert!(parse(&pats(&["A*"])).is_ok());
  }

  #[test]
  fn child_env_is_restricted_to_allowlists() {
    let snap = snapshot(&[
      ("PATH", "/bin"),
      ("LC_ALL", "C"),
      ("SECRET", "shh"),
      ("CI", "1"),
      ("VITE_API", "x"),
      ("VITE_OTHER", "y"),
    ]);
    let hashed = parse(&pats(&["CI"])).unwrap();
    let passed = parse(&pats(&["VITE_*"])).unwrap();
    let names: Vec<String> = snap
      .child_env(&hashed, &passed)
      .into_iter()
      .map(|(k, _)| k.into_string().unwrap())
      .collect();
    assert_eq!(names, ["CI", "LC_ALL", "PATH", "VITE_API", "VITE_OTHER"]);
  }

  /// A task talks to the machine's daemons and toolchain without declaring
  /// anything. Rootless Docker is the case that made this necessary: its
  /// socket is not in /var/run, and DOCKER_HOST is the only thing that says so.
  #[test]
  fn the_machine_reaches_a_task_without_being_declared() {
    let snap = snapshot(&[
      ("DOCKER_HOST", "unix:///run/user/1000/docker.sock"),
      ("BUILDKIT_HOST", "x"),
      ("COMPOSE_PROJECT_NAME", "app"),
      ("XDG_RUNTIME_DIR", "/run/user/1000"),
      ("NIX_LD", "/nix/store/ld"),
      ("__NIXOS_SET_ENVIRONMENT_DONE", "1"),
      ("COREPACK_ENABLE_AUTO_PIN", "0"),
      ("PNPM_HOME", "/home/u/.local/share/pnpm"),
      ("LD_LIBRARY_PATH", "/nix/store/lib"),
      ("COLORTERM", "truecolor"),
    ]);
    let names = child_names(&snap, &[], &[]);
    assert_eq!(names.len(), 10, "every one of them should pass: {names:?}");
  }

  /// Ambient credentials are not part of the bargain. A task that wants one
  /// says so.
  #[test]
  fn credentials_and_unrelated_variables_are_still_dropped() {
    let snap = snapshot(&[
      ("SSH_AUTH_SOCK", "/run/user/1000/ssh-agent"),
      ("AWS_SECRET_ACCESS_KEY", "shh"),
      ("GITLAB_NPM_TOKEN", "shh"),
      ("NPM_CONFIG_//registry.npmjs.org/:_authToken", "shh"),
      ("EDITOR", "vim"),
      ("DOCKER_HOST", "unix:///x"),
    ]);
    assert_eq!(child_names(&snap, &[], &[]), ["DOCKER_HOST"]);
    // Declaring one is all it takes.
    let passed = parse(&pats(&["SSH_AUTH_SOCK"])).unwrap();
    assert_eq!(
      child_names(&snap, &[], &passed),
      ["DOCKER_HOST", "SSH_AUTH_SOCK"]
    );
  }

  /// The allowlist describes the machine, not the build, so none of it may
  /// reach a hash: widening it must not invalidate a single cache entry.
  #[test]
  fn the_allowlist_is_never_hashed() {
    let snap = snapshot(&[("DOCKER_HOST", "unix:///x"), ("CI", "1")]);
    let declared = parse(&pats(&["CI"])).unwrap();
    let hashed: Vec<&str> = snap
      .matching(&declared)
      .into_iter()
      .map(|(k, _)| k)
      .collect();
    assert_eq!(hashed, ["CI"]);
  }

  #[test]
  fn matching_is_sorted_and_scoped() {
    let snap = snapshot(&[("B", "2"), ("A", "1"), ("OTHER", "x")]);
    let hashed = parse(&pats(&["A", "B"])).unwrap();
    let got: Vec<&str> = snap.matching(&hashed).into_iter().map(|(k, _)| k).collect();
    assert_eq!(got, ["A", "B"]);
  }
}
