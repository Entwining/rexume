//! Registering a worker pane for a task, and each round after the first.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use super::{LATEST_ROUND, caller, finished, owned, round_state, shown};
use crate::runtime::Runtime;
use crate::store::{Store, ensure_lineage, live_of, new_id, now_ms};

pub struct NewTask<'a> {
    pub worker: &'a str,
    pub title: &'a str,
    pub result_file: Option<&'a Path>,
    pub keep_pane: bool,
}

pub fn add(state_dir: &Path, new: &NewTask) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let mut caller = caller(&store)?;
    let session = caller
        .rex
        .block_session(new.worker)?
        .with_context(|| format!("pane {} is not on the server", new.worker))?;
    let process = caller.rex.block_method(&session, new.worker, "process")?;
    let foreground = process.pointer("/foreground/pid").and_then(Value::as_i64);
    // Rex reports only the exit of the program the pane started, so an exit already recorded
    // would be taken for this run's.
    if new.result_file.is_some()
        && foreground.is_none()
        && process.get("last_exit").is_some_and(|exit| !exit.is_null())
    {
        bail!("the program in pane {} has already exited", new.worker);
    }
    let id = new_id();
    let now = now_ms();
    let tx = store.write()?;
    let worker = ensure_lineage(&tx, &caller.instance, new.worker)?;
    if worker == caller.lineage {
        bail!(
            "pane {} is the orchestrator's own pane; pass the worker's block ID",
            caller.block
        );
    }
    refuse_closing(&tx, &worker, new.worker)?;
    let bound = match foreground {
        Some(pid) if new.result_file.is_none() => occupant_of(&tx, &worker, pid)?,
        _ => None,
    };
    tx.execute(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, worker_pid,
           result_file, keep_pane, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            new.title,
            caller.runtime.as_str(),
            caller.native_id,
            worker,
            foreground,
            new.result_file.map(|f| f.to_string_lossy().into_owned()),
            new.keep_pane,
            now,
        ],
    )?;
    insert_round(&tx, &id, 1, bound.as_ref(), 0, now)?;
    tx.commit()?;
    shown(store.conn(), &caller.instance, &id, None)
}

/// The conversation whose process `pid` last reported from the pane, which a round follows.
fn occupant_of(conn: &Connection, lineage: &str, pid: i64) -> Result<Option<(Runtime, String)>> {
    Ok(conn
        .query_row(
            "SELECT runtime, native_id FROM occupant WHERE block_lineage = ?1 AND pid = ?2",
            params![lineage, pid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// The daemon holds the pane for one close attempt; a task registered meanwhile would lose it.
fn refuse_closing(conn: &Connection, lineage: &str, block: &str) -> Result<()> {
    let closing: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM task WHERE worker_lineage = ?1 AND pane_state = 'closing')",
        params![lineage],
        |row| row.get(0),
    )?;
    if closing {
        bail!("rexume is closing pane {block}");
    }
    Ok(())
}

fn insert_round(
    tx: &Transaction,
    id: &str,
    round: i64,
    bound: Option<&(Runtime, String)>,
    cursor: i64,
    now: i64,
) -> Result<()> {
    let (runtime, native_id) = bound.map(|(r, n)| (r.as_str(), n.as_str())).unzip();
    tx.execute(
        "INSERT INTO task_round (task_id, round, worker_runtime, worker_native_id, cursor, state,
           created_at) VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6)",
        params![id, round, runtime, native_id, cursor, now],
    )?;
    Ok(())
}

/// Starts the next round with the same worker pane: only a turn the worker finishes after now
/// counts.
pub fn rearm(state_dir: &Path, id: &str) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let mut caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    let worker: String = store.conn().query_row(
        "SELECT worker_lineage FROM task WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    let block = live_of(store.conn(), &caller.instance, &worker)?
        .with_context(|| format!("the worker pane of task {id} is not on the server"))?;
    let session = caller
        .rex
        .block_session(&block)?
        .with_context(|| format!("the worker pane of task {id} is not on the server"))?;
    let process = caller.rex.block_method(&session, &block, "process")?;
    let foreground = process.pointer("/foreground/pid").and_then(Value::as_i64);
    let conn = store.conn();
    // A round finishing after the length below is read would leave its turn after the cursor.
    latest_finished(conn, id)?;
    let bound = foreground.map_or(Ok(None), |pid| occupant_of(conn, &worker, pid))?;
    // The round starts from the record's length now, read outside the transaction: the bound
    // conversation's, or the previous round's while the worker has not reported yet.
    let transcript: Option<String> = conn
        .query_row(
            &format!(
                "SELECT c.transcript FROM task t JOIN task_round r ON r.task_id = t.id
                 JOIN conversation c ON c.runtime = COALESCE(?2, r.worker_runtime)
                   AND c.native_id = COALESCE(?3, r.worker_native_id)
                 WHERE t.id = ?1 AND {LATEST_ROUND}"
            ),
            params![
                id,
                bound.as_ref().map(|(r, _)| r.as_str()),
                bound.as_ref().map(|(_, n)| n.as_str())
            ],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let cursor = transcript
        .and_then(|t| std::fs::metadata(t).ok())
        .map_or(0, |m| m.len());
    let tx = store.write()?;
    let round = start_round(&tx, id, bound.as_ref(), i64::try_from(cursor)?)?;
    tx.commit()?;
    shown(store.conn(), &caller.instance, id, Some(round))
}

/// Inserts the task's next round after checking it can take one, and returns its number.
pub(super) fn start_round(
    tx: &Transaction,
    id: &str,
    bound: Option<&(Runtime, String)>,
    cursor: i64,
) -> Result<i64> {
    // Another task on the same pane may hold it for a close.
    let (interactive, closed, closing): (bool, bool, bool) = tx.query_row(
        "SELECT t.result_file IS NULL, t.pane_state = 'closed',
           EXISTS (SELECT 1 FROM task o WHERE o.worker_lineage = t.worker_lineage
                   AND o.pane_state = 'closing')
         FROM task t WHERE t.id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if !interactive {
        bail!("task {id} is non_interactive and cannot be rearmed");
    }
    if closed {
        bail!("the worker pane of task {id} is closed, so it cannot be rearmed");
    }
    if closing {
        bail!("rexume is closing the worker pane of task {id}, so it cannot be rearmed");
    }
    let round = latest_finished(tx, id)?;
    insert_round(tx, id, round + 1, bound, cursor, now_ms())?;
    Ok(round + 1)
}

/// The task's latest round, which must have finished before another starts.
fn latest_finished(conn: &Connection, id: &str) -> Result<i64> {
    let (round, state) = round_state(conn, id, None)?;
    if !finished(&state) {
        bail!("task {id} is {state}; rearm it once the round has finished");
    }
    Ok(round)
}
