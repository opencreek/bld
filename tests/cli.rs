//! End-to-end tests: real workspaces on disk, driven through the binary.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// A throwaway workspace with a git directory, so root discovery and
/// gitignore handling behave as they do in a real repository.
struct Fixture {
  _dir: TempDir,
  root: PathBuf,
}

impl Fixture {
  fn new() -> Self {
    let dir = TempDir::new().unwrap();
    // macOS reaches the temp directory through a symlink.
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join(".gitignore"), "dist/\n.bld/\n").unwrap();
    Self { _dir: dir, root }
  }

  fn write(&self, rel: &str, contents: &str) -> PathBuf {
    let path = self.root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, contents).unwrap();
    path
  }

  fn path(&self, rel: &str) -> PathBuf {
    self.root.join(rel)
  }

  fn exists(&self, rel: &str) -> bool {
    self.path(rel).exists()
  }

  fn read(&self, rel: &str) -> String {
    std::fs::read_to_string(self.path(rel)).unwrap()
  }

  fn command(&self) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bld"));
    cmd.current_dir(&self.root);
    // Edition 2024 makes `set_var` unsafe, and the child's environment is
    // what these tests are about anyway.
    cmd.env_clear();
    for name in ["PATH", "HOME"] {
      if let Some(value) = std::env::var_os(name) {
        cmd.env(name, value);
      }
    }
    cmd.env("NO_COLOR", "1");
    cmd
  }

  fn bld(&self, args: &[&str]) -> Run {
    Run::from(self.command().args(args).output().unwrap())
  }

  /// How many entries the cache holds, not counting the scratch directory.
  fn cache_entries(&self) -> usize {
    let dir = self.path(".bld/cache");
    if !dir.is_dir() {
      return 0;
    }
    std::fs::read_dir(dir)
      .unwrap()
      .filter_map(Result::ok)
      .filter(|e| e.file_name() != "tmp")
      .count()
  }
}

struct Run {
  code: i32,
  out: String,
}

impl From<Output> for Run {
  fn from(output: Output) -> Self {
    let mut out = String::from_utf8_lossy(&output.stdout).into_owned();
    out.push_str(&String::from_utf8_lossy(&output.stderr));
    Self {
      code: output.status.code().unwrap_or(-1),
      out,
    }
  }
}

impl Run {
  #[track_caller]
  fn ok(&self) -> &Self {
    assert_eq!(
      self.code, 0,
      "expected success, got {}:\n{}",
      self.code, self.out
    );
    self
  }

  #[track_caller]
  fn code(&self, expected: i32) -> &Self {
    assert_eq!(self.code, expected, "unexpected exit code:\n{}", self.out);
    self
  }

  #[track_caller]
  fn has(&self, needle: &str) -> &Self {
    assert!(
      self.out.contains(needle),
      "expected {needle:?} in:\n{}",
      self.out
    );
    self
  }

  #[track_caller]
  fn lacks(&self, needle: &str) -> &Self {
    assert!(
      !self.out.contains(needle),
      "did not expect {needle:?} in:\n{}",
      self.out
    );
    self
  }

  /// Asserts that one line comes before another.
  #[track_caller]
  fn before(&self, first: &str, second: &str) -> &Self {
    let a = self
      .out
      .find(first)
      .unwrap_or_else(|| panic!("{first:?} missing:\n{}", self.out));
    let b = self
      .out
      .find(second)
      .unwrap_or_else(|| panic!("{second:?} missing:\n{}", self.out));
    assert!(
      a < b,
      "expected {first:?} before {second:?} in:\n{}",
      self.out
    );
    self
  }
}

/// A single-package workspace whose build copies a source file.
fn simple(fx: &Fixture) {
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/web/bld.toml",
    r#"
      [tasks.build]
      command = "mkdir -p dist && cp src/in.txt dist/out.txt && echo built"
      inputs = ["src/**"]
      outputs = ["dist/**"]
    "#,
  );
  fx.write("packages/web/src/in.txt", "one");
}

