# rexume

rexume restores Rex sessions after the Rex server stops, including the Claude Code, Codex, or Pi conversation each pane ran, and carries task results from worker panes back to the orchestrator conversation that registered them. One job, `rexume daemon` under launchd or systemd, observes the server; hooks, restored panes, and task commands are short processes writing the same SQLite state. [docs/design.md](docs/design.md) explains the states; read it before changing restore, task, notice, or cleanup code.

## Goals

- **A pane the user closed stays closed; everything else comes back** after a Rex crash, a server restart, or a normal stop. The only accepted loss is a change the machine had no time to save.
- **Continue only what the server's end cut off, exactly once.** A finished, cancelled, or waiting conversation reopens without a new turn, and no conversation runs in two panes. rexume decides this from the runtimes' own records, not from each agent's resume setting.
- **An unknown side effect is never repeated.** A create, layer, or notice that may have happened is reconciled from claims or the owner's own record, or left visibly `unknown` or `partial`; it is not retried to make a state look complete.
- **A worker's result reaches its owner, and only an unused disposable worker's pane is closed.** An acknowledged worker and a pane someone took over are different states and stay distinguishable in the schema.
- **No work while nothing changes.** rexume's own code sets no timers, does not poll, and starts no process per event; with nothing changing, the daemon does not wake. `rex do` is Rex's program, and its runtime's own wakeups are not rexume's to remove.
- **Saved soon, restored soon.** A rename reaches disk within a second, so a crash loses only the latest changes, and 20 sessions are restored within about a second of the new server starting. `tests/it/bench.rs` measures both, and memory, which has no target but is reported with any change that may move it.
- **Install once, then forget it.** `rexume install` owns every hook and launchd or systemd resource rexume needs. Running it again leaves one copy of each entry, nothing left over from an earlier install, and the user's other settings as they were.

## Where things are owned

| File | Owns |
| --- | --- |
| `README.md` | What rexume is for, installing the program and its hooks, checking a restore, uninstall, and known limits |
| `docs/cli.md` | Commands, JSON outputs, exit codes, and how `install` recognizes its entries: the contract hooks, skills, and configuration managers rely on |
| `docs/design.md` | Processes, saved state, restore and task states, notices, cleanup, limits, and how to read the bench |
| `src/store/schema.sql` | The schema; table comments are the only place a column's meaning lives |
| This file | Goals, decisions that are easy to undo by accident, workflow, and environment traps |

Change the owner, not a copy. A change to a command, output field, hook identity, or the plist or systemd unit changes `docs/cli.md` in the same commit.

## Decisions to keep

Each gives its reason; reopen one only with evidence that the reason no longer holds.

- **Unsafe code only in `src/proc.rs`.** rexume denies unsafe code crate-wide, so that what the compiler cannot check stays in one small module, and allows it there, on macOS only, for `libc::proc_pidinfo`, which reads a process's parent and name; Linux reads them from `/proc`. Forking `ps` per report was rejected, and the `libproc` crate needs bindgen with a hard-coded Command Line Tools SDK path in its build script, which makes CI and bottle builds depend on that path. Every unsafe block carries a `SAFETY` comment (`undocumented_unsafe_blocks` is denied).
- **The product never polls or sleeps to wait.** The daemon's event loop wakes on its socket, the database's write-ahead log, followed files and process exits, `SIGTERM` and `SIGINT`, and deadlines it computes; `task wait` waits on the write-ahead log. Tests may poll.
- **Settle after a write-ahead-log wake.** A commit wakes the log's watchers before readers can see it, so after each such wake the daemon calls `Store::settle` and `task wait` reads inside a write transaction, which takes the same lock; without it a woken reader can miss the commit and sleep through it. `docs/design.md` explains the race.
- **The adapter starts its relay with `detach = true`.** Rex stops a script's non-detached commands when the script's top level finishes.
- **Signals through the event loop.** The daemon blocks `SIGTERM` and `SIGINT` and reads them as kqueue `EVFILT_SIGNAL` events or from a signalfd, then ends `rex do` and exits 0; no signal handler. The job ends the whole process group anyway, but a daemon run by hand or by a test would otherwise leave `rex do` running until the server stops. An orphaned relay exits on its socket's end.
- **One writer per resource, short transactions, no I/O inside one.** Durable state is committed before the side effect it guards (`sent` before `session.create`, `layer_sent_at` before `session.new_layer`, `sending` before a notice). A successful close forgets the pane and removes it from the saved layout in one transaction, so a restore never depends on event order.
- **Schema.** One `schema.sql`, which sets the current `user_version`, and a byte copy of each version's schema in `tests/fixtures/schema-vN.sql`, which goes back to the bytes its release shipped once a later version replaces it: `tests/it/schema.rs` compares what the current one creates, not its comments, so a comment change is copied by hand, and the upgrade tests start from the older ones. `STRICT` tables, `CHECK` on every state column, Unix-millisecond integers, and UUIDv7 IDs. A state the code adds is added to the `CHECK`, never by loosening it. A later schema change bumps `user_version`, adds a fixture for it, and upgrades each released version; `Store::open` refuses a database newer than the binary. A new column needs a reader or a requirement.
- **Store access.** A function that writes takes `&Transaction`, one that only reads takes `&Connection`. Values are always bound, never formatted into SQL or into a dynamic `IN` list. A partial index's predicate lives in one constant (`task::OPEN` for `task_round_open`) that every query relying on the index uses, because SQLite uses a partial index only for a predicate it can prove matches; `every_pass_statement_uses_an_index` in `src/task/tests.rs` runs `EXPLAIN QUERY PLAN` on the statements every daemon pass runs and fails on a `SCAN` of any table but `task` and `notice`, or when a statement on open rounds stops using `task_round_open`; add a new per-pass statement to it.
- **`enum Runtime` is the extension point** for another agent runtime: its record parser, resume command, and notice transport.
- **`install` adds to the user's files and replaces its own.** The hooks live in the user's own settings, so install recognizes its hook by the command and appends it only when it is missing, leaving every other entry, and a hook the user edited, as it is; it never rewrites existing groups, because Codex keys hook approval by position. The Pi extension and the plist or systemd unit hold nothing of the user's, so install replaces each whole through a rename when its content differs and keeps no backup; they are customized through install's flags. See `docs/cli.md`.
- **No `GOGC` for `rex do`.** Setting 50 or 25 changed total resident memory by under 1 MiB.

