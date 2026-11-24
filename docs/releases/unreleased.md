Rexume now follows the Rex server 0.1.0-preview.23, whose terminals set `REX_BLOCK` and no longer `REX_SESSION`. `rexume report` and the skill pass the pane's block ID where Rex takes a session, which needs the Rex server 0.1.0-preview.23 or later, so upgrade Rex, and its CLI with it, before Rexume: with an earlier server, the hooks record no conversation for a restore to reopen. The skill no longer starts programs with `env -u REX_SESSION`.

With Rex's Let agents name sessions setting on, Rex's session-start hook asks agents that run it, such as Claude Code and Codex, to name the session that holds their pane, so a worker could rename the orchestrator's session. The skill now begins the task of each worker agent it starts in the orchestrator's session by saying that the orchestrator owns the session name, an instruction Rexume does not enforce.

The state database moves to schema version 13, which earlier versions of Rexume refuse to open.