#[test]
fn runs_a_task_and_caches_its_outputs() {
  let fx = Fixture::new();
  simple(&fx);

  fx.bld(&["run", "build"])
    .ok()
    .has("web#build")
    .has("built")
    .has("1 ran");
  assert_eq!(fx.read("packages/web/dist/out.txt"), "one");
  assert_eq!(fx.cache_entries(), 1);
}

#[test]
fn replays_logs_and_restores_outputs_on_a_cache_hit() {
  let fx = Fixture::new();
  simple(&fx);
  fx.bld(&["run", "build"]).ok();

  std::fs::remove_dir_all(fx.path("packages/web/dist")).unwrap();
  fx.bld(&["run", "build"])
    .ok()
    .has("cache hit")
    .has("built")
    .has("1 cached");
  assert_eq!(
    fx.read("packages/web/dist/out.txt"),
    "one",
    "outputs should come back from the cache"
  );
  assert_eq!(fx.cache_entries(), 1, "a hit must not write a new entry");
}

#[test]
fn hidden_cached_logs_are_not_replayed() {
  let fx = Fixture::new();
  simple(&fx);
  fx.write(
    "packages/web/bld.toml",
    r#"
      [tasks.build]
      command = "mkdir -p dist && cp src/in.txt dist/out.txt && echo built"
      inputs = ["src/**"]
      outputs = ["dist/**"]
      show_cached_logs = false
    "#,
  );
  fx.bld(&["run", "build"]).ok().has("built");
  fx.bld(&["run", "build"])
    .ok()
    .has("cache hit")
    .lacks("built");
}

#[test]
fn changing_an_input_misses_the_cache() {
  let fx = Fixture::new();
  simple(&fx);
  fx.bld(&["run", "build"]).ok();

  fx.write("packages/web/src/in.txt", "two");
  fx.bld(&["run", "build"])
    .ok()
    .lacks("cache hit")
    .has("1 ran");
  assert_eq!(fx.read("packages/web/dist/out.txt"), "two");
  assert_eq!(fx.cache_entries(), 2);

  // Going back to the old contents finds the old entry again.
  fx.write("packages/web/src/in.txt", "one");
  fx.bld(&["run", "build"]).ok().has("cache hit");
  assert_eq!(fx.cache_entries(), 2);
}

#[test]
fn excluded_inputs_do_not_invalidate() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/web/bld.toml",
    r#"
      [tasks.build]
      command = "echo built"
      inputs = ["src/**", "!src/**/*.test.ts"]
    "#,
  );
  fx.write("packages/web/src/a.ts", "source");
  fx.write("packages/web/src/a.test.ts", "test");
  fx.bld(&["run", "build"]).ok();

  fx.write("packages/web/src/a.test.ts", "changed test");
  fx.bld(&["run", "build"]).ok().has("cache hit");

  fx.write("packages/web/src/a.ts", "changed source");
  fx.bld(&["run", "build"]).ok().lacks("cache hit");
}

#[test]
fn gitignored_files_are_not_inputs() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/web/bld.toml",
    "[tasks.build]\ncommand = \"echo built\"\n",
  );
  fx.write("packages/web/src/a.ts", "source");
  fx.bld(&["run", "build"]).ok();

  fx.write(
    "packages/web/dist/generated.js",
    "output of some other tool",
  );
  fx.bld(&["run", "build"]).ok().has("cache hit");
}

