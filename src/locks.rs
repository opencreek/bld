//! Cross-process task locking.
//!
//! The scheduler decides how many tasks run at once inside one bld. Nothing
//! stops a second bld from starting, though, and in a monorepo the two
//! overlap all the time: `bld watch dev` rebuilds codegen in one terminal
//! while `bld run check` wants the same codegen in another. Without a lock
//! both run it, both write the same output directory, and one of them loses a
//! race it did not know it was in.
//!
//! So a task takes an exclusive lock on its own name before it reads or writes
//! its outputs, and holds it until it is finished. The second bld waits, and
//! by the time the lock comes free the result is in the cache, so it replays
//! instead of repeating the work.
//!
//! The lock is an `flock` on a file under `.bld/locks`. That is what makes it
//! robust: the kernel drops the lock when the file descriptor closes, so a
//! crash, a `kill -9` or a panic leaves nothing stale behind, on Linux and on
//! macOS alike. A pid file would need liveness checks and would be wrong the
//! moment a pid was reused.
//!
//! Locks are keyed by task, not by task hash. Two bld processes that disagree
//! about the inputs still write the same directory, so they have to take turns
//! even when neither can use the other's result.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use twox_hash::XxHash3_64;

use crate::printer::PrinterHandle;
use crate::workspace::TaskIdx;

/// How long to wait before saying that a task is blocked. A handover between
/// two bld processes is usually quicker than this, and silence is better than
/// a line that scrolls past before it is read.
const ANNOUNCE_AFTER: Duration = Duration::from_millis(250);
/// Polling bounds. `flock` can block instead, but a blocked syscall cannot be
/// cancelled, and Ctrl-C has to work while a task waits for another bld.
const POLL_MIN: Duration = Duration::from_millis(10);
const POLL_MAX: Duration = Duration::from_millis(200);

/// An exclusive lock on one task, released when this is dropped.
pub struct Guard {
  file: File,
}

impl Drop for Guard {
  fn drop(&mut self) {
    // Closing the descriptor would release the lock on its own. Unlocking
    // first states the intent and does not depend on when the file is closed.
    let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
  }
}

/// The lock directory for one workspace.
#[derive(Clone)]
pub struct Locks {
  dir: PathBuf,
}

impl Locks {
  pub fn new(dir: PathBuf) -> Self {
    Self { dir }
  }

  pub fn prepare(&self) -> Result<()> {
    std::fs::create_dir_all(&self.dir)
      .with_context(|| format!("creating the lock directory {}", self.dir.display()))
  }

  fn path(&self, label: &str) -> PathBuf {
    self.dir.join(file_name(label))
  }

  /// Takes the lock if it is free. `Ok(None)` means another process holds it.
  pub fn try_acquire(&self, label: &str) -> Result<Option<Guard>> {
    let path = self.path(label);
    let mut file = OpenOptions::new()
      .create(true)
      .read(true)
      .truncate(false)
      .write(true)
      .open(&path)
      .with_context(|| format!("opening the lock file {}", path.display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
      let err = std::io::Error::last_os_error();
      if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(None);
      }
      return Err(anyhow::Error::new(err).context(format!("locking {}", path.display())));
    }
    stamp(&mut file);
    Ok(Some(Guard { file }))
  }

  /// Waits until the lock is free, saying who holds it if the wait is long
  /// enough to be worth mentioning. `Ok(None)` means the run was cancelled
  /// before the lock came free.
  pub async fn acquire(
    &self,
    label: &str,
    task: TaskIdx,
    printer: &PrinterHandle,
    cancel: &CancellationToken,
  ) -> Result<Option<Guard>> {
    if let Some(guard) = self.try_acquire(label)? {
      return Ok(Some(guard));
    }
    let started = Instant::now();
    let mut announced = false;
    let mut wait = POLL_MIN;
    loop {
      tokio::select! {
        _ = cancel.cancelled() => return Ok(None),
        _ = tokio::time::sleep(wait) => {}
      }
      if let Some(guard) = self.try_acquire(label)? {
        return Ok(Some(guard));
      }
      if !announced && started.elapsed() >= ANNOUNCE_AFTER {
        announced = true;
        printer
          .status(task, format!("waiting for {}", self.holder(label)))
          .await;
      }
      wait = (wait * 2).min(POLL_MAX);
    }
  }

  /// Who holds the lock, for a message. Best effort: the pid is read without
  /// the lock, so it can be missing or stale. Nothing depends on it.
  pub fn holder(&self, label: &str) -> String {
    match std::fs::read_to_string(self.path(label))
      .ok()
      .and_then(|text| text.trim().parse::<u32>().ok())
    {
      Some(pid) => format!("another bld (pid {pid})"),
      None => "another bld".to_string(),
    }
  }
}

/// Records who holds the lock, so a waiting process can name it.
fn stamp(file: &mut File) {
  use std::io::{Seek, SeekFrom, Write};
  let _ = file.set_len(0);
  let _ = file.seek(SeekFrom::Start(0));
  let _ = write!(file, "{}", std::process::id());
  let _ = file.flush();
}

/// A file name for a task label. A label is not a valid file name -- `/`, `#`
/// and `:` all appear in `@scope/pkg#task:step` -- so it is flattened to stay
/// readable in a directory listing, and a hash of the original keeps two
/// labels that flatten alike from sharing a lock.
fn file_name(label: &str) -> String {
  let flat: String = label
    .chars()
    .map(|c| {
      if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
        c
      } else {
        '-'
      }
    })
    .collect();
  // Every character is ASCII after the mapping, so this cannot split a char.
  let short = &flat[..flat.len().min(60)];
  format!(
    "{short}-{:016x}.lock",
    XxHash3_64::oneshot(label.as_bytes())
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  fn locks() -> (TempDir, Locks) {
    let dir = TempDir::new().unwrap();
    let locks = Locks::new(dir.path().join("locks"));
    locks.prepare().unwrap();
    (dir, locks)
  }

  #[test]
  fn a_label_becomes_a_readable_unique_file_name() {
    let name = file_name("@2ig-sigma/backend#codegen:db");
    assert!(name.starts_with("-2ig-sigma-backend-codegen-db-"), "{name}");
    assert!(name.ends_with(".lock"), "{name}");
    // Labels that flatten to the same text still get their own lock.
    assert_ne!(file_name("a#b"), file_name("a/b"));
    // A label far longer than any file name limit still produces one.
    let long = file_name(&"x".repeat(500));
    assert!(long.len() < 90, "{}", long.len());
  }

  #[test]
  fn one_holder_at_a_time() {
    let (_dir, locks) = locks();
    let first = locks.try_acquire("web#build").unwrap();
    assert!(first.is_some(), "an unheld lock should be free");
    // flock is per open file description, so a second attempt is refused even
    // from the same process. That is what makes the test meaningful here and
    // what makes the lock meaningful across processes.
    assert!(locks.try_acquire("web#build").unwrap().is_none());
    // A different task is unaffected.
    assert!(locks.try_acquire("web#test").unwrap().is_some());
    drop(first);
    assert!(locks.try_acquire("web#build").unwrap().is_some());
  }

  #[test]
  fn the_holder_names_itself() {
    let (_dir, locks) = locks();
    let _held = locks.try_acquire("web#build").unwrap().unwrap();
    let who = locks.holder("web#build");
    assert!(who.contains(&std::process::id().to_string()), "{who}");
    // Nothing has ever held this one, so there is no pid to report.
    assert_eq!(locks.holder("web#never"), "another bld");
  }
}
