//! Walking package directories and hashing the files a task depends on.
//!
//! One walk per package serves all of that package's tasks: the walker is
//! gitignore-aware, so generated and ignored files never reach the hash, and
//! files are hashed in parallel. A memo keyed on length and modification time
//! keeps watch-mode rebuilds from re-reading files that did not change.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};
use twox_hash::XxHash3_64;

use tokio::sync::OnceCell;

use crate::globs::{self, Globs};
use crate::workspace::{PkgIdx, Workspace};

/// Files up to this size are hashed in one read.
const SMALL_FILE: u64 = 1 << 20;
/// Chunk size for hashing larger files.
const CHUNK: usize = 64 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
  File,
  Symlink,
}

/// What the hash of one path depends on.
#[derive(Debug, Clone, Copy)]
pub struct FileEntry {
  pub kind: Kind,
  /// Content hash, or the hash of the link target for a symlink.
  pub hash: u64,
  /// Whether any execute bit is set.
  pub exec: bool,
}

/// Validity key for the memo: a file whose length and mtime are unchanged is
/// assumed to have unchanged content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
  len: u64,
  mtime_ns: u128,
}

/// The files of one directory tree that some task might depend on, sorted by
/// path so that hashing is deterministic.
#[derive(Debug, Default)]
pub struct Files {
  pub entries: Vec<(PathBuf, FileEntry)>,
}

/// Remembers file hashes across walks within one process.
#[derive(Debug, Clone, Default)]
pub struct FileHashCache(Arc<Mutex<HashMap<PathBuf, (Stamp, FileEntry)>>>);

impl FileHashCache {
  pub fn new() -> Self {
    Self::default()
  }

  fn get(&self, path: &Path, stamp: Stamp) -> Option<FileEntry> {
    let map = self.0.lock().expect("file hash cache");
    map.get(path).filter(|(s, _)| *s == stamp).map(|(_, e)| *e)
  }

  fn put(&self, path: PathBuf, stamp: Stamp, entry: FileEntry) {
    self
      .0
      .lock()
      .expect("file hash cache")
      .insert(path, (stamp, entry));
  }
}

/// How to walk: which directories to stay out of.
#[derive(Debug, Clone, Default)]
pub struct WalkOpts {
  /// Directories never descended into, such as the cache.
  pub skip: Vec<PathBuf>,
}

/// Walks `dir`, hashing every file that `keep` accepts. Paths in the result
/// are relative to `dir`.
pub fn walk(dir: &Path, keep: &Globs, cache: &FileHashCache, opts: &WalkOpts) -> Result<Files> {
  if keep.is_empty() || !dir.is_dir() {
    return Ok(Files::default());
  }
  let skip: Vec<PathBuf> = opts.skip.clone();
  let mut builder = ignore::WalkBuilder::new(dir);
  builder
    .hidden(false) // dotfiles are ordinary inputs
    .ignore(false) // only .gitignore, not .ignore
    .git_global(false)
    .git_exclude(false)
    // A package inherits the ignore files above it, up to the repository
    // root, where `ignore` stops. Outside a repository, `.gitignore` files
    // have no effect at all, exactly as they have none for git.
    .parents(true)
    .require_git(true)
    .follow_links(false)
    .filter_entry(move |e| e.file_name() != ".git" && !skip.iter().any(|s| s == e.path()));

  let (tx, rx) = std::sync::mpsc::channel::<(PathBuf, FileEntry)>();
  let failed: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
  builder.build_parallel().run(|| {
    let tx = tx.clone();
    let cache = cache.clone();
    let failed = failed.clone();
    let root = dir.to_path_buf();
    let mut buf = Vec::new();
    Box::new(move |entry| {
      let entry = match entry {
        Ok(e) => e,
        Err(e) => {
          record(
            &failed,
            anyhow::Error::new(e).context("walking the workspace"),
          );
          return ignore::WalkState::Continue;
        }
      };
      let Some(file_type) = entry.file_type() else {
        return ignore::WalkState::Continue; // stdin, never in a directory walk
      };
      if file_type.is_dir() {
        return ignore::WalkState::Continue;
      }
      let Ok(rel) = entry.path().strip_prefix(&root) else {
        return ignore::WalkState::Continue;
      };
      if !keep.is_match(rel) {
        return ignore::WalkState::Continue;
      }
      match entry_for(entry.path(), &cache, &mut buf) {
        Ok(file) => {
          let _ = tx.send((rel.to_path_buf(), file));
        }
        // A file that vanished mid-walk is not an error: the next run sees
        // the new state, and watch mode will be told about the change.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => record(
          &failed,
          anyhow::Error::new(e).context(format!("hashing {}", entry.path().display())),
        ),
      }
      ignore::WalkState::Continue
    })
  });
  drop(tx);

  let mut entries: Vec<(PathBuf, FileEntry)> = rx.into_iter().collect();
  if let Some(e) = failed.lock().expect("walk error").take() {
    return Err(e);
  }
  entries.sort_by(|a, b| a.0.cmp(&b.0));
  Ok(Files { entries })
}

