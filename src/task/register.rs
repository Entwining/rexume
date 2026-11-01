//! Registering a worker pane for a task, each round after the first, abandoning a round, and
//! releasing the worker.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use super::{caller, finished, notice, outcome, owned, round_state, shown, unread};
use crate::runtime::Runtime;
use crate::store::{Store, ensure_lineage, new_id, now_ms};

pub struct NewTask<'a> {
    pub worker: &'a str,
    pub title: &'a str,
    pub result_file: Option<&'a Path>,
    pub keep_pane: bool,
}

pub fn add(state_dir: &Path, new: &NewTask) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let asked_at = now_ms();
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
    refuse_stopped(&tx, &caller.instance, asked_at)?;
    let bound = match foreground {
        Some(pid) if new.result_file.is_none() => occupant_of(&tx, &worker, &caller.instance, pid)?,
        _ => None,
    };
    let (runtime, native_id, position) = match bound {
        Some((runtime, native_id, position)) => (Some(runtime.as_str()), Some(native_id), position),
        None => (None, None, 0),
    };
    tx.execute(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, worker_pid,
           worker_instance_id, result_file, keep_pane, worker_runtime, worker_native_id, position,
           created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            id,
            new.title,
            caller.runtime.as_str(),
            caller.native_id,
            worker,
            foreground,
            caller.instance,
            new.result_file.map(|f| f.to_string_lossy().into_owned()),
            new.keep_pane,
            runtime,
            native_id,
            position,
            now,
        ],
    )?;
    insert_round(&tx, &id, 1, now)?;
    tx.commit()?;
    shown(store.conn(), store.dir(), &caller.instance, &id, None)
}

/// The conversation whose process `pid` on `instance` last reported from the pane, which the task
/// follows, and where in its record the task starts. A report from before version 3 names no
/// instance.
fn occupant_of(
    conn: &Connection,
    lineage: &str,
    instance: &str,
    pid: i64,
) -> Result<Option<(Runtime, String, i64)>> {
    Ok(conn
        .query_row(
            "SELECT runtime, native_id, start_position FROM occupant
             WHERE block_lineage = ?1 AND pid = ?2 AND (instance_id IS NULL OR instance_id = ?3)",
            params![lineage, pid, instance],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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

/// `instance` answered when asked at `asked_at`, so a server the daemon started to observe after
/// that came after it: `instance` has stopped, and that start settled its runs, so a task
/// registered on it now would never end. The epoch commits before it settles them, so a
/// registration either comes before the settling or sees the epoch.
pub(super) fn refuse_stopped(conn: &Connection, instance: &str, asked_at: i64) -> Result<()> {
    let stopped: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM epoch WHERE instance_id != ?1 AND started_at >= ?2)",
        params![instance, asked_at],
        |row| row.get(0),
    )?;
    if stopped {
        bail!("the Rex server instance {instance} has stopped");
    }
    Ok(())
}

fn insert_round(tx: &Transaction, id: &str, round: i64, now: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO task_round (task_id, round, state, created_at)
         VALUES (?1, ?2, 'running', ?3)",
        params![id, round, now],
    )?;
    Ok(())
}

/// The largest message `task send` takes: Codex receives it as one argument, and a command's
/// arguments and environment share about 1 MiB.
pub const MESSAGE_LIMIT: u64 = 256 * 1024;

pub fn refuse_too_large(len: usize) -> Result<()> {
    if u64::try_from(len)? > MESSAGE_LIMIT {
        bail!("the message is larger than {} KiB", MESSAGE_LIMIT / 1024);
    }
    Ok(())
}

/// How a round after the first chooses its turn.
pub(super) enum Kind<'a> {
    /// The earliest finished turn no round took, from a closed pane too while turns are unread.
    Next { unread: Option<usize> },
    /// The turn that takes in this message, which Rexume delivers.
    Request(&'a str),
}

/// Starts the next round with the earliest finished turn no round has taken.
pub fn next(state_dir: &Path, id: &str) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    let unread = unread(store.conn(), store.dir(), id)?;
    let tx = store.write()?;
    let round = start_round(&tx, id, &Kind::Next { unread })?;
    tx.commit()?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

