//! Input hashing.
//!
//! A task's hash covers everything that could change its output: the command
//! and the shell that runs it, the contents of its input files, the values of
//! the env vars it declared, where it runs, the platform, and the hashes of
//! the tasks it depends on. Every field is written as `tag | length | bytes`,
//! so two different field lists can never produce the same byte stream.

use std::hash::Hasher as _;

use twox_hash::XxHash3_64;

use crate::env::EnvSnapshot;
use crate::walk::{Files, Kind, path_bytes};
use crate::workspace::{TaskIdx, Workspace};

/// Bump when the meaning of any field changes, to invalidate old caches.
const FORMAT: &[u8] = b"bld-hash-v1";

mod tag {
  pub const FORMAT: u8 = 0x01;
  pub const OS: u8 = 0x02;
  pub const ARCH: u8 = 0x03;
  pub const PACKAGE: u8 = 0x10;
  pub const TASK: u8 = 0x11;
  pub const SHELL: u8 = 0x12;
  pub const COMMAND: u8 = 0x13;
  pub const NO_COMMAND: u8 = 0x14;
  pub const ARG: u8 = 0x15;
  pub const GLOBAL_INPUT: u8 = 0x20;
  pub const INPUT: u8 = 0x21;
  pub const FILE_META: u8 = 0x22;
  pub const ENV_NAME: u8 = 0x30;
  pub const ENV_VALUE: u8 = 0x31;
  pub const OUTPUT_GLOB: u8 = 0x40;
  pub const DEP_LABEL: u8 = 0x50;
  pub const DEP_HASH: u8 = 0x51;
}

/// A hasher over length-framed, tagged fields.
pub struct FieldHasher(XxHash3_64);

impl Default for FieldHasher {
  fn default() -> Self {
    Self::new()
  }
}

impl FieldHasher {
  pub fn new() -> Self {
    let mut h = Self(XxHash3_64::new());
    h.field(tag::FORMAT, FORMAT);
    h
  }

  pub fn field(&mut self, tag: u8, bytes: &[u8]) {
    self.0.write_u8(tag);
    self.0.write_u64(bytes.len() as u64);
    self.0.write(bytes);
  }

  pub fn num(&mut self, tag: u8, value: u64) {
    self.0.write_u8(tag);
    self.0.write_u64(8);
    self.0.write_u64(value);
  }

  pub fn finish(&self) -> u64 {
    self.0.finish()
  }
}

/// Hashes everything about a task except its dependencies: its identity, its
/// command, its input files and its declared env vars.
pub fn inputs_hash(
  ws: &Workspace,
  task: TaskIdx,
  package_files: &Files,
  global_files: &Files,
  env: &EnvSnapshot,
  args: &[String],
) -> u64 {
  let def = ws.task(task);
  let mut h = FieldHasher::new();
  h.field(tag::OS, std::env::consts::OS.as_bytes());
  h.field(tag::ARCH, std::env::consts::ARCH.as_bytes());
  h.field(tag::PACKAGE, path_bytes(&ws.pkg(def.pkg).rel));
  h.field(tag::TASK, def.name.as_bytes());
  for word in &ws.settings.shell {
    h.field(tag::SHELL, word.as_bytes());
  }
  match &def.command {
    Some(cmd) => h.field(tag::COMMAND, cmd.as_bytes()),
    None => h.field(tag::NO_COMMAND, b""),
  }
  // Nothing is written when there are none, so adding this field left every
  // hash bld had already written exactly where it was.
  for arg in args {
    h.field(tag::ARG, arg.as_bytes());
  }

  // Global inputs first; both lists are already sorted by path.
  for (path, entry) in &global_files.entries {
    h.field(tag::GLOBAL_INPUT, path_bytes(path));
    file_meta(&mut h, entry.kind, entry.exec, entry.hash);
  }
  for (path, entry) in &package_files.entries {
    if !def.is_input(path) {
      continue;
    }
    h.field(tag::INPUT, path_bytes(path));
    file_meta(&mut h, entry.kind, entry.exec, entry.hash);
  }

  for (name, value) in env.matching(&def.env) {
    h.field(tag::ENV_NAME, name.as_bytes());
    h.field(tag::ENV_VALUE, os_bytes(value));
  }
  // Output globs are part of the identity: the same command with different
  // outputs produces a different cache entry.
  for glob in &def.output_patterns {
    h.field(tag::OUTPUT_GLOB, glob.as_bytes());
  }
  h.finish()
}

/// Combines a task's own hash with the hashes of what it depends on.
pub fn task_hash(inputs: u64, deps: &[(String, u64)]) -> u64 {
  let mut sorted: Vec<&(String, u64)> = deps.iter().collect();
  sorted.sort_by(|a, b| a.0.cmp(&b.0));
  let mut h = FieldHasher::new();
  h.num(tag::DEP_HASH, inputs);
  for (label, hash) in sorted {
    h.field(tag::DEP_LABEL, label.as_bytes());
    h.num(tag::DEP_HASH, *hash);
  }
  h.finish()
}

fn file_meta(h: &mut FieldHasher, kind: Kind, exec: bool, hash: u64) {
  let kind = match kind {
    Kind::File => 0u8,
    Kind::Symlink => 1,
  };
  h.field(tag::FILE_META, &[kind, u8::from(exec)]);
  h.num(tag::FILE_META, hash);
}

fn os_bytes(value: &std::ffi::OsStr) -> &[u8] {
  use std::os::unix::ffi::OsStrExt;
  value.as_bytes()
}

/// Sixteen hex digits, the on-disk name of a cache entry.
pub fn hex(hash: u64) -> String {
  format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn framing_separates_adjacent_fields() {
    let mut a = FieldHasher::new();
    a.field(tag::TASK, b"ab");
    a.field(tag::TASK, b"c");
    let mut b = FieldHasher::new();
    b.field(tag::TASK, b"a");
    b.field(tag::TASK, b"bc");
    assert_ne!(a.finish(), b.finish());
  }

  #[test]
  fn tags_distinguish_otherwise_equal_fields() {
    let mut a = FieldHasher::new();
    a.field(tag::TASK, b"x");
    let mut b = FieldHasher::new();
    b.field(tag::PACKAGE, b"x");
    assert_ne!(a.finish(), b.finish());
  }

  #[test]
  fn same_fields_hash_the_same() {
    let build = || {
      let mut h = FieldHasher::new();
      h.field(tag::TASK, b"build");
      h.num(tag::DEP_HASH, 7);
      h.finish()
    };
    assert_eq!(build(), build());
  }

  #[test]
  fn dependency_order_does_not_matter() {
    let deps_a = vec![("a#build".to_string(), 1u64), ("b#build".to_string(), 2)];
    let deps_b = vec![("b#build".to_string(), 2u64), ("a#build".to_string(), 1)];
    assert_eq!(task_hash(9, &deps_a), task_hash(9, &deps_b));
  }

  #[test]
  fn dependency_hashes_change_the_task_hash() {
    let base = vec![("a#build".to_string(), 1u64)];
    let changed = vec![("a#build".to_string(), 2u64)];
    assert_ne!(task_hash(9, &base), task_hash(9, &changed));
    assert_ne!(task_hash(9, &base), task_hash(10, &base));
    assert_ne!(task_hash(9, &base), task_hash(9, &[]));
  }

  #[test]
  fn hex_is_fixed_width() {
    assert_eq!(hex(0), "0000000000000000");
    assert_eq!(hex(u64::MAX), "ffffffffffffffff");
  }
}
