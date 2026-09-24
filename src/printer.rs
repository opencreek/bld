//! Interleaved output.
//!
//! Every byte a task writes, plus every status line bld writes, goes through
//! one channel into a single writer thread. That thread owns line splitting,
//! the colored `package#task` prefix, and the log capture used for caching,
//! so what gets cached is byte-identical to what was shown.

use std::io::{BufWriter, IsTerminal, Write};

use owo_colors::{OwoColorize, Style};
use tokio::sync::{mpsc, oneshot};

use crate::cli::ColorChoice;
use crate::workspace::TaskIdx;

/// Log bytes kept in memory for one task before capture stops.
const CAPTURE_LIMIT: usize = 64 << 20;

const MARKER_TRUNCATED: &[u8] = b"[bld] log truncated\n";

#[derive(Debug)]
pub enum OutMsg {
  /// Starts a task's output block. `capture` keeps the bytes for the cache.
  Begin { task: TaskIdx, capture: bool },
  /// Raw bytes from a child process or a replayed log.
  Chunk { task: TaskIdx, data: Vec<u8> },
  /// A line written by bld itself, optionally attributed to a task.
  Status { task: Option<TaskIdx>, line: String },
  /// Flushes a trailing partial line and returns the captured log.
  End {
    task: TaskIdx,
    reply: oneshot::Sender<Captured>,
  },
  /// Waits until everything queued so far has been written.
  Barrier(oneshot::Sender<()>),
  /// Replaces the task table after the workspace is reloaded.
  Labels { labels: Vec<String>, width: usize },
  /// Stops the writer thread even if handles are still alive.
  Shutdown,
}

/// The bytes a task produced, for storing in the cache.
#[derive(Debug, Default)]
pub struct Captured {
  pub bytes: Vec<u8>,
  pub truncated: bool,
}

/// Cloneable sender used by tasks, the runner and cache replay.
#[derive(Debug, Clone)]
pub struct PrinterHandle {
  tx: mpsc::Sender<OutMsg>,
}

impl PrinterHandle {
  /// Sends from async code. A closed printer is not an error: it only means
  /// the process is shutting down.
  pub async fn send(&self, msg: OutMsg) {
    let _ = self.tx.send(msg).await;
  }

  /// Re-renders the prefixes, for instance after a config reload changed
  /// which tasks exist.
  pub async fn set_labels(&self, labels: Vec<String>, width: usize) {
    self.send(OutMsg::Labels { labels, width }).await;
  }

  pub async fn begin(&self, task: TaskIdx, capture: bool) {
    self.send(OutMsg::Begin { task, capture }).await;
  }

  pub async fn chunk(&self, task: TaskIdx, data: Vec<u8>) {
    self.send(OutMsg::Chunk { task, data }).await;
  }

  pub async fn status(&self, task: TaskIdx, line: impl Into<String>) {
    self
      .send(OutMsg::Status {
        task: Some(task),
        line: line.into(),
      })
      .await;
  }

  pub async fn note(&self, line: impl Into<String>) {
    self
      .send(OutMsg::Status {
        task: None,
        line: line.into(),
      })
      .await;
  }

  /// Ends a task's output block and returns whatever was captured.
  pub async fn end(&self, task: TaskIdx) -> Captured {
    let (reply, rx) = oneshot::channel();
    self.send(OutMsg::End { task, reply }).await;
    rx.await.unwrap_or_default()
  }

  /// A handle wired to a plain channel, for asserting on messages in tests.
  #[cfg(test)]
  pub fn for_test(tx: mpsc::Sender<OutMsg>) -> Self {
    Self { tx }
  }

  /// Waits for all queued output to reach the terminal.
  pub async fn barrier(&self) {
    let (reply, rx) = oneshot::channel();
    self.send(OutMsg::Barrier(reply)).await;
    let _ = rx.await;
  }
}

/// Owns the writer thread; dropping it closes the channel and joins.
pub struct Printer {
  handle: Option<PrinterHandle>,
  thread: Option<std::thread::JoinHandle<()>>,
}

