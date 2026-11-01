# Rexume command line

A command that succeeds prints one JSON object on stdout, when it has a result, and exits 0. A command that fails prints one line, `rexume: <reason>`, on stderr and exits 1. A usage error prints clap's message on stderr and exits 2. `task wait` exits 3 when it returns because the worker is blocked and 124 when its timeout passes, with the task still on stdout. `report` exits 0 except on a usage error. The stderr text is written for people and may change between versions; scripts and skills branch on the exit code and the JSON, not on it.

A command's JSON object carries `version` at its top level, which changes only when a field changes meaning or goes away. Times are Unix milliseconds. `rexume COMMAND --help` lists each command's options.

`daemon`, `status`, `report`, and `task` take `--state-dir DIR`, the directory holding `state.db`; it defaults to `$HOME/.local/state/rexume`. The Pi extension always uses the default.

## install

```sh
rexume install [--rex PATH] [--server ADDRESS]
```

Installs the hooks of Claude Code and Codex, the Pi extension, and the job that runs the daemon, a LaunchAgent on macOS and a systemd user service on Linux, as the README describes. `--rex` defaults to the first `rex` on `PATH` and fails when there is none; `--server` defaults to `REX_SERVER`, then `unix://$HOME/Library/Application%20Support/rex/server.sock` on macOS or `unix://$XDG_RUNTIME_DIR/rex/server.sock` on Linux, where install fails without `XDG_RUNTIME_DIR`. The job runs this executable by the path install was started with (`rexume` found on `PATH` is used as that path, not resolved further), so a package manager that replaces the file keeps the job working. `install` and `daemon` still accept `--theme-foreground` and `--theme-background`, which a job an earlier install wrote passes, and ignore them with a warning; install writes the job without them. The job runs with install's own `PATH`, through which the daemon finds `codex` to send notices; run `install` again if `codex` moves.

```json
{ "version": 1, "claude_hook": "created", "claude_hook_note": null, "codex_hook": "unchanged", "codex_hook_note": null,
  "pi_extension": "skipped", "pi_extension_note": "`~/.pi/agent` does not exist", "launch_agent": "created",
  "launch_agent_note": null, "launch_agent_label": "io.github.entwining.rexume" }
```

| Field | Values |
| --- | --- |
| `claude_hook`, `codex_hook` | `created` (a hook was added to at least one of its events), `unchanged`, `skipped` (no `~/.claude` or `~/.codex`) |
| `pi_extension` | `created`, `updated`, `unchanged`, `skipped` (no `~/.pi/agent`) |
| `launch_agent` | `created`, `updated`, `unchanged` |

On Linux, `systemd_unit`, `systemd_unit_note`, and `systemd_unit_name` (`rexume.service`) take the place of `launch_agent`, `launch_agent_note`, and `launch_agent_label`, with the same values.

Each field has a `_note` beside it, which says why when the value needs a reason and is `null` otherwise.

### How install recognizes its entries

- **Hooks.** Rexume's Claude Code hook runs on `SessionStart`, `UserPromptSubmit`, `Stop`, and `StopFailure` in `~/.claude/settings.json`, and its Codex hook on `SessionStart` in `~/.codex/hooks.json`. A hook in any group of one of those events is Rexume's when its `command`, split as a shell would, is exactly three words: a path whose file name is `rexume`, `report`, and the runtime (`claude` or `codex`). Any path to `rexume` counts, so a hook written by a configuration manager as `/opt/homebrew/bin/rexume report claude` is kept as it is. For each of those events without one, install appends a new group, `{"hooks": [{"type": "command", "command": "rexume report claude", "timeout": 5}]}` for Claude Code and `{"hooks": [{"command": "rexume report codex", "timeout": 5, "type": "command"}]}` for Codex, and leaves every other key, group, and hook in place and in order.
- **Codex trust.** Codex keys its approval of a hook by the hook's position in `hooks.json`. Install appends and never moves existing groups, so hooks you already trusted stay trusted, and only the appended entry waits for approval in `/hooks`.
- **Pi extension.** `~/.pi/agent/extensions/rexume.ts` is rewritten whenever its bytes differ from the extension this version ships. A file install wrote starts with `` // Written by `rexume install` ``, in every version; the daemon replaces only such a file, and uninstall removes only such a file.
- **LaunchAgent.** The job is the label `io.github.entwining.rexume` in `gui/$UID`, defined by `~/Library/LaunchAgents/io.github.entwining.rexume.plist`, which runs `rexume daemon` and logs to `~/Library/Logs/rexume.log`. Install compares the plist it would write with the one on disk by their parsed content, and reloads the job only when the plist differs or the job is not loaded; otherwise launchd is not touched. It writes the new plist only once the old job's daemon has exited, so a reload that fails leaves the plist differing or the job unloaded, and the next install finishes it.
- **systemd user service.** The job is `rexume.service` in the user's systemd manager, defined by `rexume.service` in `$XDG_CONFIG_HOME/systemd/user` (`~/.config` when the variable is unset or not absolute), which runs `rexume daemon`; its log goes to the journal, read with `journalctl --user -u rexume`. Install compares the unit it would write with the one on disk byte for byte, and touches nothing when they match and the service is running, enabled, and loaded from that file. Otherwise it stops the service, writes the unit when it differs, and enables and starts the service again, so a step that fails leaves something the next install finishes. The service restarts 2 seconds after any exit and gives up after 5 starts within 30 seconds, as when its executable is gone; the next install starts it again. Install fails when it cannot reach the user manager, after it has written the hooks and the Pi extension; uninstall still removes them, and `rexume daemon` can run under another service manager instead.

