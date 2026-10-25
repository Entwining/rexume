//! Brings the database of an earlier released version up to `schema.sql`, inside the transaction
//! that holds the write lock. Each step rebuilds only the tables whose shape changed and copies
//! their rows into the new shape; `schema.sql` creates whatever is missing and sets the version.

use anyhow::{Result, bail};
use rusqlite::Transaction;

use super::SCHEMA;

pub(super) fn from(tx: &Transaction, version: i64) -> Result<()> {
    match version {
        1 => from_v1(tx),
        _ => bail!("schema {version} has no upgrade"),
    }
}

/// Version 2 binds a task's worker conversation to the task instead of each round, keeps the task's
/// position in that record and when it was released, records the turn each round took, and adds
/// requests. A version 1 task stands as its latest round left it: that round's conversation is the
/// task's, and its `cursor` the position, an offset in that conversation's record. A task whose
/// latest round is unbound binds the conversation that reports next and reads its record from the
/// start, where the answers earlier rounds took stay taken. Acknowledging the latest round released
/// only a disposable worker; a `--keep-pane` worker took further rounds. Children are dropped
/// before their parents so that no foreign key cascades.
fn from_v1(tx: &Transaction) -> Result<()> {
    tx.execute_batch(
        "CREATE TEMP TABLE v1_task AS SELECT * FROM main.task;
         CREATE TEMP TABLE v1_round AS SELECT * FROM main.task_round;
         CREATE TEMP TABLE v1_notice AS SELECT * FROM main.notice;
         DROP TABLE main.notice;
         DROP TABLE main.task_round;
         DROP TABLE main.task;",
    )?;
    tx.execute_batch(SCHEMA)?;
    tx.execute_batch(
        "INSERT INTO main.task (id, title, owner_runtime, owner_native_id, worker_lineage,
           worker_pid, result_file, keep_pane, worker_runtime, worker_native_id, position,
           released_at, pane_state, pane_note, pane_closed_at, created_at)
         SELECT t.id, t.title, t.owner_runtime, t.owner_native_id, t.worker_lineage, t.worker_pid,
           t.result_file, t.keep_pane, l.worker_runtime, l.worker_native_id,
           CASE WHEN l.worker_runtime IS NULL THEN 0 ELSE l.cursor END,
           CASE WHEN t.keep_pane = 0 THEN l.acked_at END,
           t.pane_state, t.pane_note, t.pane_closed_at, t.created_at
         FROM v1_task t
         LEFT JOIN v1_round l ON l.task_id = t.id
           AND l.round = (SELECT MAX(round) FROM v1_round WHERE task_id = t.id);
         INSERT INTO main.task_round (task_id, round, waiter_pid, state, note, finished_at,
           finished_status_at, finished_generation, acked_at, created_at, result_ref, result)
         SELECT task_id, round, waiter_pid, state, note, finished_at, finished_status_at,
           finished_generation, acked_at, created_at, result_ref, result
         FROM v1_round;
         INSERT INTO main.notice (id, task_id, round, state, note)
         SELECT id, task_id, round, state, note FROM v1_notice;
         DROP TABLE temp.v1_notice;
         DROP TABLE temp.v1_round;
         DROP TABLE temp.v1_task;",
    )?;
    Ok(())
}
