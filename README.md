# bld

A task runner for monorepos: TOML configuration, content-hashed inputs, a
cache that replays logs and restores outputs, and a watch mode that re-runs
only what a change can affect.

```
bld run                        # list the available tasks and their packages
bld run build                  # every package that defines `build`
bld run lint frontend          # one task, one package
bld run lint,check web,api     # several of each, comma separated
bld run web#build --force      # name a package inline, ignoring the cache
bld run test api -- -u          # pass the rest of the line to the task
bld watch dev frontend         # run, then keep up to date until interrupted
bld hash build --files         # why did that run again?
bld clean                      # delete the local cache
```

`r` and `w` are short for `run` and `watch`, and `run` can be left out
entirely: `bld build web` is `bld run build web`. The exception is a task
that shares its name with a command — `bld clean` always deletes the cache,
so a task called `clean` needs `bld run clean`.

## Selecting what to run

`run`, `watch` and `hash` all take the same pair of arguments: the tasks, and
optionally the packages they apply to. Both are comma separated.

```
bld <command> <tasks> [packages]
```

A bare task name runs in every package that defines it; a package list narrows
that down. `package#task` names one task outright and ignores the package list,
and `//` is the workspace root, so `bld run //#lint` runs the root's `lint`.

The two lists are a cross product, not a promise that every cell exists:
`bld run lint,check web,api` runs whatever those four combinations actually
define. Asking for a task that no package defines is an error, and so is a
combination that selects nothing at all — a run that silently did nothing would
be worse than a message.

Package and task names may hold letters, digits, `-`, `_`, `:` and `.`. They
sit next to commas and `#` on the command line, so nothing else is allowed.

### Passing arguments to a task

Everything after `--` goes to the tasks you named — `run`, `watch` and `hash`
all take it:

```
bld run test backend -- --update-snapshots
bld run test -- --filter "a name with spaces"
```

Two things follow from that:

- **Only the tasks you named get them**, never the dependencies those tasks
  pulled in. `bld run build frontend -- --verbose` is a request about
  frontend's build; handing `--verbose` to the schema build it happens to need
  is as likely to break it as to help. turbo matches on the task name instead,
  so there the argument would reach both.
- **They are part of the hash.** A task run with different arguments produced
  something different, so it gets its own cache entry, and running it again
  the same way is still a hit. Nothing is written into the hash when there are
  no arguments, so adding this changed no existing entry.

Arguments are quoted as needed on the way to the shell, so one carrying a
space stays one argument. Everything after `--` belongs to the task, bld's own
flags included, so put them before it.

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
show_cached_logs = false             # default; per-task override below
inputs = ["pnpm-lock.yaml"]          # hashed into every task
env = ["CI", "NODE_ENV"]             # passed to every task and hashed
pass_through_env = ["GITHUB_TOKEN"]  # passed to every task, not hashed
depends_on = []                      # packages the root package depends on
watch_exclude = ["fixtures/huge"]    # directories watch mode never registers

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
show_cached_logs = false    # true replays the cached log on a hit

[tasks.dev]
command = "vite dev"
persistent = true           # never finishes; nothing may depend on it
interruptible = true        # in watch mode, restart it when its inputs change
depends_on = ["^build"]
```

Dependencies take three forms: `build` is a task in the same package,
`core#build` names another package explicitly, and `^build` means "build in
each of my package dependencies", skipping those that do not define it.

### User config

Personal preferences live in `~/.config/bld/config.toml` (or
`$XDG_CONFIG_HOME/bld/config.toml`). The file is optional, and command line
flags override it.

```toml
align_output = false  # default; pad task labels into a column (--align-output / --no-align-output)
```

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

A task's command sees only three groups of variables: a base allowlist,
whatever `env` names, and whatever `pass_through_env` names. Both accept a
trailing `*` wildcard. Everything else is dropped, so a task cannot quietly
depend on ambient state. bld also sets `BLD=1` and `BLD_TASK=<package#task>`.

When bld colors its own output, it sets `FORCE_COLOR=1` and `CLICOLOR_FORCE=1`
so tools that see a pipe instead of a terminal still color theirs. A value
already in your environment wins, and `NO_COLOR` turns this off. When bld does
not color (`--color never`, or output piped), escape sequences in task output
are stripped, cached logs included.

The base allowlist describes the machine a build runs on, not what is being
built, so none of it is hashed and widening it invalidates no cache entry:

