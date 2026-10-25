# rexume command line

A command that succeeds prints one JSON object on stdout, when it has a result, and exits 0. A command that fails prints one line, `rexume: <reason>`, on stderr and exits 1. A usage error prints clap's message on stderr and exits 2. `task wait` exits 124 when its timeout passes, with the task still on stdout. `report` exits 0 except on a usage error. The stderr text is written for people and may change between versions; scripts and skills branch on the exit code and the JSON, not on it.

Each JSON object carries `version`, which changes only when a field changes meaning or goes away. Times are Unix milliseconds. `rexume COMMAND --help` lists each command's options.

`daemon`, `status`, `report`, and `task` take `--state-dir DIR`, the directory holding `state.db`; it defaults to `$HOME/.local/state/rexume`. The Pi extension always uses the default.

## install

```sh
rexume install [--rex PATH] [--server ADDRESS] [--theme-foreground COLOR --theme-background COLOR]
```

Installs the session hooks, the Pi extension, and the LaunchAgent, as the README describes. `--rex` defaults to the first `rex` on `PATH` and fails when there is none; `--server` defaults to `REX_SERVER`, then `unix://$HOME/Library/Application%20Support/rex/server.sock`. The LaunchAgent runs this executable by the path install was started with (`rexume` found on `PATH` is used as that path, not resolved further), so a package manager that replaces the file keeps the job working. The theme colors are the ones restored terminals start with, each written as `#rrggbb` with hex digits in either case; any other value is a usage error, here and for `daemon`. The job runs with install's own `PATH`, through which the daemon finds `codex` to send notices; run `install` again if `codex` moves.

```json
{ "version": 1, "claude_hook": "created", "claude_hook_note": null, "codex_hook": "unchanged", "codex_hook_note": null,
  "pi_extension": "skipped", "pi_extension_note": "`~/.pi/agent` does not exist", "launch_agent": "created",
  "launch_agent_note": null, "launch_agent_label": "io.github.entwining.rexume" }
```

| Field | Values |
| --- | --- |
| `claude_hook`, `codex_hook` | `created`, `unchanged`, `skipped` (no `~/.claude` or `~/.codex`) |
| `pi_extension` | `created`, `updated`, `unchanged`, `skipped` (no `~/.pi/agent`) |
| `launch_agent` | `created`, `updated`, `unchanged` |

Each field has a `_note` beside it, which says why when the value needs a reason and is `null` otherwise.

### How install recognizes its entries

- **Hooks.** A hook in any `hooks.SessionStart` group of `~/.claude/settings.json` or `~/.codex/hooks.json` is rexume's when its `command`, split as a shell would, is exactly three words: a path whose file name is `rexume`, `report`, and the runtime (`claude` or `codex`). Any path to `rexume` counts, so a hook written by a configuration manager as `/opt/homebrew/bin/rexume report claude` is kept as it is. When none is present, install appends a new group, `{"hooks": [{"type": "command", "command": "rexume report claude", "timeout": 5}]}` for Claude Code and `{"hooks": [{"command": "rexume report codex", "timeout": 5, "type": "command"}]}` for Codex, and leaves every other key, group, and hook in place and in order.
- **Codex trust.** Codex keys its approval of a hook by the hook's position in `hooks.json`. Install appends and never moves existing groups, so hooks you already trusted stay trusted, and only the appended entry waits for approval in `/hooks`.
- **Pi extension.** `~/.pi/agent/extensions/rexume.ts` is rewritten whenever its bytes differ from the extension this version ships. A file install wrote starts with `` // Written by `rexume install` ``, in every version; the daemon replaces only such a file, and uninstall removes only such a file.
- **LaunchAgent.** The job is the label `io.github.entwining.rexume` in `gui/$UID`, defined by `~/Library/LaunchAgents/io.github.entwining.rexume.plist`, which runs `rexume daemon` and logs to `~/Library/Logs/rexume.log`. Install compares the plist it would write with the one on disk by their parsed content, writes it when they differ, and boots the job out and in again only when the plist changed or the job is not loaded; otherwise launchd is not touched. A reload waits up to 30 seconds for the running daemon to exit after `launchctl bootout`, because launchd keeps the job loaded until then, and writes the new plist only once the old job is gone, so a reload that fails leaves the plist differing or the job unloaded, and the next install finishes it.

