//! Long-running tasks (dev servers).
//!
//! A persistent task never completes, so it cannot take part in the normal
//! dependency wait. Instead its process is handed to this set, which outlives
//! individual runs: a persistent task always means watching, and a rebuild
//! may come and go while the dev server keeps running. Each child is owned by
//! a supervisor task, so restarting one is just "cancel, await, spawn again".
//! The supervisor also starts the child again when it exits on its own, if
//! the task's `restart` says so.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Restart;
use crate::locks::{Guard, Locks};
use crate::printer::PrinterHandle;
use crate::process::{self, ProcessGroup, Readers, SpawnSpec, describe_status};
use crate::workspace::{TaskDef, TaskIdx};

/// How long a persistent task gets to shut down before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long to wait for a stopped task's pipes to drain.
const DRAIN_GRACE: Duration = Duration::from_secs(1);
/// How long a task that exited waits before it is started again. The wait
/// doubles with every restart up to the maximum, so a task that cannot start
/// does not flood the terminal with the same error.
const RESTART_DELAY: Duration = Duration::from_secs(1);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);
/// A run at least this long was not a crash loop, so the wait starts over.
const STABLE_RUN: Duration = Duration::from_secs(10);

/// Report that a persistent task exited on its own and is not restarted.
pub type Exit = (TaskIdx, ExitStatus);

/// Why a persistent task did not start.
pub enum Started {
  /// Another bld already runs it. Queueing behind a process that never exits
  /// would be waiting forever, and starting a second one would be two dev
  /// servers on one port.
  Busy(String),
  Failed(String),
}

struct Running {
  /// Cancelling this stops the supervisor and the process group.
  stop: CancellationToken,
  supervisor: JoinHandle<()>,
  /// Task hash the process was started with, to detect input changes.
  hash: u64,
  /// Held for as long as the process lives, restarts included.
  lock: Guard,
}

struct Inner {
  procs: HashMap<TaskIdx, Running>,
  exits: mpsc::UnboundedSender<Exit>,
  locks: Locks,
}

/// Shared handle to the set of running persistent tasks.
#[derive(Clone)]
pub struct Persistent(Arc<Mutex<Inner>>);

impl Persistent {
  pub fn new(locks: Locks) -> (Self, mpsc::UnboundedReceiver<Exit>) {
    let (exits, rx) = mpsc::unbounded_channel();
    let inner = Inner {
      procs: HashMap::new(),
      exits,
      locks,
    };
    (Self(Arc::new(Mutex::new(inner))), rx)
  }

  /// Starts the task if it is not running. If it is running with a different
  /// task hash, restarts it when it is `interruptible` and otherwise leaves
  /// it alone. Returns whether a process was started.
  pub async fn ensure(
    &self,
    task: TaskIdx,
    def: &TaskDef,
    hash: u64,
    spec: &SpawnSpec<'_>,
    printer: &PrinterHandle,
  ) -> Result<bool, Started> {
    let label = def.label.as_str();
    let mut inner = self.0.lock().await;
    let lock = if let Some(running) = inner.procs.get(&task) {
      if running.hash == hash {
        return Ok(false);
      }
      if !def.interruptible {
        return Ok(false);
      }
      printer.status(task, "inputs changed; restarting").await;
      let Running {
        stop: token,
        supervisor,
        lock,
        ..
      } = inner.procs.remove(&task).expect("just looked it up");
      stop(token, supervisor).await;
      // The lock stays ours across a restart. This process never let the task
      // go, so no other bld may slip into the gap.
      lock
    } else {
      match inner.locks.try_acquire(label) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
          return Err(Started::Busy(format!(
            "already running in {}",
            inner.locks.holder(label)
          )));
        }
        Err(e) => return Err(Started::Failed(format!("{e:#}"))),
      }
    };

    let mut pg = process::spawn(spec).map_err(|e| Started::Failed(e.to_string()))?;
    let readers = pg.pump(task, printer.clone());
    printer.begin(task, false).await;
    let stop = CancellationToken::new();
    let supervisor = Supervisor {
      task,
      restart: def.restart,
      command: Command::new(spec),
      printer: printer.clone(),
      stop: stop.clone(),
      exits: inner.exits.clone(),
    };
    let supervisor = tokio::spawn(supervisor.run(pg, readers));
    inner.procs.insert(
      task,
      Running {
        stop,
        supervisor,
        hash,
        lock,
      },
    );
    Ok(true)
  }

  /// Stops every persistent task and waits for the processes to go away.
  pub async fn shutdown(&self) {
    let procs: Vec<Running> = {
      let mut inner = self.0.lock().await;
      inner.procs.drain().map(|(_, r)| r).collect()
    };
    for Running {
      stop: token,
      supervisor,
      lock,
      ..
    } in procs
    {
      stop(token, supervisor).await;
      // Only once the process is gone: another bld taking the task over while
      // this one still had a child would put two of them on the same port.
      drop(lock);
    }
  }
}

