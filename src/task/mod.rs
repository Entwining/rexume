//! Worker tasks: work an orchestrator's conversation hands to a worker pane. A round finishes only
//! when the worker's result is saved here, its owner is told through one path, and the daemon,
//! the only closer, closes a disposable worker's pane only while nothing has happened in it since
//! its result. These are the commands the orchestrator runs; `follow` is the daemon's side.

mod blocked;
mod caller;
mod cleanup;
mod evidence;
mod follow;
mod gone;
mod notice;
mod outcome;
mod queue;
mod register;
mod request;
#[cfg(test)]
pub(crate) mod tests;
mod wait;

use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::runtime::{Runtime, Tracker};
use crate::store::Store;

use caller::{caller, owned};

pub use blocked::end_blocked_episodes;
pub use cleanup::{close_unfocused, held, hold, keep, release_held, retired_workers};
pub use evidence::is_run;
pub use follow::{Follow, Interest};
pub use gone::{block_gone, end_left_to_server};
pub use notice::{orphaned_sends, undelivered};
pub use register::{
    MESSAGE_LIMIT, NewTask, abandon, ack, add, next, refuse_too_large, release, send,
};
pub use wait::{Returned, wait};

pub const VERSION: u32 = 1;
/// The round the commands take by default of a task joined as `t`: its open round, or else its
/// latest that ran. A round is queued only behind an open one, so this is never a queued round,
/// which the commands take only by number, nor a dropped one, which never ran.
static CURRENT_ROUND: LazyLock<String> = LazyLock::new(|| {
    format!(
        "COALESCE(
           (SELECT r.round FROM task_round r WHERE r.task_id = t.id AND {OPEN}),
           (SELECT MAX(r.round) FROM task_round r WHERE r.task_id = t.id AND r.state != 'dropped'))"
    )
});
/// A round of `r` that has started and not finished: the predicate of the partial index
/// `task_round_open`, written once so that every query matches it.
const OPEN: &str = "r.state IN ('running', 'blocked')";

const COLUMNS: &str = "t.id, t.title, r.round, r.state, t.result_file IS NULL, t.keep_pane,
  l.live_id, r.result_ref, r.note, r.finished_at, r.acked_at, t.pane_state, t.pane_closed_at,
  t.pane_note, t.released_at, q.id, q.state, q.note";

/// Joins the request of a round joined as `r` as `q`.
const ROUND_REQUEST: &str = "LEFT JOIN request q ON q.task_id = r.task_id AND q.round = r.round";

/// Joins the worker pane's current Rex block ID as `l`, on the instance bound as `?1`. A closed
/// pane keeps its `live` row until the next server instance, so it is excluded here.
const LIVE_WORKER: &str = "LEFT JOIN live l ON l.instance_id = ?1 AND l.lineage = t.worker_lineage
  AND t.pane_state != 'closed'";

/// One round of a task as the commands print it (TASK in `docs/cli.md`).
#[derive(Serialize)]
struct Task {
    id: String,
    title: String,
    round: i64,
    state: String,
    mode: &'static str,
    keep_pane: bool,
    /// The worker pane's current Rex block ID, or `None` once it is not on the server.
    worker: Option<String>,
    result_ref: Option<String>,
    note: Option<String>,
    finished_at: Option<i64>,
    acked_at: Option<i64>,
    released_at: Option<i64>,
    /// Filled in after the row is read, from the worker's record.
    unread: Option<usize>,
    /// The task's queued rounds in order, filled in after the row is read.
    queued: Vec<i64>,
    request: Option<Request>,
    pane: Pane,
}

#[derive(Serialize)]
struct Request {
    id: String,
    state: String,
    note: Option<String>,
}

#[derive(Serialize)]
struct Pane {
    state: String,
    closed_at: Option<i64>,
    note: Option<String>,
}

impl Task {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        let interactive: bool = row.get(4)?;
        Ok(Self {
            id: row.get(0)?,
            title: row.get(1)?,
            round: row.get(2)?,
            state: row.get(3)?,
            mode: if interactive {
                "interactive"
            } else {
                "non_interactive"
            },
            keep_pane: row.get(5)?,
            worker: row.get(6)?,
            result_ref: row.get(7)?,
            note: row.get(8)?,
            finished_at: row.get(9)?,
            acked_at: row.get(10)?,
            released_at: row.get(14)?,
            unread: None,
            queued: Vec::new(),
            request: row
                .get::<_, Option<String>>(15)?
                .map(|id| -> rusqlite::Result<Request> {
                    Ok(Request {
                        id,
                        state: row.get(16)?,
                        note: row.get(17)?,
                    })
                })
                .transpose()?,
            pane: Pane {
                state: row.get(11)?,
                closed_at: row.get(12)?,
                note: row.get(13)?,
            },
        })
    }
}

