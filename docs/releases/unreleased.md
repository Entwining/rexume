The Rexume skill reads a worker's answer directly from `task wait`. When a finished notice arrives, it reads and acknowledges the round named in the notice, even if another round has started. After releasing a disposable worker, the orchestrator leaves its pane to Rexume so a user who took it over can keep working.

Rexume now copes with Rex's Program Resume, with which Rex servers start Claude Code in restored panes themselves when the hook `rex skill setup` installs reports its session; Rex's rules for Codex and OpenCode take effect once something reports theirs. Rexume no longer types its `rexume gate` line into a conversation Rex resumed, where it sat in the agent's input box and the restore ended `unknown`. It records a Claude Code that Rex resumed as the pane's conversation, which it refused before because Rex names the login shell it starts the program in as the pane's foreground, and the restore ends in the new state `opened` while the pane's tasks go on. When the server's end cut off a turn of that conversation, Rexume sends it the continuation prompt through its messaging socket once it neither runs a turn nor waits for the user, at most once, and not when its record shows a prompt or a turn's end since the restart or 30 seconds pass first; `rexume status` lists the restore, with a new `continuation` field, until the prompt is sent. A restore Rexume reopens through its gate now also leaves out the continuation prompt when a new prompt follows the cut-off turn in the record, not only a turn's end.

Rex's resume of Claude Code brings back less than Rexume's: it starts the conversation with its launch flags rather than a model, effort, or permission mode changed later. To have Rexume reopen these conversations, turn off Rex's rules for them in Rex's configuration file, as [the setup guide](../setup.md#server-and-terminal-settings) shows:

```lua
rex.set("resume.trust.claude", false)
rex.set("resume.trust.codex", false)
rex.set("resume.trust.opencode", false)
```

The state database moves to schema version 14, which earlier versions of Rexume refuse to open.
