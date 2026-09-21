//! The local cache.
//!
//! One directory per task hash holds the task's log and, if it produced any,
//! a zstd-compressed tar of its outputs. Entries are written to a temporary
//! directory and renamed into place, so a crashed or concurrent bld can never
//! leave a half-written entry that looks valid.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::hash::hex;
use crate::printer::PrinterHandle;
use crate::workspace::TaskIdx;

/// Bumped when the on-disk layout changes.
const CACHE_VERSION: u32 = 1;
/// Fast compression: this cache is read back on the same machine, so speed
/// matters far more than ratio.
const ZSTD_LEVEL: i32 = 1;
/// Chunk size for replaying a cached log.
const REPLAY_CHUNK: usize = 64 << 10;

const META_FILE: &str = "meta.toml";
const LOG_FILE: &str = "log";
const OUTPUTS_FILE: &str = "outputs.tar.zst";
const TMP_DIR: &str = "tmp";

/// Makes temporary directory names unique within a process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
  pub version: u32,
  /// `package#task`, for humans reading the cache directory.
  pub task: String,
  pub hash: String,
  pub saved_at: u64,
  pub duration_ms: u64,
  pub has_outputs: bool,
  pub log_truncated: bool,
}

impl Meta {
  pub fn new(
    task: &str,
    hash: u64,
    duration: Duration,
    has_outputs: bool,
    log_truncated: bool,
  ) -> Self {
    Self {
      version: CACHE_VERSION,
      task: task.to_string(),
      hash: hex(hash),
      saved_at: SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()),
      duration_ms: duration.as_millis() as u64,
      has_outputs,
      log_truncated,
    }
  }
}

/// A cache hit.
#[derive(Debug, Clone)]
pub struct Entry {
  pub dir: PathBuf,
  pub meta: Meta,
}

#[derive(Debug, Clone)]
pub struct Cache {
  dir: PathBuf,
}

impl Cache {
  pub fn new(dir: PathBuf) -> Self {
    Self { dir }
  }

  fn entry_dir(&self, hash: u64) -> PathBuf {
    self.dir.join(hex(hash))
  }

  /// Looks for a usable entry. An entry that cannot be read is treated as a
  /// miss and removed, so a corrupted cache heals itself.
  pub fn lookup(&self, hash: u64) -> Option<Entry> {
    let dir = self.entry_dir(hash);
    let text = std::fs::read_to_string(dir.join(META_FILE)).ok()?;
    match toml::from_str::<Meta>(&text) {
      Ok(meta) if meta.version == CACHE_VERSION => Some(Entry { dir, meta }),
      _ => {
        let _ = std::fs::remove_dir_all(&dir);
        None
      }
    }
  }

  /// Stores a task's log and outputs under its hash.
  pub async fn save(
    &self,
    hash: u64,
    meta: Meta,
    log: Vec<u8>,
    outputs: Vec<PathBuf>,
    package_dir: PathBuf,
  ) -> Result<()> {
    let cache = self.clone();
    tokio::task::spawn_blocking(move || {
      cache.save_blocking(hash, &meta, &log, &outputs, &package_dir)
    })
    .await?
  }

