# How rexume works

This describes the current design for maintainers and for anyone who needs to know what a state in `state.db` means. The schema with its comments is [`src/store/schema.sql`](../src/store/schema.sql).

## Processes

A job runs `rexume daemon`: a LaunchAgent on macOS, a systemd user service on Linux. The daemon starts `rex do` with the adapter script ([`src/rex/adapter.lua`](../src/rex/adapter.lua)) as its child; the script subscribes to the server's events and starts `rexume relay`, which carries events and calls over a Unix socket in the state directory back to the daemon. One event loop in the daemon, kqueue on macOS and epoll with inotify, pidfds, and a signalfd on Linux, waits on that socket, the state database's write-ahead log, the files it follows, the exits of processes it tracks, the path of its own executable, `SIGTERM` and `SIGINT`, and deadlines it computes; with nothing happening it does not wake. The two signals are blocked and read from the queue, so on either the daemon ends `rex do` and exits; the job ends the whole process group anyway, but a daemon run by hand or by a test would otherwise leave `rex do` running until the server stops.

A commit writes its frames to the write-ahead log, which wakes the processes watching it, before it becomes visible in the log's memory-mapped index, which no watch sees. So after each wake from the log, the daemon and `task wait` take the write lock once and release it, which waits for a commit in progress, before they read; otherwise a reader woken early could find nothing new and sleep through the commit.

