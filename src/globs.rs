//! Glob compilation for task inputs and outputs.
//!
//! Patterns are relative to the package directory. A leading `!` excludes.
//! An input pattern may start with `../` to reach outside its package, up to
//! the workspace root; [`resolve_input`] turns it into one relative to the
//! root.
//! Globs are built with `literal_separator(true)`, so `*` never crosses a
//! `/` boundary: write `src/**/*.ts`, not `src/*.ts`, to match recursively.
//! A pattern with no glob metacharacters also matches everything underneath
//! it, so `outputs = ["dist"]` behaves like `dist/**`.

use std::path::Path;

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

  /// Whether something under the directory `rel` could be matched, or `rel`
  /// lies under what could be. A directory event says nothing about which of
  /// its files changed, so this is as precise as it gets.
  pub fn reaches(&self, rel: &Path) -> bool {
    self
      .patterns
      .iter()
      .filter(|p| !p.starts_with('!'))
      .map(|p| Path::new(literal_prefix(p)))
      .any(|prefix| prefix.starts_with(rel) || rel.starts_with(prefix))
  }
}

/// Where a task's input pattern points, after resolving its leading `../`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
  /// Relative to the package directory.
  Package(String),
  /// Relative to the workspace root, outside the package.
  Workspace(String),
}

/// Resolves a task's input pattern against the package's directory, `pkg`,
/// relative to the workspace root. Leading `../` components climb out of the
/// package, but never above the root: that is where the workspace, and its
/// root `bld.toml`, ends. A pattern that climbs back into its own package is
/// an ordinary package pattern again, so that the package's files are only
/// ever matched relative to it.
pub fn resolve_input(pat: &str, pkg: &Path) -> Result<Input> {
  let (bang, body) = match pat.strip_prefix('!') {
    Some(rest) => ("!", rest),
    None => ("", pat),
  };
  let mut rest = body;
  let mut up = 0usize;
  loop {
    if let Some(r) = rest.strip_prefix("../") {
      rest = r;
    } else if rest == ".." {
      rest = "";
    } else {
      break;
    }
    up += 1;
  }
  if up == 0 {
    validate(body)?;
    return Ok(Input::Package(pat.to_string()));
  }

  let dirs: Vec<&str> = pkg
    .components()
    .map(|c| c.as_os_str().to_str().unwrap_or_default())
    .collect();
  if up > dirs.len() {
    bail!(
      "glob `{pat}` reaches above the workspace root; `..` only climbs as far \
       as the directory of the root bld.toml"
    );
  }
  if rest.split('/').any(|c| c == "..") {
    bail!("glob `{pat}` may only use `..` at its start");
  }
  // `..` on its own names a whole directory, like any literal pattern.
  let rest = if rest.is_empty() { "**" } else { rest };
  validate(rest)?;

  let base = dirs[..dirs.len() - up].join("/");
  let full = if base.is_empty() {
    rest.to_string()
  } else {
    format!("{base}/{rest}")
  };
  let own = dirs.join("/");
  if full == own {
    return Ok(Input::Package(format!("{bang}**")));
  }
  if let Some(inside) = full.strip_prefix(&format!("{own}/")) {
    return Ok(Input::Package(format!("{bang}{inside}")));
  }
  Ok(Input::Workspace(format!("{bang}{full}")))
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
      "glob `{pat}` must stay inside the package directory; only a task's \
       `inputs` may start with `../`"
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
  fn inputs_climb_out_of_their_package_to_the_root() {
    let pkg = Path::new("apps/web");
    let resolve = |pat| resolve_input(pat, pkg).unwrap();
    assert_eq!(resolve("src/**"), Input::Package("src/**".into()));
    assert_eq!(
      resolve("../shared/**"),
      Input::Workspace("apps/shared/**".into())
    );
    assert_eq!(
      resolve("!../shared/x"),
      Input::Workspace("!apps/shared/x".into())
    );
    assert_eq!(
      resolve("../../Cargo.toml"),
      Input::Workspace("Cargo.toml".into())
    );
    assert_eq!(resolve(".."), Input::Workspace("apps/**".into()));
    assert_eq!(resolve("../"), Input::Workspace("apps/**".into()));
    assert_eq!(resolve("../.."), Input::Workspace("**".into()));
    // Back into the package, where its files are matched relative to it.
    assert_eq!(resolve("../web/src/**"), Input::Package("src/**".into()));
    assert_eq!(resolve("../web"), Input::Package("**".into()));
    // A sibling whose name starts like the package's is not the package.
    assert_eq!(
      resolve("../web2/**"),
      Input::Workspace("apps/web2/**".into())
    );
  }

  #[test]
  fn inputs_never_climb_above_the_root() {
    let err = |pat, pkg| format!("{:#}", resolve_input(pat, Path::new(pkg)).unwrap_err());
    assert!(err("../../../x", "apps/web").contains("above the workspace root"));
    assert!(err("../x", "").contains("above the workspace root"));
    assert!(err("../a/../b", "apps/web").contains("only use `..` at its start"));
    assert!(err("a/../b", "apps/web").contains("must stay inside"));
  }

  #[test]
  fn reaches_relates_directories_to_literal_prefixes() {
    let g = globs(&["proto/**/*.proto", "Cargo.toml", "!docs/**"]);
    assert!(g.reaches(Path::new("proto")), "a pattern below it");
    assert!(
      g.reaches(Path::new("proto/agent/v1")),
      "a directory below a prefix"
    );
    assert!(!g.reaches(Path::new("protocol")));
    assert!(!g.reaches(Path::new("docs")), "exclusions reach nothing");
    assert!(globs(&["*/shared/**"]).reaches(Path::new("anything")));
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