  fn save_blocking(
    &self,
    hash: u64,
    meta: &Meta,
    log: &[u8],
    outputs: &[PathBuf],
    package_dir: &Path,
  ) -> Result<()> {
    let tmp = self.dir.join(TMP_DIR).join(format!(
      "{}-{}-{}",
      hex(hash),
      std::process::id(),
      COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    // Any leftover from an earlier crash is ours to reuse.
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;

    let result = (|| -> Result<()> {
      std::fs::write(tmp.join(LOG_FILE), log).context("writing the cached log")?;
      if meta.has_outputs {
        write_archive(&tmp.join(OUTPUTS_FILE), package_dir, outputs)?;
      }
      // Metadata last: an entry without it is never treated as a hit.
      let text = toml::to_string(meta).context("serializing cache metadata")?;
      std::fs::write(tmp.join(META_FILE), text).context("writing cache metadata")?;
      Ok(())
    })();
    if let Err(e) = result {
      let _ = std::fs::remove_dir_all(&tmp);
      return Err(e);
    }

    let final_dir = self.entry_dir(hash);
    match std::fs::rename(&tmp, &final_dir) {
      Ok(()) => Ok(()),
      Err(_) if final_dir.join(META_FILE).is_file() => {
        // Another bld stored the same hash first; its entry is equivalent.
        let _ = std::fs::remove_dir_all(&tmp);
        Ok(())
      }
      Err(e) => {
        let _ = std::fs::remove_dir_all(&tmp);
        Err(e).with_context(|| format!("moving the cache entry into {}", final_dir.display()))
      }
    }
  }

  /// Unpacks a hit's outputs back into the package directory.
  pub async fn restore(&self, entry: &Entry, package_dir: PathBuf) -> Result<()> {
    if !entry.meta.has_outputs {
      return Ok(());
    }
    let archive = entry.dir.join(OUTPUTS_FILE);
    tokio::task::spawn_blocking(move || restore_blocking(&archive, &package_dir)).await?
  }

  /// Prints a hit's stored log as though the task had just produced it.
  pub async fn replay_log(
    &self,
    entry: &Entry,
    task: TaskIdx,
    printer: &PrinterHandle,
  ) -> Result<()> {
    let path = entry.dir.join(LOG_FILE);
    let mut file = match tokio::fs::File::open(&path).await {
      Ok(f) => f,
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
      Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut buf = vec![0u8; REPLAY_CHUNK];
    loop {
      let n = file
        .read(&mut buf)
        .await
        .context("reading the cached log")?;
      if n == 0 {
        break;
      }
      printer.chunk(task, buf[..n].to_vec()).await;
    }
    Ok(())
  }

  /// Ensures the cache and its temporary directory exist.
  pub fn prepare(&self) -> Result<()> {
    std::fs::create_dir_all(self.dir.join(TMP_DIR))
      .with_context(|| format!("creating {}", self.dir.display()))
  }
}

/// Writes the outputs as a zstd-compressed tar, streaming file by file.
fn write_archive(path: &Path, package_dir: &Path, outputs: &[PathBuf]) -> Result<()> {
  let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
  let encoder = zstd::Encoder::new(std::io::BufWriter::new(file), ZSTD_LEVEL)
    .context("starting zstd compression")?;
  let mut builder = tar::Builder::new(encoder);
  // Keep real modes and times: restored files feed tools with their own
  // incremental caches, which epoch timestamps would confuse.
  builder.mode(tar::HeaderMode::Complete);
  builder.follow_symlinks(false);
  for rel in outputs {
    let abs = package_dir.join(rel);
    builder
      .append_path_with_name(&abs, rel)
      .with_context(|| format!("adding {} to the cache", rel.display()))?;
  }
  let encoder = builder.into_inner().context("finishing the archive")?;
  let mut writer = encoder.finish().context("finishing zstd compression")?;
  writer.flush().context("flushing the cache archive")?;
  Ok(())
}

fn restore_blocking(archive: &Path, package_dir: &Path) -> Result<()> {
  let file =
    std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
  let decoder =
    zstd::Decoder::new(std::io::BufReader::new(file)).context("starting zstd decompression")?;
  let mut tar = tar::Archive::new(decoder);
  tar.set_overwrite(true);
  // `unpack` refuses absolute and `..` entries and checks that each parent
  // stays inside the destination, so a damaged archive cannot escape.
  tar
    .unpack(package_dir)
    .with_context(|| format!("restoring outputs into {}", package_dir.display()))?;
  Ok(())
}

/// Reads a whole cached log, for tests and diagnostics.
#[cfg(test)]
pub fn read_log(entry: &Entry) -> Result<Vec<u8>> {
  Ok(std::fs::read(entry.dir.join(LOG_FILE))?)
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::PermissionsExt;

  use super::*;

  fn cache_in(dir: &Path) -> Cache {
    let cache = Cache::new(dir.join("cache"));
    cache.prepare().unwrap();
    cache
  }

  fn meta(has_outputs: bool) -> Meta {
    Meta::new(
      "pkg#build",
      0xabc,
      Duration::from_millis(12),
      has_outputs,
      false,
    )
  }

  #[tokio::test]
  async fn round_trips_outputs_and_logs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(pkg.join("dist/nested")).unwrap();
    std::fs::write(pkg.join("dist/a.js"), b"content-a").unwrap();
    std::fs::write(pkg.join("dist/nested/b.js"), b"content-b").unwrap();
    std::fs::set_permissions(
      pkg.join("dist/a.js"),
      std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let cache = cache_in(tmp.path());
    let outputs = vec![
      PathBuf::from("dist/a.js"),
      PathBuf::from("dist/nested/b.js"),
    ];
    cache
      .save(1, meta(true), b"built\n".to_vec(), outputs, pkg.clone())
      .await
      .unwrap();

    std::fs::remove_dir_all(pkg.join("dist")).unwrap();
    let entry = cache.lookup(1).expect("entry should be a hit");
    assert_eq!(read_log(&entry).unwrap(), b"built\n");
    cache.restore(&entry, pkg.clone()).await.unwrap();

    assert_eq!(std::fs::read(pkg.join("dist/a.js")).unwrap(), b"content-a");
    assert_eq!(
      std::fs::read(pkg.join("dist/nested/b.js")).unwrap(),
      b"content-b"
    );
    let mode = std::fs::metadata(pkg.join("dist/a.js"))
      .unwrap()
      .permissions()
      .mode();
    assert!(
      mode & 0o111 != 0,
      "the execute bit should survive, got {mode:o}"
    );
  }

  #[tokio::test]
  async fn restores_symlinks_as_links() {
    let tmp = tempfile::TempDir::new().unwrap();
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(pkg.join("dist")).unwrap();
    std::fs::write(pkg.join("dist/real.js"), b"x").unwrap();
    std::os::unix::fs::symlink("real.js", pkg.join("dist/link.js")).unwrap();

    let cache = cache_in(tmp.path());
    let outputs = vec![PathBuf::from("dist/real.js"), PathBuf::from("dist/link.js")];
    cache
      .save(2, meta(true), Vec::new(), outputs, pkg.clone())
      .await
      .unwrap();
    std::fs::remove_dir_all(pkg.join("dist")).unwrap();
    let entry = cache.lookup(2).unwrap();
    cache.restore(&entry, pkg.clone()).await.unwrap();

    let meta = std::fs::symlink_metadata(pkg.join("dist/link.js")).unwrap();
    assert!(meta.is_symlink());
  }

  #[tokio::test]
  async fn caches_a_task_without_outputs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = cache_in(tmp.path());
    cache
      .save(
        3,
        meta(false),
        b"tests passed\n".to_vec(),
        Vec::new(),
        tmp.path().to_path_buf(),
      )
      .await
      .unwrap();
    let entry = cache
      .lookup(3)
      .expect("a task with no outputs is still cacheable");
    assert!(!entry.meta.has_outputs);
    assert_eq!(read_log(&entry).unwrap(), b"tests passed\n");
    cache
      .restore(&entry, tmp.path().to_path_buf())
      .await
      .unwrap();
  }

  #[test]
  fn missing_and_damaged_entries_are_misses() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = cache_in(tmp.path());
    assert!(cache.lookup(99).is_none());

    let dir = cache.entry_dir(99);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(META_FILE), "not valid toml {{").unwrap();
    assert!(cache.lookup(99).is_none());
    assert!(!dir.exists(), "a damaged entry should be cleaned up");
  }

