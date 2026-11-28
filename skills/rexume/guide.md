# Orchestrate in Rex

The calling agent owns the assignment and its validation, Rex owns terminals and layout, and each launched tool owns its authentication and conversation.

Before the first `rex` command, have Rex's guide for the installed version in context: `rex skill show` prints it, the same text Rex's own `rex` skill loads. Read the whole output, in one call or in consecutive parts to its last line, never cut by `head` or another filter, and read it again whenever its text is no longer in context, as after compaction. Take commands and syntax from the guide; where this guide differs from it, follow this guide.

Programs in Rex terminals run with the user's full permissions, outside the calling agent's sandbox and approval checks, and a sandboxed `rex` call can start them without any review. Take a command that needs escalation through the host's own escalation path. When the host cannot run it even after approval, as Codex keeps approved escalations inside its sandbox while its permission profile denies a read, request escalation for the `rex` call that runs it: the call itself needs none, but the request puts the command before the host's reviewer. Without that approval, run such a command in Rex only when the user asked for it there; a refusal holds in Rex too.

Read each topic before its first action with `rexume skill show TOPIC`, the same way as this guide, and again whenever its text is no longer in context:

- Before starting, prompting, or collecting from another coding agent, read `agents`; for several participants on one question, also its "Run a panel" section.
- When Claude Code is the host, read the "Claude Code host" section of `runtimes` before the first `rex` call. Before launching or sending input to Claude Code, Codex, or Pi, read its section of `runtimes`.
- Before acting on a task field, state, or exit status this guide does not explain, read `task`, the task commands' contract.

## Find the target

`rex whoami --json` reports `session_id` and `block_id` only while the inherited `REX_BLOCK` still resolves; it cannot discover a pane whose ID has changed. When a saved session or block no longer resolves, re-list current sessions and blocks and verify any candidate restored agent by its native conversation ID and task state before declaring it lost or starting a replacement. If the match remains uncertain, report that without redispatching the same work.

When your startup context says Rex can't tell which pane you're in, or current caller IDs cannot otherwise be verified, treat this agent as outside a Rex terminal. In the first case `REX_BLOCK` and `rex whoami` still name a pane, but it may be another agent's, and task commands run from there would act for that pane's conversation instead of yours. There, with no named or matching session, create one labelled for the task when you launch its first program, as "Launch" shows; the user sees that work only in the Rex app, so report the session's label, and show it with `session.select` only when the user asks.

Inspect with `--autostart=false` so that probing for an absent server does not start one.

## Label and place work

What your commands create belongs to this task until the user takes it over. Nothing else does, including the caller's tab and panes, the original pane that `rex split` also returns, and agents you only inspected or prompted: leave them unclosed and unmoved, and change their size only by splitting beside them, unless the user asks.

Label by purpose: session = the shared work, tab = the purpose or round, pane = the participant. A helper names its own pane and tabs it creates, even when it launches helpers of its own. The orchestrator, which coordinates the session's overall work, names the session for that work when it first starts or arranges work there, without waiting for a naming request, and maintains the session and caller's tab names as the work changes. Keep a name that already describes the work; preserve a user-chosen name or a resource the user has taken over unless the user asks for a change. When a session has tabs for different purposes, the orchestrator also gives its default-named initial tab a purpose label; a lone tab can keep its default name.

Start a worker that nobody needs to watch in a detached pane of your own session, as "Launch" shows: it runs, answers by its block ID, and comes back after a Rex restart like any pane, but no tab shows it, the Rex app included. Give a visible worker or round its own tab in that session. A single helper uses the caller's working directory unless the task specifies another. When participants share a tab, budget each pane against the window's actual width, usually at least 80 columns per pane. Use batches or another tab when a round cannot fit, and say so.

An agent participant's label names its runtime and model, and its role when siblings share them, such as `Pi DeepSeek reviewer`, so the user can tell panes apart without remembering their launch order. Update these labels within that scope once they no longer match the work, with `rex block rename -s <session> <block> '<participant>'` for a pane, unless the user has taken that resource over.

## Launch

