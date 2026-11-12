# <img src="brand/logo/rexume-mark-color.svg" width="34" height="34" align="texttop" hspace="4" alt="">Rexume

Rexume brings agent conversations back to their [Rex](https://www.superlogical.com/rex) panes after the server restarts. It supports Claude Code, Codex, Pi, and omp on macOS and Linux.

![Rexume: Pick up where you left off. Worker results, returned.](https://github.com/user-attachments/assets/87e5b8a8-ec76-4532-b353-7384ea249325)

## Conversations come back

Rex restores your tabs and panes. Rexume [reopens the conversations they ran](docs/usage.md#restoring-conversations). Only a turn cut off by the server stopping continues once.

Finished, cancelled, or waiting conversations reopen without a new turn. Panes you closed stay closed. The same conversation never reopens in two panes.

## Keep your settings

Conversations reopen with their model and effort. Claude Code also keeps its provider choice and supported launch flags. [Permission modes follow each runtime's rules](docs/usage.md#conversation-settings).

## Worker results come back

The orchestrator does not have to wait on its workers. Rexume saves each registered worker's result and [notifies the orchestrator that registered the worker](docs/usage.md#hand-work-to-a-worker). The orchestrator can then prompt the worker again, or release it so Rexume closes its pane.

## Install

Run Rexume on the machine running the Rex server. It [requires Rex's session restore](docs/setup.md). Put `rex` on `PATH` first. On macOS:

```sh
brew install entwining/tap/rexume
rexume install
```

On Linux, [download and verify a static build](docs/setup.md#linux) before running `rexume install`. You can also [build from source](docs/setup.md#from-source).

Installation adds runtime hooks, the Claude Code messaging plugin, an agent skill, and a background job. In Codex, open `/hooks` and trust `rexume report codex`. Installation [keeps your other hooks and settings](docs/setup.md#install-the-hooks-and-the-daemon).

You can [check the daemon without stopping Rex](docs/setup.md#check-the-installation). [Uninstall removes the hooks and job](docs/setup.md#uninstall) but keeps saved results.

## Known limits

Rex does not restore scrollback. Rexume does not restart other programs. Restoring a conversation [requires typing a line into its shell](docs/usage.md#shell-input), which can race with your typing.

Claude Code's plugin [treats Rexume-tagged messages as your prompts](docs/usage.md#claude-code-messages). Another session can forge those tags. Runtime restrictions can also hold or drop messages.

## Contributing

[AGENTS.md](AGENTS.md#develop-and-validate) lists the checks CI runs. One daemon [records pane state and coordinates recovery](docs/design.md). Task commands [return JSON with versioned fields](docs/cli.md#task).

Rexume is released under the [MIT License](LICENSE).
