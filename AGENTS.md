# rexume

rexume restores Rex sessions after the Rex server stops, including the Claude Code, Codex, or Pi conversation each pane ran, and carries task results from worker panes back to the orchestrator conversation that registered them. One launchd job, `rexume daemon`, observes the server; hooks, restored panes, and task commands are short processes writing the same SQLite state. [docs/design.md](docs/design.md) explains the states; read it before changing restore, task, notice, or cleanup code.

## Goals

The architecture can change; these goals do not.

- **A pane the user closed stays closed; everything else comes back** after a Rex crash, a server restart, or a normal stop. The only accepted loss is a change the machine had no time to save.
- **Continue only what the server's end cut off, exactly once.** A finished, cancelled, or waiting conversation reopens without a new turn, and no conversation runs in two panes. rexume decides this from the runtimes' own records, not from each agent's resume setting.
- **An unknown side effect is never repeated.** A create, layer, or notice that may have happened is reconciled from claims or the owner's own record, or left visibly `unknown` or `partial`; it is not retried to make a state look complete.
- **A worker's result reaches its owner, and only an unused disposable worker's pane is closed.** An acknowledged worker and a pane someone took over are different states and stay distinguishable in the schema.
- **Idle costs nothing.** No timer wakeups, no process per event, a rename on disk well within a second, and a restore of 20 sessions within about a second, as `tests/it/bench.rs` measures. Memory slightly above a pure-Lua observer is accepted; measure and report it.
- **Install once, then forget it.** `rexume install` is incremental and idempotent and owns every hook and launchd resource rexume needs.

## Where things are owned

| File | Owns |
| --- | --- |
| `README.md` | What rexume is for, installing the program and its hooks, checking a restore, uninstall, and known limits |
| `docs/cli.md` | Commands, JSON outputs, exit codes, and how `install` recognizes its entries: the contract hooks, skills, and configuration managers rely on |
| `docs/design.md` | Processes, saved state, restore and task states, notices, cleanup, limits, and measurements |
| `src/store/schema.sql` | The schema; table comments are the only place a column's meaning lives |
| This file | Goals, decisions that are easy to undo by accident, workflow, and environment traps |

Change the owner, not a copy. A change to a command, output field, hook identity, or the plist changes `docs/cli.md` in the same commit.

## Decisions to keep

Each was argued and measured; reopen one only with new evidence.

- **Unsafe code only in `src/proc.rs`.** The org's other Rust repositories forbid unsafe code; rexume denies it crate-wide and allows it in that one module for `libc::proc_pidinfo`, which reads a process's parent and name. Forking `ps` per report was rejected, and the `libproc` crate needs bindgen with a hard-coded Command Line Tools SDK path in its build script, which makes CI and bottle builds depend on that path. Every unsafe block carries a `SAFETY` comment (`undocumented_unsafe_blocks` is denied).
- **The product never polls or sleeps to wait.** The daemon's kqueue loop wakes on its socket, the database's write-ahead log, followed files and process exits, `SIGTERM` and `SIGINT`, and deadlines it computes; `task wait` waits on the write-ahead log. Tests may poll.
- **The adapter starts its relay with `detach = true`.** Rex stops a script's non-detached commands when the script's top level finishes.
- **Signals through kqueue.** The daemon blocks `SIGTERM` and `SIGINT` and reads them as `EVFILT_SIGNAL` events, then ends `rex do` and exits 0; no signal handler and no extra dependency. launchd ends the whole process group anyway, but a daemon run by hand or by a test would otherwise leave `rex do` running until the server stops. An orphaned relay exits on its socket's end.
- **One writer per resource, short transactions, no I/O inside one.** Durable state is committed before the side effect it guards (`sent` before `session.create`, `layer_sent_at` before `session.new_layer`, `sending` before a notice). A successful close forgets the pane and removes it from the saved layout in one transaction, so a restore never depends on event order.
- **Schema.** One `schema.sql` with `user_version = 1` and its copy in `tests/fixtures/schema-v1.sql`, which `tests/it/schema.rs` compares; version 1 has no migrations. `STRICT` tables, `CHECK` on every state column, Unix-millisecond integers, and UUIDv7 IDs. A state the code adds is added to the `CHECK`, never by loosening it. A later schema change bumps `user_version`, adds a fixture for it, and upgrades each released version; `Store::open` refuses a database newer than the binary. A new column needs a reader or a requirement.
- **Store access.** A function that writes takes `&Transaction`, one that only reads takes `&Connection`. Values are always bound, never formatted into SQL or into a dynamic `IN` list. A partial index's predicate lives in one constant (`task::OPEN` for `task_round_open`) that every query relying on the index uses, because SQLite uses a partial index only for a predicate it can prove matches; `every_pass_statement_uses_an_index` in `src/task/tests.rs` runs `EXPLAIN QUERY PLAN` on the statements every daemon pass runs and fails on a `SCAN` of any table but `task` and `notice`, or when a statement on open rounds stops using `task_round_open`; add a new per-pass statement to it.
- **`enum Runtime` is the extension point** for another agent runtime: its record parser, resume command, and notice transport.
- **`install` appends hooks and never rewrites existing groups**, because Codex keys hook approval by position; see `docs/cli.md`.
- **No `GOGC` for `rex do`.** Setting 50 or 25 changed total resident memory by under 1 MiB.
- **The release profile is Cargo's default**, as in the org's other repositories.

