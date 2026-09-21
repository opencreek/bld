//! Spawning task commands and keeping their processes under control.
//!
//! Every command runs in its own process group so that a task's whole tree,
//! not just the shell bld spawned, can be signalled. The group is killed on
//! every exit path, including panics, through [`ProcessGroup`]'s `Drop`.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::printer::PrinterHandle;
use crate::workspace::TaskIdx;

/// Read size for a child's pipes.
const PIPE_BUF: usize = 8 << 10;

/// Set by a second interrupt: stop being polite about shutting children down.
static HARD_KILL: AtomicBool = AtomicBool::new(false);

/// Makes every later termination skip its grace period.
pub fn request_hard_kill() {
  HARD_KILL.store(true, Ordering::Relaxed);
}

pub fn hard_kill_requested() -> bool {
  HARD_KILL.load(Ordering::Relaxed)
}

pub struct SpawnSpec<'a> {
  /// Shell argv, e.g. `["sh", "-c"]`; the command is appended.
  pub shell: &'a [String],
  pub command: &'a str,
  /// Directory the command runs in.
  pub cwd: &'a Path,
  /// The complete environment; nothing else is inherited.
  pub env: &'a [(OsString, OsString)],
}

/// A running command and the process group it leads.
pub struct ProcessGroup {
  child: tokio::process::Child,
  pgid: i32,
  reaped: bool,
}

/// The tasks forwarding a child's pipes into the printer.
pub struct Readers {
  handles: Vec<JoinHandle<()>>,
}

/// Starts `spec` in a fresh process group with a closed stdin.
pub fn spawn(spec: &SpawnSpec<'_>) -> io::Result<ProcessGroup> {
  let (program, args) = spec
    .shell
    .split_first()
    .ok_or_else(|| io::Error::other("shell must not be empty"))?;
  let mut cmd = Command::new(program);
  cmd
    .args(args)
    .arg(spec.command)
    .current_dir(spec.cwd)
    .env_clear()
    .envs(spec.env.iter().map(|(k, v)| (k, v)))
    .stdin(Stdio::null()) // a background group reading the tty would stop on SIGTTIN
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0)
    .kill_on_drop(true);
  let child = cmd.spawn()?;
  let pgid = child
    .id()
    .ok_or_else(|| io::Error::other("child exited before its process group could be recorded"))?
    as i32;
  Ok(ProcessGroup {
    child,
    pgid,
    reaped: false,
  })
}

impl ProcessGroup {
  /// Forwards stdout and stderr into the printer, merged in arrival order.
  pub fn pump(&mut self, task: TaskIdx, printer: PrinterHandle) -> Readers {
    let mut handles = Vec::with_capacity(2);
    if let Some(out) = self.child.stdout.take() {
      handles.push(tokio::spawn(pump(out, task, printer.clone())));
    }
    if let Some(err) = self.child.stderr.take() {
      handles.push(tokio::spawn(pump(err, task, printer)));
    }
    Readers { handles }
  }

  pub async fn wait(&mut self) -> io::Result<ExitStatus> {
    let status = self.child.wait().await?;
    self.reaped = true;
    Ok(status)
  }

  /// Sends a signal to the whole group.
  pub fn signal(&self, sig: i32) {
    // SAFETY: `kill` with a negative pgid signals a process group and has no
    // memory effects. The group is the one bld created for this child.
    unsafe {
      libc::kill(-self.pgid, sig);
    }
  }

  /// Asks the group to stop, then forces it after `grace`.
  pub async fn terminate(&mut self, grace: Duration) -> io::Result<ExitStatus> {
    if hard_kill_requested() {
      self.signal(libc::SIGKILL);
      return self.wait().await;
    }
    self.signal(libc::SIGTERM);
    match timeout(grace, self.wait()).await {
      Ok(status) => status,
      Err(_) => {
        self.signal(libc::SIGKILL);
        self.wait().await
      }
    }
  }

  /// Kills anything the command left behind after it exited. Strays would
  /// otherwise hold the pipes open and keep the readers from seeing EOF.
  pub fn kill_strays(&self) {
    self.signal(libc::SIGKILL);
  }
}

impl Drop for ProcessGroup {
  fn drop(&mut self) {
    if !self.reaped {
      self.signal(libc::SIGKILL);
    }
  }
}

impl Readers {
  /// Waits for the pipes to drain, giving up after `grace` so that a stray
  /// grandchild holding a pipe cannot stall the run.
  pub async fn drain(self, grace: Duration) {
    let aborts: Vec<_> = self.handles.iter().map(|h| h.abort_handle()).collect();
    let joined = async move {
      for h in self.handles {
        let _ = h.await;
      }
    };
    if timeout(grace, joined).await.is_err() {
      for a in aborts {
        a.abort();
      }
    }
  }
}

async fn pump<R: AsyncRead + Unpin>(mut src: R, task: TaskIdx, printer: PrinterHandle) {
  let mut buf = vec![0u8; PIPE_BUF];
  loop {
    match src.read(&mut buf).await {
      Ok(0) | Err(_) => break,
      Ok(n) => printer.chunk(task, buf[..n].to_vec()).await,
    }
  }
}

/// A short human description of how a command ended.
pub fn describe_status(status: ExitStatus) -> String {
  match (status.code(), status.signal()) {
    (Some(code), _) => format!("exit code {code}"),
    (None, Some(sig)) => match signal_name(sig) {
      Some(name) => format!("killed by {name}"),
      None => format!("killed by signal {sig}"),
    },
    _ => "terminated".to_string(),
  }
}