impl Printer {
  /// Starts the writer thread. `labels` is indexed by [`TaskIdx`]; `width`
  /// is the prefix column width: the longest active label when aligning, else 0.
  pub fn start(labels: Vec<String>, width: usize, color: bool) -> Self {
    let (tx, rx) = mpsc::channel(1024);
    let thread = std::thread::Builder::new()
      .name("bld-printer".into())
      .spawn(move || writer_loop(rx, labels, width, color))
      .expect("spawning the printer thread");
    Self {
      handle: Some(PrinterHandle { tx }),
      thread: Some(thread),
    }
  }

  pub fn handle(&self) -> PrinterHandle {
    self.handle.clone().expect("printer is still running")
  }
}

impl Drop for Printer {
  /// Waits for the last line to reach the terminal. A `Shutdown` message
  /// ends the writer even if some handle is still alive somewhere, so a
  /// forgotten clone cannot hang the process on exit.
  fn drop(&mut self) {
    if let Some(handle) = self.handle.take() {
      let mut msg = OutMsg::Shutdown;
      // The writer drains continuously, so a full channel clears at once.
      for _ in 0..10_000 {
        match handle.tx.try_send(msg) {
          Ok(()) => break,
          Err(mpsc::error::TrySendError::Full(returned)) => {
            msg = returned;
            std::thread::yield_now();
          }
          Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
      }
    }
    if let Some(t) = self.thread.take() {
      let _ = t.join();
    }
  }
}

/// Whether to emit ANSI colors, honouring `--color`, `NO_COLOR` and the
/// usual force variables.
pub fn use_color(choice: ColorChoice) -> bool {
  match choice {
    ColorChoice::Always => true,
    ColorChoice::Never => false,
    ColorChoice::Auto => {
      if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
      }
      if std::env::var_os("FORCE_COLOR").is_some() || std::env::var_os("CLICOLOR_FORCE").is_some() {
        return true;
      }
      std::io::stdout().is_terminal() && std::env::var("TERM").as_deref() != Ok("dumb")
    }
  }
}

struct TaskOut {
  prefix: Vec<u8>,
  partial: Vec<u8>,
  capture: Option<Captured>,
}

fn writer_loop(mut rx: mpsc::Receiver<OutMsg>, labels: Vec<String>, width: usize, color: bool) {
  let mut tasks = build_tasks(&labels, width, color);
  let stdout = std::io::stdout();
  let mut out = BufWriter::with_capacity(64 << 10, stdout.lock());
  'outer: while let Some(msg) = rx.blocking_recv() {
    let mut stop = matches!(msg, OutMsg::Shutdown);
    handle(&mut out, &mut tasks, msg, color);
    // Drain whatever else is queued before paying for a flush.
    while let Ok(msg) = rx.try_recv() {
      stop |= matches!(msg, OutMsg::Shutdown);
      handle(&mut out, &mut tasks, msg, color);
    }
    let _ = out.flush();
    if stop {
      break 'outer;
    }
  }
  let _ = out.flush();
}

fn build_tasks(labels: &[String], width: usize, color: bool) -> Vec<TaskOut> {
  let styles = palette();
  labels
    .iter()
    .enumerate()
    .map(|(i, label)| TaskOut {
      prefix: render_prefix(label, width, color.then(|| styles[i % styles.len()])),
      partial: Vec::new(),
      capture: None,
    })
    .collect()
}

