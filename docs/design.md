# How rexume works

This describes the current design for maintainers and for anyone who needs to know what a state in `state.db` means. The schema with its comments is [`src/store/schema.sql`](../src/store/schema.sql).

## Processes

launchd runs `rexume daemon`. The daemon starts `rex do` with the adapter script ([`src/rex/adapter.lua`](../src/rex/adapter.lua)) as its child; the script subscribes to the server's events and starts `rexume relay`, which carries events and calls over a Unix socket in the state directory back to the daemon. One kqueue loop in the daemon waits on that socket, the state database's write-ahead log, the files it follows, the exits of processes it tracks, the path of its own executable, `SIGTERM` and `SIGINT`, and deadlines it computes; with nothing happening it does not wake. The two signals are blocked and read from the queue, so on either the daemon ends `rex do` and exits; launchd ends the whole process group anyway, but a daemon run by hand or by a test would otherwise leave `rex do` running until the server stops.

The daemon alone writes layouts, drives restores, sends notices, and closes worker panes; hooks, gates, and task commands are short processes that write the same database in short transactions. When the executable at the path launchd starts is replaced or removed, as an upgrade does (Homebrew relinks it, Cargo renames a new file over it), the daemon exits and launchd starts the new one; restores and notices resume from the database.

## What is saved

The daemon keeps one observation of one server instance as an **epoch**. It ends as `server_gone` when the connection ended and the server no longer answers as the same instance, so the last observed program states are what the server held when it stopped, or as `disconnected` when only the connection ended. Only after `server_gone` can a turn be continued.

Each session's layout is one JSON document in `session`, keyed by the session's **lineage**, the ID it had when first seen; the `live` table maps each server instance's IDs to lineages. The daemon reads a session again only when its revision moves past the saved one, and reads it at once after a change, but at most once per 300 ms: a change within 300 ms of the last read is saved when that interval ends. A pane closed while the server runs leaves the document in the same transaction that forgets its row, so it does not come back. On connecting, the daemon compares its saved state with the server's and forgets sessions and panes the server no longer has.

`occupant` records which agent conversation a pane runs, from the session-start hook, and `block` caches the pane's directory, size, foreground program, and program status. `report` records an occupant only when the reporting agent's parent process is the pane's foreground program, so a run the agent starts in another process does not replace the pane's conversation. Sessions the agent runs inside its own process, such as Claude Code subagents, Codex guardian reviews, and Pi SDK sessions, pass that check, so the runtime's marks decide ([cli.md](cli.md#report) lists them). Codex and Pi mark only their interactive conversations, not their inner sessions, so a start without that mark is ignored while its process already holds the pane; from a new process, such as an ephemeral `codex exec` or `pi -p`, it becomes the occupant, so that `task` commands in the pane name it, and a restore starts a shell for it. The conversation's own record (Claude Code and Codex transcripts, Pi session files) is the authority on its turns; rexume reads it line by line and never writes it. A line that is complete but not JSON is logged with its path and offset, and the turn it belongs to counts as unknown; a line still being written is read once it ends.

`Store::open` applies the schema to a new, empty database, uses one whose `application_id` and `user_version` match this version without taking the write lock, and refuses anything else, including a database written by a newer version. `rexume status` opens the database read-only. When the daemon starts observing an instance, it deletes what nothing refers to any more: other instances' `live` rows, panes no saved layout or open task names, earlier instances' restores, epochs, and conversations older than 30 days, and tasks whose latest round was acknowledged, or if never acknowledged finished, more than 30 days ago and has no notice that may still reach its owner.

## Restore

When a new server instance appears, every saved session from an earlier instance with no restore on this instance gets one `restore_op`, planned in one transaction with a `restore_slot` per pane before anything is created. Each new pane's command is `rexume gate OP SLOT`.

| `restore_op.state` | Meaning |
| --- | --- |
| `planned` | Nothing requested yet. |
| `sent` | `session.create` was requested; from here it is never requested again. |
| `created` | The server answered, or a gate's claim showed the session. Floating layers, focus, zoom, and labels follow. |
| `completed` | Every pane exists. |
| `partial` | A floating layer was refused, or was requested and none of its panes claimed within 5 seconds, so it may or may not exist. A late claim completes it. |
| `unknown` | The create got no answer. A gate's claim seen within 5 seconds, or when the daemon next connects to the server, makes it `created`. |
| `failed` | The server refused the create. |

A floating layer creates panes like a session does, so its slots carry `layer_sent_at` before the request and the layer is never requested twice. Display settings that fail are logged and do not hold the operation back.

