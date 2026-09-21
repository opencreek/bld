//! Glob compilation for task inputs and outputs.
//!
//! Patterns are relative to the package directory. A leading `!` excludes.
//! Globs are built with `literal_separator(true)`, so `*` never crosses a
//! `/` boundary: write `src/**/*.ts`, not `src/*.ts`, to match recursively.
//! A pattern with no glob metacharacters also matches everything underneath
//! it, so `outputs = ["dist"]` behaves like `dist/**`.

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};

/// Compiled include/exclude pattern pair.
#[derive(Debug, Clone)]
pub struct Globs {
  include: GlobSet,
  exclude: GlobSet,
  /// The patterns as written, so a subtree walk can re-root them.
  patterns: Vec<String>,
  /// True when there are no include patterns at all (matches nothing).
  empty: bool,
}

impl Globs {
  /// Compiles `patterns`, where entries starting with `!` are exclusions.
  pub fn new(patterns: &[String]) -> Result<Self> {
    let mut include = GlobSetBuilder::new();
    let mut exclude = GlobSetBuilder::new();
    let mut n_include = 0usize;
    for pat in patterns {
      let (raw, set, counted) = match pat.strip_prefix('!') {
        Some(rest) => (rest, &mut exclude, false),
        None => (pat.as_str(), &mut include, true),
      };
      validate(raw)?;
      set.add(compile(raw)?);
      // A bare literal such as `dist` also covers everything below it.
      if !has_meta(raw) {
        set.add(compile(&format!("{}/**", raw.trim_end_matches('/')))?);
      }
      if counted {
        n_include += 1;
      }
    }
    Ok(Self {
      include: include.build()?,
      exclude: exclude.build()?,
      patterns: patterns.to_vec(),
      empty: n_include == 0,
    })
  }

  /// The union of several pattern lists, keeping only inclusions. Used to
  /// decide, in one walk, which files any task of a package might want.
  pub fn union<'a>(lists: impl Iterator<Item = &'a [String]>) -> Result<Self> {
    let mut merged: Vec<String> = Vec::new();
    for list in lists {
      for pat in list {
        // Exclusions are per task, so a union must not apply them.
        if !pat.starts_with('!') && !merged.contains(pat) {
          merged.push(pat.clone());
        }
      }
    }
    Self::new(&merged)
  }

  /// The same patterns rewritten for paths relative to `prefix`. Patterns
  /// that cannot match anything under `prefix` are dropped.
  pub fn strip_prefix(&self, prefix: &str) -> Result<Self> {
    if prefix.is_empty() {
      return Ok(self.clone());
    }
    let mut out = Vec::new();
    for pat in &self.patterns {
      let (bang, body) = match pat.strip_prefix('!') {
        Some(rest) => ("!", rest),
        None => ("", pat.as_str()),
      };
      if body == prefix {
        out.push(format!("{bang}**"));
      } else if let Some(rest) = body.strip_prefix(&format!("{prefix}/")) {
        out.push(format!("{bang}{rest}"));
      }
    }
    Self::new(&out)
  }

  /// Compiles patterns that are all inclusions (`!` is rejected).
  pub fn includes_only(patterns: &[String]) -> Result<Self> {
    for pat in patterns {
      if pat.starts_with('!') {
        bail!("`!` exclusion patterns are not allowed here: `{pat}`");
      }
    }
    Self::new(patterns)
  }

  /// Whether `rel` (a path relative to the package dir) is matched.
  pub fn is_match(&self, rel: &std::path::Path) -> bool {
    !self.empty && self.include.is_match(rel) && !self.exclude.is_match(rel)
  }

  /// Whether any include pattern exists.
  pub fn is_empty(&self) -> bool {
    self.empty
  }
}

fn compile(pat: &str) -> Result<Glob> {
  GlobBuilder::new(pat)
    .literal_separator(true)
    .build()
    .with_context(|| format!("invalid glob `{pat}`"))
}

fn has_meta(pat: &str) -> bool {
  pat.contains(['*', '?', '[', '{'])
}

/// Rejects patterns that would reach outside the package directory.
pub fn validate(pat: &str) -> Result<()> {
  if pat.is_empty() {
    bail!("empty glob pattern");
  }
  if pat.starts_with('/') {
    bail!("glob `{pat}` must be relative to the package directory");
  }
  if pat.split('/').any(|c| c == "..") {
    bail!(
      "glob `{pat}` must stay inside the package directory; make the other \
       directory a package and add it to `depends_on`, or use root `inputs`"
    );
  }
  Ok(())
}