#[test]
fn declared_env_is_hashed_and_passed_through() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/web/bld.toml",
    r#"
      [tasks.build]
      command = "echo mode=$MODE token=${TOKEN:-none} secret=${SECRET:-none}"
      env = ["MODE"]
      pass_through_env = ["TOKEN"]
    "#,
  );

  let run = |mode: &str, token: &str, secret: &str| {
    Run::from(
      fx.command()
        .args(["run", "build"])
        .env("MODE", mode)
        .env("TOKEN", token)
        .env("SECRET", secret)
        .output()
        .unwrap(),
    )
  };

  run("dev", "t1", "s1")
    .ok()
    .has("mode=dev token=t1 secret=none")
    .lacks("cache hit");
  // A hashed variable changes the hash.
  run("prod", "t1", "s1")
    .ok()
    .lacks("cache hit")
    .has("mode=prod");
  // A pass-through variable does not.
  run("prod", "t2", "s2").ok().has("cache hit");
}

#[test]
fn a_global_input_invalidates_every_task() {
  let fx = Fixture::new();
  fx.write(
    "bld.toml",
    "packages = [\"packages/*\"]\ninputs = [\"bld.lock\"]\n",
  );
  fx.write("bld.lock", "v1");
  fx.write(
    "packages/web/bld.toml",
    "[tasks.build]\ncommand = \"echo built\"\n",
  );
  fx.bld(&["run", "build"]).ok();
  fx.bld(&["run", "build"]).ok().has("cache hit");

  fx.write("bld.lock", "v2");
  fx.bld(&["run", "build"]).ok().lacks("cache hit");
}

#[test]
fn dependencies_run_before_dependents() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/core/bld.toml",
    r#"
      [tasks.build]
      command = "mkdir -p dist && echo core > dist/marker && echo core-done"
      outputs = ["dist/**"]
    "#,
  );
  fx.write(
    "packages/web/bld.toml",
    r#"
      depends_on = ["core"]
      [tasks.build]
      command = "test -f ../core/dist/marker && echo web-done"
      depends_on = ["^build"]
    "#,
  );
  fx.write("packages/core/src.txt", "x");
  fx.write("packages/web/src.txt", "x");

  fx.bld(&["run", "build"])
    .ok()
    .before("core-done", "web-done")
    .has("2 ran");
}

#[test]
fn a_task_without_a_command_only_orders_others() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/web/bld.toml",
    r#"
      [tasks.first]
      command = "echo first"
      [tasks.gate]
      depends_on = ["first"]
      [tasks.last]
      command = "echo last"
      depends_on = ["gate"]
    "#,
  );
  fx.bld(&["run", "last"]).ok().before("first", "last");
}

#[test]
fn a_failing_task_stops_its_dependents() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/core/bld.toml",
    "[tasks.build]\ncommand = \"echo core-ran; exit 3\"\n",
  );
  fx.write(
    "packages/web/bld.toml",
    r#"
      depends_on = ["core"]
      [tasks.build]
      command = "echo web-ran"
      depends_on = ["^build"]
    "#,
  );
  fx.write(
    "packages/other/bld.toml",
    "[tasks.build]\ncommand = \"echo other-ran\"\n",
  );

  fx.bld(&["run", "core#build", "web#build"])
    .code(1)
    .has("core-ran")
    .lacks("web-ran")
    .has("exit code 3");
  assert_eq!(fx.cache_entries(), 0, "failures are never cached");

  // --continue still skips dependents, but unrelated work goes ahead.
  fx.bld(&["run", "build", "--continue"])
    .code(1)
    .has("core-ran")
    .has("other-ran")
    .lacks("web-ran");
}

#[test]
fn caching_can_be_turned_off_per_task() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/web/bld.toml",
    "[tasks.build]\ncommand = \"echo built\"\ncache = false\n",
  );
  fx.bld(&["run", "build"]).ok().has("built");
  fx.bld(&["run", "build"])
    .ok()
    .has("built")
    .lacks("cache hit");
  assert_eq!(fx.cache_entries(), 0);
}

#[test]
fn force_reruns_a_cached_task() {
  let fx = Fixture::new();
  simple(&fx);
  fx.bld(&["run", "build"]).ok();
  fx.bld(&["run", "build"]).ok().has("cache hit");
  fx.bld(&["run", "build", "--force"])
    .ok()
    .lacks("cache hit")
    .has("1 ran");
}

