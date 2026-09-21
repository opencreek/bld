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

use anyhow::Result;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::printer::PrinterHandle;
use crate::process::{self, ProcessGroup, Readers, SpawnSpec};
use crate::workspace::TaskIdx;

/// How long a persistent task gets to shut down before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long to wait for a stopped task's pipes to drain.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Report that a persistent task exited on its own, which ends a run.
pub type Exit = (TaskIdx, std::process::ExitStatus);

struct Running {
  /// Cancelling this stops the supervisor and the process group.
  stop: CancellationToken,
  supervisor: JoinHandle<()>,
  /// Task hash the process was started with, to detect input changes.
  hash: u64,
}

struct Inner {
  procs: HashMap<TaskIdx, Running>,
  exits: mpsc::UnboundedSender<Exit>,
}

/// Shared handle to the set of running persistent tasks.
#[derive(Clone)]
pub struct Persistent(Arc<Mutex<Inner>>);

impl Persistent {
  pub fn new() -> (Self, mpsc::UnboundedReceiver<Exit>) {
    let (exits, rx) = mpsc::unbounded_channel();
    let inner = Inner {
      procs: HashMap::new(),
      exits,
    };
    (Self(Arc::new(Mutex::new(inner))), rx)
  }

  /// Starts the task if it is not running. If it is running with a different
  /// task hash, restarts it when it is `interruptible` and otherwise leaves
  /// it alone. Returns whether a process was started.
  pub async fn ensure(
    &self,
    task: TaskIdx,
    hash: u64,
    interruptible: bool,
    spec: &SpawnSpec<'_>,
    printer: &PrinterHandle,
  ) -> Result<bool> {
    let mut inner = self.0.lock().await;
    if let Some(running) = inner.procs.get(&task) {
      if running.hash == hash {
        return Ok(false);
      }
      if !interruptible {
        printer
          .status(
            task,
            "inputs changed; leaving the process running (not interruptible)",
          )
          .await;
        return Ok(false);
      }
      printer.status(task, "inputs changed; restarting").await;
      let running = inner.procs.remove(&task).expect("just looked it up");
      stop(running).await;
    }

    let mut pg = process::spawn(spec)?;
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
    for running in procs {
      stop(running).await;
    }
  }
}

async fn stop(running: Running) {
  running.stop.cancel();
  let _ = running.supervisor.await;
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