fn handle(out: &mut impl Write, tasks: &mut Vec<TaskOut>, msg: OutMsg, color: bool) {
  match msg {
    OutMsg::Begin { task, capture } => {
      let Some(t) = tasks.get_mut(task.i()) else {
        return;
      };
      t.partial.clear();
      t.capture = capture.then(Captured::default);
    }
    OutMsg::Chunk { task, data } => {
      let Some(t) = tasks.get_mut(task.i()) else {
        return;
      };
      if let Some(cap) = &mut t.capture {
        if cap.bytes.len() + data.len() > CAPTURE_LIMIT {
          if !cap.truncated {
            cap.truncated = true;
            cap.bytes.extend_from_slice(MARKER_TRUNCATED);
          }
        } else {
          cap.bytes.extend_from_slice(&data);
        }
      }
      write_lines(out, t, &data);
    }
    OutMsg::Status { task, line } => {
      if let Some(t) = task.and_then(|task| tasks.get_mut(task.i())) {
        flush_partial(out, t);
        let _ = out.write_all(&t.prefix);
      }
      if color {
        let _ = writeln!(out, "{}", line.dimmed());
      } else {
        let _ = writeln!(out, "{line}");
      }
    }
    OutMsg::End { task, reply } => {
      // A stale index is possible only right after a config reload, and the
      // caller must still be answered.
      let captured = match tasks.get_mut(task.i()) {
        Some(t) => {
          flush_partial(out, t);
          t.capture.take().unwrap_or_default()
        }
        None => Captured::default(),
      };
      let _ = reply.send(captured);
    }
    OutMsg::Barrier(reply) => {
      let _ = out.flush();
      let _ = reply.send(());
    }
    OutMsg::Labels { labels, width } => {
      for t in tasks.iter_mut() {
        flush_partial(out, t);
      }
      *tasks = build_tasks(&labels, width, color);
    }
    OutMsg::Shutdown => {}
  }
}

/// Writes every complete line in `data`, keeping any trailing partial line.
fn write_lines(out: &mut impl Write, t: &mut TaskOut, data: &[u8]) {
  let mut rest = data;
  while let Some(nl) = memchr(b'\n', rest) {
    let (line, after) = rest.split_at(nl + 1);
    let _ = out.write_all(&t.prefix);
    if t.partial.is_empty() {
      let _ = out.write_all(line);
    } else {
      let _ = out.write_all(&t.partial);
      t.partial.clear();
      let _ = out.write_all(line);
    }
    rest = after;
  }
  t.partial.extend_from_slice(rest);
}

/// Emits a trailing line that never got its newline, so a task's last line is
/// not swallowed and does not run into the next task's prefix.
fn flush_partial(out: &mut impl Write, t: &mut TaskOut) {
  if t.partial.is_empty() {
    return;
  }
  let _ = out.write_all(&t.prefix);
  let _ = out.write_all(&t.partial);
  let _ = out.write_all(b"\n");
  if let Some(cap) = &mut t.capture
    && !cap.truncated
    && !cap.bytes.ends_with(b"\n")
  {
    cap.bytes.push(b'\n');
  }
  t.partial.clear();
}

fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
  haystack.iter().position(|&b| b == needle)
}

fn render_prefix(label: &str, width: usize, style: Option<Style>) -> Vec<u8> {
  let padded = format!("{label:<width$}");
  match style {
    Some(style) => format!("{} {} ", padded.style(style), "\u{2502}".dimmed()).into_bytes(),
    None => format!("{padded} | ").into_bytes(),
  }
}