#[test]
fn root_tasks_and_filters_select_the_right_packages() {
  let fx = Fixture::new();
  fx.write(
    "bld.toml",
    "packages = [\"packages/*\"]\n[tasks.lint]\ncommand = \"echo linted\"\n",
  );
  fx.write(
    "packages/a/bld.toml",
    "[tasks.build]\ncommand = \"echo a-built\"\n",
  );
  fx.write(
    "packages/b/bld.toml",
    "[tasks.build]\ncommand = \"echo b-built\"\n",
  );

  fx.bld(&["run", "//#lint"])
    .ok()
    .has("linted")
    .lacks("a-built");
  fx.bld(&["run", "build"]).ok().has("a-built").has("b-built");
  fx.bld(&["run", "build", "--filter", "a"])
    .ok()
    .has("a-built")
    .lacks("b-built");
}

/// Three packages whose task brackets a short sleep with markers, so the
/// order in the shared log shows whether they overlapped.
fn overlapping(fx: &Fixture) {
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  for name in ["a", "b", "c"] {
    fx.write(
      &format!("packages/{name}/bld.toml"),
      &format!(
        r#"
          [tasks.work]
          command = "echo start-{name} >> ../../log; sleep 0.3; echo end-{name} >> ../../log"
          cache = false
        "#
      ),
    );
  }
}

/// True when some task started while another was still running.
fn overlapped(log: &str) -> bool {
  let events: Vec<&str> = log.lines().filter(|l| !l.is_empty()).collect();
  events
    .windows(2)
    .any(|w| w[0].starts_with("start") && w[1].starts_with("start"))
}

#[test]
fn concurrency_one_runs_tasks_one_at_a_time() {
  let fx = Fixture::new();
  overlapping(&fx);
  fx.bld(&["run", "work", "-c", "1"]).ok();
  let log = fx.read("log");
  assert_eq!(log.lines().count(), 6, "every task should have run:\n{log}");
  assert!(!overlapped(&log), "tasks overlapped despite -c 1:\n{log}");
}

#[test]
fn a_higher_concurrency_runs_tasks_together() {
  let fx = Fixture::new();
  overlapping(&fx);
  fx.bld(&["run", "work", "-c", "3"]).ok();
  let log = fx.read("log");
  assert_eq!(log.lines().count(), 6);
  assert!(overlapped(&log), "tasks did not run concurrently:\n{log}");
}

#[test]
fn dry_run_prints_the_plan_without_running_anything() {
  let fx = Fixture::new();
  simple(&fx);
  fx.bld(&["run", "build", "--dry-run"])
    .ok()
    .has("web#build")
    .lacks("built");
  assert!(!fx.exists("packages/web/dist"), "nothing should have run");
}

#[test]
fn hash_reports_the_same_hash_the_cache_uses() {
  let fx = Fixture::new();
  simple(&fx);

  let run = fx.bld(&["hash", "build"]);
  run.ok().has("web#build");
  let hash = run
    .out
    .lines()
    .find(|l| l.contains("web#build"))
    .and_then(|l| l.split_whitespace().next())
    .expect("a hash on the task's line")
    .to_string();
  assert_eq!(hash.len(), 16, "expected 16 hex digits, got {hash:?}");

  // The cache entry a run creates is named after exactly that hash.
  fx.bld(&["run", "build"]).ok();
  assert!(
    fx.exists(&format!(".bld/cache/{hash}")),
    "no cache entry named {hash}:\n{}",
    run.out
  );

  // Changing an input changes it.
  fx.write("packages/web/src/in.txt", "different");
  fx.bld(&["hash", "build"]).ok().lacks(&hash);
}