The Pi extension and the job's plist or unit hold nothing of the user's, so they are replaced whole and no backup is kept; the plist or unit is written from install's flags and its `PATH`. Files are replaced through a rename that keeps their permissions; a symbolic link is followed and its target replaced.

## uninstall

```sh
rexume uninstall
```

Boots the LaunchAgent out, or disables and stops the systemd service, and waits for the daemon to exit, so that it cannot write the Pi extension back, then removes Rexume's hooks and any group they leave empty from each of their events, the Pi extension when install wrote it, and the plist or unit. The state directory and the executable stay.

```json
{ "version": 1, "claude_hook": "removed", "claude_hook_note": null, "codex_hook": "absent", "codex_hook_note": null,
  "pi_extension": "kept", "pi_extension_note": "not written by install", "launch_agent": "removed", "launch_agent_note": null }
```

Each field is `removed` or `absent`; `pi_extension` can also be `kept`. On Linux, `systemd_unit` and `systemd_unit_note` take the place of `launch_agent` and `launch_agent_note`.

## daemon

```sh
rexume daemon [--rex PATH] [--server ADDRESS]
```

The process the job runs. It runs until stopped, logs to stderr (set `RUST_LOG=rexume=debug` for every call it makes through its adapter), and exits 1 at once when another daemon holds the same state directory. It starts `rex do` with its adapter script as a child process and reconnects with a growing delay, from 1 to 30 seconds, when the server goes away; a new server socket wakes it early. On `SIGTERM` or `SIGINT` it ends `rex do` and exits 0; it also exits 0 when its executable is replaced, so that its job starts the new one. When it starts, it replaces the Pi extension install wrote, as install does, when that differs from this version's; it never creates a missing one, leaves a file install did not write as it is, and logs a failure without stopping. Likewise, when Rexume's `SessionStart` hook is in `~/.claude/settings.json`, it appends a group to each Claude Code turn event that lacks Rexume's hook, running the same command as that hook with a `timeout` of 5, so an install from before the turn hooks gets them; settings without that hook are left as they are.

## status

```sh
rexume status
```

What to check after a restart, read without changing anything: restored panes whose conversation has not reopened, notices not yet confirmed as delivered, and the error that ended the daemon's latest epoch.

```json
{ "version": 2, "restores": [{ "id": "…", "session": "work", "pane": "block:…", "state": "skipped", "note": "…", "created_at": 1792855785738 }],
  "undelivered_notices": [NOTICE],
  "epoch_error": { "error": "…", "started_at": 1792855785738, "ended_at": 1792855785790 } }
```

