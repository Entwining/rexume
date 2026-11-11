//! A request round queues behind the open round, since the turn that takes its request in is its
//! result and the worker takes one request at a time, between its turns.

use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{OPEN, outcome};
use crate::store::{new_id, now_ms};

pub(super) enum Queued {
    Start,
    End,
}

pub(super) static STANDING: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT EXISTS (SELECT 1 FROM task_round r WHERE r.task_id = t.id AND {OPEN}),
           t.pane_state = 'closed'
         FROM task t WHERE t.id = ?1"
    )
});

pub(super) const START_NEXT: &str = "UPDATE task_round SET state = 'running'
  WHERE task_id = ?1 AND round = (SELECT MIN(round) FROM task_round
                                  WHERE task_id = ?1 AND state = 'queued')
  RETURNING round";

pub(super) const QUEUED_ROUNDS: &str =
    "SELECT round FROM task_round WHERE task_id = ?1 AND state = 'queued' ORDER BY round";

pub(super) fn add(tx: &Transaction, id: &str, round: i64, message: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO task_round (task_id, round, state, created_at) VALUES (?1, ?2, 'queued', ?3)",
        params![id, round, now_ms()],
    )?;
    tx.execute(
        "INSERT INTO request (id, task_id, round, note, text) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            new_id(),
            id,
            round,
            "waiting for the rounds before it to finish",
            message
        ],
    )?;
    Ok(())
}

pub(super) fn advance(
    tx: &Transaction,
    id: &str,
    round: i64,
    state: &str,
    queued: Queued,
) -> Result<()> {
    let (open, closed): (bool, bool) = tx
        .prepare_cached(&STANDING)?
        .query_row(params![id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    if open {
        return Ok(());
    }
    let why = match queued {
        Queued::Start if !closed => return start_next(tx, id),
        Queued::Start => "the worker's pane closed before the round started".to_owned(),
        Queued::End => format!("round {round} ended as {state} before it started"),
    };
    for later in rounds(tx, id)? {
        outcome::end(tx, id, later, "dropped", None, Some(&why))?;
    }
    Ok(())
}

fn start_next(tx: &Transaction, id: &str) -> Result<()> {
    let started: Option<i64> = tx
        .prepare_cached(START_NEXT)?
        .query_row(params![id], |row| row.get(0))
        .optional()?;
    if let Some(started) = started {
        tx.execute(
            "UPDATE request SET note = NULL WHERE task_id = ?1 AND round = ?2",
            params![id, started],
        )?;
    }
    Ok(())
}

pub(super) fn rounds(conn: &Connection, id: &str) -> Result<Vec<i64>> {
    Ok(conn
        .prepare_cached(QUEUED_ROUNDS)?
        .query_map(params![id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests;