/// How many finished turns in an interactive worker's record no round took, as `task next` would
/// take them; `None` for a run, a released task, a worker not bound yet, and a record that cannot
/// be read or was replaced. Reads the record, so it runs outside any transaction.
fn unread(conn: &Connection, state_dir: &Path, id: &str) -> Result<Option<usize>> {
    let found: Option<(Runtime, String, Option<String>, i64)> = conn
        .query_row(
            "SELECT t.worker_runtime, t.worker_native_id, c.transcript, t.position FROM task t
             JOIN conversation c ON c.runtime = t.worker_runtime AND c.native_id = t.worker_native_id
             WHERE t.id = ?1 AND t.released_at IS NULL",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((runtime, native_id, transcript, position)) = found else {
        return Ok(None);
    };
    let Some(path) = runtime.record(state_dir, &native_id, transcript.as_deref()) else {
        return Ok(None);
    };
    let position = u64::try_from(position)?;
    let taken = follow::taken(conn, id)?;
    let mut tracker = Tracker::new(runtime, &path, position, taken, None);
    if let Err(e) = tracker.update() {
        log::debug!("task {id}: the worker's record could not be read: {e:#}");
        return Ok(None);
    }
    Ok((!tracker.replaced()).then(|| tracker.unread()))
}

/// A task with one round's result, as every task command but `list` prints it.
#[derive(Serialize)]
struct Shown {
    version: u32,
    #[serde(flatten)]
    task: Task,
    result: Option<String>,
}

pub fn list(state_dir: &Path) -> Result<Value> {
    let store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    let mut tasks: Vec<Task> = store
        .conn()
        .prepare(&format!(
            "SELECT {COLUMNS} FROM task t JOIN task_round r ON r.task_id = t.id {LIVE_WORKER}
             {ROUND_REQUEST}
             WHERE t.owner_runtime = ?2 AND t.owner_native_id = ?3 AND r.round = {current}
             ORDER BY t.created_at",
            current = *CURRENT_ROUND
        ))?
        .query_map(
            params![caller.instance, caller.runtime.as_str(), caller.native_id],
            Task::from_row,
        )?
        .collect::<rusqlite::Result<_>>()?;
    for task in &mut tasks {
        task.unread = unread(store.conn(), store.dir(), &task.id)?;
        task.queued = queue::rounds(store.conn(), &task.id)?;
    }
    let notices = notice::undelivered(store.conn(), Some((caller.runtime, &caller.native_id)))?;
    Ok(json!({ "version": VERSION, "tasks": tasks, "undelivered_notices": notices }))
}

/// The TASK object for one round, the open or latest one when `round` is `None`, with its result.
fn shown(
    conn: &Connection,
    state_dir: &Path,
    instance: &str,
    id: &str,
    round: Option<i64>,
) -> Result<Value> {
    let mut found = conn
        .query_row(
            &format!(
                "SELECT {COLUMNS}, r.result FROM task t JOIN task_round r ON r.task_id = t.id
                 {LIVE_WORKER} {ROUND_REQUEST} WHERE t.id = ?2
                   AND r.round = COALESCE(?3, {current})",
                current = *CURRENT_ROUND
            ),
            params![instance, id, round],
            |row| {
                Ok(Shown {
                    version: VERSION,
                    task: Task::from_row(row)?,
                    result: row.get(18)?,
                })
            },
        )
        .optional()?
        .with_context(|| format!("task {id} has no round {}", round.unwrap_or_default()))?;
    found.task.unread = unread(conn, state_dir, id)?;
    found.task.queued = queue::rounds(conn, id)?;
    Ok(serde_json::to_value(found)?)
}

pub fn show(state_dir: &Path, id: &str, round: Option<i64>) -> Result<Value> {
    let store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    shown(store.conn(), store.dir(), &caller.instance, id, round)
}

/// A round's number and state: the given one, or `CURRENT_ROUND`.
fn round_state(conn: &Connection, id: &str, round: Option<i64>) -> Result<(i64, String)> {
    conn.query_row(
        &format!(
            "SELECT r.round, r.state FROM task t JOIN task_round r ON r.task_id = t.id
             WHERE t.id = ?1 AND r.round = COALESCE(?2, {current})",
            current = *CURRENT_ROUND
        ),
        params![id, round],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()?
    .with_context(|| format!("task {id} has no round {}", round.unwrap_or_default()))
}

fn finished(state: &str) -> bool {
    !matches!(state, "queued" | "running" | "blocked")
}

/// The rounds of the tasks a conversation owns that it has not acknowledged, queued ones included,
/// one line each, for the prompt that continues it. A queued round starts without acknowledging the
/// round before it, so that round can wait for its acknowledgement behind later ones.
pub fn unacknowledged(conn: &Connection, runtime: Runtime, native_id: &str) -> Result<Vec<String>> {
    Ok(conn
        .prepare(
            "SELECT t.id, t.title, r.round, r.state FROM task t JOIN task_round r ON r.task_id = t.id
             WHERE t.owner_runtime = ?1 AND t.owner_native_id = ?2 AND r.acked_at IS NULL
             ORDER BY t.created_at, r.round",
        )?
        .query_map(params![runtime.as_str(), native_id], |row| {
            Ok(format!(
                "{} \"{}\": round {} is {}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?
            ))
        })?
        .collect::<rusqlite::Result<_>>()?)
}
