# Rexume command line

A command that succeeds prints one JSON object on stdout, when it has a result, and exits 0; `skill show` prints Markdown instead, and `export` JSON Lines. A command that fails prints one line, `rexume: <reason>`, on stderr and exits 1. A usage error prints clap's message on stderr and exits 2. `task wait` exits 3 when it returns because the worker is blocked and 124 when its timeout passes, with the task still on stdout. `report` exits 0 except on a usage error. The stderr text is written for people and may change between versions; scripts and skills branch on the exit code and the JSON, not on it.

A command's JSON object carries `version` at its top level, which changes only when a field changes meaning or goes away. Times are Unix milliseconds. `rexume COMMAND --help` lists each command's options.

`daemon`, `status`, `export`, `report`, and `task` take `--state-dir DIR`, the directory holding `state.db`; it defaults to `$HOME/.local/state/rexume`. The Pi and omp extensions always use the default. A directory there, and each missing parent, is created with mode `0700` by whichever command or extension needs it first. An existing directory keeps its mode, except the state directory itself, which the daemon restricts when it starts.

## install

```sh
rexume install [--rex PATH] [--server ADDRESS]
```

Installs the [runtime hooks, Claude Code plugin, Pi and omp extensions, skill stub, and background job](setup.md#install-the-hooks-and-the-daemon). The job is a LaunchAgent on macOS and a systemd user service on Linux. `--rex` defaults to the first `rex` on `PATH` and fails when there is none; `--server` defaults to `REX_SERVER`, then `unix://$HOME/Library/Application%20Support/rex/server.sock` on macOS or `unix://$XDG_RUNTIME_DIR/rex/server.sock` on Linux, where install fails without `XDG_RUNTIME_DIR`. The job runs this executable by the path install was started with (`rexume` found on `PATH` is used as that path, not resolved further), so a package manager that replaces the file keeps the job working. `install` and `daemon` still accept `--theme-foreground` and `--theme-background`, which a job an earlier install wrote passes, and ignore them with a warning; install writes the job without them. The job runs with install's own `PATH`, through which the daemon finds `codex` to send notices; run `install` again if `codex` moves. Install writes the skill stubs after the job, so a stub it cannot write leaves the job running. Last, install registers the Claude Code plugin through the `claude` it finds on that `PATH`, so a `claude` that fails leaves only the plugin for the next install.

```json
{ "version": 1, "claude_hook": "created", "claude_hook_note": null, "claude_plugin": "created",
  "claude_plugin_note": null, "codex_hook": "unchanged", "codex_hook_note": null,
  "pi_extension": "skipped", "pi_extension_note": "`~/.pi/agent` does not exist", "omp_extension": "created",
  "omp_extension_note": null, "agents_skill": "created", "agents_skill_note": null,
  "claude_skill": "kept", "claude_skill_note": "not written by install", "launch_agent": "created",
  "launch_agent_note": null, "launch_agent_label": "io.github.entwining.rexume" }
```

| Field | Values |
| --- | --- |
| `claude_hook`, `codex_hook` | `created` (a hook was added to at least one of its events), `unchanged`, `skipped` (no `~/.claude` or `~/.codex`) |
| `claude_plugin` | `created` (its files were new, or install registered it with Claude Code), `updated` (its files changed), `unchanged`, `skipped` (no `~/.claude`, or no `claude` on `PATH` to register it with, after which install removes the plugin's directory if it created it); the note is `disabled in Claude Code` when you turned it off there, which install leaves as it is |
| `pi_extension` | `created`, `updated`, `unchanged`, `skipped` (no `~/.pi/agent`) |
| `omp_extension` | `created`, `updated`, `unchanged`, `skipped` (no `~/.omp/agent`) |
| `agents_skill`, `claude_skill` | `created`, `updated`, `unchanged`, `kept` (a `rexume` skill install did not write is there), `skipped` (no `~/.codex` or `~/.pi/agent` for `agents_skill`, no `~/.claude` for `claude_skill`); when `~/.claude/skills` links to `~/.agents/skills`, the one file is written when any of those runtimes is set up, and both give what was done to it |
| `launch_agent` | `created`, `updated`, `unchanged` |

On Linux, `systemd_unit`, `systemd_unit_note`, and `systemd_unit_name` (`rexume.service`) take the place of `launch_agent`, `launch_agent_note`, and `launch_agent_label`, with the same values.

Each field has a `_note` beside it, which says why when the value needs a reason and is `null` otherwise.

### How install recognizes its entries

- **Hooks.** Rexume's Claude Code hook runs on `SessionStart`, `UserPromptSubmit`, `Stop`, and `StopFailure` in `~/.claude/settings.json`, and its Codex hook on `SessionStart` in `~/.codex/hooks.json`. A hook in any group of one of those events is Rexume's when its `command`, split as a shell would, is exactly three words: a path whose file name is `rexume`, `report`, and the runtime (`claude` or `codex`). Any path to `rexume` counts, so a hook written by a configuration manager as `/opt/homebrew/bin/rexume report claude` is kept as it is. For each of those events without one, install appends a new group, `{"hooks": [{"type": "command", "command": "rexume report claude", "timeout": 5}]}` for Claude Code and `{"hooks": [{"command": "rexume report codex", "timeout": 5, "type": "command"}]}` for Codex, and leaves every other key, group, and hook in place and in order.
- **Codex trust.** Codex keys its approval of a hook by the hook's position in `hooks.json`. Install appends and never moves existing groups, so hooks you already trusted stay trusted, and only the appended entry waits for approval in `/hooks`.
- **Claude Code plugin.** Its files are `~/.local/share/rexume/claude-plugin`, one directory that is both the plugin `rexume` and the marketplace `rexume` it is installed from, and are rewritten whenever they differ from the ones this version ships. Install runs `claude plugin marketplace add` with that directory when `~/.claude/settings.json` has no `extraKnownMarketplaces.rexume`, and `claude plugin install rexume@rexume` when it has no `enabledPlugins["rexume@rexume"]`; it never writes either key itself, and a plugin set to `false` there stays off. Claude Code also keeps the plugin in a registry of its own, which only its commands change.
- **Pi and omp extensions.** `~/.pi/agent/extensions/rexume.ts` and `~/.omp/agent/extensions/rexume.ts` are each rewritten whenever their bytes differ from the extension this version ships for that runtime, and each is written only when its runtime's agent directory exists; install never creates `~/.pi/agent` or `~/.omp/agent`. A file install wrote starts with `` // Written by `rexume install` ``, in every version and for both runtimes; the daemon replaces only such a file, and uninstall removes only such a file. omp also reads extensions from a profile's agent directory (`omp --profile NAME`) and from one that `PI_CODING_AGENT_DIR` or `PI_CONFIG_DIR` names; install writes only to the default `~/.omp/agent`.
- **Skill stub.** `~/.agents/skills/rexume/SKILL.md`, which Codex and Pi read, and `~/.claude/skills/rexume/SKILL.md`, which Claude Code reads, are each the skill's `SKILL.md` this version ships, which tells the agent to run `rexume skill show`. A stub install wrote has a line that starts with `` <!-- Written by `rexume install` ``, in every version; install and the daemon replace only such a file, leaving any other `rexume` skill as it is, and uninstall removes only such a file, with its `rexume` directory once that is empty and not a link.
- **LaunchAgent.** The job is the label `io.github.entwining.rexume` in `gui/$UID`, defined by `~/Library/LaunchAgents/io.github.entwining.rexume.plist`, which runs `rexume daemon` and logs to `~/Library/Logs/rexume.log`. Install compares the plist it would write with the one on disk by their parsed content, and reloads the job only when the plist differs or the job is not loaded; otherwise launchd is not touched. It writes the new plist only once the old job's daemon has exited, so a reload that fails leaves the plist differing or the job unloaded, and the next install finishes it.
- **systemd user service.** The job is `rexume.service` in the user's systemd manager, defined by `rexume.service` in `$XDG_CONFIG_HOME/systemd/user` (`~/.config` when the variable is unset or not absolute), which runs `rexume daemon`; its log goes to the journal, read with `journalctl --user -u rexume`. Install compares the unit it would write with the one on disk byte for byte, and touches nothing when they match and the service is running, enabled, and loaded from that file. Otherwise it stops the service, writes the unit when it differs, and enables and starts the service again, so a step that fails leaves something the next install finishes. The service restarts 2 seconds after any exit and gives up after 5 starts within 30 seconds, as when its executable is gone; the next install starts it again. Install fails when it cannot reach the user manager, after it has written the hooks and the extensions; uninstall still removes them, and `rexume daemon` can run under another service manager instead.

The plugin's files, the extensions, the skill stub, and the job's plist or unit hold nothing of the user's, so they are replaced whole and no backup is kept; the plist or unit is written from install's flags and its `PATH`. Files are replaced through a rename that keeps their permissions; a symbolic link is followed and its target written, even one that does not exist yet, while uninstall removes the link itself and leaves its target.

## uninstall

```sh
rexume uninstall
```

Boots the LaunchAgent out, or disables and stops the systemd service, and waits for the daemon to exit, so that it cannot write the plugin or an extension back. Then it removes Rexume's hooks and any group they leave empty from each of their events, the Pi and omp extensions and the skill stubs, each when install wrote it, and the plist or unit. Last, it runs `claude plugin uninstall rexume@rexume` and `claude plugin marketplace remove rexume`, each when `~/.claude/settings.json` still names what it removes, through the `claude` on its `PATH`, and removes the plugin's directory; it fails when that `claude` is missing or fails, leaving only the plugin for the next uninstall. The state directory and the executable stay.

```json
{ "version": 1, "claude_hook": "removed", "claude_hook_note": null, "claude_plugin": "removed",
  "claude_plugin_note": null, "codex_hook": "absent", "codex_hook_note": null,
  "pi_extension": "kept", "pi_extension_note": "not written by install", "omp_extension": "removed",
  "omp_extension_note": null, "agents_skill": "removed", "agents_skill_note": null,
  "claude_skill": "removed", "claude_skill_note": null, "launch_agent": "removed",
  "launch_agent_note": null }
```

Each field is `removed` or `absent`; `pi_extension`, `omp_extension`, `agents_skill`, and `claude_skill` can also be `kept`. On Linux, `systemd_unit` and `systemd_unit_note` take the place of `launch_agent` and `launch_agent_note`.

## daemon

```sh
rexume daemon [--rex PATH] [--server ADDRESS]
```

The process the job runs. It runs until stopped, logs to stderr, and exits 1 at once when another daemon holds the same state directory. It starts `rex do` with its adapter script as a child process and reconnects with a growing delay, from 1 to 30 seconds, when the server goes away; a new server socket wakes it early. It logs each call it makes through the adapter, with the call's block method, if any, and how long it took: as a warning when the call took a second or longer, and otherwise only with `RUST_LOG=rexume=debug`. On `SIGTERM` or `SIGINT` it ends `rex do` and exits 0; it also exits 0 when its executable is replaced, so that its job starts the new one. When it starts, it sets the state directory's mode to `0700` if other users have any access to it, as one an earlier version created does, and logs that it did or that it failed, without stopping. It also rewrites the Claude Code plugin's files and each of the Pi and omp extensions and the skill stubs install wrote, as install does, when they differ from this version's; it never creates a missing plugin directory, extension, or stub, leaves an extension or a skill install did not write as it is, and logs a failure without stopping, each extension on its own. A running Claude Code session reads the new plugin when it starts again or on `/reload-plugins`. Likewise, when Rexume's `SessionStart` hook is in `~/.claude/settings.json`, it appends a group to each Claude Code turn event that lacks Rexume's hook, running the same command as that hook with a `timeout` of 5, so an install from before the turn hooks gets them; settings without that hook are left as they are.

## status

```sh
rexume status
```

What to check after a restart, read without changing anything: restored panes whose conversation has not reopened, notices that may not have reached their owner, the error that ended the daemon's latest epoch, and Claude Code hooks missing from the settings.

```json
{ "version": 2, "restores": [{ "id": "…", "session": "work", "pane": "block:…", "state": "skipped", "note": "…", "created_at": 1792855785738 }],
  "undelivered_notices": [NOTICE],
  "epoch_error": { "error": "…", "started_at": 1792855785738, "ended_at": 1792855785790 },
  "claude_hooks_missing": ["UserPromptSubmit", "Stop", "StopFailure"] }
```

Each restore is one pane the server restored where Rexume set out to reopen a conversation, listed unless its gate claimed it, so a restore that reopened nothing, `skipped` or `failed`, stays listed. `session` is the session's label, `pane` the pane's ID when Rexume first saw it, and `note` why the restore waits or ended. `state` is `planned` (waiting for the pane to show its shell with nothing typed), `typed` (the line is about to be written or may have reached the shell, but its gate has not claimed the restore), `unknown` (no gate claimed the restore within 5 seconds of the write attempt; a gate that starts later still claims it, and nothing is typed again), `skipped` (the pane closed, the conversation was opened in another pane, or the pane did not show an empty shell prompt within 30 seconds), or `failed` (the line could not be sent or the server refused it). The [restore state machine](design.md#restore) records each step before its side effect. `epoch_error` is `null` unless the daemon's latest observation of the server ended on an error rather than with its connection or with the daemon's own stop or replacement; the daemon then connects again, and records nothing until it observes the server without one. `claude_hooks_missing` lists the turn events whose Rexume hooks are missing from `~/.claude/settings.json`, provided Rexume's `SessionStart` hook is present. Without that hook, as after uninstall, the list is empty. Without the turn hooks, no Claude Code turn is recorded, so a Claude Code worker's round does not finish and no notice to a Claude Code owner is confirmed. The daemon adds missing turn hooks when it next starts; a configuration manager that replaces `hooks` removes them again unless its source declares all four Rexume hooks. The field is `null` when the settings cannot be read or parsed as JSON, with the reason on stderr. The list describes the settings file, not what a running session has loaded.

## export

```sh
rexume export
```

Prints Rexume's history as JSON Lines on stdout, read without changing anything: every task with its rounds, blocked episodes, requests, notices, and snapshots, every restore, and every epoch, both those still in use and those the daemon archived. Like the state directory, the output holds task results and request texts.

The first line is the header, `{"record": "header", "version": 1, "source_id", "schema_version", "exported_at"}`, and the last is `{"record": "end", "entities": N}`, where N counts the entity lines between them. An export without that last line, or with another count, is incomplete: the command failed, exiting 1, or its output was cut. `source_id` names the database and never changes; `id` is unique for a `type` within one `source_id`, and an epoch's is a number as text.

Each entity line is `{"record": "entity", "type", "id", "schema_version", "archived_at", "data"}`, with `type` `task`, `restore`, or `epoch`. `data` is an object keyed by table name: the entity's own row and, for a task, `task_round`, `blocked_episode`, `request`, `notice`, and `snapshot` as arrays of rows in primary-key order. A row has every column of its table by name, as [`schema.sql`](../src/store/schema.sql) describes them at `schema_version`, each value as the column holds it: a number, a string, or `null`, so a column holding JSON, such as `restore.plan`, is a string. `archived_at` is when the daemon archived the entity, whose line then never changes, and `null` for one still in use, which a later export may show changed or archived; a reader keeps the line of the later export. The lines are one moment's state, each entity once, in no order the contract fixes.

A round names the turn it took in its worker's record, the file the table gives for the task's `worker_runtime` and `worker_native_id`: `turn_start` is the byte offset in it where the turn starts, and `result_ref`, for a completed interactive round, the ID of the turn's answer. A round's times do not bound its turn, since a round can take a turn that finished before it started. A non-interactive run's round names no turn, and its `result_ref` is the result file.

| Worker | Record | `result_ref` |
| --- | --- | --- |
| Codex | its transcript | the `turn_id` of the turn's `task_complete` event |
| Pi | its session file | the `id` of the answer's message entry |
| Claude Code | Rexume's turn record, `claude/SESSION_ID.jsonl` in the state directory, whose prompt lines carry the `prompt_id` that Claude Code's transcript gives as `promptId` (observed with Claude Code 2.1.322) | the ID of the `stop` line that ends the turn |
| omp | Rexume's turn record, `omp/SESSION_ID.jsonl` in the state directory | the ID of the `end` line that ends the turn |

`version` changes only when a field of these lines changes meaning or goes away; `schema_version` changes with the schema.

## report

```sh
rexume report claude|codex|pi|omp
```

Run by the hooks with the hook's JSON on stdin, which it reads whole before anything else, so the agent writing it never meets a closed pipe. Outside a Rex pane (no `REX_BLOCK`) it then does nothing. It records the conversation as the pane's occupant only when its parent agent process is the pane's foreground program, so a run the agent starts in another process does not replace the pane's conversation. Sessions the agent runs inside its own process pass that check too, so the runtime's marks decide, as the table says for each runtime: a start marked as such a session is ignored, and a start the runtime leaves unmarked is ignored while its process already holds the pane and otherwise recorded as a conversation that cannot be reopened. It prints nothing, and any failure goes to stderr with exit status 0, so a hook never holds up or fails the session it reports.

For Claude Code's `UserPromptSubmit`, `Stop`, and `StopFailure` (`hook_event_name`), `report` instead appends one line to the conversation's turn record, `claude/SESSION_ID.jsonl` in the state directory, and touches neither the database nor the server: a prompt with its `prompt_id` and, when `prompt` starts with a notice's or request's token (`[rexume ID]`), that token and no other text of it; a stop with its `prompt_id`, a new ID, and `last_assistant_message` as the answer; a failure with its `prompt_id` and `last_assistant_message`, or else `error`, as its text. An event with `agent_id` is a subagent's and is ignored. [design.md](design.md#what-is-saved) explains how the turns are read.

| Runtime | Fields read from stdin |
| --- | --- |
| `claude` | `session_id`, `transcript_path`, `model`; a start with `agent_id` is a subagent's or an in-process teammate's and is ignored |
| `codex` | `session_id`, `transcript_path`, `model`; a start without `transcript_path` is unmarked: Codex keeps no record of its guardian reviews and side conversations, nor of an ephemeral `codex exec` run |
| `pi` | `session_id`, `session_file`, `session_file_size` (the file's length when the session started; without it the report measures the file itself), `mode`, `provider`, `model`, `thinking`; a start outside `tui` mode is unmarked, which covers SDK sessions an extension creates and `pi -p`, `--mode json`, and `--mode rpc` runs, and only a `tui` session with a file can be reopened |
| `omp` | `session_id`, `session_file`, `record_length` (the length, before the extension's next line, of [the turn record](design.md#what-is-saved) it writes; without it the report measures the record itself), and `mode` (`tui`, `rpc`, `json`, or `print`); the extension runs only in a Rex pane and reports only the session omp was started for, never a subagent's; a start outside `tui` mode is unmarked, and only a `tui` session whose file exists can be reopened. The extension reports on session start, when the session changes (`/new`, `/resume`, `/fork`, or a branch), and at the next event it records after omp moved the session to another file or ID, as `/move` does. omp reopens with its session's own model and thinking, so it reports neither |

For Claude Code, `report` also reads what the agent process was started with: its arguments, and its environment as the kernel keeps it from the start. The hook's own environment is not used: it also holds the `env` of Claude Code's settings, which Claude Code applies again when it resumes. From the arguments it keeps `--remote-control` or `--rc` with an optional name, `--add-dir`, `--mcp-config`, `--settings`, `--plugin-dir`, `--setting-sources`, `--allowedTools` or `--allowed-tools`, `--disallowedTools` or `--disallowed-tools`, `--tools`, `--strict-mcp-config`, `--chrome`, and `--no-chrome`, with their values and in their order; inline JSON given to `--settings` or `--mcp-config` is dropped, and with it a flag left without a path. The launch prompt and the flags that name the conversation or set its model, effort, or permission mode are not kept. From the environment it keeps only the variables that choose the provider, where it runs, and the models its aliases name: `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_MANTLE`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`, `CLAUDE_CODE_USE_ANTHROPIC_AWS`, `AWS_REGION`, `AWS_DEFAULT_REGION`, `AWS_PROFILE`, `ANTHROPIC_BEDROCK_REGION_PREFIX`, `ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION`, `CLOUD_ML_REGION`, `ANTHROPIC_VERTEX_PROJECT_ID`, `VERTEX_REGION_CLAUDE_*`, `ANTHROPIC_FOUNDRY_RESOURCE`, `ANTHROPIC_AWS_WORKSPACE_ID`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, `ANTHROPIC_DEFAULT_FABLE_MODEL`, and `ANTHROPIC_SMALL_FAST_MODEL`. Endpoint URLs, authentication settings, and credentials are left out: a URL can carry a credential, and skipping a provider's authentication works only with a gateway's URL. Each report replaces the saved flags and variables, or removes them when there are none; a report that cannot read the process keeps them. The model merges into the recorded settings, so a resumed start that omits it keeps it.

A Claude Code conversation also records `CLAUDE_CODE_MESSAGING_SOCKET`, through which notices reach it.

For every runtime, `report` reads `REXUME_TASK_META` from the same start environment, so that a restore reopens the conversation with it and the tasks it registers afterwards carry the same `meta`; this holds also for a conversation whose launcher declared its command. Each report replaces the saved value, or removes it when the variable is unset or empty; a report that cannot read the process keeps it. A conversation with no saved value reopens with the variable empty, so that one the reopening shell exports or inherits from the Rex server does not reach it.

### A launcher's command

A launcher that makes a choice for Claude Code that its flags and variables do not show, such as which provider or account it uses, declares the command that makes the same choice again. Otherwise a restore runs `claude`, and the launcher chooses by its default at that time. Just before it `exec`s Claude Code, the launcher exports:

| Variable | Value |
| --- | --- |
| `REXUME_LAUNCH_PID` | the process that becomes Claude Code, in decimal |
| `REXUME_LAUNCH` | the command, as a JSON array of strings such as `["claude","--profile","work"]` |

The command states the choice as the launcher resolved it, not as it was asked: one that used its default names that default, so that changing the default later does not change how the conversation reopens. A restore runs the command through the pane's interactive shell with Rexume's arguments after it, as for `claude`, so the launcher passes the arguments after its own on to Claude Code; and without the provider variables `report` otherwise saves, which the launcher sets again. The command is saved in the state database and in each restore's plan, so it must not hold a credential; the launcher can fetch one when it runs.

Children inherit both variables, including the processes Claude Code starts from its own binary, so the declaration counts only for the process `REXUME_LAUNCH_PID` names. An executable that `exec`s Claude Code names itself, as `$$` in a shell script. In a shell function `$$` is the interactive shell, so the function runs the `exec` from a shell of its own:

```sh
claude-work() {
  REXUME_LAUNCH='["claude-work"]' sh -c 'export REXUME_LAUNCH_PID=$$ CLAUDE_CONFIG_DIR="$HOME/.claude-work"; exec claude "$@"' sh "$@"
}
```

When `REXUME_LAUNCH_PID` names the process but `REXUME_LAUNCH` is missing, is not a JSON array of strings, is empty, holds a NUL, or starts with an empty element or one with `=`, a restore leaves the pane's shell, and the daemon's log says why. Each report replaces what an earlier one saved, so a conversation started again without a declaration reopens as that start did. Only Claude Code's report reads these variables.

## task

The task commands run from the orchestrator's own Rex pane: they find the calling conversation through `REX_BLOCK` and the conversation last recorded there, and refuse a task another conversation owns.

| Command | Does | Prints |
| --- | --- | --- |
| `task add --worker BLOCK --title TEXT [--result-file PATH] [--keep-pane]` | Registers the worker running in pane `BLOCK`. Without `--result-file`, the worker is an interactive conversation and the first turn it finishes is the result; with it, the worker is a non-interactive run and the absolute `PATH` is the result once the run exits 0, as the paragraph after this table describes. `--keep-pane` keeps the pane after the task is released. Use it for a worker just started; prompt it again only through `task send`. Fails for a non-interactive worker whose program has already exited, while another task that is not released has the pane as its worker, while Rexume is closing the pane, and once the daemon observes a later server instance than the one `BLOCK` is on; also for a pane on another machine than the Rex server, since Rexume follows a worker through this machine's files and processes. Takes the task's `meta` from [`REXUME_TASK_META`](#environment). Records the Git working tree the worker's program stands in and takes the first round's `before` snapshot ([Snapshots](#snapshots)). | `TASK` |
| `task next ID` | Starts the next round with the same interactive worker: its result is the earliest finished turn no round has taken, including one that finished before this command. Use it to follow a worker that ends turns on its own, such as interim reports while it waits for CI. Acknowledges every earlier round. Fails while the round is unfinished, once the task is released, while Rexume is closing the pane, and on a closed pane unless turns are unread. | `TASK` of the new round |
| `task send ID (--message TEXT \| --message-file PATH)` | Starts the next round with a new request: Rexume delivers the text to the worker's conversation once it is idle, and the round's result is the turn that takes it in. Acknowledges every earlier round. While a round is `running` or `blocked`, the new round is `queued` behind it and any rounds queued before it, acknowledges nothing, and starts once they have finished; `task wait` returns the open round first. Fails once the task is released, for a non-interactive task, and when the pane is closed or Rexume is closing it. | `TASK` of the new round |
| `task list` | This conversation's tasks, each with its open round, else its latest that ran. | `{"version", "tasks": [TASK], "undelivered_notices": [NOTICE]}` |
| `task show ID [--round N]` | One task with a round's result, by default its open round, else its latest that ran. | `TASK` |
| `task wait ID [--timeout SECONDS]` | Waits up to `SECONDS` (default 600) for the task's open round, else its latest that ran, to finish, in any state, or its worker to block, and shows that round, so a round that `failed` also exits 0. While no other wait that is still running holds the round, the wait holds it: the round's notices are left to it, and it returns once it takes one still unsent; a blocked episode whose notice was sent or taken by an earlier wait is not taken again, so a second wait on it waits for what comes next. While another running wait holds the round, it returns when the round finishes or an episode begins that it had not seen when it came to wait behind one; once that wait returns or exits, it holds the round itself, and an episode that wait took no longer ends it. Two waits running at once may both report one episode. Exits 3 when it returns on a blocked episode and 124 when the timeout passes first, as decided when it returned; the `TASK` is read afterwards and may have moved on. | `TASK` with `timed_out` |
| `task ack ID [--round N]` | Records that the owner has a finished round's result, by default the open or latest round's; a notice not yet sent for it is dropped. Fails while the round is `queued`, `running`, or `blocked`. | `TASK` |
| `task abandon ID [--round N]` | Stops waiting for a round, by default the open one, which ends as `abandoned` and is acknowledged at once, so no notice is sent for it; a running `task wait` returns with it and exits 0. The worker is left as it is and the task is not released: a turn the worker finishes later is unread, which `task next` takes and `task release` waits for, and a request that already left is not withdrawn. Abandoning the open round starts the next queued round; a queued round abandoned ends as `dropped`, its request never sent. Fails once the round has finished. | `TASK` |
| `task release ID` | Ends the task and acknowledges its rounds; the daemon then closes a disposable worker's pane if it is unused, for a non-interactive task too. Fails while a round is unfinished or the worker's record shows unread turns. | `TASK` |

An interactive task's first round takes the first turn the worker's process finished, also one that finished before `add`, and none of the turns a resumed conversation's record already held; a process that answered other prompts first hands back the first of those. A non-interactive run's result file must hold UTF-8 text, at most 1 MiB and not empty; a run that exits 0 with a file that is missing, unreadable, empty, or larger fails its round, with the reason in `note`.

A message is the text itself or a file's contents, at most 256 KiB, read when the command runs.

When the daemon starts observing the server, it archives a task with its results, requests, and notices if all its rounds have finished and none finished or was acknowledged within the last 30 days, whatever its notices' states; `export` still prints it, but the task commands no longer find it.

`TASK` has `id`, `title`, `round`, `state`, `mode` (`interactive` or `non_interactive`), `keep_pane`, `worker`, `result_ref`, `note`, `finished_at`, `acked_at`, `released_at`, `unread`, `queued`, `blocked`, `request`, `pane`, `snapshot`, and, except in `task list`, `result`.

- `worker` is the pane's current Rex block ID, or `null` once it is not on the server.
- `result_ref` is, for a completed round, the record ID of an interactive worker's answer (for Claude Code, the ID of the stop in its turn record) or a non-interactive run's result file.
- `note` says why the round ended without a result.
- `unread` is the number of finished turns in an interactive worker's record, from where the task started reading it, that no round has taken, which `task next` takes in order. It is `null` for a non-interactive or released task, before the worker's conversation is known, and when its record cannot be read or was replaced.
- `queued` lists, in the order they start, the numbers of the request rounds waiting behind the open round.
- `blocked` is `null` except while the round is in a blocked episode, where it is `{kind, message, started_at}`: what the worker's program said it waits for when the episode began, as Rex reports it (`permission`, `question`, or `auth` for a password or other credential, and `null` when the program did not say; read another value as `null`), the one line of text it showed, which is the program's own and is read as plain text, and when the episode began. A status that changes while the worker stays blocked does not change them.
- `request` is `null` except for a request round, where it is `{id, state, note}`. `state` is `pending`, `sending`, `sent`, `accepted`, `unknown`, `delivered`, or `superseded`, with a NOTICE's meanings: `delivered` once the worker's own record shows the request received, which for Claude Code is once the turn that took it in ends, and `superseded` when the round ended before the request was sent, which is then never sent. A request whose round ended after it was sent keeps the state it had then. `note` says why a request still waits or how its send went.
- `pane` is `{state, closed_at, note}`. `state` is `open` while the pane exists and Rexume has neither closed nor given it up (a `--keep-pane` pane stays `open`), `closing` during a close attempt after the task was released, `closed` once the pane is gone, or `kept` when it was used after the result, or restored by the server and not left showing its shell, and is left to whoever uses it. `note` says why a pane was kept or a close put off; a kept pane that closes later keeps why it was kept.
- `snapshot` is `{worktree, note, object_directory, before, after}`, with the round's snapshots. `worktree` is the top level of the Git working tree the worker's program stood in at `task add`, or `null` with `note` saying why the task has none, as for a worker outside a repository, and `object_directory` is then `null` too. `before` and `after` are each `null` when no snapshot was taken, or `{tree, note, created_at}`: `tree` is a Git tree ID, or `null` with `note` saying why the snapshot was not taken.

A round's `state` is `queued` (a request round waiting behind the open round, with its request `pending`, which may also finish without running), then `running` or `blocked` (the task's conversation has reported from the worker's pane on the current server, for a second without a break, that it is waiting on a dialog or a question; each such episode has one blocked notice) until it finishes as `completed` (its result is saved), `cancelled` (the worker's turn was cancelled; for Claude Code, also a prompt another hook rejected, known once the worker takes other input), `failed`, `terminated` (a non-interactive run ended by a signal, or cut off when the Rex server stopped; or an interactive round in a worker pane whose conversation is not reopened after a restart, there or in another pane), `worker_closed`, `abandoned` (the owner stopped waiting with `task abandon`), or `dropped` (a queued round that ended before it started, because the worker could take no further request or the owner abandoned it; its `note` says which).

`NOTICE` has `id`, `kind` (`finished` when the round finished, `blocked` when it began a blocked episode), `task_id`, `round`, `round_state`, `state`, and `note`. A notice is listed only while its round needs it: a finished notice until the owner acknowledges the round, and a blocked notice while its episode lasts. Its delivery must also be unconfirmed: `sending`, `sent` (handed to Claude Code's socket, or written to the inbox the Pi or omp extension watches), `accepted` (Codex queued it, or the Pi or omp extension took it), or `unknown` (the send may or may not have happened; Rexume never sends it again). It leaves the list when the owner's own conversation record shows it received (`delivered`), for Claude Code once the turn that took it in ends. It also leaves when its round no longer needs it, without that changing its state: an acknowledgement or an episode's end is not a receipt. A `pending` notice is listed only when a send failed before anything left: its `note` holds the error, and the daemon tries again on its next pass. One that `task wait` took first, whose round was acknowledged before it was sent, or, for a blocked notice, whose episode ended before it was sent, is `superseded` and never sent; its `note` says which. Each notice has one claimant, `task wait` or the daemon, and is never sent again once it may have been sent; that is not a receipt, and two waits running at once may both report the same episode.

A notice's text starts with `[rexume NOTICE_ID]` and names the task, its title, and the round. A finished notice names the `task show` and `task ack` commands for that round and, when the worker has unread turns and no later round of the task is open, how many. A blocked notice names the worker's current block ID while the pane is on the server, says what the worker is waiting on, which may have been answered since: a permission prompt, a question, or a credential prompt when its program said so, and otherwise a dialog or question, names the `task show` command for that round, and says when the round's request is still pending; it needs no `task ack`, since the round has not finished. A request's text starts with `[rexume REQUEST_ID]`, followed by the message.

### Snapshots

A snapshot is what `git add -A` would stage in the task's `worktree` at one moment, as a Git tree: tracked files and untracked files that are not ignored, a submodule as the commit it has checked out, and a file a clean filter covers as that filter writes it, such as a Git LFS pointer. Rexume takes it with the installed `git` and writes nothing in the repository; the objects the repository does not already hold go to `object_directory`, in the state directory.

A round's `before` is taken by the command that started it, `task add`, `task next`, or a `task send` that started the round at once; a round that waited in the queue has none. Its `after` is taken by the owner's first command on the task after the round finished, a `task wait` that returns it finished, `show`, `ack`, `next`, `send`, `abandon`, or `release`, while it is still the task's latest round that ran; `task next` and `task send` take one snapshot for both sides. A round a queued round followed has no `after`, since its worker went on with the next round at once. `created_at` can be later than `finished_at` by as long as the owner took, and the `after` holds any change made meanwhile. A snapshot is the tree when its command ran: it does not bound the round's turn, which may have finished before the round started, and two equal trees show equal endpoints, not that nothing changed between them.

To read or compare snapshots, give Git the directory as an alternate: `GIT_ALTERNATE_OBJECT_DIRECTORIES=OBJECT_DIRECTORY git -C WORKTREE diff-tree -r -p BEFORE AFTER`. The objects are deleted once the task is archived; a tree ID in `export` then still compares, but resolves only if the repository itself holds that tree.

A snapshot that cannot be taken never fails the command or changes the round; `note` says why: `git` is missing or failed, the repository has a split index, more than 10,000 files are changed or untracked, they total more than 64 MiB or the objects the snapshot wrote of them do, what Git reads from the repository for the snapshot takes more than 64 MiB to copy, a changed or untracked file needs a clean or process filter other than Git LFS's and Rexume cannot deny that filter writes to the repository ([design.md](design.md#snapshots)), one needs Git LFS's filter in a repository whose own configuration does not set `lfs.repositoryformatversion` to 0, a filter failed, the snapshots of every task would take more than 1 GiB of the state directory, or the snapshot took longer than 5 seconds. A note quotes no message of a command that may have run a filter, since a filter can print a file's content. A command that `SIGINT`, `SIGTERM`, or `SIGHUP` reaches while it takes a snapshot ends the snapshot's processes, deletes what they wrote, and then stops by that signal.

## skill

```sh
rexume skill show [TOPIC]
rexume skill status
```

`skill show` prints the guide that Rexume's skill gives agents, as Markdown: the skill install writes is a stub that runs this command, so an agent reads the guide of the installed version, and an agent without the skill can run it to read the same guide. Without `TOPIC` it prints the main guide, which names the topic to read before each action a topic covers: `agents` for starting, prompting, and collecting from other coding agents and running a panel, `runtimes` for what differs between Claude Code, Codex, and Pi, and `task` for this file's opening conventions followed by its `task` section. The texts are `skills/rexume` and this file as the executable was built with them.

The guide tells agents to read Rex's own guide first and to follow Rexume's where the two differ, so each difference holds for the Rex guide it was reviewed against. `skill status` prints that review's record, built in from `skills/rexume/rex-guide.json`, for a check that compares it with the running Rex:

```json
{ "version": 1, "rex_version": "…", "rex_guide_sha256": "…" }
```

`rex_version` is the `rex_version` that `rex version --json` reported for the Rex the review used, and `rex_guide_sha256` the SHA-256, in lowercase hex, of that Rex's complete `rex skill show` output. Only a finished review changes them. They are not a minimum Rex version, and an equal hash from a later Rex shows that the guide text is the same, not that the later Rex behaves the same.

## Environment

| Variable | Read by | Use |
| --- | --- | --- |
| `HOME` | every command | the default state directory; install's and uninstall's targets, the plugin files, Pi and omp extensions, skill stubs, and Claude Code turn hooks the daemon updates, and the Claude Code settings `status` checks |
| `PATH` | `install`, `uninstall`, `daemon`, `report`, `task` | finding `rex` (and, for `install`, `rexume`), and for `install` and `uninstall`, `claude`; install writes its own into the job, where the daemon finds `codex` through it |
| `REX_SERVER` | `install`, `daemon`, `report`, `task` | the server address when `--server` is not given; `report` and `task` use it to reach the server |
| `XDG_RUNTIME_DIR` | `install`, `daemon` (Linux) | the default server address when neither `--server` nor `REX_SERVER` is given |
| `XDG_CONFIG_HOME` | `install`, `uninstall` (Linux) | where the systemd unit goes, when absolute |
| `REX_BLOCK` | `report`, `task`, `gate` | the calling pane |
| `REX_SESSION` | `report`, `gate` | the calling pane's session, which `gate` leaves out of a reopened conversation that ran without it |
| `CLAUDE_CODE_MESSAGING_SOCKET` | `report claude` | where notices to that conversation go |
| `REXUME_LAUNCH_PID`, `REXUME_LAUNCH` | `report claude`, from the environment Claude Code was started with | [a launcher's command](#a-launchers-command) |
| `REXUME_TASK_META` | `task add`; `report`, from the environment the agent was started with | a JSON object of at most 1 KiB, saved as the `meta` of each task the command registers and printed only by `export`; Rexume reads no key of it, and unset or empty saves `{}`. `task add` fails when it is not a JSON object or is larger. A launcher or harness sets it for the orchestrator, whose commands inherit it, and [`report`](#report) saves it so that a restore reopens the orchestrator with it |
| `SHELL` | `gate` | the interactive shell that runs a reopened conversation |
| `RUST_LOG` | every command | log filter; `daemon` logs at `info` by default, the others at `warn` |