Give the first program and its arguments to the command that creates its tab: `rex new <session> --window <purpose> -c <directory> --json -- <command> [args...]` for a new session, whose reply has the pane as `.initial_windows[0].block_ids[0]` and the tab as `.initial_windows[0].window_id`, or `rex window new -s <session> <purpose> -c <directory> --focus=false --json -- <command> [args...]` in an existing one, whose reply has them as `.block_ids[0]` and `.window_id`. A shell the user asks for, or a quick command, is itself that program. Split beside a running program when another participant or command needs a pane in the same tab.

Pass `--focus=false` to `rex run`, `rex split`, and `rex window new` unless the user asks to switch, background work or not: a new tab or pane takes focus, and with it whatever the user is typing. `rex new` has no such option, because a new session is not shown. Pass `--keep-open` whenever you or the user must read the output or exit status afterwards, because a pane created for a program closes with it.

Start a detached pane with `session.new_block`, its program and task in `command`, and add `exit: {on_completion: false}` wherever `--keep-open` would apply; outside Rex, or for another session, pass the full session ID "Find the target" chose to `-s` instead of `"$REX_BLOCK"`:

```sh
jq -n --arg label '<participant>' --arg cwd "$PWD" \
  '{flavor: "com.superlogical.terminal.shell", label: $label,
    options: {cwd: $cwd, command: ["<program>", "<argument>"],
      exit: {on_completion: false}}}' |
  rex api call -s "$REX_BLOCK" session.new_block -
```

The reply's `block_id` is the pane, which starts at 80 columns by 24 rows. For a session that holds only detached work, `session.create` with a `label` and no `initial_windows` returns its ID without opening a tab.

A delegated worker that starts a new tab needs that tab muted on every Rex app attached to the session before it runs, because each app alerts on its own and a worker that stops on a question alerts at once in an unmuted tab. Hold it until then: give the command that creates the tab `--keep-open` and the program `sh -c 'read -r _; exec "$@"' sh <program> [args...]`, register the reply's pane as "Collect the result" directs, mute the tab, and then press Enter in the pane with `rex send-key -s <session> -b <block> enter`. The worker replaces `sh` in that pane and process, with its task in its arguments, so the registered task, the result, and the exit status are the worker's.

To mute a tab, list the apps with `rex client ls -s <session> -q --short=false`, where `<session>` is the session's full ID, not a block ID, and run `rex -C <app> do client.tab.mute window_id=<window_id> muted=true` for each. Muting needs no device choice from the user, unlike showing a session. When an app lacks the action or refuses it, go on with the other apps and the launch, and name that app once in your report.

Name each pane for its participant, and place the rest of a round beside a participant in that tab.

Only a worker already running before the user asks to see it, a request from a remote Rex app included, is moved into view: move its own block with `session.move_block`, its `block_id` as `block_id` and a participant already placed in its muted tab as `anchor_block_id`, and its task stays as registered. `session.move_block` needs an anchor already in a window, so for a tab of its own, create the tab with no program, mute it, move the worker beside the tab's shell, and close that shell by its block ID. A worker to be seen from its start begins in the pane its tab opens with, not detached and moved later.

The tab stays muted while its workers answer to you, also while the user watches one or answers a dialog in it. When the user takes a worker over as their own conversation, unmute its tab on the same apps with `muted=false`; when other workers still share the tab, leave it muted and tell the user that it is.

Start the program with its task in the launch command instead of opening a shell and typing into it. When the program is a worker agent in your shared session, begin its task with `You are a worker in a shared Rex session. The coordinator owns the session name.`: with Rex's Let agents name sessions setting on, Rex's session-start hook tells an agent that runs it, such as Claude Code or Codex, to name the session that holds its pane after the agent's own task. Unless a helper's task drives Rex, such as opening its own reviewers, tell it in its task that it is the participant and does the work itself: a brief quoting the user's request to orchestrate otherwise leads it to load this skill and launch agents of its own.

Answer a dialog in an agent you launched, a folder trust prompt included, as `agents` directs.

## Collect the result