The Pi extension and the plist hold nothing of the user's, so they are replaced whole and no backup is kept; the plist is written from install's flags and its `PATH`. Files are replaced through a rename that keeps their permissions; a symbolic link is followed and its target replaced.

## uninstall

```sh
rexume uninstall
```

Boots the job out and waits for the daemon to exit, so that it cannot write the Pi extension back, then removes rexume's hooks and any `SessionStart` group they leave empty, the Pi extension when install wrote it, and the plist. The state directory and the executable stay.

```json
{ "version": 1, "claude_hook": "removed", "claude_hook_note": null, "codex_hook": "absent", "codex_hook_note": null,
  "pi_extension": "kept", "pi_extension_note": "not written by install", "launch_agent": "removed", "launch_agent_note": null }
```

Each field is `removed` or `absent`; `pi_extension` can also be `kept`.

## daemon

```sh
rexume daemon [--rex PATH] [--server ADDRESS] [--theme-foreground COLOR --theme-background COLOR]
```

The process the LaunchAgent runs. It runs until stopped, logs to stderr (set `RUST_LOG=rexume=debug` for every call it makes through its adapter), and exits 1 at once when another daemon holds the same state directory. It starts `rex do` with its adapter script as a child process and reconnects with a growing delay, from 1 to 30 seconds, when the server goes away; a new server socket wakes it early. On `SIGTERM` or `SIGINT` it ends `rex do` and exits 0; it also exits 0 when its executable is replaced, so that launchd starts the new one. When it starts, it replaces the Pi extension install wrote, as install does, when that differs from this version's; it never creates a missing one, leaves a file install did not write as it is, and logs a failure without stopping.

## status

```sh
rexume status
```

What to check after a restart, read without changing anything: restores that did not complete and notices not yet confirmed as delivered.

```json
{ "version": 1, "restores": [{ "id": "…", "session": "work", "state": "partial", "note": "…", "created_at": 1792855785738 }],
  "undelivered_notices": [NOTICE] }
```

`session` is the session's label. [docs/design.md](design.md#restore) explains each restore state.

## report

```sh
rexume report claude|codex|pi
```

Run by the session-start hooks with the hook's JSON on stdin, which it reads whole before anything else, so the agent writing it never meets a closed pipe. Outside a Rex pane (no `REX_BLOCK`) it then does nothing. It records the conversation as the pane's occupant only when its parent agent process is the pane's foreground program, so a run the agent starts in another process does not replace the pane's conversation. Sessions the agent runs inside its own process pass that check too, so the runtime's marks decide, as the table says for each runtime: a start marked as such a session is ignored, and a start the runtime leaves unmarked is ignored while its process already holds the pane and otherwise recorded as a conversation that cannot be reopened. It prints nothing, and any failure goes to stderr with exit status 0, so a hook never holds up or fails the session it reports.

| Runtime | Fields read from stdin |
| --- | --- |
| `claude` | `session_id`, `transcript_path`, `model`, `effort`; a start with `agent_id` is a subagent's or an in-process teammate's and is ignored |
| `codex` | `session_id`, `transcript_path`, `model`, `effort`; a start without `transcript_path` is unmarked: Codex keeps no record of its guardian reviews and side conversations, nor of an ephemeral `codex exec` run |
| `pi` | `session_id`, `session_file`, `mode`, `provider`, `model`, `thinking`; a start outside `tui` mode is unmarked, which covers SDK sessions an extension creates and `pi -p`, `--mode json`, and `--mode rpc` runs, and only a `tui` session with a file can be reopened |

A Claude Code conversation also records `CLAUDE_CODE_MESSAGING_SOCKET`, through which notices reach it.

## task

The task commands run from the orchestrator's own Rex pane: they find the calling conversation through `REX_BLOCK` and the conversation last recorded there, and refuse a task another conversation owns.

