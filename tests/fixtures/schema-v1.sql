-- rexume state. One SQLite database (state.db) shared by the daemon and the short-lived
-- commands; Store::open sets the connection pragmas and applies this file to a new database.
-- Conventions: every table is STRICT; every closed set of states is a CHECK, and a state the
-- code adds is added to the CHECK, never loosened; every time is Unix milliseconds; an ID this
-- tool mints is UUIDv7 text; a lineage is the Rex ID a resource had when first seen. A JSON
-- column is read and written whole by one module, whose serde types are its shape; its validity
-- CHECK is a column constraint so that the column can still be dropped. Table comments are the
-- only place a column's meaning lives.

-- One observation of one server instance, from a confirmed subscription until it ended.
-- 'server_gone': the connection ended and the server then no longer answered as the same
-- instance, so the last observed states are what the server held when it stopped.
-- 'disconnected': only the connection ended, or a daemon died while observing.
-- AUTOINCREMENT: readers take the newest epoch as MAX(id), so an id is never reused.
CREATE TABLE epoch (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  instance_id TEXT    NOT NULL,
  started_at  INTEGER NOT NULL,
  ended_at    INTEGER,
  end_reason  TEXT    CHECK (end_reason IN ('disconnected', 'server_gone')),
  CHECK ((ended_at IS NULL) = (end_reason IS NULL))
) STRICT;
CREATE INDEX epoch_instance ON epoch (instance_id, id);

-- The layout to restore: one document per session, keyed by the session's lineage, read and
-- written whole. Only the layout module and restore/create.rs parse `doc`, whose shape is their
-- serde types; store::sweep finds the pane lineages in it by the key `lineage`. `instance_id` is
-- where the layout was last saved: a session saved on an earlier instance and not yet restored
-- on the current one is what a restore plans from. `position` is the session's place in the
-- server's list, the order restores are planned in; `updated_at` is when the document was last
-- written.
CREATE TABLE session (
  lineage     TEXT    PRIMARY KEY,
  instance_id TEXT    NOT NULL,
  position    INTEGER NOT NULL,
  doc         TEXT    NOT NULL CHECK (json_valid(doc)),
  updated_at  INTEGER NOT NULL
) STRICT;

-- What the server last said about each pane: a cache of the live server while its instance
-- runs, and after 'server_gone' the only record of how the pane stood when the server stopped
-- (cwd and size for the restore, foreground and status for the continue decision, status time
-- and generation for the cleanup). `instance_id` is the server instance the row describes; a
-- row on the current instance was seen in that instance's open epoch. A row that moves to a new
-- instance keeps its cwd and size until the new instance reports them, while its foreground,
-- status, and exit, which describe processes of one instance, start empty. The row goes when the
-- pane closes. Program states are Rex's vocabulary and are not constrained here. `last_exit`
-- is the exit code of the program the terminal started (Rex's child_exited), which a pane
-- reports once; it is attributed to a task by the pane having that program as its foreground
-- when the task was registered.
CREATE TABLE block (
  lineage       TEXT    PRIMARY KEY,
  instance_id   TEXT    NOT NULL,
  cwd           TEXT,
  columns       INTEGER,
  rows          INTEGER,
  fg_pid        INTEGER,
  fg_generation INTEGER,
  status        TEXT,
  status_pid    INTEGER,
  status_name   TEXT,
  status_at     INTEGER,
  last_exit     INTEGER
) STRICT;

-- Which live Rex ID stands for which lineage on one server instance. A restored session,
-- window, or block is bound to its old lineage from a create or layer reply, by the restore
-- adopting a session whose create reply was lost, or by its gate, which binds its own block and
-- session when it claims its slot. When a reply is lost, the daemon can see the resource first
-- and bind it to its own ID; binding it to its old lineage replaces that binding and moves what
-- was recorded under it (layout::bind_restored).
-- One lineage per live ID and one live ID per lineage on an instance; a binding that would
-- break that is an error, not a silent second row.
CREATE TABLE live (
  instance_id TEXT NOT NULL,
  live_id     TEXT NOT NULL,
  lineage     TEXT NOT NULL,
  PRIMARY KEY (instance_id, live_id)
) STRICT, WITHOUT ROWID;
CREATE UNIQUE INDEX live_lineage ON live (instance_id, lineage);

-- A runtime's own conversation and what reopens it. `runtime` has no CHECK: the set of
-- runtimes grows in code (enum Runtime), which refuses an unknown name before writing.
-- `settings` is a JSON object owned by the runtime's module (model, effort, provider, ...,
-- whatever its resume command needs); a later report merges into it (json_patch), so a value
-- a resumed session's start omits is kept. `transcript` is the runtime's record of the
-- conversation, which a later report without one leaves in place. `resumable` is 0 for a run
-- its runtime cannot reopen, such as a Pi run outside its terminal interface. `reported_at` is
-- when the conversation was last reported.
CREATE TABLE conversation (
  runtime     TEXT    NOT NULL,
  native_id   TEXT    NOT NULL,
  transcript  TEXT,
  resumable   INTEGER NOT NULL CHECK (resumable IN (0, 1)),
  settings    TEXT    NOT NULL DEFAULT '{}' CHECK (json_type(settings) = 'object'),
  reported_at INTEGER NOT NULL,
  PRIMARY KEY (runtime, native_id)
) STRICT, WITHOUT ROWID;