The daemon alone writes layouts, drives restores, sends notices, and closes worker panes; hooks, gates, and task commands are short processes that write the same database in short transactions. When the executable at the path the job starts is replaced or removed, as an upgrade does (Homebrew relinks it, Cargo renames a new file over it), the daemon exits and the job starts the new one; restores and notices resume from the database, and the new daemon brings the Pi extension install wrote up to date ([cli.md](cli.md#daemon)).

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

Reopening keeps the conversation's settings: Claude Code gets the model and effort it reported, kept from its first start since a resumed session's start omits the model; Codex gets the model and effort its record last applied; Pi reopens an existing session file without flags, so Pi applies its current branch's model and thinking itself. Reopening also keeps the permission mode. Claude Code gets the mode its record last shows with `--permission-mode`, read from the end of the record at plan time: Claude Code writes the mode on each prompt and when a turn ends or the session exits, so a mode changed after the last prompt counts, while its own resume starts a session that was in `bypassPermissions` in a new session's mode and a Manual one in a settings `defaultMode`. A record that shows no mode, as an older one may not, leaves the mode to Claude Code's resume. Codex's resume restores the approval policy, approvals reviewer, and permission profile its record last shows unless the command line overrides them (Codex 0.164.0), so rexume passes none; managed requirements can still narrow them. Pi has no permission modes.

A released disposable worker whose pane is unchanged since its latest result, with no unfinished round of any task on that pane, is left out of the plan.

## Tasks

An orchestrator conversation registers a worker pane with `task add`, and each result it collects is a `task_round`. A non-interactive worker (`--result-file`) has no conversation: its one round ends when the program that was the pane's foreground at `add` exits, and `add` refuses a pane whose program has already exited.

An interactive worker's task follows one conversation, bound at `add` when the pane's program had already reported, otherwise by the daemon from the first report after; a conversation that takes over the pane later is never followed. Its record is a sequence of turns, which the daemon reads in order, yielding each finished turn (answered, cancelled, or failed) with the offset just after it; a turn still open where the record ends is not yielded, so nothing after it is passed over. Each round takes at most one turn, in the transaction that saves its result, writes its notice, and records the turn as taken, and no two rounds of a task take the same turn: a turn is named by where it starts, and an answered one also by its answer's record ID. The task's **position** is the offset up to which every finished turn has been taken; it moves in that same transaction. A daemon or a server restart therefore reads again from the position and takes nothing twice, also when a restore reopens the conversation, as long as its runtime writes on in the same record. The position starts at the record's start.

How a round starts decides which turn it takes:

| Round | Started by | Takes |
| --- | --- | --- |
| first | `task add` | the earliest finished turn in the record |
| next | `task next` | the earliest finished turn after the position that no round took, including one that finished before the command ran |
| request | `task send` | the turn that took the request in: the first turn to end at or after the record shows the request received |

A request is a prompt the orchestrator hands to rexume, which delivers it to the worker's conversation tagged with the request's ID; nothing is typed into the pane. A turn the worker finishes before it takes the request in, such as the end of work it was still doing, an interim report, a background job waking it, or a question the user typed, is not the request's answer: it stays untaken, the task counts it as **unread**, and `task next` takes it later, in order. A task has at most one unfinished round.

A round becomes `completed` only when its result is saved in the same transaction that writes its notice: the answer text for an interactive worker, or the result file for a non-interactive run, read before the transaction and up to 1 MiB. A cancel finishes the round as `cancelled` and an error as `failed`; a non-interactive run that exits without a readable result file, or with a nonzero status, is `failed`, and one ended by a signal is `terminated`. When the worker's pane closes, its record can end no turn any more, so an answer the record leaves open counts as finished: an unfinished round takes a turn the record already shows, and otherwise ends as `worker_closed`, and `task next` still takes unread turns from a closed pane's record, that last answer included. A record that was replaced, or is now shorter than the position, ends the unfinished round as `failed`, because the turns after the position can no longer be told apart; it is never read again from its start. A status that leaves `working` without a recorded result does not finish a round. A Claude Code turn stopped before any output records nothing, so a prompt left open ends as `cancelled` when the worker goes idle after it; Claude Code can also write the end of a turn after its idle status, so that happens only when the record still shows the prompt open 3 seconds after the status.

### Requests

The command that starts a request round stores the request's text with the round and leaves the delivery to the daemon, the only sender. The daemon sends it once the worker's conversation is the foreground program of its pane on the current server and is neither `working` nor `blocked`, so that the worker takes it as a new turn instead of in the middle of one, through the worker's runtime: the transports of the notices table below, aimed at the worker. Until then it waits as `pending`, with the reason in `note`; a request whose round ends first becomes `superseded` and is never sent. The text starts with `[rexume REQUEST_ID]`, and from there a request moves through a notice's states with their meanings, settled by the worker's record.

What a runtime does with a message it accepted is its own policy, which rexume does not model: a request stays in the last state rexume observed until the worker's record shows it received, and its round stays unfinished meanwhile; a request that never arrives, such as one a Claude Code worker held and then dropped, leaves its round unfinished until the worker's pane closes, which ends it as `worker_closed`. `task show` gives the request's state and `task wait` bounds a wait for it.

### Notices

Each finished round has one `notice`, and one claimant moves it out of `pending`. A running `task wait` registers itself on the round with a compare-and-set and takes the result itself; acknowledging the round before the notice is sent also makes it `superseded`. Otherwise, 3 seconds after the round finished, the daemon claims it as `sending`, checking in the same statement that no waiter registered since it looked, and sends it through the owner's runtime once the owner conversation is the foreground program of a pane on the current server and is neither `working` nor `blocked`:

| Runtime | Transport | Recorded as |
| --- | --- | --- |
| Claude Code | a user line on the conversation's messaging socket | `sent` |
| Codex | `codex queue --thread ID --message TEXT` | `accepted` when it prints `Queued message` |
| Pi | a file in the inbox the extension watches, which it renames to `.accepted` | `sent`, then `accepted` once renamed |

The notice text starts with `[rexume NOTICE_ID]`, names the round, and says how many turns are unread when the worker has any. It becomes `delivered` only when the owner's own record shows that text as received input: a peer-origin user record in Claude Code, a user response item in Codex, a `custom_message` in Pi. A send that fails before anything leaves, such as a `codex queue` that could not start, goes back to `pending` with the error in `note`, is tried again on the next pass, and is listed with the undelivered notices meanwhile; one that may have gone becomes `unknown` and is settled only by that record, never by a resend. A daemon that starts finds any `sending` row left by its predecessor, which held the same lock, and makes it `unknown`.

### Acknowledging and releasing

`task ack` records that the owner has a finished round's result and drops the round's notice if it has not been sent; it does nothing to the worker. Starting a round with `task next` or `task send` acknowledges the round before it in the same transaction. `task release` records that the orchestrator is done with the worker: it acknowledges the latest round, refuses while a round is unfinished or turns are unread, and a released task starts no more rounds. Only a released task's pane is ever closed, so starting a round never races a close.

### Cleanup

The daemon is the only closer of worker panes: for a released task without `--keep-pane`, it claims the pane as `closing` in a short transaction, which fails while any round on that pane is unfinished, and `add` refuses a `closing` pane. It then closes the pane only when every check holds: the pane's foreground generation is the one recorded with the latest result, and for an interactive worker also its status time, no conversation reported in the pane since, the worker's record has no turn after the task's position, and the server still shows the worker as the foreground program with the same status time. A non-interactive worker has exited by then, so only the generation and the foreground process tell whether another program ran in its pane. The attempt ends as `closed`; as `kept` for good, with the reason in `pane_note`, when the pane was found in use; or back to `open` with the reason in `pane_note`, when the pane has focus or the close failed or got no answer, to be tried again when the server's view changes. A successful close forgets the pane and removes it from the saved layout in one transaction. A daemon that starts puts any `closing` pane back to `open`.

## Limits

Most of these come from what Rex and the agents expose; the others need a failure rare enough that handling it is not worth more code:

- A close carries no reason, so every close on a live server counts as the user's.
- There is no conditional close: a person can take over a worker's pane between the last check and the close.
- There is no per-pane notification audience for a worker's Done or Blocked indicator.
- `session.create` and `session.new_layer` have no idempotency key, so an unanswered create stays `unknown` or `partial` rather than being retried.
- `rex` calls outside the adapter and `codex queue` have no deadline, so a server or Codex that never answers holds up the daemon or the task command that called it.
- A notice or request whose send failed before anything left is tried again only on the daemon's next pass, which a change on the server or in the database starts; no timer retries it.
- A restore step that fails on a database or server error, such as reading the created session or requesting a floating layer, leaves the restore in the state it reached, and it is followed again only when the daemon next connects to the server.
- A layout change within 300 ms of the previous read is saved only when that interval ends, so it is lost if the server crashes first.
- A non-interactive run's exit is attributed to its task by the pane's foreground program at `task add`, so `add` belongs right after the worker starts, and an interactive worker is prompted again only through `task send`.
- A task's first round reads the worker's conversation record from its start, so a worker started by resuming an earlier conversation and registered before its new prompt is recorded can take the earlier conversation's last turn as its result.
- A turn that takes a request in also takes whatever other input its runtime merges into the running turn, such as text the user submits in the worker meanwhile, and it is still the request's answer.
- While a request round waits for its answer, turns that finished before the request was taken in are taken only after it, since a task has one unfinished round at a time.
- Claude Code applies its inbound-message rules to notices and requests, which arrive as messages from another session without a permission class: with no `crossSessionInbound` set, a conversation that prompts for permissions gets them, and one that bypasses permission prompts holds each for the user's approval in its pane; `hold` holds them until an `accept` applies, and `refuse` drops them. Claude Code drops a message held this way when the user denies it or leaves the approval unanswered for [`dialogExpiry`](https://code.claude.com/docs/en/settings-reference#dialogexpiry), five minutes by default ([cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging#control-inbound-messages)), so a held notice or request may never be delivered: it stays `sent`, and a held request's round stays unfinished. rexume does not change the setting.
- Codex holds queued input on purpose while a conversation's last turn was interrupted (`turn_aborted` with reason `interrupted` or `budget_limited`) and no turn has run since, in a running session as in one reopened with `codex resume`, so that it does not run on after the user stopped it (observed on Codex 0.164.0). A notice or request sent meanwhile is `accepted` and waits until someone gives the conversation input; rexume reports it as the record shows it and does not predict the hold, which [openai/codex#56914](https://github.com/openai/codex/issues/56914) asks Codex to expose.
- Pi's inbox, through which notices and requests reach Pi, is under the default state directory.

## Measurements

`cargo test --release --test it bench -- --ignored --nocapture` starts its own server with 20 sessions of three panes each, runs the daemon against it, and prints one JSON report. It runs only on macOS, whose accounting of memory and wakeups it reads. It asserts nothing: read the report against the goals in [AGENTS.md](../AGENTS.md), and a change's report against the baseline commit's from the same machine. `REXUME_BENCH_GOGC` sets `GOGC` for the daemon's `rex do`.

| Key | What it measures |
| --- | --- |
| `idle_30s` | The daemon's CPU seconds and the idle wakeups `top` counts over 30 seconds with nothing happening; both should be 0. |
| `memory_kib` | The daemon, `rex do`, and the relay, each the median of three samples a second apart. `rss` (`ps -o rss`) counts shared and clean file pages in every process that maps them; `phys_footprint` (`top`'s `MEM`) counts what the kernel charges to the process, including compressed pages. `rex do` is Rex's own client and Go runtime. |
| `title_changes_50_calls` | Server calls the daemon made while a pane changed its title 50 times; should be 0. |
| `rename_to_disk_ms` | For 20 session renames, the time from the command until the saved layout holds the new name: the median, the maximum, and each. |
| `rename_views_for_20` | The `session.view` calls those renames caused. |
| `daemon_children_during_renames` | The daemon's child processes seen meanwhile; only `rex do` is expected, since no process starts per event. |
| `restore_20_sessions_60_panes_ms` | The time from the new server's start until every session's restore completed and every pane's directory is known again. |
