//! Worker tasks: work an orchestrator's conversation hands to a worker pane. A round finishes only
//! when the worker's result is saved here, its owner is told through one path, and the daemon,
//! the only closer, closes a disposable worker's pane only while nothing has happened in it since
//! its result. These are the commands the orchestrator runs; `follow` is the daemon's side.

mod cleanup;
mod evidence;
mod follow;
mod notice;
mod outcome;
mod register;
#[cfg(test)]
mod tests;
mod wait;

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::rex::{Cli, Rex, require_str};
use crate::runtime::Runtime;
use crate::store::{Store, lineage_of, now_ms};

pub use cleanup::retired_workers;
pub use follow::{Follow, Interest};
pub use notice::{orphaned_sends, undelivered};
pub use outcome::block_gone;
pub use register::{NewTask, add, rearm};
pub use wait::wait;

pub const VERSION: u32 = 1;
/// Selects a task's latest round as `r` for a task joined as `t`.
const LATEST_ROUND: &str = "r.round = (SELECT MAX(round) FROM task_round WHERE task_id = t.id)";
/// A round of `r` that has not finished: the predicate of the partial index `task_round_open`,
/// written once so that every query matches it.
const OPEN: &str = "r.state IN ('running', 'blocked')";

struct Caller {
    rex: Rex,
    instance: String,
    block: String,
    lineage: String,
    runtime: Runtime,
    native_id: String,
}

/// The conversation in the calling pane. Inside an agent's sandbox the process tree cannot be
/// read, so the pane's recorded conversation is the caller.
fn caller(store: &Store) -> Result<Caller> {
    let block = std::env::var_os("REX_BLOCK")
        .context("REX_BLOCK is not set; run this from the orchestrator's own Rex pane")?
        .to_string_lossy()
        .into_owned();
    let mut rex = Rex::Cli(Cli::from_env()?);
    let instance = require_str(&rex.call("server.status", &json!({}))?, "/instance_id")?;
    let conn = store.conn();
    let recorded = match lineage_of(conn, &instance, &block)? {
        Some(lineage) => conn
            .query_row(
                "SELECT runtime, native_id FROM occupant WHERE block_lineage = ?1",
                params![lineage],
                |row| Ok((lineage.clone(), row.get(0)?, row.get(1)?)),
            )
            .optional()?,
        None => None,
    };
    let (lineage, runtime, native_id) =
        recorded.with_context(|| format!("no conversation recorded for pane {block}"))?;
    Ok(Caller {
        rex,
        instance,
        block,
        lineage,
        runtime,
        native_id,
    })
}

const COLUMNS: &str = "t.id, t.title, r.round, r.state, t.result_file IS NULL, t.keep_pane,
  l.live_id, r.result_ref, r.note, r.finished_at, r.acked_at, t.pane_state, t.pane_closed_at,
  t.pane_note";

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
    pane: Pane,
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
            pane: Pane {
                state: row.get(11)?,
                closed_at: row.get(12)?,
                note: row.get(13)?,
            },
        })
    }
}

/// A task with one round's result, as `show`, `wait`, `ack`, `add`, and `rearm` print it.
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
    let tasks: Vec<Task> = store
        .conn()
        .prepare(&format!(
            "SELECT {COLUMNS} FROM task t JOIN task_round r ON r.task_id = t.id {LIVE_WORKER}
             WHERE t.owner_runtime = ?2 AND t.owner_native_id = ?3 AND {LATEST_ROUND}
             ORDER BY t.created_at"
        ))?
        .query_map(
            params![caller.instance, caller.runtime.as_str(), caller.native_id],
            Task::from_row,
        )?
        .collect::<rusqlite::Result<_>>()?;
    let notices = notice::undelivered(store.conn(), Some((caller.runtime, &caller.native_id)))?;
    Ok(json!({ "version": VERSION, "tasks": tasks, "undelivered_notices": notices }))
}

fn owned(conn: &Connection, id: &str, caller: &Caller) -> Result<()> {
    let owner: Option<(Runtime, String)> = conn
        .query_row(
            "SELECT owner_runtime, owner_native_id FROM task WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match owner {
        None => bail!("no task {id}"),
        Some((runtime, native_id))
            if runtime != caller.runtime || native_id != caller.native_id =>
        {
            bail!("task {id} belongs to another conversation")
        }
        Some(_) => Ok(()),
    }
}

/// The TASK object for one round, the latest when `round` is `None`, with its result.
fn shown(conn: &Connection, instance: &str, id: &str, round: Option<i64>) -> Result<Value> {
    let found = conn
        .query_row(
            &format!(
                "SELECT {COLUMNS}, r.result FROM task t JOIN task_round r ON r.task_id = t.id
                 {LIVE_WORKER} WHERE t.id = ?2
                   AND r.round = COALESCE(?3, (SELECT MAX(round) FROM task_round WHERE task_id = t.id))"
            ),
            params![instance, id, round],
            |row| {
                Ok(Shown {
                    version: VERSION,
                    task: Task::from_row(row)?,
                    result: row.get(14)?,
                })
            },
        )
        .optional()?
        .with_context(|| format!("task {id} has no round {}", round.unwrap_or_default()))?;
    Ok(serde_json::to_value(found)?)
}

pub fn show(state_dir: &Path, id: &str, round: Option<i64>) -> Result<Value> {
    let store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    shown(store.conn(), &caller.instance, id, round)
}

/// A round's number and state: the given one, or the latest.
fn round_state(conn: &Connection, id: &str, round: Option<i64>) -> Result<(i64, String)> {
    conn.query_row(
        "SELECT round, state FROM task_round WHERE task_id = ?1
           AND round = COALESCE(?2, (SELECT MAX(round) FROM task_round WHERE task_id = ?1))",
        params![id, round],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()?
    .with_context(|| format!("task {id} has no round {}", round.unwrap_or_default()))
}

fn finished(state: &str) -> bool {
    !matches!(state, "running" | "blocked")
}

/// Records that the owner has a finished round's result. The daemon closes a disposable worker's
/// pane afterwards, when that is still safe.
pub fn ack(state_dir: &Path, id: &str, round: Option<i64>) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    let tx = store.write()?;
    owned(&tx, id, &caller)?;
    let (round, state) = round_state(&tx, id, round)?;
    if !finished(&state) {
        bail!("task {id} is {state}; ack it once the round has finished");
    }
    tx.execute(
        "UPDATE task_round SET acked_at = COALESCE(acked_at, ?1) WHERE task_id = ?2 AND round = ?3",
        params![now_ms(), id, round],
    )?;
    // The owner has what a notice not yet sent would have told it.
    tx.execute(
        "UPDATE notice SET state = 'superseded', note = 'taken by task ack'
         WHERE task_id = ?1 AND round = ?2 AND state = 'pending'",
        params![id, round],
    )?;
    tx.commit()?;
    shown(store.conn(), &caller.instance, id, Some(round))
}

/// The tasks a conversation owns whose latest round it has not acknowledged, one line each, for
/// the prompt that continues it.
pub fn unacknowledged(conn: &Connection, runtime: Runtime, native_id: &str) -> Result<Vec<String>> {
    Ok(conn
        .prepare(&format!(
            "SELECT t.id, t.title, r.round, r.state FROM task t JOIN task_round r ON r.task_id = t.id
             WHERE t.owner_runtime = ?1 AND t.owner_native_id = ?2 AND {LATEST_ROUND}
               AND r.acked_at IS NULL ORDER BY t.created_at"
        ))?
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