The gate claims its slot with a compare-and-set and, in the same transaction, binds its own pane and session to their lineages, so a create or layer whose reply was lost is still reconciled. When that reply was lost, the daemon may have seen the session, its windows, and its panes first and recorded each under its own ID. Every binding of a restored resource, from a reply, an adoption, or a claim, goes through one path that moves what was recorded to the restored lineage, ending as if the resource had been bound first: the pane's record takes the new instance's processes, statuses, and exit and keeps the earlier directory and size until the new instance reports them; a conversation reported in the pane and a task registered on it follow it; the session's saved layout replaces the earlier one; and saved layouts name the window and pane by their restored lineages, through which floating layers and focus and zoom are found. The pane still starts what was planned, the restore completes, and the session is saved once. It then becomes the planned program. A slot is claimed once, and the partial unique index `restore_slot_executor` lets only one slot per instance start a given conversation; a second pane showing the same conversation gets the reopen command typed instead. If the conversation reported itself from another pane at or after the plan's time, the gate starts a shell; if the claim fails, it starts a shell too. An agent runs from an interactive shell (`SHELL -i -c "trap : INT; COMMAND; exec SHELL -l"`), so ending the agent leaves a shell instead of closing the pane.

A pane's slot starts as:

- `continue`: the epoch ended `server_gone`, the pane's last status came from the same process and was `working`, and the conversation's record shows a prompt that no answer, cancel, or error ended. The conversation reopens with one continuation prompt carrying a reference, so its record shows the prompt was sent once; the prompt also lists the tasks it registered that are not acknowledged. The gate reads the record again before claiming, and reopens without the prompt when the turn has ended meanwhile; `restore_slot.started` records what it ran.
- `agent`: the conversation reopens without a new turn. This includes a record with a line that is not JSON: without the whole record, rexume cannot tell that the turn was cut off.
- `typed`: the reopen command is typed but not run, because the conversation had already left the pane or another slot runs it.
- `shell`: a login shell in the pane's directory, also for a Claude Code or Codex conversation whose record does not exist yet: both write it only with the first prompt and refuse to reopen without it. Pi opens a missing session file as a new session, so a Pi conversation reopens with the settings it reported.

Reopening keeps the conversation's settings: Claude Code gets the model and effort it reported, kept from its first start since a resumed session's start omits the model; Codex gets the model and effort its record last applied; Pi reopens an existing session file without flags, so Pi applies its current branch's model and thinking itself. rexume records no permission mode: a reopened conversation gets whatever its runtime's resume restores.

An acknowledged disposable worker whose pane is unchanged since its result, with no unfinished round of any task on that pane, is left out of the plan.

## Tasks

An orchestrator conversation registers a worker pane with `task add`. Each prompt the worker answers is a `task_round`. A round follows one worker conversation, bound at `add` or `rearm` when the pane's program had already reported, otherwise by the daemon from the first report after; a conversation that takes over the pane later is not followed. The daemon reads the conversation's record from the round's cursor, or from its start when a rearmed round is bound to a different conversation than the round before. A non-interactive worker (`--result-file`) has no conversation: its round ends when the program that was the pane's foreground at `add` exits, and `add` refuses a pane whose program has already exited.

A round becomes `completed` only when its result is saved in the same transaction that writes its notice: the answer text for an interactive worker, or the result file for a non-interactive run, read before the transaction and up to 1 MiB. A cancel finishes the round as `cancelled` and an error as `failed`; a non-interactive run that exits without a readable result file, or with a nonzero status, is `failed`, and one ended by a signal is `terminated`; a closed pane makes every unfinished round on it `worker_closed`. A status that leaves `working` without a recorded result does not finish a round. A Claude Code turn stopped before any output records nothing, so a prompt left open ends as `cancelled` when the worker goes idle after it; Claude Code can also write the end of a turn after its idle status, so that happens only when the record still shows the prompt open 3 seconds after the status.

### Notices

Each finished round has one `notice`, and one claimant moves it out of `pending`. A running `task wait` registers itself on the round with a compare-and-set and takes the result itself; `task ack` before the notice is sent also makes it `superseded`. Otherwise, 3 seconds after the round finished, the daemon claims it as `sending`, checking in the same statement that no waiter registered since it looked, and sends it through the owner's runtime once the owner conversation is the foreground program of a pane on the current server and is neither `working` nor `blocked`:

| Runtime | Transport | Recorded as |
| --- | --- | --- |
| Claude Code | a user line on the conversation's messaging socket | `sent` |
| Codex | `codex queue --thread ID --message TEXT` | `accepted` when it prints `Queued message` |
| Pi | a file in the inbox the extension watches, which it renames to `.accepted` | `sent`, then `accepted` once renamed |