/// Walks the parts of `root` that the global input globs can reach.
pub fn walk_globals(
  root: &Path,
  patterns: &[String],
  keep: &Globs,
  cache: &FileHashCache,
  opts: &WalkOpts,
) -> Result<Files> {
  if patterns.is_empty() {
    return Ok(Files::default());
  }
  // Walking from each pattern's literal prefix avoids scanning the whole
  // repository for something like `pnpm-lock.yaml`.
  let mut roots: Vec<&str> = patterns.iter().map(|p| globs::literal_prefix(p)).collect();
  roots.sort_unstable();
  roots.dedup();
  if roots.iter().any(|r| r.is_empty()) {
    roots = vec![""];
  }

  let mut merged: BTreeMap<PathBuf, FileEntry> = BTreeMap::new();
  for prefix in roots {
    let start = if prefix.is_empty() {
      root.to_path_buf()
    } else {
      root.join(prefix)
    };
    if start.is_file() {
      let rel = PathBuf::from(prefix);
      if keep.is_match(&rel) {
        let mut buf = Vec::new();
        match entry_for(&start, cache, &mut buf) {
          Ok(entry) => {
            merged.insert(rel, entry);
          }
          Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
          Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("hashing {}", start.display())));
          }
        }
      }
      continue;
    }
    // Paths from a nested walk are relative to `start`, not to the root.
    let sub = walk(&start, &keep.strip_prefix(prefix)?, cache, opts)?;
    for (rel, entry) in sub.entries {
      let full = if prefix.is_empty() {
        rel
      } else {
        Path::new(prefix).join(rel)
      };
      merged.insert(full, entry);
    }
  }
  Ok(Files {
    entries: merged.into_iter().collect(),
  })
}

/// Lists the files matching a task's output globs. Outputs are usually
/// gitignored, so this walk deliberately ignores ignore files.
pub fn walk_outputs(dir: &Path, patterns: &[String], keep: &Globs) -> Result<Vec<PathBuf>> {
  if patterns.is_empty() {
    return Ok(Vec::new());
  }
  let mut roots: Vec<&str> = patterns.iter().map(|p| globs::literal_prefix(p)).collect();
  roots.sort_unstable();
  roots.dedup();
  if roots.iter().any(|r| r.is_empty()) {
    roots = vec![""];
  }

  let mut found: BTreeMap<PathBuf, ()> = BTreeMap::new();
  for prefix in roots {
    let start = if prefix.is_empty() {
      dir.to_path_buf()
    } else {
      dir.join(prefix)
    };
    if !start.exists() {
      continue;
    }
    for entry in ignore::WalkBuilder::new(&start)
      .standard_filters(false)
      .follow_links(false)
      .build()
    {
      let entry = entry.context("listing outputs")?;
      if entry.file_type().is_some_and(|t| t.is_dir()) {
        continue;
      }
      let Ok(rel) = entry.path().strip_prefix(dir) else {
        continue;
      };
      if keep.is_match(rel) {
        found.insert(rel.to_path_buf(), ());
      }
    }
  }
  Ok(found.into_keys().collect())
}

fn record(slot: &Mutex<Option<anyhow::Error>>, err: anyhow::Error) {
  let mut slot = slot.lock().expect("walk error");
  if slot.is_none() {
    *slot = Some(err);
  }
}

/// Hashes one path, reusing the memo when length and mtime are unchanged.
fn entry_for(path: &Path, cache: &FileHashCache, buf: &mut Vec<u8>) -> std::io::Result<FileEntry> {
  let meta = std::fs::symlink_metadata(path)?;
  let stamp = Stamp {
    len: meta.len(),
    mtime_ns: meta
      .modified()
      .ok()
      .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
      .map_or(0, |d| d.as_nanos()),
  };
  if let Some(entry) = cache.get(path, stamp) {
    return Ok(entry);
  }

  let entry = if meta.is_symlink() {
    // The link itself is the input; whatever it points at is hashed on its
    // own if it is inside the package.
    let target = std::fs::read_link(path)?;
    FileEntry {
      kind: Kind::Symlink,
      hash: XxHash3_64::oneshot(path_bytes(&target)),
      exec: false,
    }
  } else {
    FileEntry {
      kind: Kind::File,
      hash: hash_contents(path, meta.len(), buf)?,
      exec: is_executable(&meta),
    }
  };
  cache.put(path.to_path_buf(), stamp, entry);
  Ok(entry)
}