/// Distinct, readable foreground colors cycled across tasks.
fn palette() -> Vec<Style> {
  use owo_colors::AnsiColors as C;
  [
    C::Cyan,
    C::Magenta,
    C::Green,
    C::Yellow,
    C::Blue,
    C::BrightCyan,
    C::BrightMagenta,
    C::BrightGreen,
  ]
  .into_iter()
  .map(|c| Style::new().color(c))
  .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn state(labels: &[&str]) -> Vec<TaskOut> {
    let width = labels.iter().map(|l| l.len()).max().unwrap_or(0);
    labels
      .iter()
      .map(|l| TaskOut {
        prefix: render_prefix(l, width, None),
        partial: Vec::new(),
        capture: None,
      })
      .collect()
  }

  fn drive(labels: &[&str], msgs: Vec<OutMsg>) -> String {
    let mut tasks = state(labels);
    let mut out = Vec::new();
    for msg in msgs {
      handle(&mut out, &mut tasks, msg, false);
    }
    String::from_utf8(out).unwrap()
  }

  fn chunk(task: u32, s: &str) -> OutMsg {
    OutMsg::Chunk {
      task: TaskIdx(task),
      data: s.as_bytes().to_vec(),
    }
  }

  #[test]
  fn prefixes_each_line_and_pads_labels() {
    let out = drive(
      &["a#build", "b#b"],
      vec![chunk(0, "one\ntwo\n"), chunk(1, "three\n")],
    );
    assert_eq!(out, "a#build | one\na#build | two\nb#b     | three\n");
  }

  #[test]
  fn joins_lines_split_across_chunks() {
    let out = drive(
      &["a"],
      vec![chunk(0, "he"), chunk(0, "llo\nwor"), chunk(0, "ld\n")],
    );
    assert_eq!(out, "a | hello\na | world\n");
  }

  #[test]
  fn interleaves_two_tasks_without_mixing_partial_lines() {
    let out = drive(
      &["a", "b"],
      vec![chunk(0, "a-par"), chunk(1, "b-full\n"), chunk(0, "tial\n")],
    );
    assert_eq!(out, "b | b-full\na | a-partial\n");
  }

  #[test]
  fn end_flushes_a_line_without_a_newline() {
    let (reply, _rx) = oneshot::channel();
    let out = drive(
      &["a"],
      vec![
        chunk(0, "no newline"),
        OutMsg::End {
          task: TaskIdx(0),
          reply,
        },
      ],
    );
    assert_eq!(out, "a | no newline\n");
  }

  #[test]
  fn captures_exactly_what_was_written() {
    let mut tasks = state(&["a"]);
    let mut out = Vec::new();
    let msgs = vec![
      OutMsg::Begin {
        task: TaskIdx(0),
        capture: true,
      },
      chunk(0, "one\ntw"),
      chunk(0, "o"),
    ];
    for msg in msgs {
      handle(&mut out, &mut tasks, msg, false);
    }
    let (reply, rx) = oneshot::channel();
    handle(
      &mut out,
      &mut tasks,
      OutMsg::End {
        task: TaskIdx(0),
        reply,
      },
      false,
    );
    let captured = rx.blocking_recv().unwrap();
    assert_eq!(String::from_utf8(captured.bytes).unwrap(), "one\ntwo\n");
    assert!(!captured.truncated);
    assert_eq!(String::from_utf8(out).unwrap(), "a | one\na | two\n");
  }

  #[test]
  fn capture_is_off_unless_requested() {
    let mut tasks = state(&["a"]);
    let mut out = Vec::new();
    handle(&mut out, &mut tasks, chunk(0, "hi\n"), false);
    let (reply, rx) = oneshot::channel();
    handle(
      &mut out,
      &mut tasks,
      OutMsg::End {
        task: TaskIdx(0),
        reply,
      },
      false,
    );
    assert!(rx.blocking_recv().unwrap().bytes.is_empty());
  }

  #[test]
  fn status_lines_carry_the_task_prefix() {
    let out = drive(
      &["a#build"],
      vec![OutMsg::Status {
        task: Some(TaskIdx(0)),
        line: "cache hit".into(),
      }],
    );
    assert_eq!(out, "a#build | cache hit\n");
  }

  #[test]
  fn global_notes_have_no_prefix() {
    let out = drive(
      &["a"],
      vec![OutMsg::Status {
        task: None,
        line: "2 tasks".into(),
      }],
    );
    assert_eq!(out, "2 tasks\n");
  }

  #[test]
  fn passes_through_invalid_utf8() {
    let mut tasks = state(&["a"]);
    let mut out = Vec::new();
    handle(
      &mut out,
      &mut tasks,
      OutMsg::Chunk {
        task: TaskIdx(0),
        data: vec![0xff, 0xfe, b'\n'],
      },
      false,
    );
    assert_eq!(out, b"a | \xff\xfe\n");
  }
}
