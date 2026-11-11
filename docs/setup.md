# Set up Rexume

Rexume runs on the machine that runs the Rex server. On macOS, it needs the Rex app and is developed on Apple Silicon. On Linux, it needs Linux 5.3 or later on x86_64 or arm64 and runs as a systemd user service. Installing its hooks and job needs the Rex CLI, `rex`, on `PATH`.

Rexume needs Rex's own session restore: the Rex app 0.1.0-preview.23 or later on macOS, or the Rex server 0.1.0-preview.11 or later on Linux. The first restart after upgrading Rex brings nothing back. The earlier server saved nothing.

## Install the program

### macOS

```sh
brew install entwining/tap/rexume
```

### Linux

Each release carries a static build for each architecture. With the [GitHub CLI](https://cli.github.com), download the latest one into an empty directory. Verify that this repository's release workflow built it before putting the program on `PATH`:

```sh
cd "$(mktemp -d)"
gh release download --repo Entwining/rexume --pattern "rexume-*-$(uname -m)-unknown-linux-musl.tar.gz"
gh attestation verify rexume-*.tar.gz --repo Entwining/rexume --signer-workflow Entwining/rexume/.github/workflows/publish.yml
tar -xzf rexume-*.tar.gz
install -D -m 755 rexume-*/bin/rexume ~/.local/bin/rexume
```

### From source

You need Rust at or above the `rust-version` in `Cargo.toml`. Bundled SQLite needs a C compiler, provided by the Xcode Command Line Tools on macOS.

```sh
cargo install --locked --git https://github.com/Entwining/rexume --root ~/.local
```

This installs `~/.local/bin/rexume`. Put that directory on `PATH`, because the hooks run `rexume` by name. From a checkout, use `--path .` instead of `--git`.

## Install the hooks and the daemon

Installing the program does not install its hooks or background job. The Claude Code plugin [submits Rexume-tagged messages as your prompts](usage.md#claude-code-messages), including tags another session can forge. To install:

```sh
rexume install
```

`install` writes the runtimes' hooks and your platform's job. It prints what it did as JSON.

| What | Where |
| --- | --- |
| Claude Code `SessionStart`, `UserPromptSubmit`, `Stop`, and `StopFailure` hooks | `~/.claude/settings.json` |
| Claude Code plugin `rexume`, registered through the `claude` on your `PATH` | `~/.local/share/rexume/claude-plugin` |
| Codex `SessionStart` hook | `~/.codex/hooks.json` |
| Pi extension | `~/.pi/agent/extensions/rexume.ts` |
| omp extension | `~/.omp/agent/extensions/rexume.ts` |
| Rexume skill stub, which has the agent read the guide `rexume skill show` prints | `~/.claude/skills/rexume/SKILL.md` for Claude Code, `~/.agents/skills/rexume/SKILL.md` for Codex and Pi |
| On macOS, LaunchAgent `io.github.entwining.rexume`, which runs `rexume daemon` | `~/Library/LaunchAgents/io.github.entwining.rexume.plist` |
| On Linux, systemd user service `rexume.service`, which runs `rexume daemon` | `~/.config/systemd/user/rexume.service` |

A runtime whose directory does not exist is skipped. The JSON says so. Pi needs `~/.pi/agent` and omp needs `~/.omp/agent`; install creates neither.

The skill goes to `~/.claude/skills` when `~/.claude` exists. It goes to `~/.agents/skills` when `~/.codex` or `~/.pi/agent` exists. A `rexume` skill you wrote yourself there stays as it is.

An omp profile's agent directory gets no extension. Neither does one that `PI_CODING_AGENT_DIR` or `PI_CONFIG_DIR` names. Sessions there report nothing and take no notices.

If you install Claude Code, Codex, Pi, or omp later, run `rexume install` again once it is set up. Running it again leaves one copy of each entry and keeps your other hooks and settings. Install [recognizes its entries by their commands, names, and markers](cli.md#how-install-recognizes-its-entries).

Codex runs a new hook only after you approve it. Open `/hooks` in Codex and trust the `rexume report codex` entry.

## Server and terminal settings

`install` records the first `rex` on `PATH` in the job. It uses the server in `REX_SERVER`, or else the default server: the Rex app's on macOS, `$XDG_RUNTIME_DIR/rex/server.sock` on Linux. Pass `--rex` or `--server` to choose others. The daemon keeps the `PATH` you ran `install` with. That is how it finds `codex` to send notices.

On Linux, systemd runs your user services only while you are logged in. If your Rex server keeps running after you log out, run `loginctl enable-linger` once so that the daemon does too.

## Check the installation

`launchctl print gui/$(id -u)/io.github.entwining.rexume` on macOS shows whether the daemon runs. On Linux, use `systemctl --user status rexume`.

The daemon's log shows each restore and why each pane reopened its conversation or kept its shell. On macOS, it is `~/Library/Logs/rexume.log`. On Linux, read the journal with `journalctl --user -u rexume`.

If a configuration manager writes `~/.claude/settings.json`, declare all four Rexume hooks for Claude Code in its source. The daemon adds missing turn hooks only when it starts. `rexume status` lists them in `claude_hooks_missing` until then.

Agents started before `install` are recorded from their next session start. Seeing a restore happen means stopping the Rex server. That ends every terminal it runs, so it is not part of checking an install.

## Upgrade

Upgrade the program with `brew upgrade` or by installing over it. The daemon notices that its executable was replaced, and its job starts the new one.

The new daemon replaces the Pi and omp extensions `install` wrote when their content differs. It never creates a missing extension, so a removed one stays removed. It adds any Claude Code turn hooks the new version needs beside the `SessionStart` hook `install` wrote. Without that `SessionStart` hook, it creates no Claude Code hook. Removing it with a turn hook therefore keeps the turn hook removed too.

The daemon rewrites skill stubs `install` wrote but never creates one. It also rewrites the Claude Code plugin's files but never creates their directory. An installation from before the plugin gets it only when you run `rexume install` again. A running Claude Code session picks up a new plugin when it starts again or on `/reload-plugins`.

An installation from before omp support gets the omp extension only when you run `rexume install` again.

## Uninstall

```sh
rexume uninstall
```

This removes Rexume's hooks and any hook group left empty. It removes the Claude Code plugin through the `claude` on your `PATH`. It removes the Pi and omp extensions and skill stubs if `install` wrote them. It also removes the LaunchAgent or systemd service, which stops the daemon.

It keeps `~/.local/state/rexume`. That directory holds recorded panes, conversations, and task results. Delete it to remove them too. Then remove the program with `brew uninstall rexume`, or delete `~/.local/bin/rexume`.