## Develop and validate

- The repository is public. Never copy real session content, conversation IDs, paths, host names, or user names into code, tests, docs, commit messages, or issues; fixtures are synthetic.
- `make check` runs what CI runs: `cargo deny --locked check` (needs network), `cargo fmt --check`, Clippy with warnings denied, and `cargo test --locked`; on Linux, CI runs it with `TARGET` set to each static musl target the release ships, such as `make check TARGET=x86_64-unknown-linux-musl`. A workflow change also needs `actionlint`. Workflows name each action by its version tag, and `.github/workflows/actions.lock` pins it to a commit; run `gh actions-lock` after adding a workflow or changing a `uses` line, because GitHub enforces the lockfile only for the workflows it lists.
- Tests of pure functions and in-memory data stay in their module; tests that need a `Store` live in the owning module's `tests.rs` with a temporary database; only tests that need the built binary, the job's service manager, or a Rex server go to `tests/it`.
- Functions stay within Clippy's `too_many_lines` (100 lines, tests included); a test that is one continuous scenario may carry `#[expect(clippy::too_many_lines, reason = "...")]` after its fixtures and observations are extracted. Rust files stay within 300 lines not counting blank and `//` lines, which `tests/it/structure.rs` enforces. Over the limit, move the test block to `foo/tests.rs` first, then split along an owner seam; never delete comments or squeeze blank lines to get under it.
- Integration tests are one `tests/it` binary with a module per behavior and helpers in `common.rs`. Open the state database there with `common::db` so a test's own writes meet the same foreign keys, and read it with `common::query`, which fails on a broken query instead of returning `None`. The scenarios that need a real Rex server are `#[ignore]`d: run them with `cargo test --test it -- --ignored`. They use `REXUME_TEST_REX`, or else the Rex CLI the app installs on macOS and the first `rex` on `PATH` on Linux. Each test starts its own server with its own socket, home, state directory, and `REX_SERVER` from a cleared environment, and runs its daemon in its own process group; keep new scenarios on that harness so nothing reaches the user's server or `~/.local/state/rexume`.
- Never point a development build at the user's server or state directory: pass `--server` with a test endpoint and `--state-dir` with a scratch path.
- A change to the event loop, restore planning, or task transitions reruns the `--ignored` scenarios, and a change to code under `cfg(target_os = "linux")` reruns them on Linux. A change that could affect speed or memory runs, on macOS, `cargo test --release --test it bench -- --ignored --nocapture` for the change and for its baseline commit, on the same otherwise idle machine, and compares the two reports; `docs/design.md` explains them.
- A regression test for a bug must fail without the fix; check that once by reverting the fix.
- Commits follow Conventional Commits.
- Releases stay at `0.1.x`, breaking changes included, until the maintainer approves another version.
- Releases are tag-driven. Bump `version` in `Cargo.toml`, add `docs/releases/VERSION.md` (the publish workflow refuses a tag without it), and push a `vVERSION` tag; the workflow reruns the checks, creates the GitHub Release with the Linux archives and their provenance attestations, and asks `Entwining/homebrew-tap` to update its formula. The formula runs `make build`, so packaging changes belong there.

## Environment traps

- Agent sandboxes block what the integration tests need: signalling and listing processes (`kill`, `ps`, `pgrep`) and Unix sockets outside the allowlist. Run the `--ignored` scenarios and the bench from an ordinary shell.
- An agent a harness starts on a test Rex server still runs the user's installed hooks, which find `rexume` through a login shell that puts the installed binary before the test's `PATH`, so they report into `~/.local/state/rexume`. Start Claude Code with `--setting-sources project,local` and a settings file whose hook names the test binary by absolute path, and Pi with `-ne` and `-e` naming a copy of the extension whose `rexume` command and inbox are the test's. Codex runs hooks through a login shell, and neither a `SHELL` override nor `-c features.hooks=false` kept its hook out of the user's state, so leave Codex out of such a harness.
- Kill a test daemon by its process group (`kill -KILL -- -PGID`), or its `rex do` and relay outlive it.
- The Pi extension's unit test runs the TypeScript extension in `node`, which must strip types without a flag (Node 22.18 or 23.6 and later).
- macOS has no `timeout`; use `perl -e 'alarm shift; exec @ARGV' SECONDS COMMAND`.

## Dependency updates

Before changing `.github/renovate.json`, or deciding how a dependency's updates are grouped, delayed, or merged, read the shared Renovate policy in full: `gh repo read-file skills/renovate/SKILL.md -R Entwining/renovate-config`.