fn hash_contents(path: &Path, len: u64, buf: &mut Vec<u8>) -> std::io::Result<u64> {
  let mut file = std::fs::File::open(path)?;
  if len <= SMALL_FILE {
    buf.clear();
    buf.reserve(len as usize);
    file.read_to_end(buf)?;
    return Ok(XxHash3_64::oneshot(buf));
  }
  let mut hasher = XxHash3_64::new();
  let mut chunk = vec![0u8; CHUNK];
  loop {
    let n = file.read(&mut chunk)?;
    if n == 0 {
      break;
    }
    std::hash::Hasher::write(&mut hasher, &chunk[..n]);
  }
  Ok(std::hash::Hasher::finish(&hasher))
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
  use std::os::unix::fs::PermissionsExt;
  meta.permissions().mode() & 0o111 != 0
}

pub fn path_bytes(path: &Path) -> &[u8] {
  use std::os::unix::ffi::OsStrExt;
  path.as_os_str().as_bytes()
}

/// Per-run memo of the walks, so all tasks of a package share one scan and
/// watch mode can re-walk only the packages that changed.
#[derive(Clone)]
pub struct Walks(Arc<Inner>);

struct Inner {
  ws: Arc<Workspace>,
  files: FileHashCache,
  opts: WalkOpts,
  packages: Mutex<HashMap<u32, Arc<OnceCell<Arc<Files>>>>>,
  globals: Mutex<Arc<OnceCell<Arc<Files>>>>,
}

impl Walks {
  pub fn new(ws: Arc<Workspace>, files: FileHashCache) -> Self {
    let opts = WalkOpts {
      skip: vec![ws.settings.cache_dir.clone()],
    };
    Self(Arc::new(Inner {
      ws,
      files,
      opts,
      packages: Mutex::new(HashMap::new()),
      globals: Mutex::new(Arc::new(OnceCell::new())),
    }))
  }

  /// The hashed files of one package, walked at most once per run.
  pub async fn package(&self, pkg: PkgIdx) -> Result<Arc<Files>> {
    let cell = {
      let mut map = self.0.packages.lock().expect("walk memo");
      map.entry(pkg.0).or_default().clone()
    };
    let inner = self.0.clone();
    cell
      .get_or_try_init(|| async move {
        let package = inner.ws.pkg(pkg);
        let dir = package.dir.clone();
        let keep = package.input_union.clone();
        let files = inner.files.clone();
        let opts = inner.opts.clone();
        let walked =
          tokio::task::spawn_blocking(move || walk(&dir, &keep, &files, &opts)).await??;
        Ok::<_, anyhow::Error>(Arc::new(walked))
      })
      .await
      .map(Arc::clone)
  }

  /// The hashed files matched by the workspace-wide `inputs` globs.
  pub async fn globals(&self) -> Result<Arc<Files>> {
    let cell = self.0.globals.lock().expect("walk memo").clone();
    let inner = self.0.clone();
    cell
      .get_or_try_init(|| async move {
        let root = inner.ws.root.clone();
        let patterns = inner.ws.settings.global_input_patterns.clone();
        let keep = inner.ws.settings.global_inputs.clone();
        let files = inner.files.clone();
        let opts = inner.opts.clone();
        let walked =
          tokio::task::spawn_blocking(move || walk_globals(&root, &patterns, &keep, &files, &opts))
            .await??;
        Ok::<_, anyhow::Error>(Arc::new(walked))
      })
      .await
      .map(Arc::clone)
  }

  /// Forces the next run to re-walk one package.
  pub fn invalidate_package(&self, pkg: PkgIdx) {
    self.0.packages.lock().expect("walk memo").remove(&pkg.0);
  }

