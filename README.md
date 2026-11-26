<h1 align="center"><a href="brand/logo/rexume-mark-color.svg"><img src="brand/logo/rexume-mark-readme.png" width="72" height="72" alt="View the Rexume logo as SVG"></a><br>Rexume</h1>

Rexume reopens Claude Code, Codex, Pi, omp, and OpenCode conversations in their [Rex](https://www.superlogical.com/rex) panes after a server restart. Of those five agents, only Claude Code comes back with Rex's built-in [Program Resume (OSC 7577)](https://www.superlogical.com/rex/docs/features/session-restore#programs-that-resume), using its launch flags. Conversations Rexume reopens keep their last model and effort. Rexume also returns worker results to the orchestrator that registered them, and those tasks survive the restart.

[Install Rexume](#install) on the macOS or Linux machine running the Rex server.

## Conversations come back

When Rexume [reopens a conversation](docs/usage.md#restoring-conversations), a turn cut short by the restart continues once. Finished, cancelled, or waiting conversations reopen without a new turn. Panes you closed stay closed. Rexume never opens a second copy of the same conversation in another pane.

If you keep [Rex's Claude Code rule](docs/usage.md#rexs-program-resume), Rexume still tracks the conversation's worker tasks and can continue a cut-off turn once after Rex reopens it.

## Keep your settings

If you started Claude Code with `--model sonnet` and used Opus for your last turn, Rex's built-in rule brings back Sonnet. [Turn off that rule](docs/setup.md#server-and-terminal-settings) to have Rexume restore Opus along with your provider choice and supported launch flags. [Permission modes follow each runtime's rules](docs/usage.md#conversation-settings).

## Worker results come back

The orchestrator does not have to wait on its workers. Rexume saves each registered worker's result and [notifies the orchestrator that registered the worker](docs/usage.md#hand-work-to-a-worker). The orchestrator can then prompt the worker again or release it. Rexume closes a disposable worker's pane only if it is still unused.

## Install

Run Rexume on the machine running the Rex server. It [requires a recent Rex server](docs/setup.md). Put `rex` on `PATH` first. On macOS:

```sh
brew install entwining/tap/rexume
rexume install
```

On Linux, [download and verify a static build](docs/setup.md#linux) before running `rexume install`. You can also [build from source](docs/setup.md#from-source).

Installation adds runtime hooks, the Claude Code messaging plugin, an agent skill, and a background job. In Codex, open `/hooks` and trust `rexume report codex`. Installation [keeps your other hooks and settings](docs/setup.md#install-the-hooks-and-the-daemon).

You can [check the daemon without stopping Rex](docs/setup.md#check-the-installation). [Uninstall removes the hooks and job](docs/setup.md#uninstall) but keeps saved results.

## Known limits

Rex does not restore scrollback. Rexume does not restart other programs. When Rexume reopens a conversation, it [types a line into the pane's shell](docs/usage.md#shell-input), which can race with your typing.

Claude Code's plugin [treats Rexume-tagged messages as your prompts](docs/usage.md#claude-code-messages). Another session can forge those tags. Runtime restrictions can also hold or drop messages.

## Contributing

[AGENTS.md](AGENTS.md#develop-and-validate) lists the checks CI runs. One daemon [records pane state and coordinates recovery](docs/design.md). Task commands [return JSON with versioned fields](docs/cli.md#task).

Rexume is released under the [MIT License](LICENSE).
