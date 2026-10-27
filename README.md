# rexume

rexume brings back your [Rex](https://www.superlogical.com/rex) sessions after the Rex server stops: sessions, tabs, split and floating panes, labels, and each pane's working directory, plus the Claude Code, Codex, or Pi conversation that was running in a pane, reopened with the model and effort it last used. A turn the server's end cut off continues once; a finished, cancelled, or waiting conversation reopens without a new turn. Panes you closed stay closed.

It also lets an orchestrator agent hand work to a worker pane and get the result back: rexume records the task, tells the orchestrator when the worker finishes, even while the orchestrator is idle, and closes a disposable worker's pane once the orchestrator releases it, unless someone has used the pane since or a restore brought it back after the worker finished.

rexume is developed on Apple Silicon macOS with the Rex app installed. It also runs on Linux, x86_64 or arm64, on the machine that runs the Rex server, as a systemd user service.

## Install

On macOS:

```sh
brew install entwining/tap/rexume
```

On Linux, each release carries a static build for each architecture. With the [GitHub CLI](https://cli.github.com), download the latest one, check that this repository's release workflow built it, and put the program on `PATH`:

```sh
gh release download --repo Entwining/rexume --pattern "rexume-*-$(uname -m)-unknown-linux-musl.tar.gz"
gh attestation verify rexume-*-linux-musl.tar.gz --repo Entwining/rexume
tar -xzf rexume-*-linux-musl.tar.gz
install -D -m 755 rexume-*-linux-musl/bin/rexume ~/.local/bin/rexume
```

To build from source instead, you need Rust at or above the `rust-version` in `Cargo.toml` and a C compiler for the bundled SQLite (the Xcode Command Line Tools on macOS):

```sh
cargo install --locked --git https://github.com/Entwining/rexume --root ~/.local
```

This installs `~/.local/bin/rexume`; put that directory on `PATH`, because the session hooks run `rexume` by name. From a checkout, use `--path .` instead of `--git`.

Installing the program does not change anything else. Then install rexume's hooks and its background job:

```sh
rexume install
```

`install` writes four things and prints what it did as JSON:

| What | Where |
| --- | --- |
| Claude Code `SessionStart` hook | `~/.claude/settings.json` |
| Codex `SessionStart` hook | `~/.codex/hooks.json` |
| Pi extension | `~/.pi/agent/extensions/rexume.ts` |
| On macOS, LaunchAgent `io.github.entwining.rexume`, which runs `rexume daemon` | `~/Library/LaunchAgents/io.github.entwining.rexume.plist` |
| On Linux, systemd user service `rexume.service`, which runs `rexume daemon` | `~/.config/systemd/user/rexume.service` |

A runtime whose directory does not exist is skipped; if you install Claude Code, Codex, or Pi later, run `rexume install` again once it is set up. Running it again leaves one copy of each entry and keeps your other hooks and settings; [docs/cli.md](docs/cli.md#how-install-recognizes-its-entries) describes how it recognizes its entries. Codex runs a new hook only after you approve it: open `/hooks` in Codex and trust the `rexume report codex` entry.

`install` records the first `rex` on `PATH` and the server in `REX_SERVER`, or else the default server (the Rex app's on macOS, `$XDG_RUNTIME_DIR/rex/server.sock` on Linux), in the job; pass `--rex` or `--server` to choose others. The daemon keeps the `PATH` you ran `install` with, which is how it finds `codex` to send notices. Restored terminals answer color queries before the app shows them only if they know your theme, so pass your terminal's colors as `--theme-foreground '#rrggbb' --theme-background '#rrggbb'`. The daemon logs to `~/Library/Logs/rexume.log` on macOS and to the journal on Linux (`journalctl --user -u rexume`).

On Linux, systemd runs your user services only while you are logged in. If your Rex server keeps running after you log out, run `loginctl enable-linger` once so that the daemon does too.

Agents started before `install` are recorded from their next session start. Upgrading the program, with `brew upgrade` or by installing over it, is enough: the daemon notices that its executable was replaced, its job starts the new one, and it replaces the Pi extension `install` wrote when the new version's differs. It never creates the extension, so one you removed stays removed.

## What comes back, and what does not

When the daemon sees a server go away and a new one start, it recreates every saved session that the new server does not already have. Each pane starts with one of:

- the conversation, reopened as it was, when it was still the pane's program;
- the conversation with one prompt to continue, when it was working and its own record shows a turn nothing ended;
- its reopen command typed but not run, when the conversation had already left the pane, or another pane reopens the same conversation;
- a login shell in the pane's directory, for anything else, including a Claude Code or Codex conversation that had no prompt yet. Other programs are not restarted.

Every restored pane first runs `rexume gate`, which claims the pane's place in the restore before it starts anything, so a conversation is never started twice. A reopened conversation keeps its permission mode: a Claude Code conversation reopens in the mode its record last shows, including bypass mode, which Claude Code's own resume does not bring back; Codex restores its approval policy and permissions itself; Pi has no permission modes.

## Hand work to a worker

An orchestrator agent registers a worker pane right after starting it, then carries on:

```sh
rexume task add --worker "$WORKER_BLOCK" --title 'review the parser'
```

When the worker's turn ends, rexume saves its result and sends the orchestrator one notice. The orchestrator reads the result with `rexume task show ID` and acknowledges it with `rexume task ack ID`. To give the same worker a new prompt, it runs `rexume task send ID --message-file PATH`: rexume delivers the prompt to the worker's conversation itself, without typing into the pane, and the result is the turn that answers it. A turn the worker ends on its own, such as an interim report while it waits for CI, is collected with `rexume task next ID`, also when it finished before the command. Once the orchestrator is done with the worker, `rexume task release ID` lets the daemon close a disposable worker's pane, but only if nothing has happened in it since the result. A non-interactive run that writes its result to a file is registered with `--result-file PATH`, and the file is its result once the run exits 0. [docs/cli.md](docs/cli.md) lists every task command and its output.

Run these from the orchestrator's own Rex pane: they find the orchestrator through `REX_BLOCK`. A worker should be started without `REX_SESSION` and with `REX_BLOCK`, as Rex sets it, so that it reports from its own pane.

## Check on it

`launchctl print gui/$(id -u)/io.github.entwining.rexume` on macOS, or `systemctl --user status rexume` on Linux, shows whether the daemon runs; its log shows each restore and why each pane started the way it did. `rexume status` lists restores that did not complete and notices not yet confirmed as delivered:

- `failed`: the server refused to create the session.
- `unknown`: the server did not answer the create, and no pane of the session reported.
- `partial`: a floating layer was refused, or may exist without any of its panes having reported.
- `planned`, `sent`, `created`: still in progress, or stopped at that step after an error until the daemon next connects.

rexume does not request an unanswered create or layer again, because doing so could duplicate panes; check the session in the Rex app and recreate what is missing. [docs/design.md](docs/design.md) describes these states.

## Uninstall

```sh
rexume uninstall
```

This removes the two hooks (and any hook group left empty), the Pi extension if `install` wrote it, and the LaunchAgent or systemd service, which stops the daemon. It keeps `~/.local/state/rexume`, which holds saved layouts, recorded conversations, and task results; delete that directory to remove them too. Then remove the program with `brew uninstall rexume`, or delete `~/.local/bin/rexume`.

## Known limits

Rex and the agents do not provide what these would need:

- A close carries no reason, so rexume treats every close on a live server as yours, including one an app or script made.
- Rex has no conditional close. Between rexume's check that a worker's pane is unused and the close, a person can still start typing in it.
- Rex has no per-pane notification audience, so a worker's Done or Blocked indicator notifies you like any other pane.
- `session.create` has no idempotency key. When the server does not answer a create, rexume waits for the new panes' claims instead of creating the session again; a session none of whose panes report stays `unknown`.
- Claude Code applies [its inbound-message rules](https://code.claude.com/docs/en/cross-session-messaging#control-inbound-messages) to rexume's notices and requests, which carry no permission class. With `crossSessionInbound` unset, a session that prompts for permissions receives them, but one that bypasses permission prompts holds each for your approval and drops it when you deny it or leave the approval unanswered for [`dialogExpiry`](https://code.claude.com/docs/en/settings-reference#dialogexpiry), five minutes by default; `hold` holds them until an `accept` applies, and `refuse` drops them. A held request that is dropped is never delivered: its round stays unfinished until the worker's pane closes, and rexume never sends it again. You can set it to `accept` to receive them without approval, which also lets that session's other cross-session messages through; rexume does not change the setting.
- Codex holds queued messages while a conversation's last turn was interrupted and no turn has run since, so a notice or request to it waits until someone gives it input. This was observed on Codex 0.164.0, and [openai/codex#56914](https://github.com/openai/codex/issues/56914) asks Codex to show when it holds them.

This one is rexume's own trade-off:

- The daemon reads a session's layout at once after a change but at most once per 300 ms, so a change made within 300 ms of the previous read is lost if the server crashes before that interval ends.

## Documentation

- [docs/cli.md](docs/cli.md): commands, JSON outputs, exit codes, and how `install` recognizes its entries.
- [docs/design.md](docs/design.md): what the daemon saves, how a restore and a task move through their states, and why.
- [AGENTS.md](AGENTS.md): goals, decisions, and how to change and validate the code.

## License

[MIT](LICENSE)