Each restore is one pane the server restored where Rexume set out to reopen a conversation, listed unless its gate claimed it, so a restore that reopened nothing, `skipped` or `failed`, stays listed. `session` is the session's label, `pane` the pane's ID when Rexume first saw it, and `note` why the restore waits or ended. `state` is `planned` (waiting for the pane to show its shell with nothing typed), `typed` (the line was typed and its gate has not started yet), `unknown` (the gate did not start within 5 seconds of the line; a gate that starts later still claims it, and nothing is typed again), `skipped` (the pane closed, the conversation was opened in another pane, or the pane did not show an empty shell prompt within 30 seconds), or `failed` (the server refused the line). [docs/design.md](design.md#restore) explains them. `epoch_error` is `null` unless the daemon's latest observation of the server ended on an error rather than with its connection or with the daemon's own stop or replacement; the daemon then connects again, and records nothing until it observes the server without one.

## report

```sh
rexume report claude|codex|pi
```

Run by the hooks with the hook's JSON on stdin, which it reads whole before anything else, so the agent writing it never meets a closed pipe. Outside a Rex pane (no `REX_BLOCK`) it then does nothing. It records the conversation as the pane's occupant only when its parent agent process is the pane's foreground program, so a run the agent starts in another process does not replace the pane's conversation. Sessions the agent runs inside its own process pass that check too, so the runtime's marks decide, as the table says for each runtime: a start marked as such a session is ignored, and a start the runtime leaves unmarked is ignored while its process already holds the pane and otherwise recorded as a conversation that cannot be reopened. It prints nothing, and any failure goes to stderr with exit status 0, so a hook never holds up or fails the session it reports.

For Claude Code's `UserPromptSubmit`, `Stop`, and `StopFailure` (`hook_event_name`), `report` instead appends one line to the conversation's turn record, `claude/SESSION_ID.jsonl` in the state directory, and touches neither the database nor the server: a prompt with its `prompt_id` and, when `prompt` starts with a notice's or request's token (`[rexume ID]`), that token and no other text of it; a stop with its `prompt_id`, a new ID, and `last_assistant_message` as the answer; a failure with its `prompt_id` and `last_assistant_message`, or else `error`, as its text. An event with `agent_id` is a subagent's and is ignored. [design.md](design.md#what-is-saved) explains how the turns are read.

| Runtime | Fields read from stdin |
| --- | --- |
| `claude` | `session_id`, `transcript_path`, `model`, `effort`; a start with `agent_id` is a subagent's or an in-process teammate's and is ignored |
| `codex` | `session_id`, `transcript_path`, `model`, `effort`; a start without `transcript_path` is unmarked: Codex keeps no record of its guardian reviews and side conversations, nor of an ephemeral `codex exec` run |
| `pi` | `session_id`, `session_file`, `session_file_size` (the file's length when the session started; without it the report measures the file itself), `mode`, `provider`, `model`, `thinking`; a start outside `tui` mode is unmarked, which covers SDK sessions an extension creates and `pi -p`, `--mode json`, and `--mode rpc` runs, and only a `tui` session with a file can be reopened |

For Claude Code, `report` also reads what the agent process was started with: its arguments, and its environment as the kernel keeps it from the start. The hook's own environment is not used: it also holds the `env` of Claude Code's settings, which Claude Code applies again when it resumes. From the arguments it keeps `--remote-control` or `--rc` with an optional name, `--add-dir`, `--mcp-config`, `--settings`, `--plugin-dir`, `--setting-sources`, `--allowedTools` or `--allowed-tools`, `--disallowedTools` or `--disallowed-tools`, `--tools`, `--strict-mcp-config`, `--chrome`, and `--no-chrome`, with their values and in their order; inline JSON given to `--settings` or `--mcp-config` is dropped, and with it a flag left without a path. The launch prompt and the flags that name the conversation or set its model, effort, or permission mode are not kept. From the environment it keeps only the variables that choose the provider, where it runs, and the models its aliases name: `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_MANTLE`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`, `CLAUDE_CODE_USE_ANTHROPIC_AWS`, `AWS_REGION`, `AWS_DEFAULT_REGION`, `AWS_PROFILE`, `ANTHROPIC_BEDROCK_REGION_PREFIX`, `ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION`, `CLOUD_ML_REGION`, `ANTHROPIC_VERTEX_PROJECT_ID`, `VERTEX_REGION_CLAUDE_*`, `ANTHROPIC_FOUNDRY_RESOURCE`, `ANTHROPIC_AWS_WORKSPACE_ID`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, `ANTHROPIC_DEFAULT_FABLE_MODEL`, and `ANTHROPIC_SMALL_FAST_MODEL`. Endpoint URLs, authentication settings, and credentials are left out: a URL can carry a credential, and skipping a provider's authentication works only with a gateway's URL. Each report replaces the saved flags and variables, or removes them when there are none; a report that cannot read the process keeps them. Model and effort merge into the recorded settings, so a resumed start that omits them keeps them.

A Claude Code conversation also records `CLAUDE_CODE_MESSAGING_SOCKET`, through which notices reach it.

## task

The task commands run from the orchestrator's own Rex pane: they find the calling conversation through `REX_BLOCK` and the conversation last recorded there, and refuse a task another conversation owns.

| Command | Does | Prints |
| --- | --- | --- |
| `task add --worker BLOCK --title TEXT [--result-file PATH] [--keep-pane]` | Registers the worker running in pane `BLOCK`. Without `--result-file`, the worker is an interactive conversation and the first turn it finishes is the result; with it, the worker is a non-interactive run and the absolute `PATH` is the result once the run exits 0, as the paragraph after this table describes. `--keep-pane` keeps the pane after the task is released. Use it for a worker just started; prompt it again only through `task send`. Fails for a non-interactive worker whose program has already exited, while Rexume is closing the pane, and once the daemon observes a later server instance than the one `BLOCK` is on. | `TASK` |
| `task next ID` | Starts the next round with the same interactive worker: its result is the earliest finished turn no round has taken, including one that finished before this command. Use it to follow a worker that ends turns on its own, such as interim reports while it waits for CI. Acknowledges the previous round. Fails while the round is unfinished, once the task is released, while Rexume is closing the pane, and on a closed pane unless turns are unread. | `TASK` of the new round |
| `task send ID (--message TEXT \| --message-file PATH)` | Starts the next round with a new request: Rexume delivers the text to the worker's conversation once it is idle, and the round's result is the turn that takes it in. Acknowledges the previous round. Fails while the round is unfinished, once the task is released, for a non-interactive task, and when the pane is closed or Rexume is closing it. | `TASK` of the new round |
| `task list` | This conversation's tasks, latest round each. | `{"version", "tasks": [TASK], "undelivered_notices": [NOTICE]}` |
| `task show ID [--round N]` | One task with a round's result, the latest by default. | `TASK` |
| `task wait ID [--timeout SECONDS]` | Waits up to `SECONDS` (default 600) for the latest round to finish, in any state, or its worker to block, and shows that round, so a round that `failed` also exits 0. While no other wait that is still running holds the round, the wait holds it: the round's notices are left to it, and it returns once it takes one still unsent; a blocked episode whose notice was sent or taken by an earlier wait is not taken again, so a second wait on it waits for what comes next. While another running wait holds the round, it returns when the round finishes or an episode begins that it had not seen when it came to wait behind one; once that wait returns or exits, it holds the round itself, and an episode that wait took no longer ends it. Two waits running at once may both report one episode. Exits 3 when it returns on a blocked episode and 124 when the timeout passes first, as decided when it returned; the `TASK` is read afterwards and may have moved on. | `TASK` with `timed_out` |
| `task ack ID [--round N]` | Records that the owner has a finished round's result; a notice not yet sent for it is dropped. Fails while the round is `running` or `blocked`. | `TASK` |
| `task abandon ID` | Stops waiting for the latest round, which ends as `abandoned` and is acknowledged at once, so no notice is sent for it; a running `task wait` returns with it and exits 0. The worker is left as it is and the task is not released: a turn the worker finishes later is unread, which `task next` takes and `task release` waits for, and a request that already left is not withdrawn. Fails once the round has finished. | `TASK` |
| `task release ID` | Ends the task and acknowledges its latest round; the daemon then closes a disposable worker's pane if it is unused, for a non-interactive task too. Fails while a round is unfinished or the worker's record shows unread turns. | `TASK` |

An interactive task's first round takes the first turn the worker's process finished, also one that finished before `add`, and none of the turns a resumed conversation's record already held; a process that answered other prompts first hands back the first of those. A non-interactive run's result file must hold UTF-8 text, at most 1 MiB and not empty; a run that exits 0 with a file that is missing, unreadable, empty, or larger fails its round, with the reason in `note`.

A message is the text itself or a file's contents, at most 256 KiB, read when the command runs.

`TASK` has `id`, `title`, `round`, `state`, `mode` (`interactive` or `non_interactive`), `keep_pane`, `worker`, `result_ref`, `note`, `finished_at`, `acked_at`, `released_at`, `unread`, `request`, `pane`, and, except in `task list`, `result`.

- `worker` is the pane's current Rex block ID, or `null` once it is not on the server.
- `result_ref` is, for a completed round, the record ID of an interactive worker's answer (for Claude Code, the ID of the stop in its turn record) or a non-interactive run's result file.
- `note` says why the round ended without a result.
- `unread` is the number of finished turns in an interactive worker's record, from where the task started reading it, that no round has taken, which `task next` takes in order. It is `null` for a non-interactive or released task, before the worker's conversation is known, and when its record cannot be read or was replaced.
- `request` is `null` except for a request round, where it is `{id, state, note}`. `state` is `pending`, `sending`, `sent`, `accepted`, `unknown`, `delivered`, or `superseded`, with a NOTICE's meanings: `delivered` once the worker's own record shows the request received, which for Claude Code is once the turn that took it in ends, and `superseded` when the round ended before the request was sent, which is then never sent. A request whose round ended after it was sent keeps the state it had then. `note` says why a request still waits or how its send went.
- `pane` is `{state, closed_at, note}`. `state` is `open` while the pane exists and Rexume has neither closed nor given it up (a `--keep-pane` pane stays `open`), `closing` during a close attempt after the task was released, `closed` once the pane is gone, or `kept` when it was used after the result, or restored by the server and not left showing its shell, and is left to whoever uses it. `note` says why a pane was kept or a close put off.

A round's `state` is `running` or `blocked` (the task's conversation has reported from the worker's pane on the current server, for a second without a break, that it is waiting on a dialog or a question; each such episode has one blocked notice) until it finishes as `completed` (its result is saved), `cancelled` (the worker's turn was cancelled; for Claude Code, also a prompt another hook rejected, known once the worker takes other input), `failed`, `terminated` (a non-interactive run ended by a signal, or cut off when the Rex server stopped; or an interactive round in a worker pane whose conversation is not reopened after a restart, there or in another pane), `worker_closed`, or `abandoned` (the owner stopped waiting with `task abandon`).

`NOTICE` has `id`, `kind` (`finished` when the round finished, `blocked` when it began a blocked episode), `task_id`, `round`, `round_state`, `state`, and `note`. A notice is listed while it may not have reached the owner yet: `sending`, `sent` (handed to Claude Code's socket, or written to Pi's inbox), `accepted` (Codex queued it, or the Pi extension took it), or `unknown` (the send may or may not have happened; Rexume never sends it again). It leaves the list only once the owner's own conversation record shows it received (`delivered`), for Claude Code once the turn that took it in ends. A `pending` notice is listed only when a send failed before anything left: its `note` holds the error, and the daemon tries again on its next pass. One that `task wait` took first, whose round was acknowledged first, or, for a blocked notice, whose episode ended first, is `superseded` and never sent; its `note` says which. Each notice has one claimant, `task wait` or the daemon, and is never sent again once it may have been sent; that is not a receipt, and two waits running at once may both report the same episode.

A notice's text starts with `[rexume NOTICE_ID]` and names the task, its title, and the round. A finished notice names the `task show` and `task ack` commands for that round and, when the worker has unread turns, how many. A blocked notice names the worker's current block ID while the pane is on the server, says the worker is waiting on a dialog or a question that may have been answered since, names the `task show` command for that round, and says when the round's request is still pending; it needs no `task ack`, since the round has not finished. A request's text starts with `[rexume REQUEST_ID]`, followed by the message.

## Environment

| Variable | Read by | Use |
| --- | --- | --- |
| `HOME` | every command | the default state directory; install's and uninstall's targets, and the Pi extension the daemon replaces |
| `PATH` | `install`, `daemon`, `report`, `task` | finding `rex` (and, for `install`, `rexume`); install writes its own into the job, where the daemon finds `codex` through it |
| `REX_SERVER` | `install`, `daemon`, `report`, `task` | the server address when `--server` is not given; `report` and `task` use it to reach the server |
| `XDG_RUNTIME_DIR` | `install`, `daemon` (Linux) | the default server address when neither `--server` nor `REX_SERVER` is given |
| `XDG_CONFIG_HOME` | `install`, `uninstall` (Linux) | where the systemd unit goes, when absolute |
| `REX_BLOCK` | `report`, `task`, `gate` | the calling pane |
| `REX_SESSION` | `report`, `gate` | the calling pane's session, which `gate` leaves out of a reopened conversation that ran without it |
| `CLAUDE_CODE_MESSAGING_SOCKET` | `report claude` | where notices to that conversation go |
| `SHELL` | `gate` | the interactive shell that runs a reopened conversation |
| `RUST_LOG` | every command | log filter; `daemon` logs at `info` by default, the others at `warn` |
