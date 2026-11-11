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

/// `asked_at` is before `instance` answered. The daemon commits an epoch before it settles the
/// earlier instance's runs, so a registration either comes before the settling or sees the epoch.
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

/// Codex receives the message after the request's token as one argument of `codex queue`, which
/// takes it no other way, and Linux takes at most 128 KiB in one argument, its terminating NUL
/// included (`MAX_ARG_STRLEN` with 4 KiB pages).
pub const MESSAGE_LIMIT: u64 = 127 * 1024;

pub fn refuse_too_large(len: usize) -> Result<()> {
    if u64::try_from(len)? > MESSAGE_LIMIT {
        bail!("the message is larger than {} KiB", MESSAGE_LIMIT / 1024);
    }
    Ok(())
}

pub(super) enum Kind<'a> {
    Next { unread: Option<usize> },
    Request(&'a str),
}

pub fn next(state_dir: &Path, id: &str) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    let unread = unread(store.conn(), store.dir(), id)?;
    let round = start(&mut store, id, &Kind::Next { unread })?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

pub fn send(state_dir: &Path, id: &str, message: &str) -> Result<Value> {
    refuse_too_large(message.len())?;
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    let round = start(&mut store, id, &Kind::Request(message))?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

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

#[derive(Debug, PartialEq)]
pub(super) enum Started {
    Round(i64),
    SnapshotDue(String),
}

pub(super) fn start_round(
    tx: &Transaction,
    id: &str,
    kind: &Kind,
    taken: Option<&snapshot::Taken>,
) -> Result<Started> {
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

pub(super) fn latest_finished(conn: &Connection, id: &str) -> Result<i64> {
    let (round, state) = round_state(conn, id, None)?;
    if !finished(&state) {
        bail!("task {id} is {state}; wait until the round has finished");
    }
    Ok(round)
}

#[cfg(test)]
mod tests;
