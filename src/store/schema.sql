-- Rexume state. One SQLite database (state.db) shared by the daemon and the short-lived
-- commands; Store::open sets the connection pragmas and applies this file to a new database.
-- Conventions: every table is STRICT; every closed set of states is a CHECK, and a state the
-- code adds is added to the CHECK, never loosened; every time is Unix milliseconds; an ID this
-- tool mints is UUIDv7 text; a lineage is the Rex ID a resource had when first seen, or a
-- minted ID when another resource holds that one already (store::ensure_lineage). A JSON
-- column is read and written whole by one module, whose serde types are its shape; its validity
-- CHECK is a column constraint so that the column can still be dropped. Table comments are the
-- only place a column's meaning lives.

-- One observation of one server instance, from a confirmed subscription until it ended.
-- 'server_gone': the connection ended and the server then no longer answered as the same
-- instance, so the last observed states are what the server held when it stopped.
-- 'disconnected': only the connection ended, or a daemon died while observing.
-- `error`: why the daemon stopped observing before the connection ended, which `status` shows
-- for the newest epoch; NULL for an epoch that is open, ended with its connection, or ended because
-- the daemon was stopped or replaced.
-- `host_id`: the machine and account the server runs as, as Rex reports it in its status, where
-- servers that see the same files report the same value; NULL when the server reported none, as
-- one without a state directory does, and for an epoch from before version 10.
-- `daemon_version`: the version of the Rexume daemon that observed the epoch; NULL for an epoch
-- from before version 10.
-- AUTOINCREMENT: readers take the newest epoch as MAX(id), and `archive` keeps an epoch under its
-- id, so an id is never reused.
CREATE TABLE IF NOT EXISTS epoch (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  instance_id    TEXT    NOT NULL,
  host_id        TEXT,
  daemon_version TEXT,
  started_at     INTEGER NOT NULL,
  ended_at       INTEGER,
  end_reason     TEXT    CHECK (end_reason IN ('disconnected', 'server_gone')),
  error          TEXT,
  CHECK ((ended_at IS NULL) = (end_reason IS NULL)),
  CHECK (error IS NULL OR ended_at IS NOT NULL)
) STRICT;
CREATE INDEX IF NOT EXISTS epoch_instance ON epoch (instance_id, id);

