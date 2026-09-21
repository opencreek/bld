# bld

A task runner for monorepos: TOML configuration, content-hashed inputs, a
cache that replays logs and restores outputs, and a watch mode that re-runs
only what a change can affect.

```
bld run build                  # every package that defines `build`
bld run web#build --force      # one task, ignoring the cache
bld watch dev                  # run, then keep up to date until interrupted
bld hash build --files         # why did that run again?
bld clean                      # delete the local cache
```

## Configuration

### Root `bld.toml`

Workspace settings plus the tasks of the root package, which is called `//`.
bld finds this file by walking up from the current directory, stopping at the
enclosing git repository.

```toml
packages = ["apps/*", "packages/*"]  # directory globs; a directory without a bld.toml is skipped
concurrency = 8                      # default: the number of available cores
shell = ["sh", "-c"]                 # default
cache_dir = ".bld/cache"             # default, relative to the root
show_cached_logs = true              # default; per-task override below
inputs = ["pnpm-lock.yaml"]          # hashed into every task
env = ["CI", "NODE_ENV"]             # passed to every task and hashed
pass_through_env = ["GITHUB_TOKEN"]  # passed to every task, not hashed
depends_on = []                      # packages the root package depends on

[tasks.lint]
command = "biome check ."
inputs = ["**/*.ts", "biome.json"]
```

### Package `bld.toml`

Each package defines its own tasks in full. Nothing is inherited from the
root except the workspace-wide settings above.

```toml
name = "frontend"           # default: the directory name
depends_on = ["core", "ui"] # other packages; this is what `^task` expands over

[tasks.build]
command = "vite build"      # optional: a task without one only orders other tasks
inputs = ["src/**", "index.html", "!src/**/*.test.ts"]
outputs = ["dist/**"]
depends_on = ["^build", "core#codegen", "typecheck"]
env = ["VITE_API_URL"]
pass_through_env = ["SENTRY_TOKEN"]
cache = true
show_cached_logs = true

[tasks.dev]
command = "vite dev"
persistent = true           # never finishes; nothing may depend on it
interruptible = true        # in watch mode, restart it when its inputs change
depends_on = ["^build"]
```

Dependencies take three forms: `build` is a task in the same package,
`core#build` names another package explicitly, and `^build` means "build in
each of my package dependencies", skipping those that do not define it.

## Caching

A task's hash covers its command and shell, its input files' contents,
permission bits and symlink targets, the values of its declared `env` vars,
its output globs, the platform, and the hashes of the tasks it depends on. On
a hit, the stored log is replayed and the outputs are unpacked; on a miss the
command runs and, if it succeeds, its log and outputs are stored. Failures are
never cached.

Entries live in `cache_dir` as `<hash>/{meta.toml, log, outputs.tar.zst}`,
compressed with zstd at its fastest level. When a task runs that you expected
to be cached, `bld hash <task> --files` prints the hash of every input that
went into it, which is usually enough to spot the one that moved.

## Environment

A task's command sees only three groups of variables: a base allowlist
(`PATH`, `HOME`, `SHELL`, `TMPDIR`, `USER`, `TERM`, `LANG`, `TZ`, `LC_*`),
whatever `env` names, and whatever `pass_through_env` names. Both accept a
trailing `*` wildcard. Everything else is dropped, so a task cannot quietly
depend on ambient state. bld also sets `BLD=1` and `BLD_TASK=<package#task>`.

## Things worth knowing

- **Globs do not cross `/`.** `src/*.ts` matches `src/a.ts` but not
  `src/deep/a.ts`; write `src/**/*.ts`. A pattern with no wildcards also
  covers everything beneath it, so `outputs = ["dist"]` means `dist/**`.
- **Inputs respect `.gitignore`**, including ignore files in parent
  directories up to the repository root. Gitignore your build outputs. As for
  git itself, ignore files have no effect outside a repository.
- **A task's own outputs are never its inputs**, so producing them cannot
  invalidate the task that produced them.
- **Restoring outputs does not delete stale files.** Files that a previous
  build left in an output directory stay there.
- **Input globs may not leave the package.** Make the other directory a
  package and add it to `depends_on`, or use the root `inputs`.
- **Commands run in their own process group** with stdin closed. Ctrl-C
  reaches bld, which passes it on and waits up to five seconds before killing
  the group, so a task's grandchildren cannot outlive the run. A second
  interrupt skips the wait.
- **Watch mode on Linux** uses one inotify watch per directory. A very large
  workspace can exhaust `fs.inotify.max_user_watches`; bld says so when it
  does.

Exit codes: `0` success, `1` a task failed, `2` a configuration or usage
error, `130` interrupted.

## Development

```
cargo test        # unit and end-to-end tests
cargo clippy --all-targets -- --deny warnings
cargo fmt --all
nix flake check   # everything above, as CI runs it
```
