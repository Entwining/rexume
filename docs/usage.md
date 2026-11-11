# Restores and worker results

## Restoring conversations

Rex restores its own sessions after its server stops. Tabs, panes, and labels come back under the same IDs. Each pane starts a new login shell. Rexume needs [a Rex version that saves sessions](setup.md) to reopen their conversations.

In each restored pane that ran Claude Code, Codex, Pi, or omp, Rexume waits for the shell to be ready with nothing typed at its prompt.

An interrupted conversation continues once. Its last status must have been working, and its own record must show a turn that had not ended. Rexume reopens it with one prompt to continue.

A conversation otherwise reopens without a new turn when it was still the pane's program. It also reopens when the pane was created to run it and it had exited. Finished, cancelled, or waiting conversations get no new turn. Panes you closed stay closed. The same conversation never reopens in two panes.

The shell stays in every other pane. That includes a pane its conversation already left, or whose conversation another pane reopens. It includes a Claude Code, Codex, or omp conversation that had no session record yet. Runs with no record to reopen also leave the shell, such as an ephemeral `codex exec`, `pi -p`, or `omp -p`. Other programs are not restarted.

Rexume types a short `rexume gate` line into the shell to reopen a conversation. The command runs through your alias or shell function for the agent's command, as it would when you start it yourself. The line starts with a space. A shell history that ignores such lines keeps it out.