fn signal_name(sig: i32) -> Option<&'static str> {
  Some(match sig {
    libc::SIGHUP => "SIGHUP",
    libc::SIGINT => "SIGINT",
    libc::SIGQUIT => "SIGQUIT",
    libc::SIGABRT => "SIGABRT",
    libc::SIGKILL => "SIGKILL",
    libc::SIGSEGV => "SIGSEGV",
    libc::SIGPIPE => "SIGPIPE",
    libc::SIGTERM => "SIGTERM",
    _ => return None,
  })
}

#[cfg(test)]
mod tests {
  use tokio::sync::mpsc;

  use super::*;
  use crate::printer::OutMsg;

  struct Harness {
    rx: mpsc::Receiver<OutMsg>,
    printer: PrinterHandle,
  }

  fn harness() -> Harness {
    let (tx, rx) = mpsc::channel(256);
    Harness {
      rx,
      printer: PrinterHandle::for_test(tx),
    }
  }

  impl Harness {
    /// Everything the task wrote, as text.
    async fn collect(&mut self) -> String {
      let mut out = Vec::new();
      while let Ok(msg) = self.rx.try_recv() {
        if let OutMsg::Chunk { data, .. } = msg {
          out.extend_from_slice(&data);
        }
      }
      String::from_utf8_lossy(&out).into_owned()
    }
  }

  fn shell() -> Vec<String> {
    vec!["sh".to_string(), "-c".to_string()]
  }

  /// bld always passes `PATH` through, so tests that call real programs
  /// have to as well.
  fn path_env() -> Vec<(OsString, OsString)> {
    vec![(
      OsString::from("PATH"),
      std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
    )]
  }

  async fn run(command: &str, env: &[(OsString, OsString)], cwd: &Path) -> (ExitStatus, String) {
    let mut h = harness();
    let sh = shell();
    let spec = SpawnSpec {
      shell: &sh,
      command,
      cwd,
      env,
    };
    let mut pg = spawn(&spec).unwrap();
    let readers = pg.pump(TaskIdx(0), h.printer.clone());
    let status = pg.wait().await.unwrap();
    pg.kill_strays();
    readers.drain(Duration::from_secs(1)).await;
    let text = h.collect().await;
    (status, text)
  }

  #[tokio::test]
  async fn captures_stdout_and_stderr() {
    let (status, out) = run("echo one; echo two >&2", &[], Path::new("/")).await;
    assert!(status.success());
    assert!(out.contains("one") && out.contains("two"), "{out:?}");
  }

  #[tokio::test]
  async fn reports_a_failing_exit_code() {
    let (status, _) = run("exit 3", &[], Path::new("/")).await;
    assert_eq!(status.code(), Some(3));
    assert_eq!(describe_status(status), "exit code 3");
  }

  #[tokio::test]
  async fn runs_in_the_given_directory() {
    let dir = tempfile::TempDir::new().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let (_, out) = run("pwd", &[], &dir).await;
    assert_eq!(out.trim(), dir.to_str().unwrap());
  }

  #[tokio::test]
  async fn passes_only_the_given_environment() {
    // A real `PATH` so that `env` itself can be found, plus one variable
    // that stands in for a declared one.
    let mut env = path_env();
    env.push((OsString::from("KEPT"), OsString::from("yes")));
    let (_, out) = run("env", &env, Path::new("/")).await;
    let mut names: Vec<&str> = out.lines().filter_map(|l| l.split('=').next()).collect();
    // The shell sets a few of its own; everything else must come from us.
    names.retain(|n| !matches!(*n, "PWD" | "SHLVL" | "_" | "OLDPWD"));
    names.sort();
    assert_eq!(names, ["KEPT", "PATH"], "{out:?}");
  }

  #[tokio::test]
  async fn stdin_is_closed_so_reads_do_not_hang() {
    let (status, out) = run("cat; echo done", &path_env(), Path::new("/")).await;
    assert!(status.success());
    assert_eq!(out.trim(), "done");
  }

  #[tokio::test]
  async fn terminate_kills_the_whole_group() {
    let dir = tempfile::TempDir::new().unwrap();
    let pidfile = dir.path().join("grandchild.pid");
    let h = harness();
    let sh = shell();
    // The shell backgrounds a grandchild, then waits forever itself.
    let command = format!(
      "sh -c 'echo $$ > {}; sleep 60' & sleep 60",
      pidfile.display()
    );
    let env = path_env();
    let spec = SpawnSpec {
      shell: &sh,
      command: &command,
      cwd: dir.path(),
      env: &env,
    };
    let mut pg = spawn(&spec).unwrap();
    let readers = pg.pump(TaskIdx(0), h.printer.clone());

    let grandchild: i32 = wait_for(|| std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok())
      .await
      .expect("grandchild never reported its pid");
    assert!(alive(grandchild), "grandchild should be running");

    let status = pg.terminate(Duration::from_secs(2)).await.unwrap();
    readers.drain(Duration::from_secs(1)).await;
    assert!(!status.success());

    // Signal delivery to the group is asynchronous; give it a moment.
    assert!(
      wait_for(|| (!alive(grandchild)).then_some(()))
        .await
        .is_some(),
      "grandchild {grandchild} survived the group kill"
    );
  }

  fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs the permission and existence check only.
    unsafe { libc::kill(pid, 0) == 0 }
  }

  /// Polls `f` for up to two seconds.
  async fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
    for _ in 0..100 {
      if let Some(v) = f() {
        return Some(v);
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
  }
}
