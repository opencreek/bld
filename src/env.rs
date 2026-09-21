//! Environment variable filtering.
//!
//! Only three groups of variables reach a task's command: a small base
//! allowlist, the `env` vars (whose values are also hashed into the task's
//! input hash), and the `pass_through_env` vars (not hashed). Everything else
//! is dropped, so a task cannot accidentally depend on ambient state.

use std::ffi::{OsStr, OsString};

use anyhow::{Result, bail};

/// Variables always passed through, so commands can find a shell, a home
/// directory and a locale.
fn is_base(name: &str) -> bool {
  matches!(
    name,
    "PATH" | "HOME" | "SHELL" | "TMPDIR" | "USER" | "TERM" | "LANG" | "TZ"
  ) || name.starts_with("LC_")
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

  #[test]
  fn matching_is_sorted_and_scoped() {
    let snap = snapshot(&[("B", "2"), ("A", "1"), ("OTHER", "x")]);
    let hashed = parse(&pats(&["A", "B"])).unwrap();
    let got: Vec<&str> = snap.matching(&hashed).into_iter().map(|(k, _)| k).collect();
    assert_eq!(got, ["A", "B"]);
  }
}