#[test]
fn hash_can_list_the_files_behind_a_task() {
  let fx = Fixture::new();
  simple(&fx);
  fx.bld(&["hash", "web#build", "--files"])
    .ok()
    .has("src/in.txt")
    .lacks("dist/");
}

#[test]
fn clean_removes_the_cache() {
  let fx = Fixture::new();
  simple(&fx);
  fx.bld(&["run", "build"]).ok();
  assert_eq!(fx.cache_entries(), 1);
  fx.bld(&["clean"]).ok();
  assert_eq!(fx.cache_entries(), 0);
}

#[test]
fn configuration_errors_exit_with_two() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/web/bld.toml",
    "[tasks.build]\ncommand = \"echo x\"\ndepends_on = [\"ghost\"]\n",
  );
  fx.bld(&["run", "build"]).code(2).has("no task `ghost`");
  fx.bld(&["run", "nosuchtask"]).code(2);
}

#[test]
fn a_task_runs_in_its_own_package_directory() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/web/bld.toml",
    "[tasks.where]\ncommand = \"pwd\"\n",
  );
  let run = fx.bld(&["run", "where"]);
  run.ok().has(fx.path("packages/web").to_str().unwrap());
}

/// Polls `f` until it returns true, for up to `limit`.
fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
  let deadline = Instant::now() + limit;
  while Instant::now() < deadline {
    if f() {
      return true;
    }
    std::thread::sleep(Duration::from_millis(50));
  }
  false
}

fn interrupt(pid: u32) {
  Command::new("kill")
    .args(["-INT", &pid.to_string()])
    .status()
    .expect("sending SIGINT");
}

#[test]
fn an_interrupt_stops_a_persistent_task_and_its_children() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  // The shell starts a grandchild, which a plain kill of the shell would
  // leave behind, and records its pid so the test can watch for it.
  fx.write(
    "packages/web/bld.toml",
    r#"
      [tasks.dev]
      command = "sleep 989796 & echo $! > grandchild.pid; echo dev-up; wait"
      persistent = true
    "#,
  );
  let log = fx.path("watch.out");
  let file = std::fs::File::create(&log).unwrap();
  let mut child = fx
    .command()
    .args(["run", "dev"])
    .stdout(Stdio::from(file))
    .stderr(Stdio::null())
    .spawn()
    .unwrap();

  assert!(
    wait_until(Duration::from_secs(10), || {
      std::fs::read_to_string(&log).is_ok_and(|s| s.contains("dev-up"))
    }),
    "the dev task never started"
  );
  let grandchild = std::fs::read_to_string(fx.path("packages/web/grandchild.pid"))
    .expect("the dev task should have recorded its child's pid")
    .trim()
    .to_string();
  assert!(alive(&grandchild), "the grandchild should be running");

  interrupt(child.id());
  let exited = wait_until(Duration::from_secs(10), || {
    matches!(child.try_wait(), Ok(Some(_)))
  });
  assert!(exited, "bld did not exit after the interrupt");
  assert_eq!(child.wait().unwrap().code(), Some(130));
  assert!(
    wait_until(Duration::from_secs(5), || !alive(&grandchild)),
    "the grandchild outlived the interrupt, so the process group was not signalled"
  );
}

