//! Brings the database of an earlier released version up to `schema.sql`, inside the transaction
//! that holds the write lock. Each step rebuilds the tables whose shape changed, and the tables
//! that reference them, and copies their rows into the new shape; `schema.sql` creates whatever is
//! missing and sets the version.

use anyhow::{Result, bail};
use rusqlite::Transaction;

use super::SCHEMA;

pub(super) fn from(tx: &Transaction, version: i64) -> Result<()> {
    match version {
        1 => from_v1(tx)?,
        2 => rebuild(tx, &["notice", "request", "task_round", "task"])?,
        // Version 4 adds `occupant.start_position`.
        3 => return rebuild(tx, &["occupant"]),
        _ => bail!("schema {version} has no upgrade"),
    }
    // Version 3 drops the start that typed the reopen command: such a slot starts a shell.
    tx.execute_batch(
        "UPDATE restore_slot SET start = 'shell', runtime = NULL, native_id = NULL,
           started = CASE WHEN started IS NOT NULL THEN 'shell' END
         WHERE start = 'typed'",
    )?;
    rebuild(tx, &["restore_slot", "occupant"])?;
    // Version 3 also records the server instance of a task's worker, of a pane's occupant, and of
    // the pane when a round finished. An earlier one's is the instance an epoch was observing at
    // the time, which is the server `task add` or the report reached. Only an epoch whose end is
    // known dates it: one the server's end closed, or one the next epoch, on the same instance,
    // followed. A daemon that died left its epoch open, and the next daemon closed it when it
    // started, by when another server may have been running.
    for (table, column, at) in [
        ("task", "worker_instance_id", "created_at"),
        ("occupant", "instance_id", "reported_at"),
        ("task_round", "finished_instance_id", "finished_at"),
    ] {
        tx.execute_batch(&format!(
            "UPDATE {table} SET {column} =
               (SELECT e.instance_id FROM epoch e
                WHERE e.started_at <= {table}.{at} AND e.ended_at >= {table}.{at}
                  AND (e.end_reason = 'server_gone'
                       OR (SELECT n.instance_id FROM epoch n WHERE n.id > e.id ORDER BY n.id LIMIT 1)
                          = e.instance_id)
                ORDER BY e.id DESC LIMIT 1)"
        ))?;
    }
    Ok(())
}

/// Rebuilds `tables`, each listed before the tables it references, in the shape `schema.sql`
/// gives them, keeping every row and the columns both shapes have; a new column takes its
/// default. Dropping a referenced table first would cascade into the rows that reference it.
fn rebuild(tx: &Transaction, tables: &[&str]) -> Result<()> {
    for table in tables {
        tx.execute_batch(&format!(
            "CREATE TEMP TABLE old_{table} AS SELECT * FROM main.{table}; DROP TABLE main.{table};"
        ))?;
    }
    tx.execute_batch(SCHEMA)?;
    for table in tables.iter().rev() {
        let columns: Vec<String> = tx
            .prepare(&format!(
                "SELECT name FROM pragma_table_info('old_{table}', 'temp')"
            ))?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let columns = columns.join(", ");
        tx.execute_batch(&format!(
            "INSERT INTO main.{table} ({columns}) SELECT {columns} FROM temp.old_{table};
             DROP TABLE temp.old_{table};"
        ))?;
    }
    Ok(())
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

#[cfg(test)]
mod tests;