  #[test]
  fn entries_from_another_layout_version_are_misses() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = cache_in(tmp.path());
    let dir = cache.entry_dir(7);
    std::fs::create_dir_all(&dir).unwrap();
    let mut m = meta(false);
    m.version = CACHE_VERSION + 1;
    std::fs::write(dir.join(META_FILE), toml::to_string(&m).unwrap()).unwrap();
    assert!(cache.lookup(7).is_none());
  }

  #[tokio::test]
  async fn a_losing_concurrent_save_is_not_an_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = cache_in(tmp.path());
    let pkg = tmp.path().to_path_buf();
    cache
      .save(5, meta(false), b"first\n".to_vec(), Vec::new(), pkg.clone())
      .await
      .unwrap();
    cache
      .save(5, meta(false), b"second\n".to_vec(), Vec::new(), pkg)
      .await
      .unwrap();
    let entry = cache.lookup(5).unwrap();
    assert_eq!(
      read_log(&entry).unwrap(),
      b"first\n",
      "the first writer wins"
    );
    let leftovers: Vec<_> = std::fs::read_dir(cache.dir.join(TMP_DIR))
      .unwrap()
      .collect();
    assert!(
      leftovers.is_empty(),
      "temporary directories should be cleaned up"
    );
  }

  #[tokio::test]
  async fn a_malicious_archive_cannot_escape_the_package() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = cache_in(tmp.path());
    let dir = cache.entry_dir(11);
    std::fs::create_dir_all(&dir).unwrap();

    // Hand-build an archive naming a path outside the destination.
    let file = std::fs::File::create(dir.join(OUTPUTS_FILE)).unwrap();
    let encoder = zstd::Encoder::new(file, ZSTD_LEVEL).unwrap();
    let mut builder = tar::Builder::new(encoder);
    let payload = b"pwned";
    let mut header = tar::Header::new_gnu();
    header.set_size(payload.len() as u64);
    header.set_mode(0o644);
    // The tar crate refuses to write a `..` path through its safe API, so
    // the name is poked straight into the header, as a hostile writer would.
    let name = b"../escaped.txt";
    let gnu = header.as_gnu_mut().expect("gnu header");
    gnu.name[..name.len()].copy_from_slice(name);
    header.set_cksum();
    builder.append(&header, &payload[..]).unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    std::fs::write(dir.join(META_FILE), toml::to_string(&meta(true)).unwrap()).unwrap();

    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    let entry = cache.lookup(11).unwrap();
    let _ = cache.restore(&entry, pkg.clone()).await;
    assert!(
      !tmp.path().join("escaped.txt").exists(),
      "restore must stay inside the package"
    );
    assert!(!pkg.join("escaped.txt").exists());
  }
}