async fn stop(token: CancellationToken, supervisor: JoinHandle<()>) {
  token.cancel();
  let _ = supervisor.await;
}

/// What a supervisor needs to start its process again, owned because it
/// outlives the run that started it.
struct Command {
  shell: Vec<String>,
  command: String,
  cwd: PathBuf,
  env: Vec<(OsString, OsString)>,
}

impl Command {
  fn new(spec: &SpawnSpec<'_>) -> Self {
    Self {
      shell: spec.shell.to_vec(),
      command: spec.command.to_string(),
      cwd: spec.cwd.to_path_buf(),
      env: spec.env.to_vec(),
    }
  }

  fn spec(&self) -> SpawnSpec<'_> {
    SpawnSpec {
      shell: &self.shell,
      command: &self.command,
      cwd: &self.cwd,
      env: &self.env,
    }
  }
}

/// Owns one persistent child: forwards its output, shuts it down when asked,
/// and when it exits on its own either starts it again or reports that it
/// exited.
struct Supervisor {
  task: TaskIdx,
  restart: Restart,
  command: Command,
  printer: PrinterHandle,
  stop: CancellationToken,
  exits: mpsc::UnboundedSender<Exit>,
}

impl Supervisor {
  async fn run(self, mut pg: ProcessGroup, mut readers: Readers) {
    let mut started = Instant::now();
    let mut delay = RESTART_DELAY;
    loop {
      let exited = tokio::select! {
        status = pg.wait() => status.ok(),
        _ = self.stop.cancelled() => {
          let _ = pg.terminate(STOP_GRACE).await;
          None
        }
      };
      pg.kill_strays();
      readers.drain(DRAIN_GRACE).await;
      self.printer.end(self.task).await;
      // One that exits just as it is being stopped did not exit on its own.
      let Some(status) = exited.filter(|_| !self.stop.is_cancelled()) else {
        return;
      };
      if !restarts(self.restart, status) {
        let _ = self.exits.send((self.task, status));
        return;
      }

      if started.elapsed() >= STABLE_RUN {
        delay = RESTART_DELAY;
      }
      let mut why = format!("exited on its own ({})", describe_status(status));
      (pg, readers) = loop {
        let line = format!("{why}; starting it again in {}s", delay.as_secs());
        self.printer.status(self.task, line).await;
        tokio::select! {
          _ = tokio::time::sleep(delay) => {}
          _ = self.stop.cancelled() => return,
        }
        delay = (delay * 2).min(MAX_RESTART_DELAY);
        match process::spawn(&self.command.spec()) {
          Ok(mut fresh) => {
            let readers = fresh.pump(self.task, self.printer.clone());
            self.printer.begin(self.task, false).await;
            break (fresh, readers);
          }
          Err(e) => why = format!("could not start the command: {e}"),
        }
      };
      started = Instant::now();
    }
  }
}

/// Whether a task that exited on its own with `status` is started again.
fn restarts(restart: Restart, status: ExitStatus) -> bool {
  match restart {
    Restart::Never => false,
    Restart::OnFailure => !status.success(),
    Restart::Always => true,
  }
}

#[cfg(test)]
mod tests {
  use std::os::unix::process::ExitStatusExt;

  use super::*;

  #[test]
  fn restarts_as_the_task_says() {
    let success = ExitStatus::from_raw(0);
    let failure = ExitStatus::from_raw(1 << 8);
    let killed = ExitStatus::from_raw(libc::SIGKILL);
    assert!(
      ![success, failure, killed]
        .iter()
        .any(|&s| restarts(Restart::Never, s))
    );
    assert!(!restarts(Restart::OnFailure, success));
    assert!(restarts(Restart::OnFailure, failure) && restarts(Restart::OnFailure, killed));
    assert!(
      [success, failure, killed]
        .iter()
        .all(|&s| restarts(Restart::Always, s))
    );
  }
}