Collect an agent you launch for a result through a Rexume task. The task commands find you through your own pane's `REX_BLOCK`, so outside a Rex terminal, and for a program you did not start as a task, collect directly through the "Choose how the agent runs" section of `agents`: a task registered on an agent that has already answered takes its first answer as the result.

### Register and read a round

Right after the agent starts, register it from your own pane with `rexume task add --worker <block> --title '<task>'`: add `--result-file <absolute path>` for a non-interactive run whose output file is the result, and `--keep-pane` when the pane has a purpose after the task, such as an agent the user watches or asked to keep. Registration is what lets Rexume match the run's exit or the agent's first finished turn to this task, so register before doing anything else with that pane. `task add` refuses a run that already exited, in a kept pane too: read its exit as `last_exit` from `rex block call -s <session> -b <block> process` and its result file instead of running it again.

Rexume saves the result and sends your conversation a notice naming the task once you are idle, so do other work or end the turn instead of polling. When the next step needs the result now, `rexume task wait <id> --timeout <seconds>` exits 124 on timeout and 3 when the worker blocks. On exit 3 or a blocked notice, read the worker's screen, handle the dialog or question, and continue the same round. Rexume watches the worker until you release the task, so a blocked notice can also come while no round is open: the dialog may then be the user's own work in that pane, so read the screen first and leave a dialog the user is handling to them.

`task wait` returns the round it waited on. Check its `round` and `state`. For `completed`, read the full `result` from that JSON. If the round ended without a result, read `note`. Interactive results come from the native session record. After a finished notice, or if the output was cut, read that round with `rexume task show <id> --round <n>`. Save long JSON in the task's temporary directory and read it in parts. After reading the result, acknowledge just that round with `rexume task ack <id> --round <n>`. If you have handled every round and are done with the worker, `rexume task release <id>` also acknowledges them all.

### Continue or release a task

Prompt the worker with `rexume task send <id> --message-file <path>`, or `--message` for one short line; Rexume delivers it once the worker is idle, without touching its input box, and the round's result is the turn that takes it in. A request reaches `delivered` once the worker's own record shows it received, which can be only after the turn that took it in ends; before that it may be in that turn, waiting, or held by the runtime, such as for the user's approval: read the worker's screen, and never send it again while it may still arrive.

A worker that ends its turn to wait for its own background job, such as CI it follows, answers with an interim reply: while the work it reports is still running, `rexume task next <id>` takes its next finished turn without prompting it, including one that has already finished. Turns the worker finishes outside your rounds, such as a reply to the user, wait as `unread` in `task show` until `task next` takes them; while no round is open, Rexume sends you a notice for them. Read the pane before acting on such a turn, since it may answer the user rather than you.

If you choose to stop waiting for an unfinished round, end it with `rexume task abandon <id>`; it leaves the worker alone and does not withdraw a request already sent, so reconcile before resending and take later unread turns with `task next`.

When you need nothing more from a registered worker, run `rexume task release <id>`. It refuses while a round is unfinished or turns are unread. After release, leave a disposable pane to Rexume, including when cleaning up its tab or session: Rexume closes it once it checks that the pane is still unused, and otherwise leaves it to whoever used it. Rexume never closes a pane registered with `--keep-pane`; close or keep it as "Release what you opened" directs.

Before you finish, release the registered detached workers you are done with. Close an unregistered detached worker yourself only if this task still owns it and its work is done. Detached panes survive a restart and are invisible in the Rex app. To find them, `rexume task list` shows your tasks with their `released_at`, and `rex block ls -s <session> --json` each pane's `placement`. Report any detached worker you leave running, with its block ID and why.

### Collect directly

After a `rex wait` timeout, keep following the same block while its program runs; never start a replacement because one wait ended.

`rex block call -s <session> -b <block> program_status` returns the states a program reports (`idle`, `working`, `blocked`, `done`, `error`); Claude Code, Codex, and Pi report them. Check another program's records before relying on them. A state tells when to look, not what was answered. An interactive program does not exit when its turn ends, so `rex wait` cannot collect a turn: note the record's `updated_at` before sending, then poll `program_status` finitely until a record with a changed `updated_at` reports a state other than `working`.