| Command | Does | Prints |
| --- | --- | --- |
| `task add --worker BLOCK --title TEXT [--result-file PATH] [--keep-pane]` | Registers the worker running in pane `BLOCK`. Without `--result-file`, the worker is an interactive conversation and its next finished turn is the result; with it, the worker is a non-interactive run and the absolute `PATH` is the result once the run exits 0. `--keep-pane` keeps the pane after its result is acknowledged. Use it for a worker just started; prompt it again only through `task rearm`. | `TASK` |
| `task list` | This conversation's tasks, latest round each. | `{"version", "tasks": [TASK], "undelivered_notices": [NOTICE]}` |
| `task show ID [--round N]` | One task with a round's result, the latest by default. | `TASK` |
| `task wait ID [--timeout SECONDS]` | Waits up to `SECONDS` (default 600) for the latest round to finish, and shows that round. While it waits, the round's notice is left to it. Exits 124 when the timeout passes first. | `TASK` with `timed_out` |
| `task ack ID [--round N]` | Records that the owner has a finished round's result; a notice not yet sent for it is dropped. The daemon then closes a disposable worker's pane if it is unused. Fails while the round is `running` or `blocked`. | `TASK` |
| `task rearm ID` | Starts the next round with the same interactive worker; only a turn it finishes after now counts. Fails while the round is unfinished, or when the pane is closed or rexume is closing it. | `TASK` of the new round |

`TASK` has `id`, `title`, `round`, `state`, `mode` (`interactive` or `non_interactive`), `keep_pane`, `worker` (the pane's current Rex block ID, or `null` once it is not on the server), `result_ref` (for a completed round, the record ID of an interactive worker's answer or a non-interactive run's result file), `note` (why the round ended without a result), `finished_at`, `acked_at`, `pane`, and, except in `task list`, `result`. `pane` is `{state, closed_at, note}`: `state` is `open` while the pane exists and rexume has neither closed nor given it up (a `--keep-pane` pane stays `open`), `closing` during a close attempt, `closed` once the pane is gone, or `kept` when it was used after the result and is left to whoever uses it; `note` says why a pane was kept or a close put off.

A round's `state` is `running` or `blocked` (the worker is waiting for someone) until it finishes as `completed` (its result is saved), `cancelled`, `failed`, `terminated` (a non-interactive run ended by a signal), or `worker_closed`.

`NOTICE` has `id`, `task_id`, `round`, `round_state`, `state`, and `note`. A notice is listed while it may not have reached the owner yet: `sending`, `sent` (handed to Claude Code's socket, or written to Pi's inbox), `accepted` (Codex queued it, or the Pi extension took it), or `unknown` (the send may or may not have happened; rexume never sends it again). It leaves the list only once the owner's own conversation record shows it (`delivered`). A `pending` notice is listed only when a send failed before anything left: its `note` holds the error, and the daemon tries again on its next pass. One that `task wait` or `task ack` took first is `superseded` and never sent.

A notice's text starts with `[rexume NOTICE_ID]` and names the task, its title, the round, and the `task show` and `task ack` commands for that round.

## Environment

| Variable | Read by | Use |
| --- | --- | --- |
| `HOME` | every command | the default state directory; install's and uninstall's targets, and the Pi extension the daemon replaces |
| `PATH` | `install`, `daemon`, `report`, `task` | finding `rex` (and, for `install`, `rexume`); install writes its own into the LaunchAgent, where the daemon finds `codex` through it |
| `REX_SERVER` | `install`, `daemon`, `report`, `task` | the server address when `--server` is not given; `report` and `task` use it to reach the server |
| `REX_BLOCK` | `report`, `task`, `gate` | the calling pane |
| `REX_SESSION` | `report`, `gate` | the calling pane's session |
| `CLAUDE_CODE_MESSAGING_SOCKET` | `report claude` | where notices to that conversation go |
| `SHELL` | `gate` | the shell a restored pane starts |
| `RUST_LOG` | every command | log filter; `daemon` logs at `info` by default, the others at `warn` |