| | |
|---|---|
| Identity, locale | `HOME` `USER` `LOGNAME` `SHELL` `TZ` `LANG` `LC_*` |
| Locations | `PATH` `TMPDIR` `TMP` `TEMP` `XDG_*` |
| Dynamic linking | `LD_LIBRARY_PATH` `LD_PRELOAD` `DYLD_FALLBACK_LIBRARY_PATH` `DYLD_INSERT_LIBRARIES` `LIBPATH` |
| Nix | `NIX_*` `__NIXOS_*` |
| Terminal | `TERM` `TERM_PROGRAM` `COLORTERM` `NO_COLOR` `FORCE_COLOR` `CLICOLOR_FORCE` |
| Desktop session | `DISPLAY` `WAYLAND_DISPLAY` `XAUTHORITY` `DBUS_SESSION_BUS_ADDRESS` |
| Container daemons | `DOCKER_*` `BUILDKIT_*` `BUILDX_*` `COMPOSE_*` |
| Package managers | `COREPACK_*` `PNPM_HOME` `NPM_CONFIG_PREFIX` `NPM_CONFIG_STORE_DIR` `NPM_CONFIG_CACHE` `NODE_OPTIONS` |

Credentials are deliberately not on it, `SSH_AUTH_SOCK` included. A task that
needs a token or the agent names it in `pass_through_env`.

## Toolchain discovery

A command runs under a plain shell, not a package manager, so `tsc` would not
normally be on PATH the way it is under `npm run`. bld looks for directories a
toolchain keeps its executables in and prepends them, searching from the
package up to the workspace root, nearest first — npm's own rule, so a command
that works under `npm run` works here:

```toml
[tasks.check]
command = "tsc -b"          # not "pnpm run check"
```

Today one thing is looked for, `node_modules/.bin`. Adding another is a line
in `PROBES` in `src/toolchain.rs`; the lookup, the ordering and the PATH
splicing are shared.

None of it is hashed. Which tools happen to be installed describes the machine
a task runs on, exactly as PATH itself does — pin your toolchain by putting the
lockfile in the root `inputs`. `bld hash <task> --files` prints what will be on
PATH, marked as not hashed, next to the inputs that are.

## Concurrency

`concurrency` bounds how many tasks one bld runs at a time. Across processes,
bld coordinates with a lock per task, because a second bld is normal rather
than exceptional: `bld watch dev` rebuilding codegen in one terminal while
`bld run check` wants the same codegen in another.

A task takes an exclusive lock on its own name before it reads or writes its
outputs, and holds it until it is done. A second process waits, says who it is
waiting for if the wait lasts longer than a moment, and by the time it gets in
the result is in the cache, so it replays instead of repeating the work:

```
$ bld run build schema                 # in another terminal
schema#build | waiting for another bld (pid 885184)
schema#build | cache hit
0 ran, 1 cached in 1.33s
```

Worth knowing about it:

- **Locks are `flock`s on files under `.bld/locks`.** The kernel releases them
  when the process ends, so a crash, a panic or a `kill -9` leaves nothing
  stale to clean up, on Linux and macOS alike. Gitignore `.bld`.
- **Locks are keyed by task, not by task hash.** Two processes that disagree
  about the inputs still write one output directory, so they take turns even
  when neither can use the other's result.
- **A cache hit takes the lock too**, because restoring outputs writes the same
  directory a running task is writing.
- **A persistent task holds its lock for as long as it runs**, restarts
  included. A second bld asked to start the same one fails with `already
  running in another bld (pid N)` rather than queueing behind a process that
  never exits, or starting a rival dev server on the same port.
- **A task waiting for a lock still counts against `concurrency`.** Under heavy
  contention with a low limit this throttles the rest of the run; nothing
  deadlocks, because a lock is only ever held by a task that is running.

## Things worth knowing

- **A cache hit says `cache hit` and nothing else.** Replaying the log of a
  task that did not run buries the ones that did. `show_cached_logs = true`,
  at the root or on a single task, replays it.
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
- **Watch mode watches what it hashes.** The directories registered with the
  kernel come from the same gitignore-aware walk that decides which files are
  inputs, so an ignored tree costs nothing, and symlinks are not followed —
  a link into a package store or a nix store path does not drag its target in.
  `.git` and `node_modules` are always skipped, and `watch_exclude` skips more.
- **inotify's budget is per user, not per process.** On Linux each watched
  directory costs one watch descriptor out of `fs.inotify.max_user_watches`,
  shared with every other watcher running as you. When bld runs out it reports
  how many directories it wanted and what is already holding the rest.

Exit codes: `0` success, `1` a task failed, `2` a configuration or usage
error, `130` interrupted.

## Development

```
cargo test        # unit and end-to-end tests
cargo clippy --all-targets -- --deny warnings
cargo fmt --all
nix flake check   # everything above, as CI runs it
```