/// Starts the next round with a request the daemon delivers to the worker.
pub fn send(state_dir: &Path, id: &str, message: &str) -> Result<Value> {
    refuse_too_large(message.len())?;
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    let tx = store.write()?;
    let round = start_round(&tx, id, &Kind::Request(message))?;
    tx.commit()?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

/// Ends the latest round, still unfinished, as `abandoned` and acknowledges it: the owner stops
/// waiting for it, and only the owner. The task is not released, so a turn the worker finishes
/// later is unread and `task next` takes it, and `task release` still refuses until then.
pub fn abandon(state_dir: &Path, id: &str) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    let tx = store.write()?;
    owned(&tx, id, &caller)?;
    let round = abandon_round(&tx, id)?;
    tx.commit()?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

fn abandon_round(tx: &Transaction, id: &str) -> Result<i64> {
    let (round, state) = round_state(tx, id, None)?;
    if finished(&state) {
        bail!("task {id} round {round} has already finished as {state}");
    }
    let note = "the owner abandoned the round";
    outcome::finish(tx, id, round, "abandoned", None, Some(note))?;
    acknowledge(tx, id, round, "taken by task abandon")?;
    Ok(round)
}

/// Records that the orchestrator is done with the worker and acknowledges the latest round; only a
/// released task's pane is ever closed. Releasing again changes nothing.
pub fn release(state_dir: &Path, id: &str) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    let unread = unread(store.conn(), store.dir(), id)?;
    let tx = store.write()?;
    let round = latest_finished(&tx, id)?;
    if let Some(count @ 1..) = unread {
        bail!("task {id} has unread turns ({count}); take them with `task next` first");
    }
    tx.execute(
        "UPDATE task SET released_at = COALESCE(released_at, ?1) WHERE id = ?2",
        params![now_ms(), id],
    )?;
    acknowledge(&tx, id, round, "taken by task release")?;
    tx.commit()?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

/// Records the owner's acknowledgement of a finished round; a notice not yet sent for it is
/// dropped, since the owner has the result.
pub(super) fn acknowledge(tx: &Transaction, id: &str, round: i64, by: &str) -> Result<()> {
    tx.execute(
        "UPDATE task_round SET acked_at = COALESCE(acked_at, ?1) WHERE task_id = ?2 AND round = ?3",
        params![now_ms(), id, round],
    )?;
    notice::supersede(tx, notice::Kind::Finished, id, round, by)?;
    Ok(())
}

/// Inserts the task's next round after checking it can take one, acknowledges the round before
/// it, and returns its number.
pub(super) fn start_round(tx: &Transaction, id: &str, kind: &Kind) -> Result<i64> {
    // Another task on the same pane may hold it for a close.
    let (interactive, released, closed, closing): (bool, bool, bool, bool) = tx.query_row(
        "SELECT t.result_file IS NULL, t.released_at IS NOT NULL, t.pane_state = 'closed',
           EXISTS (SELECT 1 FROM task o WHERE o.worker_lineage = t.worker_lineage
                   AND o.pane_state = 'closing')
         FROM task t WHERE t.id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if released {
        bail!("task {id} is released, so it starts no more rounds");
    }
    if !interactive {
        bail!("task {id} is non_interactive, so it has only one round");
    }
    if closing {
        bail!("rexume is closing the worker pane of task {id}");
    }
    match kind {
        Kind::Request(_) if closed => bail!("the worker pane of task {id} is closed"),
        Kind::Next {
            unread: None | Some(0),
        } if closed => {
            bail!("the worker pane of task {id} is closed and no turn is unread");
        }
        _ => {}
    }
    let round = latest_finished(tx, id)?;
    let by = match kind {
        Kind::Next { .. } => "taken by task next",
        Kind::Request(_) => "taken by task send",
    };
    acknowledge(tx, id, round, by)?;
    insert_round(tx, id, round + 1, now_ms())?;
    if let Kind::Request(message) = kind {
        tx.execute(
            "INSERT INTO request (id, task_id, round, text) VALUES (?1, ?2, ?3, ?4)",
            params![new_id(), id, round + 1, message],
        )?;
    }
    Ok(round + 1)
}

/// The task's latest round, which must have finished before another starts or the task is
/// released.
fn latest_finished(conn: &Connection, id: &str) -> Result<i64> {
    let (round, state) = round_state(conn, id, None)?;
    if !finished(&state) {
        bail!("task {id} is {state}; wait until the round has finished");
    }
    Ok(round)
}

#[cfg(test)]
mod tests;
