<h1 align="center"><a href="brand/logo/rexume-mark-color.svg"><img src="brand/logo/rexume-mark-readme.png" width="72" height="72" alt="View the Rexume logo as SVG"></a><br>Rexume</h1>

<p align="center"><a href="#install">Install</a> · <a href="#rexume-and-program-resume">Why Rexume?</a> · <a href="#documentation">Docs</a></p>

Restart your [Rex](https://www.superlogical.com/rex) server without losing your place. Rexume brings Claude Code, Codex, Pi, omp, and OpenCode conversations back to their original panes. Results from registered workers still reach the conversation that requested them.

## Rexume and Program Resume

Rex's [Program Resume](https://www.superlogical.com/rex/docs/features/session-restore#programs-that-resume) lets an agent or hook report a resume command through OSC 7577. Rexume reads the agent's own session records, so it can reopen a conversation even before the agent adopts the protocol. Conversations Rexume reopens keep the model and effort you last used, and a turn cut short by the restart continues once.

| After a server restart | Program Resume (built-in rules) | Rexume |
| --- | --- | --- |
| Reopen Claude Code in its original pane | Yes, with Rex's hook | Yes |
| Reopen a coding conversation when Rex has no resume report | No | Yes |
| Keep model and effort changes made during a Claude Code session | No | Yes |
| Continue an interrupted Claude Code turn | No | Yes, once |
| Return a registered worker's result to the orchestrator | Outside its scope | Yes |

If Program Resume reopens a conversation, Rexume leaves it in place and keeps tracking its worker tasks. In Claude Code, Rexume can still continue an interrupted turn. To have Rexume reopen Claude Code with its last settings, turn off its [resume rule](docs/setup.md#server-and-terminal-settings) in Rex.

## Conversations come back

A turn cut short by the restart continues once. Finished, cancelled, or waiting conversations reopen without a new turn. Panes you closed stay closed, and the same conversation never opens in two panes.

## Keep your settings

Conversations come back with the model and effort they last used. If you started Claude Code with `--model sonnet` and used Opus for your last turn, Rexume brings back Opus, along with your provider choice, permission mode, and supported launch flags. The other agents follow their own rules for [permission modes](docs/usage.md#conversation-settings).

## Worker results come back

The orchestrator does not have to wait on its workers. Rexume saves each registered worker's result and notifies the orchestrator that registered it, which can then prompt the worker again or release it. Rexume closes a disposable worker's pane only if it is still unused.

## Install

Install Rexume on the macOS or Linux machine that runs the Rex server, with a recent Rex and `rex` on `PATH`. On macOS:

```sh
brew install entwining/tap/rexume
rexume install
```

On Linux, download and verify a [static build](docs/setup.md#linux), or build from [source](docs/setup.md#from-source), then run `rexume install`.

Installation adds runtime hooks, the Claude Code messaging plugin, an agent skill, and a background job, and keeps your other hooks and settings. In Codex, open `/hooks` and trust `rexume report codex`. Uninstalling removes the hooks and the job but keeps saved results.

## Known limits

Rex does not restore scrollback, and Rexume does not restart other programs. To reopen a conversation, Rexume types a line into the pane's shell, which can race with your typing.

Claude Code's plugin treats Rexume-tagged messages as your prompts. Another session can forge those tags, and runtime restrictions can hold or drop messages. The usage guide lists [every limit](docs/usage.md#known-limits).

## Documentation

- [Setup](docs/setup.md): requirements, installing, checking the installation, upgrading, and uninstalling
- [Usage](docs/usage.md): what a restore does, conversation settings, and handing work to workers
- [CLI](docs/cli.md): commands, JSON output, and exit codes
- [Design](docs/design.md): how one daemon records pane state and coordinates recovery

## Contributing

[AGENTS.md](AGENTS.md#develop-and-validate) lists the checks CI runs. Rexume is released under the [MIT License](LICENSE).
