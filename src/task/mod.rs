mod acknowledge;
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
mod snapshot;
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

pub use acknowledge::{abandon, ack, release};
pub use blocked::{EndReason, end_blocked_episodes};
pub use cleanup::{close_unfocused, held, hold, keep, release_held, retired_workers};
pub use evidence::is_run;
pub use follow::{Follow, Interest};
pub use gone::{block_gone, end_left_to_server};
pub use notice::{orphaned_sends, undelivered};
pub use register::{MESSAGE_LIMIT, NewTask, add, next, refuse_too_large, send};
pub use snapshot::sweep as sweep_snapshots;
pub use wait::{Returned, wait};

pub const VERSION: u32 = 1;
static CURRENT_ROUND: LazyLock<String> = LazyLock::new(|| {
    format!(
        "COALESCE(
           (SELECT r.round FROM task_round r WHERE r.task_id = t.id AND {OPEN}),
           (SELECT MAX(r.round) FROM task_round r WHERE r.task_id = t.id AND r.state != 'dropped'))"
    )
});
const OPEN: &str = "r.state IN ('running', 'blocked')";

const COLUMNS: &str = "t.id, t.title, r.round, r.state, t.result_file IS NULL, t.keep_pane,
  l.live_id, r.result_ref, r.note, r.finished_at, r.acked_at, t.pane_state, t.pane_closed_at,
  t.pane_note, t.released_at, q.id, q.state, q.note, e.started_at, e.kind, e.message";

static ROUND_REQUEST: LazyLock<String> = LazyLock::new(|| {
    format!(
        "LEFT JOIN request q ON q.task_id = r.task_id AND q.round = r.round
         LEFT JOIN blocked_episode e ON e.task_id = r.task_id AND e.round = r.round
           AND {}",
        blocked::OPEN_EPISODE
    )
});

const LIVE_WORKER: &str = "LEFT JOIN live l ON l.instance_id = ?1 AND l.lineage = t.worker_lineage
  AND t.pane_state != 'closed'";

#[derive(Serialize)]
struct Task {
    id: String,
    title: String,
    round: i64,
    state: String,
    mode: &'static str,
    keep_pane: bool,
    worker: Option<String>,
    result_ref: Option<String>,
    note: Option<String>,
    finished_at: Option<i64>,
    acked_at: Option<i64>,
    released_at: Option<i64>,
    unread: Option<usize>,
    queued: Vec<i64>,
    blocked: Option<Blocked>,
    request: Option<Request>,
    pane: Pane,
    snapshot: snapshot::Snapshots,
}

#[derive(Serialize)]
struct Blocked {
    kind: Option<String>,
    message: Option<String>,
    started_at: i64,
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
            blocked: row
                .get::<_, Option<i64>>(18)?
                .map(|started_at| -> rusqlite::Result<Blocked> {
                    Ok(Blocked {
                        kind: row.get(19)?,
                        message: row.get(20)?,
                        started_at,
                    })
                })
                .transpose()?,
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
            snapshot: snapshot::Snapshots::default(),
        })
    }
}

fn unread(conn: &Connection, state_dir: &Path, id: &str) -> Result<Option<usize>> {
    let found: Option<(Runtime, String, Option<String>, i64)> = conn
        .query_row(
            "SELECT t.worker_runtime, t.worker_native_id, c.transcript, t.position FROM task t
             JOIN conversation c ON c.runtime = t.worker_runtime AND c.native_id = t.worker_native_id
             WHERE t.id = ?1 AND t.result_file IS NULL AND t.released_at IS NULL",
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
             {request}
             WHERE t.owner_runtime = ?2 AND t.owner_native_id = ?3 AND r.round = {current}
             ORDER BY t.created_at",
            request = *ROUND_REQUEST,
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
        task.snapshot = snapshot::shown(store.conn(), store.dir(), &task.id, task.round)?;
    }
    let notices = notice::undelivered(store.conn(), Some((caller.runtime, &caller.native_id)))?;
    Ok(json!({ "version": VERSION, "tasks": tasks, "undelivered_notices": notices }))
}

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
                 {LIVE_WORKER} {request} WHERE t.id = ?2
                   AND r.round = COALESCE(?3, {current})",
                request = *ROUND_REQUEST,
                current = *CURRENT_ROUND
            ),
            params![instance, id, round],
            |row| {
                Ok(Shown {
                    version: VERSION,
                    task: Task::from_row(row)?,
                    result: row.get(21)?,
                })
            },
        )
        .optional()?
        .with_context(|| format!("task {id} has no round {}", round.unwrap_or_default()))?;
    found.task.unread = unread(conn, state_dir, id)?;
    found.task.queued = queue::rounds(conn, id)?;
    found.task.snapshot = snapshot::shown(conn, state_dir, id, found.task.round)?;
    Ok(serde_json::to_value(found)?)
}

pub fn show(state_dir: &Path, id: &str, round: Option<i64>) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    snapshot::after(&mut store, id)?;
    shown(store.conn(), store.dir(), &caller.instance, id, round)
}

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