-- The conversation whose process last reported itself as a pane's foreground program: the
-- pane's current binding, not its history. A report may come before the daemon has seen the
-- pane, so there is no reference to `block`; store::forget_block deletes the occupant with the
-- block. `transport` is a JSON object owned by the runtime's module: how a notice reaches this
-- process (Claude Code's messaging socket; nothing for Codex and Pi). `pid` and `pid_name` are
-- the reporting process and the name Rex reported for it, which tell whether a pane's later
-- status came from the same process. `keeps_session_env`: the process ran with `REX_SESSION`, so
-- its reopened conversation gets it too.
CREATE TABLE occupant (
  block_lineage     TEXT    PRIMARY KEY,
  runtime           TEXT    NOT NULL,
  native_id         TEXT    NOT NULL,
  pid               INTEGER NOT NULL,
  pid_name          TEXT,
  keeps_session_env INTEGER NOT NULL CHECK (keeps_session_env IN (0, 1)),
  transport         TEXT    CHECK (transport IS NULL OR json_type(transport) = 'object'),
  reported_at       INTEGER NOT NULL,
  FOREIGN KEY (runtime, native_id) REFERENCES conversation (runtime, native_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX occupant_conversation ON occupant (runtime, native_id, reported_at);

-- A session recreated on a new server instance. Its panes run `rexume gate`, which claims its
-- slot before starting anything, so an unanswered create is reconciled from the claims and a
-- late gate cannot start a second copy of a conversation. `doc` is the layout as planned (the
-- saved layout without retired workers), in the layout module's shape; the saved layout
-- itself is rewritten as soon as the new instance shows the session, so the restore reads only
-- this copy, and `status` shows its `label`. The created session's live ID is the `live` row
-- for (instance_id, session_lineage), written by the create reply or by the first gate's claim.
-- `created_at` is taken before the planner reads the conversations' records, so a report that
-- arrives while it reads counts as later than the plan.
CREATE TABLE restore_op (
  id              TEXT    PRIMARY KEY,
  instance_id     TEXT    NOT NULL,
  session_lineage TEXT    NOT NULL,
  state           TEXT    NOT NULL
    CHECK (state IN ('planned', 'sent', 'unknown', 'created', 'partial', 'completed', 'failed')),
  doc             TEXT    NOT NULL CHECK (json_valid(doc)),
  note            TEXT,
  created_at      INTEGER NOT NULL
) STRICT;
-- One restore of a session per instance: a second plan conflicts here.
CREATE UNIQUE INDEX restore_op_session ON restore_op (instance_id, session_lineage);

-- One pane of a restore. `plan` holds the launch details the gate consumes whole
-- (restore::Plan). `instance_id` repeats the operation's so that one index can hold the
-- one-executor rule across operations. The pane's live ID is the `live` row for
-- (instance_id, block_lineage), written by the reply or by the gate's claim. `layer_sent_at`:
-- a floating-layer request carrying this slot was sent; until its reply or a gate claim shows
-- the layer it may or may not exist, so it is not requested again. `claimed_at` and `started`
-- are set together, once, by the gate's compare-and-set: `started` is what the gate ran, which
-- is 'shell' instead of `start` when the conversation had reported from another pane meanwhile,
-- and 'agent' instead of 'continue' when the conversation's own record no longer showed the
-- interrupted turn when the gate re-read it.
CREATE TABLE restore_slot (
  op_id         TEXT    NOT NULL REFERENCES restore_op (id) ON DELETE CASCADE,
  slot          INTEGER NOT NULL,
  instance_id   TEXT    NOT NULL,
  block_lineage TEXT    NOT NULL,
  start         TEXT    NOT NULL CHECK (start IN ('shell', 'typed', 'agent', 'continue')),
  runtime       TEXT,
  native_id     TEXT,
  plan          TEXT    NOT NULL CHECK (json_valid(plan)),
  layer_sent_at INTEGER,
  claimed_at    INTEGER,
  started       TEXT    CHECK (started IN ('shell', 'typed', 'agent', 'continue')),
  PRIMARY KEY (op_id, slot),
  CHECK ((runtime IS NULL) = (native_id IS NULL)),
  CHECK (start IN ('shell', 'typed') OR runtime IS NOT NULL),
  CHECK ((started IS NULL) = (claimed_at IS NULL)),
  CHECK (started IS NULL OR started = start OR started = 'shell'
         OR (start = 'continue' AND started = 'agent')),
  FOREIGN KEY (runtime, native_id) REFERENCES conversation (runtime, native_id)
) STRICT;
-- One process per conversation: on one server instance a conversation is started by at most
-- one slot. A second plan for it conflicts here and the planner types the command instead.
-- The foreign key (runtime, native_id) has no index of its own: its parent rows are deleted
-- only by the retention sweep, which filters with NOT EXISTS first.
CREATE UNIQUE INDEX restore_slot_executor ON restore_slot (instance_id, runtime, native_id)
  WHERE start IN ('agent', 'continue');

-- Work an orchestrator's conversation handed to a worker pane: which conversation owns it, which
-- pane does it, and what happens to the pane afterwards. Panes are named by lineage, not
-- referenced: a task outlives its pane's row. A task with `result_file` is a non-interactive run
-- whose result is that file; without one it is an interactive worker whose rounds are answered
-- turns. `worker_pid` is the pane's foreground process when the task was registered, which is
-- the program whose exit ends a non-interactive run. `pane_state`: 'open' while the pane exists
-- and rexume has neither closed nor given it up; 'closing' while the daemon, the only closer,
-- holds the pane for one close attempt (add and rearm refuse it); 'closed' once the pane is
-- gone, by rexume or by the user; 'kept' when the pane was found in use after the result and is
-- left to whoever uses it, `pane_note` saying why. A close the daemon put off writes its reason
-- to `pane_note` and leaves 'open'. A pane the orchestrator asked to keep is never 'closing' or
-- 'kept'.
CREATE TABLE task (
  id              TEXT    PRIMARY KEY,
  title           TEXT    NOT NULL,
  owner_runtime   TEXT    NOT NULL,
  owner_native_id TEXT    NOT NULL,
  worker_lineage  TEXT    NOT NULL,
  worker_pid      INTEGER,
  result_file     TEXT,
  keep_pane       INTEGER NOT NULL CHECK (keep_pane IN (0, 1)),
  pane_state      TEXT    NOT NULL DEFAULT 'open'
    CHECK (pane_state IN ('open', 'closing', 'closed', 'kept')),
  pane_note       TEXT,
  pane_closed_at  INTEGER,
  created_at      INTEGER NOT NULL,
  CHECK ((pane_state = 'closed') = (pane_closed_at IS NOT NULL)),
  CHECK (pane_state != 'kept' OR pane_note IS NOT NULL),
  CHECK (pane_state != 'closed' OR pane_note IS NULL),
  CHECK (pane_state NOT IN ('closing', 'kept') OR keep_pane = 0),
  FOREIGN KEY (owner_runtime, owner_native_id) REFERENCES conversation (runtime, native_id)
) STRICT;
CREATE INDEX task_owner ON task (owner_runtime, owner_native_id);
CREATE INDEX task_worker ON task (worker_lineage);

-- One round of a task: the worker answering one prompt. A new round is a new row and an earlier
-- round keeps its result, so the rounds are the task's history. `worker_runtime` and
-- `worker_native_id` are the conversation whose record the round follows, bound once (at
-- registration when the pane's foreground process had reported, else by the daemon from the
-- first report after it) and never moved to a conversation that takes over the pane later; a
-- non-interactive run has none. `cursor`: byte offset into that record; while the round runs, a
-- turn after it is the round's, and once the round finishes it is the offset after the result's
-- turn, so any later turn is activity in the pane. `waiter_pid`: a `task wait` process that
-- takes the result itself while it runs. A round is 'completed' exactly when its result is saved
-- here, in the transaction that also writes its notice; every final state sets `finished_at`.
-- `finished_status_at` and `finished_generation` are the worker pane's status time and
-- foreground generation when the round finished, to tell later activity in the pane apart.
-- `result_ref` is the record ID of an interactive worker's answer, or a non-interactive run's
-- result file; `note` says why a round ended without a result. `result` is last: the one large
-- column, so reads of the others never cross its overflow pages.
CREATE TABLE task_round (
  task_id             TEXT    NOT NULL REFERENCES task (id) ON DELETE CASCADE,
  round               INTEGER NOT NULL,
  worker_runtime      TEXT,
  worker_native_id    TEXT,
  cursor              INTEGER NOT NULL,
  waiter_pid          INTEGER,
  state               TEXT    NOT NULL CHECK (state IN ('running', 'blocked', 'completed', 'cancelled',
                                                        'failed', 'terminated', 'worker_closed')),
  note                TEXT,
  finished_at         INTEGER,
  finished_status_at  INTEGER,
  finished_generation INTEGER,
  acked_at            INTEGER,
  created_at          INTEGER NOT NULL,
  result_ref          TEXT,
  result              TEXT,
  PRIMARY KEY (task_id, round),
  CHECK ((worker_runtime IS NULL) = (worker_native_id IS NULL)),
  CHECK ((state IN ('running', 'blocked')) = (finished_at IS NULL)),
  CHECK ((state = 'completed') = (result IS NOT NULL)),
  CHECK ((state = 'completed') = (result_ref IS NOT NULL)),
  CHECK (acked_at IS NULL OR finished_at IS NOT NULL),
  FOREIGN KEY (worker_runtime, worker_native_id) REFERENCES conversation (runtime, native_id)
) STRICT;
-- A task has at most one round that has not finished.
CREATE UNIQUE INDEX task_round_open ON task_round (task_id) WHERE state IN ('running', 'blocked');
CREATE INDEX task_round_worker ON task_round (worker_runtime, worker_native_id);

-- The notice that tells the owner its round finished, written in the transaction that finishes
-- the round; its kind is the round's final state and its time is the round's `finished_at`.
-- Exactly one claimant, the daemon or the owner's own `task wait` or `task ack`, moves it out of
-- 'pending'. 'sending' is the running daemon's claim and every state after it is the daemon's.
-- 'unknown': the send may have happened, so only the owner's own record settles it, never a
-- resend. 'superseded': the owner had the result before anything was sent, through `task wait`
-- or `task ack` (`note` says which). A 'pending' notice with a `note` is one whose send failed
-- before anything left. `sent_at` is when the daemon last handed it over, and `delivered_at`
-- when the owner's record showed it. `id` is the token the notice text carries and the owner's
-- record is searched for.
CREATE TABLE notice (
  id           TEXT    PRIMARY KEY,
  task_id      TEXT    NOT NULL,
  round        INTEGER NOT NULL,
  state        TEXT    NOT NULL CHECK (state IN ('pending', 'sending', 'sent', 'accepted', 'delivered',
                                                 'unknown', 'superseded')),
  sent_at      INTEGER,
  delivered_at INTEGER,
  note         TEXT,
  UNIQUE (task_id, round),
  CHECK ((state = 'delivered') = (delivered_at IS NOT NULL)),
  FOREIGN KEY (task_id, round) REFERENCES task_round (task_id, round) ON DELETE CASCADE
) STRICT;

-- 'rexu' in ASCII, which Store::open checks before it uses a database.
PRAGMA application_id = 0x72657875;
PRAGMA user_version = 1;
