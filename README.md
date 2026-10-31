# Rexume

![Rexume: Pick up where you left off. Worker results, returned.](https://github.com/user-attachments/assets/87e5b8a8-ec76-4532-b353-7384ea249325)

Rexume reopens the Claude Code, Codex, or Pi conversations in your [Rex](https://www.superlogical.com/rex) sessions once the Rex server restores them after it stops. Only a turn cut off by that stop continues once. Finished, cancelled, or waiting conversations reopen without a new turn; panes you closed stay closed.

It also saves worker results and notifies the orchestrator conversation that registered the worker, even while the orchestrator is idle.

Rexume runs on macOS and Linux.

## What comes back, and what does not

Rex restores its own sessions after its server stops: their tabs, panes, and labels come back under the same IDs, with a new login shell in each pane. Rexume needs that: the Rex app 0.1.0-preview.23 or later on macOS, or the Rex server 0.1.0-preview.11 or later on Linux. The first restart after upgrading Rex brings nothing back, because the earlier server saved nothing.

In each restored pane that ran a Claude Code, Codex, or Pi conversation, Rexume reopens that conversation once the pane shows its shell's prompt with nothing typed at it:

- **An interrupted conversation continues once.** When the conversation was working and its own record shows a turn that had not ended, it reopens with one prompt to continue.
- **A conversation reopens without a new turn** when it was still the pane's program, or when the pane was created to run it and it had exited.
- **The shell stays** in every other pane, including one whose conversation had already left it, one whose conversation another pane reopens, a Claude Code or Codex conversation that had no prompt yet, and a run its runtime keeps no record of to reopen, such as an ephemeral `codex exec` or `pi -p`. Other programs are not restarted.

Rexume reopens a conversation by typing a short `rexume gate` line into the pane's shell, which then runs the conversation the way you would, through your alias or shell function for the agent's command. A line that starts with a space, as this one does, stays out of a shell history that ignores such lines. Rexume types nothing into a pane where you started typing, or where another program runs, and stops waiting after 30 seconds, so the pane is yours. It does not reopen the same conversation in two panes.

Rex does not bring back a pane's earlier output yet, so a restored pane starts with an empty screen.

Conversations reopen with the model and effort they last used. Claude Code also keeps the provider and the [launch flags](docs/cli.md#report) it was started with, such as `--remote-control`. A reopened conversation keeps its permission mode: a Claude Code conversation reopens in the mode its record last shows, including bypass mode, which Claude Code's own resume does not bring back; Codex restores its approval policy and permissions itself; Pi has no permission modes.

## Install

Rexume runs on the machine that runs the Rex server: macOS with the Rex app installed, where it is developed on Apple Silicon, or Linux 5.3 or later on x86_64 or arm64, as a systemd user service. Installing its hooks and job needs the Rex CLI, `rex`, on `PATH`.

### Install the program

On macOS:

```sh
brew install entwining/tap/rexume
```

On Linux, each release carries a static build for each architecture. With the [GitHub CLI](https://cli.github.com), download the latest one into an empty directory, check that this repository's release workflow built it, and put the program on `PATH`:

```sh
cd "$(mktemp -d)"
gh release download --repo Entwining/rexume --pattern "rexume-*-$(uname -m)-unknown-linux-musl.tar.gz"
gh attestation verify rexume-*.tar.gz --repo Entwining/rexume --signer-workflow Entwining/rexume/.github/workflows/publish.yml
tar -xzf rexume-*.tar.gz
install -D -m 755 rexume-*/bin/rexume ~/.local/bin/rexume
```

To build from source instead, you need Rust at or above the `rust-version` in `Cargo.toml` and a C compiler for the bundled SQLite (the Xcode Command Line Tools on macOS):

```sh
cargo install --locked --git https://github.com/Entwining/rexume --root ~/.local
```

This installs `~/.local/bin/rexume`; put that directory on `PATH`, because the session hooks run `rexume` by name. From a checkout, use `--path .` instead of `--git`.

### Install the hooks and the daemon

Installing the program does not install its hooks or its background job. Run:

```sh
rexume install
```

`install` writes the runtimes' hooks and your platform's job, and prints what it did as JSON:

| What | Where |
| --- | --- |
| Claude Code `SessionStart` hook | `~/.claude/settings.json` |
| Codex `SessionStart` hook | `~/.codex/hooks.json` |
| Pi extension | `~/.pi/agent/extensions/rexume.ts` |
| On macOS, LaunchAgent `io.github.entwining.rexume`, which runs `rexume daemon` | `~/Library/LaunchAgents/io.github.entwining.rexume.plist` |
| On Linux, systemd user service `rexume.service`, which runs `rexume daemon` | `~/.config/systemd/user/rexume.service` |

A runtime whose directory does not exist is skipped, and the JSON says so; if you install Claude Code, Codex, or Pi later, run `rexume install` again once it is set up. Running it again leaves one copy of each entry and keeps your other hooks and settings; [docs/cli.md](docs/cli.md#how-install-recognizes-its-entries) describes how it recognizes its entries.

Codex runs a new hook only after you approve it: open `/hooks` in Codex and trust the `rexume report codex` entry.

### Server and terminal settings

`install` records in the job the first `rex` on `PATH` and the server in `REX_SERVER`, or else the default server (the Rex app's on macOS, `$XDG_RUNTIME_DIR/rex/server.sock` on Linux); pass `--rex` or `--server` to choose others. The daemon keeps the `PATH` you ran `install` with, which is how it finds `codex` to send notices.

On Linux, systemd runs your user services only while you are logged in. If your Rex server keeps running after you log out, run `loginctl enable-linger` once so that the daemon does too.

### Check the installation

`launchctl print gui/$(id -u)/io.github.entwining.rexume` on macOS, or `systemctl --user status rexume` on Linux, shows whether the daemon runs. Its log, `~/Library/Logs/rexume.log` on macOS and the journal on Linux (`journalctl --user -u rexume`), shows each restore and why each pane reopened its conversation or kept its shell.

Agents started before `install` are recorded from their next session start. Seeing a restore happen means stopping the Rex server, which ends every terminal it runs, so it is not part of checking an install.

### Upgrade

Upgrading the program, with `brew upgrade` or by installing over it, is enough: the daemon notices that its executable was replaced, its job starts the new one, and it replaces the Pi extension `install` wrote when the new version's differs. It never creates the extension, so one you removed stays removed.

## Check a restore

`rexume status` lists the restored panes where Rexume set out to reopen a conversation that has not reopened, and notices not yet confirmed as delivered; a reopened conversation is not listed, and the daemon's log records it. When the daemon's latest observation of the server ended with an error, `epoch_error` gives it: the daemon records nothing until it connects again without one. A pane is listed in one of these states, and its `note` says why:

- `planned`: Rexume waits for the pane's shell to read its startup files or show its prompt with nothing typed at it; a prompt that does not end in a space keeps it waiting.
- `typed`, `unknown`: the line was typed, and the conversation has not started yet; after 5 seconds it is `unknown`. Rexume does not type it again, because the line may still be running.
- `skipped`: the pane closed, the conversation was opened in another pane, or the pane did not show an empty prompt within 30 seconds; start the conversation yourself if you want it there.
- `failed`: the server refused the line.

[docs/design.md](docs/design.md#restore) describes these states.

## Hand work to a worker

Run the task commands from the orchestrator's own Rex pane: they find the orchestrator through `REX_BLOCK`. Start the worker without `REX_SESSION` and with `REX_BLOCK`, as Rex sets it, so that it reports from its own pane. Register the worker right after starting it, with its pane's block ID:

```sh
rexume task add --worker "$WORKER_BLOCK" --title 'review the parser'
```

`task add` prints the task, whose `id` is the `ID` the commands below take. The result is the first turn the worker finishes after it starts; [docs/cli.md](docs/cli.md#task) gives the cases where that is an earlier answer, such as a Claude Code fork that copies its parent's turns.

When the worker's turn ends, Rexume saves its result and sends the orchestrator one notice. The orchestrator reads the result with `rexume task show ID` and acknowledges it with `rexume task ack ID`. Once it is done with the worker, `rexume task release ID` lets the daemon close a disposable worker's pane, but only if nothing has happened in it since the result; after a restart, a released worker's pane that Rex restored is closed once it shows its shell with nothing typed, and kept otherwise.

To wait for the result in its own turn instead, the orchestrator runs `rexume task wait ID`. When the worker stops on a question or a permission dialog, Rexume sends one notice for that stop too, which needs no acknowledgement, and `task wait` returns for it with exit status 3.

To give the same worker a new prompt, the orchestrator runs `rexume task send ID --message-file PATH`: Rexume delivers the prompt to the worker's conversation itself, without typing into the pane, and the result is the turn that answers it. A turn the worker ends on its own, such as an interim report while it waits for CI, is collected with `rexume task next ID`. When the orchestrator no longer waits for a round, such as one whose worker never takes its prompt in, `rexume task abandon ID` ends that round without touching the worker; an answer the worker gives later is still collected with `task next`.

A non-interactive run that writes its result to a file is registered with `--result-file PATH` instead, and the file is its result once the run exits 0. [docs/cli.md](docs/cli.md#task) gives every task command, its output, and what a result file may hold.

## Known limits

Rex and the agents do not provide what these would need:

- A close carries no reason, so Rexume treats every close on a live server as yours, including one an app or script made.
- Rex has no conditional close. Between Rexume's check that a worker's pane is unused and the close, a person can still start typing in it.
- Rex has no per-pane notification audience, so a worker's Done or Blocked indicator notifies you like any other pane.
- Rex starts a shell in every pane it restores and offers no way to start another program there instead, so Rexume types its line into that shell. Between Rexume's check that nothing is typed at the prompt and the line, you can still start typing; the line begins with Ctrl-E and Ctrl-U, which clear what you typed instead of running it with the line. At a zsh or bash prompt, Ctrl-Y brings it back; text typed before the shell's first prompt is lost. In a vi keymap, Ctrl-U clears only what stands before the cursor, so text you typed and then moved back before stays on the line and runs with it, and a shell you left in vi command mode takes the line as commands: the conversation does not reopen.
- Rex saves its sessions about a second after a change, so a pane opened or closed within about a second before the server crashed comes back as it was before that change.
- Claude Code applies [its inbound-message rules](https://code.claude.com/docs/en/cross-session-messaging#control-inbound-messages) to Rexume's notices and requests, which carry no permission class. With `crossSessionInbound` unset, a session that prompts for permissions receives them, but one that bypasses permission prompts holds each for your approval and drops it when you deny it or leave the approval unanswered for [`dialogExpiry`](https://code.claude.com/docs/en/settings-reference#dialogexpiry), five minutes by default; `hold` holds them until an `accept` applies, and `refuse` drops them. A held request that is dropped is never delivered: its round stays unfinished until the orchestrator abandons it or the worker's pane closes, and Rexume never sends it again. You can set it to `accept` to receive them without approval, which also lets that session's other cross-session messages through; Rexume does not change the setting.
- Codex holds queued messages while a conversation's last turn was interrupted and no turn has run since, so a notice or request to it waits until someone gives it input. This was observed on Codex 0.164.0, and [openai/codex#56914](https://github.com/openai/codex/issues/56914) asks Codex to show when it holds them.

These are Rexume's own trade-offs:

- At a prompt, Rexume reads what stands before the cursor and the rows below it, so a prompt that does not end in a space keeps the pane waiting, and text after the cursor on its row, such as text you typed and then moved back before, is left to the line, which clears it or, in a vi keymap, runs it as described above. In zsh, the earlier lines of a command of several lines, with the cursor after a blank on a later one, run with the line.
- While the shell still reads its startup files, Rexume types the line at once, and the terminal holds it for the shell's first prompt. A startup file that reads from the terminal, such as one that asks a question, takes the line instead, and the conversation does not reopen.
- A provider variable your shell exports to every program, such as `AWS_PROFILE` from a shell profile, looks to Rexume like one set for a single `claude` launch, so a Claude Code conversation reopens with the value it was started with even after you change the profile, until you start that conversation again yourself.

## Uninstall

```sh
rexume uninstall
```

This removes the two hooks (and any hook group left empty), the Pi extension if `install` wrote it, and the LaunchAgent or systemd service, which stops the daemon. It keeps `~/.local/state/rexume`, which holds recorded panes, conversations, and task results; delete that directory to remove them too. Then remove the program with `brew uninstall rexume`, or delete `~/.local/bin/rexume`.

## Contributing

[AGENTS.md](AGENTS.md) gives the project's goals, the decisions to keep, and how to change and validate the code; [docs/design.md](docs/design.md) explains what the daemon saves and how a restore and a task move through their states.

## License

Rexume is released under the [MIT License](LICENSE).