Rexume types nothing into a pane where it sees that you started typing or another program runs. It stops waiting after 30 seconds and leaves the pane to you. The [shell input checks have limits](#shell-input), including a race with your typing.

Rex does not bring back a pane's earlier output yet. A restored pane starts with an empty screen.

### Conversation settings

Conversations reopen with the model and effort they last used. Claude Code also keeps its supported [launch flags](cli.md#report), such as `--remote-control`. It keeps the variables that chose its provider, or the command of a launcher that [declares its provider or account choice](cli.md#a-launchers-command).

A Claude Code conversation reopens in the permission mode its record last shows, including bypass mode. Claude Code's own resume does not bring that mode back. Codex restores its approval policy and permissions itself. Pi has no permission modes.

omp takes its approval mode from launch flags and settings rather than its session. A conversation started with `--approval-mode` or `--yolo` therefore reopens in the mode its settings give.

## Check a restore

```sh
rexume status
```

This lists restored panes whose conversation Rexume tried to reopen but has not reopened. Successful restores appear in the [daemon's log](setup.md#check-the-installation).

It also lists notices whose delivery is unconfirmed and whose round still needs them. These include notices sent or that failed to send. A finished round needs its notice until the orchestrator acknowledges it. A blocked episode needs its notice until the episode ends.

When the daemon's latest observation of the server ended with an error, `epoch_error` gives it. The daemon records nothing until it connects again without an error.

A pane's `note` says why it is in its state:

| State | What it means |
| --- | --- |
| `planned` | Rexume waits for the shell to read its startup files or show a prompt with nothing typed. A prompt that does not end in a space keeps it waiting. |
| `typed`, `unknown` | The line was typed, but the conversation has not started yet. After 5 seconds it is `unknown`. Rexume does not type it again, because the line may still be running. |
| `skipped` | The pane closed, the conversation opened in another pane, or no empty prompt appeared within 30 seconds. Start the conversation yourself if you want it there. |
| `failed` | The server refused the line. |

The daemon [records each restore step before its side effect](design.md#restore).

## Hand work to a worker

An agent with Rexume's skill reads the guide `rexume skill show` prints. The guide also covers starting agents in Rex panes and reading their answers.

Run task commands from the orchestrator's own Rex pane. They find the orchestrator through `REX_BLOCK`. Start the worker without `REX_SESSION` and with `REX_BLOCK`, as Rex sets it. That makes it report from its own pane. Register it right after starting it, with its pane's block ID:

```sh
rexume task add --worker "$WORKER_BLOCK" --title 'review the parser'
```

`task add` prints the task. Its `id` is the `ID` the following commands take. The result is the first turn the worker finishes after it starts. That [can be an answer finished before registration](cli.md#task).

### Read, acknowledge, and release

When the worker's turn ends, Rexume saves its result and sends the orchestrator one notice. Read and acknowledge it:

```sh
rexume task show ID
rexume task ack ID
```

Once you are done with the worker, run `rexume task release ID`. The daemon can then close a disposable worker's pane, but only if nothing has happened in it since the result. After a restart, it closes a released worker's restored pane once it shows its shell with nothing typed. Otherwise it keeps the pane.

To wait for the result in the orchestrator's own turn instead, run `rexume task wait ID`. When the worker stops on a question or permission dialog, Rexume sends one notice for that stop too. That notice needs no acknowledgement. `task wait` returns for it with exit status 3.

### More work for the same worker

Send a new prompt with `rexume task send ID --message-file PATH`. Rexume delivers it to the worker's conversation without typing into the pane. The result is the turn that answers it.

A prompt sent while the worker is still on a round waits as the next round. It goes out once that round ends. The orchestrator need not hold the prompt until then.

Collect a turn the worker ends on its own with `rexume task next ID`. This includes interim reports while it waits for CI.

When the orchestrator no longer waits for a round, run `rexume task abandon ID`. It ends that round without touching the worker. Use it, for example, when a worker never takes its prompt in. An answer the worker gives later is still collected with `task next`.

### Results written to a file

Register a non-interactive run that writes its result to a file with `--result-file PATH` instead. The file becomes its result once the run exits 0. It [must hold nonempty UTF-8 text of at most 1 MiB](cli.md#task).

## Known limits

### Shell input

Rex starts a shell in every restored pane. It offers no way to start another program there instead, so Rexume types its line into that shell. You can start typing between Rexume's check and the line.

The line begins with Ctrl-E and Ctrl-U. These clear what you typed instead of running it with the line. At a zsh or bash prompt, Ctrl-Y brings it back. Text typed before the shell's first prompt is lost.

In a vi keymap, Ctrl-U clears only what stands before the cursor. Text you typed and then moved back before stays on the line and runs with it. A shell left in vi command mode takes the line as commands. The conversation does not reopen.

At a prompt, Rexume reads what stands before the cursor and the rows below it. A prompt that does not end in a space keeps the pane waiting. Text after the cursor on its row is left to the line. The line clears it or, in a vi keymap, runs it as described above. In zsh, earlier lines of a command of several lines run with the line when the cursor is after a blank on a later one.

While the shell reads its startup files, Rexume types the line at once. The terminal holds it for the shell's first prompt. A startup file that reads from the terminal, such as one that asks a question, takes the line instead. The conversation does not reopen.

### Panes and server state

A close carries no reason. Rexume treats every close on a live server as yours, including one an app or script made.

Rex has no conditional close. A person can start typing between Rexume's check that a worker's pane is unused and the close.

Rex has no per-pane notification audience. A worker's Done or Blocked indicator notifies you like any other pane.

Rex saves its sessions about a second after a change. A pane opened or closed within about a second before a crash comes back as it was before that change.

### Claude Code messages

The Claude Code plugin submits every message on a conversation's messaging socket that starts with a Rexume token as your own prompt. It does not check that Rexume wrote it. Another session's model can forge such a message and bypass the approval your inbound-message rules would require. A session that can run commands could reach the conversation through `rexume task send` anyway.

Without that submission, Claude Code applies [its inbound-message rules](https://code.claude.com/docs/en/cross-session-messaging#control-inbound-messages) to Rexume's notices and requests. They carry no permission class. The rules apply when the plugin is disabled or the session started before it was installed. They also apply while Claude Code refuses plugins' prompts after you stop a turn, until you type again. `refuse` drops messages before the plugin sees them.

With `crossSessionInbound` unset, a session that prompts for permissions receives the messages. One that bypasses permission prompts holds each for your approval. It drops the message when you deny it or leave approval unanswered for [`dialogExpiry`](https://code.claude.com/docs/en/settings-reference#dialogexpiry), five minutes by default. `hold` holds messages until an `accept` applies. `refuse` drops them.

A held request that is dropped is never delivered. Its round stays unfinished until the orchestrator abandons it or the worker's pane closes. Rexume never sends it again.

Setting `crossSessionInbound` to `accept` receives messages without approval. It also lets that session's other cross-session messages through. Rexume does not change the setting.

### Claude Code turns

Claude Code runs no hook for a turn you interrupt. It runs Rexume's hook for a prompt before another `UserPromptSubmit` hook of yours can reject it. Rexume cannot tell these cases apart.

A worker whose turn you interrupt, or whose request your hook rejects, keeps its round open until it takes other input. The round then ends as `cancelled`. `rexume task abandon` stops waiting for it.

A `Stop` hook of yours that makes Claude Code go on after it finished responding leaves the conversation `working` with an answer in its record. A restore after the server stops then continues the conversation. This also happens if the server stops between a finished answer and Claude Code's `done` status.

### Claude Code providers and launchers

A provider variable your shell exports to every program looks like one set for a single `claude` launch. This includes `AWS_PROFILE` from a shell profile. The conversation therefore reopens with the value it was started with, even after you change the profile. That lasts until you start the conversation again yourself.

A launcher that chooses the provider or account without [declaring its command](cli.md#a-launchers-command) is run again as `claude`. The conversation reopens with whatever the launcher then chooses by default.

### Codex messages and skills

Codex holds queued messages while a conversation's last turn was interrupted and no turn has run since. A notice or request waits until someone gives it input. This was observed on Codex 0.164.0. [openai/codex#56914](https://github.com/openai/codex/issues/56914) asks Codex to show when it holds them.

Codex 0.167.1 passes over a `SKILL.md` that is a symbolic link when it looks for skills. It does not load Rexume's skill if the stub links elsewhere, such as to a file in a dotfiles repository.
