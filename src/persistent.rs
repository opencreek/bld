//! Long-running tasks (dev servers).
//!
//! A persistent task never completes, so it cannot take part in the normal
//! dependency wait. Instead its process is handed to this set, which outlives
//! individual runs: in watch mode a rebuild may come and go while the dev
//! server keeps running. Each child is owned by a supervisor task, so
//! restarting one is just "cancel, await, spawn again".

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::locks::{Guard, Locks};
use crate::printer::PrinterHandle;
use crate::process::{self, ProcessGroup, Readers, SpawnSpec};
use crate::workspace::TaskIdx;

/// How long a persistent task gets to shut down before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long to wait for a stopped task's pipes to drain.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Report that a persistent task exited on its own, which ends a run.
pub type Exit = (TaskIdx, std::process::ExitStatus);

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
    label: &str,
    hash: u64,
    interruptible: bool,
    spec: &SpawnSpec<'_>,
    printer: &PrinterHandle,
  ) -> Result<bool, Started> {
    let mut inner = self.0.lock().await;
    let lock = if let Some(running) = inner.procs.get(&task) {
      if running.hash == hash {
        return Ok(false);
      }
      if !interruptible {
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
    let supervisor = tokio::spawn(supervise(
      pg,
      readers,
      task,
      printer.clone(),
      stop.clone(),
      inner.exits.clone(),
    ));
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

  pub async fn is_empty(&self) -> bool {
    self.0.lock().await.procs.is_empty()
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

/// Owns one persistent child: forwards its output, and either reports that it
/// exited on its own or shuts it down when asked.
async fn supervise(
  mut pg: ProcessGroup,
  readers: Readers,
  task: TaskIdx,
  printer: PrinterHandle,
  stop: CancellationToken,
  exits: mpsc::UnboundedSender<Exit>,
) {
  let exited = tokio::select! {
    status = pg.wait() => status.ok(),
    _ = stop.cancelled() => {
      let _ = pg.terminate(STOP_GRACE).await;
      None
    }
  };
  pg.kill_strays();
  readers.drain(DRAIN_GRACE).await;
  printer.end(task).await;
  if let Some(status) = exited {
    let _ = exits.send((task, status));
  }
}