  /// Forces the next run to re-walk everything. The content memo is kept:
  /// it is keyed on length and modification time, so it invalidates itself.
  pub fn invalidate_all(&self) {
    self.0.packages.lock().expect("walk memo").clear();
    *self.0.globals.lock().expect("walk memo") = Arc::new(OnceCell::new());
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testutil::Fixture;

  fn globs(pats: &[&str]) -> Globs {
    Globs::new(&pats.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
  }

  fn names(files: &Files) -> Vec<String> {
    files
      .entries
      .iter()
      .map(|(p, _)| p.to_string_lossy().into_owned())
      .collect()
  }

  fn walk_all(dir: &Path) -> Files {
    walk(
      dir,
      &globs(&["**"]),
      &FileHashCache::new(),
      &WalkOpts::default(),
    )
    .unwrap()
  }

  #[test]
  fn respects_gitignore_files_in_the_workspace() {
    let fx = Fixture::new();
    fx.write(".gitignore", "dist/\n*.log\n");
    fx.write("packages/web/src/a.ts", "a");
    fx.write("packages/web/dist/out.js", "generated");
    fx.write("packages/web/debug.log", "noise");
    fx.write("packages/web/.gitignore", "local/\n");
    fx.write("packages/web/local/scratch.txt", "scratch");

    let files = walk_all(&fx.path("packages/web"));
    // The package's own `.gitignore` is a real file, and it decides what
    // counts as an input, so it is hashed like any other.
    assert_eq!(
      names(&files),
      [".gitignore", "src/a.ts"],
      "the root and package ignore files both apply"
    );
  }

  #[test]
  fn ignores_gitignore_files_above_the_repository() {
    let fx = Fixture::new();
    // A stray ignore file outside the repository must not change hashes.
    let outside = fx.root.parent().unwrap().join(".gitignore");
    let existing = std::fs::read_to_string(&outside).ok();
    std::fs::write(&outside, "*.ts\n").unwrap();
    fx.write("pkg/a.ts", "a");

    let files = walk_all(&fx.path("pkg"));
    match existing {
      Some(text) => std::fs::write(&outside, text).unwrap(),
      None => std::fs::remove_file(&outside).unwrap(),
    }
    assert_eq!(names(&files), ["a.ts"]);
  }

  #[test]
  fn hashes_change_with_content_permissions_and_link_targets() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    fx.write("pkg/a.txt", "one");
    std::os::unix::fs::symlink("a.txt", fx.path("pkg/link")).unwrap();
    let first = walk_all(&fx.path("pkg"));
    assert_eq!(names(&first), ["a.txt", "link"]);
    assert_eq!(first.entries[1].1.kind, Kind::Symlink);

    // Same contents, same hashes.
    let again = walk_all(&fx.path("pkg"));
    assert_eq!(first.entries[0].1.hash, again.entries[0].1.hash);

    fx.write("pkg/a.txt", "two");
    let changed = walk_all(&fx.path("pkg"));
    assert_ne!(first.entries[0].1.hash, changed.entries[0].1.hash);

    // The execute bit is part of the file's identity.
    assert!(!changed.entries[0].1.exec);
    std::fs::set_permissions(fx.path("pkg/a.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(walk_all(&fx.path("pkg")).entries[0].1.exec);

    // So is a symlink's target.
    std::fs::remove_file(fx.path("pkg/link")).unwrap();
    std::os::unix::fs::symlink("elsewhere.txt", fx.path("pkg/link")).unwrap();
    assert_ne!(
      first.entries[1].1.hash,
      walk_all(&fx.path("pkg")).entries[1].1.hash
    );
  }

  #[test]
  fn large_files_hash_the_same_as_small_ones_would() {
    let fx = Fixture::new();
    let big = "x".repeat((SMALL_FILE as usize) + 4096);
    fx.write("pkg/big.txt", &big);
    let files = walk_all(&fx.path("pkg"));
    assert_eq!(files.entries[0].1.hash, XxHash3_64::oneshot(big.as_bytes()));
  }

  #[test]
  fn lists_outputs_even_when_gitignored() {
    let fx = Fixture::new();
    fx.write(".gitignore", "dist/\n");
    fx.write("pkg/dist/a.js", "a");
    fx.write("pkg/dist/nested/b.js", "b");
    fx.write("pkg/src.ts", "s");
    let patterns = vec!["dist/**".to_string()];
    let found = walk_outputs(&fx.path("pkg"), &patterns, &globs(&["dist/**"])).unwrap();
    assert_eq!(
      found
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>(),
      ["dist/a.js", "dist/nested/b.js"]
    );
  }

  #[test]
  fn global_inputs_are_found_from_their_literal_prefix() {
    let fx = Fixture::new();
    fx.write("bld.lock", "v1");
    fx.write("config/a.json", "{}");
    fx.write("config/nested/b.json", "{}");
    fx.write("packages/web/src.ts", "x");
    let patterns = vec!["bld.lock".to_string(), "config/**/*.json".to_string()];
    let keep = globs(&["bld.lock", "config/**/*.json"]);
    let files = walk_globals(
      &fx.root,
      &patterns,
      &keep,
      &FileHashCache::new(),
      &WalkOpts::default(),
    )
    .unwrap();
    assert_eq!(
      names(&files),
      ["bld.lock", "config/a.json", "config/nested/b.json"]
    );
  }
}