-- What the server last said about each pane: a cache of the live server while its instance
-- runs, and after 'server_gone' the only record of how the pane stood when the server stopped
-- (foreground and status for the decision to reopen and continue, status time and generation
-- for the cleanup). `instance_id` is the server instance the row describes; a row on the current
-- instance was seen in that instance's open epoch. A pane the server restored itself keeps its
-- row, which moves to the new instance once the daemon reads the pane there, and its foreground,
-- status, and exit, which describe processes of one instance, start empty. The row goes when the
-- pane closes. Program states are Rex's vocabulary and are not constrained here. `status_kind` and
-- `status_message` are what a `blocked` status says the program waits for (Rex's `kind`) and its
-- one line of text (Rex's `msg`), as reported and unconstrained like the state; NULL when the
-- status gives none. `status_pid` and `status_name` are the process that reported the status,
-- which need not be the foreground, and `status_at` is the time Rex gives the status. `last_exit`
-- is the exit code of the program the terminal started (Rex's child_exited), which a pane reports
-- once, or -1 when a signal ended it; it is attributed to a task by the pane having that program
-- as its foreground when the task was registered.
CREATE TABLE IF NOT EXISTS block (
  lineage        TEXT    PRIMARY KEY,
  instance_id    TEXT    NOT NULL,
  fg_pid         INTEGER,
  fg_generation  INTEGER,
  status         TEXT,
  status_kind    TEXT,
  status_message TEXT,
  status_pid     INTEGER,
  status_name    TEXT,
  status_at      INTEGER,
  last_exit      INTEGER
) STRICT;

-- Which live Rex ID stands for which lineage on one server instance, bound the first time the
-- daemon, a hook, or a task command sees the ID there (store::ensure_lineage). A resource the
-- server brought back itself under an ID an earlier instance's row binds keeps that row's
-- lineage, so the earlier instance's rows stay until the current instance's sessions and blocks
-- are first read. A lineage that differs from its live ID comes from a session an earlier
-- version recreated. One lineage per live ID and one live ID per lineage on an instance; a
-- binding that would break that is an error, not a silent second row.
CREATE TABLE IF NOT EXISTS live (
  instance_id TEXT NOT NULL,
  live_id     TEXT NOT NULL,
  lineage     TEXT NOT NULL,
  PRIMARY KEY (instance_id, live_id)
) STRICT, WITHOUT ROWID;
CREATE UNIQUE INDEX IF NOT EXISTS live_lineage ON live (instance_id, lineage);

-- A runtime's own conversation and what reopens it. `runtime` has no CHECK: the set of
-- runtimes grows in code (enum Runtime), which refuses an unknown name before writing.
-- `settings` is a JSON object owned by the runtime's module (model, effort, provider, ...,
-- whatever its resume command needs); a later report merges into it (json_patch), so a value
-- a resumed session's start omits is kept and one the report sets to null is removed.
-- `transcript` is the runtime's own file of the conversation, from which the runtime reopens it,
-- and which a later report without one leaves in place. Codex's and Pi's turns are read from that
-- file; Claude Code's and omp's from a record in the state directory, written by Claude Code's
-- turn hooks or the omp extension and named by `runtime` and `native_id` (Runtime::record).
-- `resumable` is 0 for a conversation its runtime cannot reopen, such as a run its runtime keeps
-- no record of or a Pi or omp session without a session file.
-- `reported_at` is when the conversation was last reported.
CREATE TABLE IF NOT EXISTS conversation (
  runtime     TEXT    NOT NULL,
  native_id   TEXT    NOT NULL,
  transcript  TEXT,
  resumable   INTEGER NOT NULL CHECK (resumable IN (0, 1)),
  settings    TEXT    NOT NULL DEFAULT '{}' CHECK (json_type(settings) = 'object'),
  reported_at INTEGER NOT NULL,
  PRIMARY KEY (runtime, native_id)
) STRICT, WITHOUT ROWID;

-- The conversation whose process last reported itself as a pane's foreground program, or that a
-- restore's gate started, recorded as the gate's process, or as the child of that process the
-- pane last showed in the foreground when a shell function runs the conversation, until it
-- reports itself:
-- the pane's current binding, not its history. A report may come before the daemon has seen the
-- pane, so there is no reference to `block`; store::forget_block deletes the occupant with the
-- block, and the sweep an earlier instance's occupant whose pane it forgot. `transport` is a
-- JSON object owned by the runtime's module: how a notice reaches this process (Claude Code's
-- messaging socket, known only from its report; nothing for Codex, Pi, and omp). `pid` and
-- `pid_name` are the process and the name Rex reported for it, which tell whether a pane's later
-- status came from the same process; a process the gate recorded has no name until it reports, and
-- matches any name. `instance_id` is the server instance they are from, NULL for a report before
-- version 3 that no epoch dates. `own_program`: the process was the
-- terminal's own program rather than one started from its shell, so its exit is the terminal's,
-- as a conversation an earlier version's restore started was; NULL, unknown, before version 3.
-- `keeps_session_env`: the process ran with `REX_SESSION`, so its reopened conversation gets it
-- too. `start_position`: the length of the conversation's record of turns when the process was
-- first recorded, where a task bound to this report starts reading, so that turns the record
-- already held, as a resumed conversation's does, never belong to a task; 0 before version 4, and
-- for Claude Code before version 8. A later report of the same conversation from the same
-- process, such as Claude Code's after a compaction or its first after the gate started it, keeps
-- it.
CREATE TABLE IF NOT EXISTS occupant (
  block_lineage     TEXT    PRIMARY KEY,
  runtime           TEXT    NOT NULL,
  native_id         TEXT    NOT NULL,
  instance_id       TEXT,
  pid               INTEGER NOT NULL,
  pid_name          TEXT,
  own_program       INTEGER CHECK (own_program IN (0, 1)),
  keeps_session_env INTEGER NOT NULL CHECK (keeps_session_env IN (0, 1)),
  transport         TEXT    CHECK (transport IS NULL OR json_type(transport) = 'object'),
  start_position    INTEGER NOT NULL DEFAULT 0,
  reported_at       INTEGER NOT NULL,
  FOREIGN KEY (runtime, native_id) REFERENCES conversation (runtime, native_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS occupant_conversation ON occupant (runtime, native_id, reported_at);

-- A conversation reopened in a pane the server restored itself, one row per pane and server
-- instance, planned from the pane's record on the earlier instance. Rex starts a login shell in
-- every pane it restores; the daemon types `rexume gate ID` into that shell once, and the gate
-- claims the row before it starts the conversation, so the line runs the conversation at most
-- once however often it is typed or run again. `start` is what the plan starts and `plan` holds
-- the launch details the gate consumes whole (restore::Plan). `session_label` is the label of
-- the pane's session when the row was planned, which `status` shows. `created_at` is taken
-- before the planner reads the conversation's record, so a report that arrives while it reads
-- counts as later than the plan.
-- `state`: 'planned' until the line is typed, `note` saying why it still waits; 'typed' once
-- the daemon is about to type it, committed with `typed_at` before the write, so it is never
-- typed again; 'claimed' once the gate claimed the row, with `claimed_at`, `pid`, the gate's
-- process, which the shell it becomes keeps and whose foreground children the daemon takes for
-- the conversation until it reports itself, and `started`, what the gate starts, committed
-- before it starts it: 'shell' when the conversation had reported from another pane since the
-- plan, which leaves the pane's shell as it is, and 'agent' instead of 'continue' when the
-- conversation's record showed a turn ended after what the plan read. 'unknown' when no gate
-- claimed the row soon after the write, which may not have reached the shell; a gate that starts
-- later still claims it. 'skipped' when nothing was typed because the pane was in use, closed, or
-- the conversation reported from another pane, and 'failed' when the server refused the write;
-- `note` says which. A row that ends 'skipped' or 'failed' leaves the pane with the shell Rex
-- started.
CREATE TABLE IF NOT EXISTS restore (
  id            TEXT    PRIMARY KEY,
  instance_id   TEXT    NOT NULL,
  block_lineage TEXT    NOT NULL,
  session_label TEXT,
  start         TEXT    NOT NULL CHECK (start IN ('agent', 'continue')),
  runtime       TEXT    NOT NULL,
  native_id     TEXT    NOT NULL,
  plan          TEXT    NOT NULL CHECK (json_valid(plan)),
  state         TEXT    NOT NULL
    CHECK (state IN ('planned', 'typed', 'claimed', 'unknown', 'skipped', 'failed')),
  note          TEXT,
  created_at    INTEGER NOT NULL,
  typed_at      INTEGER,
  claimed_at    INTEGER,
  started       TEXT    CHECK (started IN ('shell', 'agent', 'continue')),
  pid           INTEGER,
  CHECK ((typed_at IS NOT NULL) = (state IN ('typed', 'claimed', 'unknown'))),
  CHECK ((claimed_at IS NOT NULL) = (state = 'claimed')),
  CHECK ((pid IS NULL) = (claimed_at IS NULL)),
  CHECK ((started IS NULL) = (claimed_at IS NULL)),
  CHECK (started IS NULL OR started = start OR started = 'shell'
         OR (start = 'continue' AND started = 'agent')),
  FOREIGN KEY (runtime, native_id) REFERENCES conversation (runtime, native_id)
) STRICT;
-- One plan per pane and instance.
CREATE UNIQUE INDEX IF NOT EXISTS restore_pane ON restore (instance_id, block_lineage);
-- One process per conversation: on one server instance a conversation is reopened in at most one
-- pane. A second plan for it conflicts here and that pane keeps its shell. The foreign key
-- (runtime, native_id) has no index of its own: its parent rows are deleted only by the retention
-- sweep, which filters with NOT EXISTS first.
CREATE UNIQUE INDEX IF NOT EXISTS restore_executor ON restore (instance_id, runtime, native_id);

-- Work an orchestrator's conversation handed to a worker pane: which conversation owns it, which
-- pane does it, and what happens to the pane afterwards. Panes are named by lineage, not
-- referenced: a task outlives its pane's row. A task with `result_file` is a non-interactive run
-- whose result is that file; without one it is an interactive worker whose rounds take turns
-- from one conversation's record. `worker_pid` is the pane's foreground process when the task was
-- registered, which is the program whose exit ends a non-interactive run, and `worker_instance_id`
-- the server instance it ran on: a PID names a process only there, so a pane recorded on another
-- instance shows nothing of the run. It is NULL for a task registered before version 3 when no
-- epoch showed the instance, and such a task's run counts as the recorded pane's. `worker_runtime`
-- and `worker_native_id` are the worker's conversation, a run's as an interactive worker's, bound
-- once (at registration when the pane's foreground process had reported, else by the daemon, while
-- a round is open, once the pane's occupant reported after registration or is its foreground
-- process) and never moved to a conversation that takes over the pane later; NULL for a worker
-- that reported none, as a run that is no agent does, and for a run from before version 10.
-- `position`: byte offset into that conversation's record of turns up to which every finished turn
-- has been taken by a round; it starts at the bound report's `occupant.start_position` and moves
-- in the transaction that takes a turn, so a run's, whose rounds take no turn, stays where its
-- conversation's record began; 0 for a Claude Code worker's task upgraded from before version 8.
-- `released_at`: when the orchestrator released the worker; a released task starts no more rounds,
-- and only its pane may be closed. `pane_state`: 'open' while the pane exists and Rexume has
-- neither closed nor given it up; 'closing' while the daemon, the only closer, holds the pane for
-- one close attempt, or while a restore holds a retired worker's pane the server restored until it
-- closes or keeps it (add and new rounds refuse it); 'closed' once the pane is gone, by Rexume or
-- by the user; 'kept' when the pane was found in use, or the server restored it and it did not
-- show its shell with nothing typed, and is left to whoever uses it, `pane_note` saying why, which
-- a kept pane that closes later keeps. A close the daemon put off writes its reason to `pane_note`
-- and leaves 'open'; any other close clears the note. A pane the orchestrator asked to keep is
-- never 'closing' or 'kept'. `meta`: a JSON object `task add` took from `REXUME_TASK_META`, stored
-- and exported whole; Rexume defines no key of it and reads none.
CREATE TABLE IF NOT EXISTS task (
  id                 TEXT    PRIMARY KEY,
  title              TEXT    NOT NULL,
  owner_runtime      TEXT    NOT NULL,
  owner_native_id    TEXT    NOT NULL,
  worker_lineage     TEXT    NOT NULL,
  worker_pid         INTEGER,
  worker_instance_id TEXT,
  result_file        TEXT,
  keep_pane          INTEGER NOT NULL CHECK (keep_pane IN (0, 1)),
  worker_runtime     TEXT,
  worker_native_id   TEXT,
  position           INTEGER NOT NULL DEFAULT 0,
  released_at        INTEGER,
  pane_state         TEXT    NOT NULL DEFAULT 'open'
    CHECK (pane_state IN ('open', 'closing', 'closed', 'kept')),
  pane_note          TEXT,
  pane_closed_at     INTEGER,
  created_at         INTEGER NOT NULL,
  meta               TEXT    NOT NULL DEFAULT '{}' CHECK (json_type(meta) = 'object'),
  CHECK ((worker_runtime IS NULL) = (worker_native_id IS NULL)),
  CHECK ((pane_state = 'closed') = (pane_closed_at IS NOT NULL)),
  CHECK (pane_state != 'kept' OR pane_note IS NOT NULL),
  CHECK (pane_state NOT IN ('closing', 'kept') OR keep_pane = 0),
  CHECK (pane_state != 'closing' OR released_at IS NOT NULL),
  FOREIGN KEY (owner_runtime, owner_native_id) REFERENCES conversation (runtime, native_id),
  FOREIGN KEY (worker_runtime, worker_native_id) REFERENCES conversation (runtime, native_id)
) STRICT;
CREATE INDEX IF NOT EXISTS task_owner ON task (owner_runtime, owner_native_id);
CREATE INDEX IF NOT EXISTS task_worker ON task (worker_lineage);
CREATE INDEX IF NOT EXISTS task_worker_conversation ON task (worker_runtime, worker_native_id);

-- One round of a task: one result the orchestrator collects. A new round is a new row and an
-- earlier round keeps its result, so the rounds are the task's history. An interactive round takes
-- at most one turn from the task's worker record: `turn_start` is the byte offset where that turn
-- starts, which identifies it however often the record is read again, and an answered turn's
-- `result_ref` identifies it too, also for a round upgraded from version 1 without a `turn_start`;
-- so no two rounds of a task take the same turn. A Claude Code worker's round from before version
-- 8 names no turn in the record its turn hooks write, which starts after it. A round with a
-- `request` row takes the turn that took its request in; any other takes the earliest turn after
-- the task's `position` that no round took. A request round sent while the task has an open round
-- is 'queued' behind it: it starts as 'running' in the transaction that finishes the open round,
-- or ends there as 'dropped' when the worker can take no further request, as it does when its owner
-- abandons it; a dropped round never ran, so it never stands for the task's latest result.
-- `waiter_pid`: the `task wait` process that takes the round's notices itself while it runs; a
-- wait takes its place whenever it is empty or names a process that is gone.
-- A round is 'blocked' exactly while it has a `blocked_episode` that has not ended. A round is
-- 'completed' exactly when its result is saved here, in the transaction that also writes its
-- finished notice; every final state sets `finished_at`. A round is 'abandoned' when its owner
-- stopped waiting for it with `task abandon`, which acknowledges it in the same transaction and
-- leaves the worker as it is.
-- `finished_instance_id`, `finished_status_at`, and `finished_generation` are the worker pane's
-- record when the round finished, to tell later activity in the pane apart: its server instance,
-- NULL when the pane was not recorded or the round finished before version 3 and no epoch dates
-- it, and its status time and foreground generation, which mean something only on that instance.
-- `acked_at`: when the owner acknowledged the result. `result_ref` is the record ID of an
-- interactive worker's answer (for Claude Code and omp, the ID of the `stop` or `end` line that
-- ends its turn in their record), or a non-interactive run's result file; `note` says why a round
-- ended without a result. `result` is last: the one large column, so reads of the others never
-- cross its overflow pages.
CREATE TABLE IF NOT EXISTS task_round (
  task_id              TEXT    NOT NULL REFERENCES task (id) ON DELETE CASCADE,
  round                INTEGER NOT NULL,
  waiter_pid           INTEGER,
  state                TEXT    NOT NULL CHECK (state IN ('queued', 'running', 'blocked', 'completed',
                                                         'cancelled', 'failed', 'terminated',
                                                         'worker_closed', 'abandoned', 'dropped')),
  note                 TEXT,
  finished_at          INTEGER,
  finished_instance_id TEXT,
  finished_status_at   INTEGER,
  finished_generation  INTEGER,
  acked_at             INTEGER,
  created_at           INTEGER NOT NULL,
  turn_start           INTEGER,
  result_ref           TEXT,
  result               TEXT,
  PRIMARY KEY (task_id, round),
  CHECK ((state IN ('queued', 'running', 'blocked')) = (finished_at IS NULL)),
  CHECK ((state = 'completed') = (result IS NOT NULL)),
  CHECK ((state = 'completed') = (result_ref IS NOT NULL)),
  CHECK (turn_start IS NULL OR state IN ('completed', 'cancelled', 'failed')),
  CHECK (acked_at IS NULL OR finished_at IS NOT NULL)
) STRICT;
-- A task has at most one open round, which the rounds queued behind it wait for.
CREATE UNIQUE INDEX IF NOT EXISTS task_round_open ON task_round (task_id) WHERE state IN ('running', 'blocked');
CREATE UNIQUE INDEX IF NOT EXISTS task_round_turn ON task_round (task_id, turn_start);

-- One blocked episode of an interactive round: the task's own conversation reported `blocked`, a
-- dialog or a question waiting for an answer, from the worker's pane on the observed server
-- instance without a break for a grace. The row is inserted in the transaction that makes the round
-- 'blocked' and writes the episode's notice. `kind`, `message`, and `status_at` are the pane's
-- `status_kind`, `status_message`, and `status_at` at that moment and stay as they are when a
-- later status, still `blocked`, says otherwise. `started_at` is that transaction's time, which is
-- also the notice's `created_at`; `status_at` is when Rex says the worker began to wait. The
-- episode ends once, in the transaction that ends it: 'status_changed' when the pane's status was
-- replaced by one that is not `blocked` or was removed; 'round_finished' when the round finished,
-- with the round's `finished_at` as `ended_at`; 'server_gone' when the pane was first recorded on a
-- later server instance or a pass found its record on an earlier one; 'occupant_changed' when a
-- pass found the status no longer reported by the task's conversation in the pane. A later block
-- in the same round is another row.
CREATE TABLE IF NOT EXISTS blocked_episode (
  id         TEXT    PRIMARY KEY,
  task_id    TEXT    NOT NULL,
  round      INTEGER NOT NULL,
  kind       TEXT,
  message    TEXT,
  status_at  INTEGER,
  started_at INTEGER NOT NULL,
  ended_at   INTEGER,
  end_reason TEXT    CHECK (end_reason IN ('status_changed', 'round_finished', 'server_gone',
                                           'occupant_changed')),
  CHECK ((ended_at IS NULL) = (end_reason IS NULL)),
  FOREIGN KEY (task_id, round) REFERENCES task_round (task_id, round) ON DELETE CASCADE
) STRICT;
CREATE INDEX IF NOT EXISTS blocked_episode_round ON blocked_episode (task_id, round);
-- A round has at most one episode that has not ended.
CREATE UNIQUE INDEX IF NOT EXISTS blocked_episode_open ON blocked_episode (task_id, round)
  WHERE ended_at IS NULL;

-- The prompt a request round hands to the worker, which the daemon, the only sender, delivers to
-- the worker's conversation. It moves through a notice's states with their meanings, aimed at the
-- worker: 'pending' until sent, with `note` saying why it still waits or why a send failed before
-- anything left; 'sending' as the daemon's claim; 'sent', 'accepted', or 'unknown' after the send,
-- never sent again; 'delivered' once the worker's record shows it received; 'superseded' when its
-- round ended before it was sent, or during a send that failed before anything left, so it is never
-- sent. `id` is the token the request's text carries and the worker's record is searched for.
-- `attempts`: how often the daemon claimed the request as 'sending', a claim whose send failed
-- before anything left included; 0 for one claimed only before version 10. `sent_at`: when the
-- daemon claimed it for a send that may have left, kept through every state after 'sending' and
-- cleared when a send that failed before anything left returns it to 'pending' or 'superseded';
-- NULL for one sent before version 10. `delivered_at`: when the daemon saw the worker's record
-- show it received, which for Claude Code is when the turn that took it in ended; NULL before
-- version 10. `text` is the message, last.
CREATE TABLE IF NOT EXISTS request (
  id           TEXT    PRIMARY KEY,
  task_id      TEXT    NOT NULL,
  round        INTEGER NOT NULL,
  state        TEXT    NOT NULL DEFAULT 'pending'
    CHECK (state IN ('pending', 'sending', 'sent', 'accepted', 'delivered', 'unknown',
                     'superseded')),
  note         TEXT,
  attempts     INTEGER NOT NULL DEFAULT 0,
  sent_at      INTEGER,
  delivered_at INTEGER,
  text         TEXT    NOT NULL,
  UNIQUE (task_id, round),
  CHECK (sent_at IS NULL OR state NOT IN ('pending', 'superseded')),
  CHECK (delivered_at IS NULL OR state = 'delivered'),
  FOREIGN KEY (task_id, round) REFERENCES task_round (task_id, round) ON DELETE CASCADE
) STRICT;

-- What a task's owner is told about one of its rounds. `kind` 'finished': the round finished; one
-- per round, written in the transaction that finishes it. 'blocked': the round began a blocked
-- episode; one per episode, written in the transaction that inserts the episode, which
-- `blocked_episode_id` names; NULL for a blocked notice from before version 10 whose episode had
-- ended. `created_at`: when the notice was written, the time that transaction gives the round's
-- `finished_at` or the episode's `started_at`, and what the notice is due from. Exactly one
-- claimant, the daemon or the owner's own `task wait` or acknowledgement, moves a notice out of
-- 'pending'. 'sending' is the running daemon's claim and every state after it is the daemon's.
-- 'unknown': the send may have happened, so only the owner's own record settles it, never a resend.
-- 'superseded': the notice is never sent, because before anything was sent the owner took it
-- through `task wait`, acknowledged the round, or the blocked episode ended (`note` says which). A
-- 'pending' notice with a `note` is one whose send failed before anything left. `id` is the token
-- the notice text carries and the owner's record is searched for. `sent_at`: when the daemon
-- claimed the notice for a send that may have left, kept through every state after 'sending'. A
-- send that failed before anything left clears `sent_at` and returns the notice to 'pending', or to
-- 'superseded' if the round was acknowledged or the episode ended during the send. A 'pending' or
-- 'superseded' notice therefore has no `sent_at`; neither does one sent before version 5. An owner
-- gets its next notice only once none is still on its way ('sending', 'sent', or 'accepted') that
-- was sent after the owner's pane last reported a status (`block.status_at`). `attempts`: as a
-- request's. `delivered_at`: when the daemon saw the owner's record show it received;
-- `superseded_at`: when it became 'superseded', which for one `task wait` took is when the owner's
-- wait returned with it; both NULL before version 10. The foreign key `blocked_episode_id` has no
-- index of its own: an episode is deleted only with its round, by the retention sweep, in the
-- statement that deletes the round's notices.
CREATE TABLE IF NOT EXISTS notice (
  id                 TEXT    PRIMARY KEY,
  task_id            TEXT    NOT NULL,
  round              INTEGER NOT NULL,
  kind               TEXT    NOT NULL CHECK (kind IN ('finished', 'blocked')),
  blocked_episode_id TEXT    REFERENCES blocked_episode (id),
  state              TEXT    NOT NULL CHECK (state IN ('pending', 'sending', 'sent', 'accepted',
                                                       'delivered', 'unknown', 'superseded')),
  note               TEXT,
  attempts           INTEGER NOT NULL DEFAULT 0,
  created_at         INTEGER NOT NULL,
  sent_at            INTEGER,
  delivered_at       INTEGER,
  superseded_at      INTEGER,
  CHECK (blocked_episode_id IS NULL OR kind = 'blocked'),
  CHECK (sent_at IS NULL OR state NOT IN ('pending', 'superseded')),
  CHECK (delivered_at IS NULL OR state = 'delivered'),
  CHECK (superseded_at IS NULL OR state = 'superseded'),
  FOREIGN KEY (task_id, round) REFERENCES task_round (task_id, round) ON DELETE CASCADE
) STRICT;
CREATE INDEX IF NOT EXISTS notice_round ON notice (task_id, round);
-- One finished notice per round, and at most one blocked notice per round that may still be sent.
-- The second holds because only the daemon begins an episode, ending the earlier one first, and it
-- sends a claimed notice within the same pass, while a stopped daemon's 'sending' rows become
-- 'unknown' when the next one starts: no blocked notice is 'sending' when an episode begins.
CREATE UNIQUE INDEX IF NOT EXISTS notice_finished ON notice (task_id, round) WHERE kind = 'finished';
CREATE UNIQUE INDEX IF NOT EXISTS notice_blocked_unsent ON notice (task_id, round)
  WHERE kind = 'blocked' AND state IN ('pending', 'sending');

-- This database's identity, one row. `source_id` is minted when the database is created or
-- upgraded to version 10 and never changes. `export` gives it in its header, so that a reader of
-- several databases' exports can tell their entities apart: `epoch.id` counts from 1 in each.
CREATE TABLE IF NOT EXISTS store (
  id        INTEGER PRIMARY KEY CHECK (id = 1),
  source_id TEXT    NOT NULL
) STRICT;

-- What the retention sweep removed, kept as it stood: one row per task, restore, or epoch, inserted
-- in the transaction that deletes the entity's rows and never updated or deleted, which `export`
-- relies on. `entity_id` is the entity's `id`, an epoch's as decimal text. `version` is the
-- `user_version` whose tables `data` describes; an upgrade leaves `data` as it is. `data` is an
-- object keyed by table name: the entity's own row and, for a task, its `task_round`,
-- `blocked_episode`, `request`, and `notice` rows as arrays in primary-key order. A row is an
-- object of every column by name, each value as the column holds it. `data` is last: the one large
-- column, so reads of the others never cross its overflow pages.
CREATE TABLE IF NOT EXISTS archive (
  entity_type TEXT    NOT NULL CHECK (entity_type IN ('epoch', 'restore', 'task')),
  entity_id   TEXT    NOT NULL,
  version     INTEGER NOT NULL,
  archived_at INTEGER NOT NULL,
  data        TEXT    NOT NULL CHECK (json_type(data) = 'object'),
  PRIMARY KEY (entity_type, entity_id)
) STRICT;

-- 'rexu' in ASCII, which Store::open checks before it uses a database.
PRAGMA application_id = 0x72657875;
PRAGMA user_version = 10;