## Develop and validate

- The repository is public. Never copy real session content, conversation IDs, paths, host names, or user names into code, tests, docs, commit messages, or issues; fixtures are synthetic.
- `make check` runs what CI runs: `cargo deny --locked check` (needs network), `cargo fmt --check`, Clippy with warnings denied, and `cargo test --locked`. A workflow change also needs `actionlint`.
- Tests of pure functions and in-memory data stay in their module; tests that need a `Store` live in the owning module's `tests.rs` with a temporary database; only tests that need the built binary, launchd, or a Rex server go to `tests/it`.
- Functions stay within Clippy's `too_many_lines` (100 lines, tests included); a test that is one continuous scenario may carry `#[expect(clippy::too_many_lines, reason = "...")]` after its fixtures and observations are extracted. Rust files stay within 300 lines not counting blank and `//` lines, which `tests/it/structure.rs` enforces. Over the limit, move the test block to `foo/tests.rs` first, then split along an owner seam; never delete comments or squeeze blank lines to get under it.
- Integration tests are one `tests/it` binary with a module per behavior and helpers in `common.rs`. Open the state database there with `common::db` so a test's own writes meet the same foreign keys, and read it with `common::query`, which fails on a broken query instead of returning `None`. The scenarios that need a real Rex server are `#[ignore]`d: run them with `cargo test --test it -- --ignored`. They use the Rex CLI the app installs, or `REXUME_TEST_REX`. Each test starts its own server with its own socket, home, state directory, and `REX_SERVER` from a cleared environment, and runs its daemon in its own process group; keep new scenarios on that harness so nothing reaches the user's server or `~/.local/state/rexume`.
- Never point a development build at the user's server or state directory: pass `--server` with a test endpoint and `--state-dir` with a scratch path.
- A change to the event loop, restore planning, or task transitions reruns the `--ignored` scenarios. A change that could affect speed or memory reruns `cargo test --release --test it bench -- --ignored --nocapture` on an otherwise idle machine and compares with `docs/design.md`; update the figures there when they change.
- A regression test for a bug must fail without the fix; check that once by reverting the fix.
- Commits follow Conventional Commits.
- Releases are tag-driven. Bump `version` in `Cargo.toml`, add `docs/releases/VERSION.md` (the publish workflow refuses a tag without it), and push a `vVERSION` tag; the workflow reruns the checks, creates the GitHub Release, and asks `Entwining/homebrew-tap` to update its formula. The formula runs `make build`, so packaging changes belong there.

## Environment traps

- Agent sandboxes block what the integration tests need: signalling and listing processes (`kill`, `ps`, `pgrep`) and Unix sockets outside the allowlist. Run the `--ignored` scenarios and the bench from an ordinary shell.
- Kill a test daemon by its process group (`kill -KILL -- -PGID`), or its `rex do` and relay outlive it.
- macOS has no `timeout`; use `perl -e 'alarm shift; exec @ARGV' SECONDS COMMAND`.

## Dependency updates

Before changing `.github/renovate.json`, or deciding how a dependency's updates are grouped, delayed, or merged, read the shared Renovate policy in full: `gh repo read-file skills/renovate/SKILL.md -R Entwining/renovate-config`.