## Send input to a running program

Rex accepts input even while the program reports that it is blocked on a question, and accepting it does not show that the program took it in. Read the target's screen right before you send, then verify the screen or receipt before sending more. Text in an agent's input box that you did not enter is the user's draft, which your text would join: leave it, and deliver through a path that bypasses the input box, such as `rexume task send` to a task's worker or Codex's queue, or once the box is empty. When delivery is uncertain, reconcile the screen, program, and result before any resend.

Enter text as a paste event and press keys separately with `rex send-key`. `rex send` types its text as keystrokes, which leaves the program to guess where the text ends: Codex takes a quickly typed prompt for a paste and turns the Enter that follows into a newline, and every newline in typed text acts as Enter. A program that turns on bracketed paste, as Claude Code, Codex, Pi, and zsh do, receives a paste event as one delimited paste, newlines included, so the next key acts on the whole text; a program whose `rex block call ... modes` shows `bracketed_paste` unset receives it as typed text, each newline as Enter. Write the text to a task-owned file and let `jq` build the event, so no shell quoting touches the text; for a short line without shell syntax, `--arg t '<text>'` replaces `--rawfile`:

```sh
jq -n --arg b "$block" --rawfile t "$text_file" \
  '{block_id: $b, event: {type: "paste", payload: {text: ($t | rtrimstr("\n"))}}}' |
  rex api call -s "$session" block.event - &&
  rex send-key -s "$session" -b "$block" enter
```

## Drive the Rex app

Besides the actions Rex's guide gates, open a session on the web or call API methods of the same reach (`server.stop`, `server.tailscale.*`, `hosts.put`, `hosts.remove`, `server.apple.grant_file_access`) only at the user's request.

## Release what you opened

Once results are collected and no further work needs the other terminals, close in one operation the largest resource this task created and still owns exclusively. Reuse the creation IDs and current evidence already gathered; inspect only unresolved ownership, occupant, or completion details, including detached blocks when closing a session (`rex block ls -s <session> --json`). Collecting helper results does not complete a session or tab being handed back to the user: close a container only when its own purpose is complete. Keep terminals handed to the user, requested services, anything the user took over, and containers with user additions or uncertain ownership; report what remains and why.

Cancel an owned helper's obsolete turn instead of waiting for it. Rex keeps a session whose last pane or tab the CLI closed, with no tab, where closing it in the app removes the session: when the pane or tab to close holds every block left in its session, detached ones included, close the session with `rex kill` instead. A successful close at the chosen level completes cleanup; inspect that same level to reconcile an uncertain result before retrying.

When the user asks you to close the pane, tab, or session you run in, a direct close ends your program before the call returns or your final reply reaches the user. Start the waiter below in a separate process session so shell-tool cleanup does not kill it, then give your final reply, saying the terminal will close, and end the turn. The waiter closes once your pane stops reporting `working` or after five minutes, and closes the session instead when your pane is the last block in it. To close your tab instead, have the test ask whether every block's `window_id` is your tab's and the `else` branch run `rex window close` for that tab; to close the session, replace the `if` command with `rex kill "$REX_BLOCK"`. Outside a Rex terminal, as "Find the target" decides, `$REX_BLOCK` may name another pane: ask the user which pane is yours and start the waiter with `REX_BLOCK=<that block ID>` before `python3`, because both its status query and Rex's default session for the close come from `REX_BLOCK`.

```sh
python3 - <<'PY'
import subprocess

subprocess.Popen(
    ['sh', '-c', r'''for _ in $(seq 300); do
  rex block call --autostart=false -b "$REX_BLOCK" program_status | jq -e "any(.records[]; .state == \"working\")" >/dev/null || break
  sleep 1
done
if [ "$(rex block ls --autostart=false -s "$REX_BLOCK" --json | jq ".blocks | length == 1")" = true ]; then
  rex kill "$REX_BLOCK"
else
  rex block close "$REX_BLOCK"
fi'''],
    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    start_new_session=True,
)
PY
```
