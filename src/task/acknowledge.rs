//! The owner ending its interest in rounds: abandoning one unfinished, acknowledging a finished
//! one, and releasing the worker (`docs/design.md`, "Acknowledging and releasing").

use std::path::Path;

use anyhow::{Result, bail};
use rusqlite::{Transaction, params};
use serde_json::Value;

use super::queue::Queued;
use super::register::latest_finished;
use super::{caller, finished, notice, outcome, owned, round_state, shown, snapshot, unread};
use crate::store::{Store, now_ms};

/// Ends a round still unfinished, the open one by default, as `abandoned` and acknowledges
/// it: the owner stops waiting for it, and only the owner. The task is not released, so a turn the
/// worker finishes later is unread and `task next` takes it, and `task release` still refuses
/// until then. Abandoning the open round starts the next queued one; abandoning a queued round
/// drops it, its request never sent.
pub fn abandon(state_dir: &Path, id: &str, round: Option<i64>) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    let tx = store.write()?;
    owned(&tx, id, &caller)?;
    let round = abandon_round(&tx, id, round)?;
    tx.commit()?;
    snapshot::after(&mut store, id)?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

pub(super) fn abandon_round(tx: &Transaction, id: &str, round: Option<i64>) -> Result<i64> {
    let (round, state) = round_state(tx, id, round)?;
    if finished(&state) {
        bail!("task {id} round {round} has already finished as {state}");
    }
    let (state, note) = if state == "queued" {
        ("dropped", "the owner abandoned the round before it started")
    } else {
        ("abandoned", "the owner abandoned the round")
    };
    outcome::finish(tx, id, round, state, None, Some(note), Queued::Start)?;
    acknowledge(tx, id, round, "taken by task abandon")?;
    Ok(round)
}

/// Records that the owner has a finished round's result.
pub fn ack(state_dir: &Path, id: &str, round: Option<i64>) -> Result<Value> {
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    let tx = store.write()?;
    owned(&tx, id, &caller)?;
    let (round, state) = round_state(&tx, id, round)?;
    if !finished(&state) {
        bail!("task {id} is {state}; ack it once the round has finished");
    }
    acknowledge(&tx, id, round, "taken by task ack")?;
    tx.commit()?;
    snapshot::after(&mut store, id)?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

/// Records that the orchestrator is done with the worker and acknowledges its rounds; only a
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
    acknowledge_all(&tx, id, "taken by task release")?;
    tx.commit()?;
    snapshot::after(&mut store, id)?;
    shown(store.conn(), store.dir(), &caller.instance, id, Some(round))
}

/// Records the owner's acknowledgement of a finished round; a notice not yet sent for it is
/// dropped, since the owner has the result.
pub(super) fn acknowledge(tx: &Transaction, id: &str, round: i64, by: &str) -> Result<()> {
    let now = now_ms();
    tx.execute(
        "UPDATE task_round SET acked_at = COALESCE(acked_at, ?1) WHERE task_id = ?2 AND round = ?3",
        params![now, id, round],
    )?;
    notice::supersede(tx, notice::Kind::Finished, id, round, by, now)?;
    Ok(())
}

/// Acknowledges every round of the task its owner has not acknowledged, which must all have
/// finished. Rounds queued behind an open one start without acknowledging it, so earlier rounds
/// than the latest can wait for an acknowledgement.
pub(super) fn acknowledge_all(tx: &Transaction, id: &str, by: &str) -> Result<()> {
    let rounds: Vec<i64> = tx
        .prepare("SELECT round FROM task_round WHERE task_id = ?1 AND acked_at IS NULL")?
        .query_map(params![id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for round in rounds {
        acknowledge(tx, id, round, by)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
