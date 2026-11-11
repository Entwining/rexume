//! Registering a worker pane for a task, and each round after the first.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use super::acknowledge::acknowledge_all;
use super::queue;
use super::{caller, finished, owned, round_state, shown, snapshot, unread};
use crate::runtime::Runtime;
use crate::store::{Store, ensure_lineage, new_id, now_ms};

pub struct NewTask<'a> {
    pub worker: &'a str,
    pub title: &'a str,
    pub result_file: Option<&'a Path>,
    pub keep_pane: bool,
    /// A JSON object, saved as the task's `meta`.
    pub meta: &'a str,
}

pub fn add(state_dir: &Path, new: &NewTask) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let asked_at = now_ms();
    let mut caller = caller(&store)?;
    let session = caller
        .rex
        .block_session(new.worker)?
        .with_context(|| format!("pane {} is not on the server", new.worker))?;
    caller.refuse_remote(&session, new.worker)?;
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
    let (worktree, worktree_note) = snapshot::worktree(&process);
    let before = worktree
        .as_deref()
        .map(|worktree| snapshot::take(&mut store, &id, worktree));
    let registered = store.write().and_then(|tx| {
        let now = now_ms();
        let worker = ensure_lineage(&tx, &caller.instance, new.worker)?;
        if worker == caller.lineage {
            bail!(
                "pane {} is the orchestrator's own pane; pass the worker's block ID",
                caller.block
            );
        }
        refuse_taken(&tx, &worker, new.worker)?;
        refuse_stopped(&tx, &caller.instance, asked_at)?;
        let bound = match foreground {
            Some(pid) => occupant_of(&tx, &worker, &caller.instance, pid)?,
            None => None,
        };
        let (runtime, native_id, position) = match bound {
            Some((runtime, native_id, position)) => {
                (Some(runtime.as_str()), Some(native_id), position)
            }
            None => (None, None, 0),
        };
        tx.execute(
            "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage,
               worker_pid, worker_instance_id, result_file, keep_pane, worker_runtime,
               worker_native_id, position, created_at, meta, worktree, worktree_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
                new.meta,
                worktree,
                worktree_note,
            ],
        )?;
        insert_round(&tx, &id, 1, now)?;
        if let Some(before) = &before {
            snapshot::record_before(&tx, &id, 1, before)?;
        }
        Ok(tx.commit()?)
    });
    if let Err(e) = registered {
        // Otherwise what the snapshot kept would count against the snapshots' space until the
        // sweep deletes a directory no task names.
        if before.is_some()
            && let Err(cleanup) = snapshot::discard(&mut store, &id)
        {
            log::warn!(
                "task {id}: failed to delete the snapshot of a failed registration: {cleanup:#}"
            );
        }
        return Err(e);
    }
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

/// A pane works for one task until that task is released: tasks follow their worker's record each
/// on their own, so a second task would take the first one's turns for unread ones of its own and
/// send while the first one's request is on its way. The daemon also holds a released task's pane
/// for one close attempt, which a task registered meanwhile would lose.
fn refuse_taken(conn: &Connection, lineage: &str, block: &str) -> Result<()> {
    let holder: Option<(String, bool)> = conn
        .query_row(
            "SELECT id, released_at IS NULL FROM task
             WHERE worker_lineage = ?1 AND (released_at IS NULL OR pane_state = 'closing')",
            params![lineage],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match holder {
        Some((id, true)) => bail!(
            "pane {block} is the worker of task {id}, which is not released; send that task another \
             round, or release it first"
        ),
        Some((_, false)) => bail!("rexume is closing pane {block}"),
        None => Ok(()),
    }
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
    let round = start(&mut store, id, &Kind::Next { unread })?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

/// Starts the next round with a request the daemon delivers to the worker, or queues it behind
/// the task's open round.
pub fn send(state_dir: &Path, id: &str, message: &str) -> Result<Value> {
    refuse_too_large(message.len())?;
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    let round = start(&mut store, id, &Kind::Request(message))?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

/// Starts the task's next round and returns its number, taking the snapshot it needs outside any
/// transaction and starting again with it. Another snapshot is due only when the latest round
/// finished, or another started and finished, while one was taken.
fn start(store: &mut Store, id: &str, kind: &Kind) -> Result<i64> {
    let mut taken = None;
    loop {
        let tx = store.write()?;
        match start_round(&tx, id, kind, taken.as_ref())? {
            Started::Round(round) => {
                tx.commit()?;
                return Ok(round);
            }
            Started::SnapshotDue(worktree) => {
                drop(tx);
                taken = Some(snapshot::take(store, id, &worktree));
            }
        }
    }
}

/// What `start_round` did.
#[derive(Debug, PartialEq)]
pub(super) enum Started {
    /// Started or queued the round with this number.
    Round(i64),
    /// Wrote nothing: the round it would start at once needs a snapshot of this working tree.
    SnapshotDue(String),
}

/// Inserts the task's next round after checking it can take one. A request sent while a round is
/// open is queued behind it and acknowledges nothing; otherwise the rounds before are
/// acknowledged, since the owner acts after their results. `taken`, the snapshot the command took
/// before, is the latest round's `after` and the new round's `before`, and must have begun after
/// that round finished.
pub(super) fn start_round(
    tx: &Transaction,
    id: &str,
    kind: &Kind,
    taken: Option<&snapshot::Taken>,
) -> Result<Started> {
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
    // A dropped round can be the latest, so the next number comes after it.
    let round: i64 = tx.query_row(
        "SELECT MAX(round) + 1 FROM task_round WHERE task_id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    if let Kind::Request(message) = kind
        && !finished(&round_state(tx, id, None)?.1)
    {
        queue::add(tx, id, round, message)?;
        return Ok(Started::Round(round));
    }
    latest_finished(tx, id)?;
    if let Some(worktree) = snapshot::due(tx, id, taken)? {
        return Ok(Started::SnapshotDue(worktree));
    }
    if let Some(taken) = taken {
        snapshot::record_after(tx, id, taken)?;
    }
    let by = match kind {
        Kind::Next { .. } => "taken by task next",
        Kind::Request(_) => "taken by task send",
    };
    acknowledge_all(tx, id, by)?;
    insert_round(tx, id, round, now_ms())?;
    if let Some(taken) = taken {
        snapshot::record_before(tx, id, round, taken)?;
    }
    if let Kind::Request(message) = kind {
        tx.execute(
            "INSERT INTO request (id, task_id, round, text) VALUES (?1, ?2, ?3, ?4)",
            params![new_id(), id, round, message],
        )?;
    }
    Ok(Started::Round(round))
}

/// The task's latest round, which must have finished before another starts or the task is
/// released.
pub(super) fn latest_finished(conn: &Connection, id: &str) -> Result<i64> {
    let (round, state) = round_state(conn, id, None)?;
    if !finished(&state) {
        bail!("task {id} is {state}; wait until the round has finished");
    }
    Ok(round)
}

#[cfg(test)]
mod tests;