The notice text starts with `[rexume NOTICE_ID]` and names the round. It becomes `delivered` only when the owner's own record shows that text as received input: a peer-origin user record in Claude Code, a user response item in Codex, a `custom_message` in Pi. A send that fails before anything leaves, such as a `codex queue` that could not start, goes back to `pending` with the error in `note`, is tried again on the next pass, and is listed with the undelivered notices meanwhile; one that may have gone becomes `unknown` and is settled only by that record, never by a resend. A daemon that starts finds any `sending` row left by its predecessor, which held the same lock, and makes it `unknown`.

### Cleanup

`task ack` records `acked_at` and nothing else. The daemon is the only closer of worker panes: for an acknowledged task without `--keep-pane`, it claims the pane as `closing` in a short transaction, which fails while any round on that pane is unfinished, and `add` and `rearm` refuse a `closing` pane. It then closes the pane only when every check holds: the pane's foreground generation is the one recorded with the result, and for an interactive worker also its status time, no conversation reported in the pane since, the worker's record has no turn after the result, and the server still shows the worker as the foreground program with the same status time. A non-interactive worker has exited by then, so only the generation and the foreground process tell whether another program ran in its pane. The attempt ends as `closed`; as `kept` for good, with the reason in `pane_note`, when the pane was found in use; or back to `open` with the reason in `pane_note`, when the pane has focus or the close failed or got no answer, to be tried again when the server's view changes. A successful close forgets the pane and removes it from the saved layout in one transaction. A daemon that starts puts any `closing` pane back to `open`.

## Limits

Most of these come from what Rex and the agents expose; the others need a failure rare enough that handling it is not worth more code:

- A close carries no reason, so every close on a live server counts as the user's.
- There is no conditional close: a person can take over a worker's pane between the last check and the close.
- There is no per-pane notification audience for a worker's Done or Blocked indicator.
- `session.create` and `session.new_layer` have no idempotency key, so an unanswered create stays `unknown` or `partial` rather than being retried.
- `rex` calls outside the adapter and `codex queue` have no deadline, so a server or Codex that never answers holds up the daemon or the task command that called it.
- A notice whose send failed before anything left is tried again only on the daemon's next pass, which a change on the server or in the database starts; no timer retries it.
- A restore step that fails on a database or server error, such as reading the created session or requesting a floating layer, leaves the restore in the state it reached, and it is followed again only when the daemon next connects to the server.
- A layout change within 300 ms of the previous read is saved only when that interval ends, so it is lost if the server crashes first.
- A non-interactive run's exit is attributed to its task by the pane's foreground program at `task add`, so `add` belongs right after the worker starts, and an interactive worker is prompted again only after `task rearm`.
- A task's first round reads the worker's conversation record from its start, so a worker started by resuming an earlier conversation and registered before its new prompt is recorded can take the earlier conversation's last turn as its result.
- Claude Code applies its inbound-message rules to notices, which arrive without a permission class: with no `crossSessionInbound` set, an orchestrator that prompts for permissions gets them, and one that bypasses permission prompts holds each for approval; `hold` and `refuse` hold or drop them.
- Pi's inbox is under the default state directory.

## Measurements

`cargo test --release --test it bench -- --ignored --nocapture` starts its own server with 20 sessions of three panes each, runs the daemon against it, and prints one JSON report. It asserts nothing: read the report against the goals in [AGENTS.md](../AGENTS.md), and a change's report against the baseline commit's from the same machine. `REXUME_BENCH_GOGC` sets `GOGC` for the daemon's `rex do`.

| Key | What it measures |
| --- | --- |
| `idle_30s` | The daemon's CPU seconds and the idle wakeups `top` counts over 30 seconds with nothing happening; both should be 0. |
| `memory_kib` | The daemon, `rex do`, and the relay, each the median of three samples a second apart. `rss` (`ps -o rss`) counts shared and clean file pages in every process that maps them; `phys_footprint` (`top`'s `MEM`) counts what the kernel charges to the process, including compressed pages. `rex do` is Rex's own client and Go runtime. |
| `title_changes_50_calls` | Server calls the daemon made while a pane changed its title 50 times; should be 0. |
| `rename_to_disk_ms` | For 20 session renames, the time from the command until the saved layout holds the new name: the median, the maximum, and each. |
| `rename_views_for_20` | The `session.view` calls those renames caused. |
| `daemon_children_during_renames` | The daemon's child processes seen meanwhile; only `rex do` is expected, since no process starts per event. |
| `restore_20_sessions_60_panes_ms` | The time from the new server's start until every session's restore completed and every pane's directory is known again. |