/// The leading literal directory of a glob, used as a walk root.
/// `src/**/*.ts` -> `src`, `dist` -> `dist`, `*.ts` -> ``.
pub fn literal_prefix(pat: &str) -> &str {
  let end = match pat.find(['*', '?', '[', '{']) {
    None => return pat.trim_end_matches('/'),
    Some(i) => i,
  };
  match pat[..end].rfind('/') {
    Some(slash) => &pat[..slash],
    None => "",
  }
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use super::*;

  fn globs(pats: &[&str]) -> Globs {
    Globs::new(&pats.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
  }

  #[test]
  fn star_does_not_cross_separators() {
    let g = globs(&["src/*.ts"]);
    assert!(g.is_match(Path::new("src/a.ts")));
    assert!(!g.is_match(Path::new("src/nested/b.ts")));
    assert!(globs(&["src/**/*.ts"]).is_match(Path::new("src/nested/b.ts")));
  }

  #[test]
  fn double_star_forms() {
    assert!(globs(&["**"]).is_match(Path::new("a/b/c.ts")));
    assert!(globs(&["src/**"]).is_match(Path::new("src/a/b.ts")));
    assert!(globs(&["**/dist/**"]).is_match(Path::new("pkg/dist/x.js")));
    assert!(!globs(&["dist/**"]).is_match(Path::new("pkg/dist/x.js")));
  }

  #[test]
  fn literal_pattern_covers_subtree() {
    let g = globs(&["dist"]);
    assert!(g.is_match(Path::new("dist")));
    assert!(g.is_match(Path::new("dist/a/b.js")));
  }

  #[test]
  fn negation_excludes() {
    let g = globs(&["src/**", "!src/**/*.test.ts"]);
    assert!(g.is_match(Path::new("src/a.ts")));
    assert!(!g.is_match(Path::new("src/a.test.ts")));
    assert!(!g.is_match(Path::new("src/deep/a.test.ts")));
  }

  #[test]
  fn negation_only_matches_nothing() {
    let g = globs(&["!src/**"]);
    assert!(g.is_empty());
    assert!(!g.is_match(Path::new("src/a.ts")));
  }

  #[test]
  fn rejects_escaping_patterns() {
    assert!(validate("../shared/**").is_err());
    assert!(validate("/abs/path").is_err());
    assert!(validate("a/../b").is_err());
    assert!(validate("src/**").is_ok());
    assert!(Globs::includes_only(&["!x".to_string()]).is_err());
  }

  #[test]
  fn union_keeps_inclusions_from_every_list() {
    let a = vec!["src/**".to_string(), "!src/x".to_string()];
    let b = vec!["assets/**".to_string()];
    let u = Globs::union([a.as_slice(), b.as_slice()].into_iter()).unwrap();
    assert!(u.is_match(Path::new("src/a.ts")));
    assert!(
      u.is_match(Path::new("src/x")),
      "one task's exclusion is another's input"
    );
    assert!(u.is_match(Path::new("assets/logo.svg")));
    assert!(!u.is_match(Path::new("docs/readme.md")));
  }

  #[test]
  fn strip_prefix_rewrites_patterns_for_a_subtree() {
    let g = globs(&["config/**/*.json", "config", "src/**"]);
    let sub = g.strip_prefix("config").unwrap();
    assert!(sub.is_match(Path::new("a.json")));
    assert!(
      sub.is_match(Path::new("nested/deep.txt")),
      "a bare literal covers the subtree"
    );
    let sub = globs(&["config/a.json"]).strip_prefix("config").unwrap();
    assert!(sub.is_match(Path::new("a.json")));
    assert!(!sub.is_match(Path::new("b.json")));
  }

  #[test]
  fn literal_prefixes() {
    assert_eq!(literal_prefix("src/**/*.ts"), "src");
    assert_eq!(literal_prefix("dist/**"), "dist");
    assert_eq!(literal_prefix("dist"), "dist");
    assert_eq!(literal_prefix("dist/"), "dist");
    assert_eq!(literal_prefix("*.ts"), "");
    assert_eq!(literal_prefix("**"), "");
    assert_eq!(literal_prefix("a/b/c.txt"), "a/b/c.txt");
  }
}