/// Whether a process still exists, by pid rather than by name: a name match
/// would also find shells that merely mention the command.
fn alive(pid: &str) -> bool {
  Command::new("kill")
    .args(["-0", pid])
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

#[test]
fn watch_reruns_the_affected_tasks_when_an_input_changes() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/core/bld.toml",
    r#"
      [tasks.build]
      command = "cat src/in.txt"
      inputs = ["src/**"]
    "#,
  );
  fx.write(
    "packages/other/bld.toml",
    r#"
      [tasks.build]
      command = "echo other-built"
      inputs = ["src/**"]
    "#,
  );
  fx.write("packages/core/src/in.txt", "first");
  fx.write("packages/other/src/in.txt", "x");

  let log = fx.path("watch.out");
  let file = std::fs::File::create(&log).unwrap();
  let mut child = fx
    .command()
    .args(["watch", "build"])
    .stdout(Stdio::from(file))
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
  let read = || std::fs::read_to_string(&log).unwrap_or_default();

  assert!(
    wait_until(Duration::from_secs(10), || read()
      .contains("watching for changes")),
    "the first run never finished:\n{}",
    read()
  );
  assert!(read().contains("first") && read().contains("other-built"));

  fx.write("packages/core/src/in.txt", "second");
  assert!(
    wait_until(Duration::from_secs(10), || read().contains("second")),
    "the change did not trigger a rebuild:\n{}",
    read()
  );
  let after = read();
  let tail = &after[after.find("second").unwrap()..];
  assert!(
    !tail.contains("other-built"),
    "an unaffected package should not run again:\n{after}"
  );

  interrupt(child.id());
  assert!(
    wait_until(Duration::from_secs(10), || matches!(
      child.try_wait(),
      Ok(Some(_))
    )),
    "watch did not exit after the interrupt"
  );
}

/// On Linux each directory carries its own watch, so a directory that did not
/// exist when the watch started has to be registered before anything inside it
/// can be seen.
#[test]
fn watch_notices_a_file_in_a_directory_created_after_it_started() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/core/bld.toml",
    r#"
      [tasks.build]
      command = "cat src/nested/in.txt 2>/dev/null || echo nothing-yet"
      inputs = ["src/**"]
    "#,
  );
  fx.write("packages/core/src/keep.txt", "x");

  let log = fx.path("watch.out");
  let file = std::fs::File::create(&log).unwrap();
  let mut child = fx
    .command()
    .args(["watch", "build"])
    .stdout(Stdio::from(file))
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
  let read = || std::fs::read_to_string(&log).unwrap_or_default();

  assert!(
    wait_until(Duration::from_secs(10), || read()
      .contains("watching for changes")),
    "the first run never finished:\n{}",
    read()
  );
  assert!(read().contains("nothing-yet"));

  // Creates the directory and the file together, the way a checkout or a
  // scaffolding tool would.
  fx.write("packages/core/src/nested/in.txt", "late-arrival");
  assert!(
    wait_until(Duration::from_secs(10), || read().contains("late-arrival")),
    "a file in a new directory did not trigger a rebuild:\n{}",
    read()
  );

  interrupt(child.id());
  assert!(
    wait_until(Duration::from_secs(10), || matches!(
      child.try_wait(),
      Ok(Some(_))
    )),
    "watch did not exit after the interrupt"
  );
}

#[test]
fn watch_picks_up_configuration_changes() {
  let fx = Fixture::new();
  fx.write("bld.toml", "packages = [\"packages/*\"]\n");
  fx.write(
    "packages/core/bld.toml",
    "[tasks.build]\ncommand = \"echo original\"\ninputs = [\"src/**\"]\n",
  );
  fx.write("packages/core/src/in.txt", "x");

  let log = fx.path("watch.out");
  let file = std::fs::File::create(&log).unwrap();
  let mut child = fx
    .command()
    .args(["watch", "build"])
    .stdout(Stdio::from(file))
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
  let read = || std::fs::read_to_string(&log).unwrap_or_default();

  assert!(
    wait_until(Duration::from_secs(10), || read().contains("original")),
    "the first run never happened:\n{}",
    read()
  );

  fx.write(
    "packages/core/bld.toml",
    "[tasks.build]\ncommand = \"echo replaced\"\ninputs = [\"src/**\"]\n",
  );
  assert!(
    wait_until(Duration::from_secs(10), || read().contains("replaced")),
    "the configuration change was not picked up:\n{}",
    read()
  );

  interrupt(child.id());
  assert!(wait_until(Duration::from_secs(10), || matches!(
    child.try_wait(),
    Ok(Some(_))
  )));
}
