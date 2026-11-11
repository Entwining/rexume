use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Params, Transaction, params};

use super::OPEN;
use super::outcome::finish;
use super::queue::Queued;
use crate::store::{self, now_ms};

pub(super) static WORKER_ROUNDS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT t.id, r.round FROM task t JOIN task_round r ON r.task_id = t.id
         LEFT JOIN conversation c ON c.runtime = t.worker_runtime AND c.native_id = t.worker_native_id
         WHERE t.worker_lineage = ?1 AND {OPEN}
           AND (t.result_file IS NOT NULL
                OR (c.transcript IS NULL
                    AND (t.worker_runtime IS NULL OR t.worker_runtime NOT IN ('claude', 'omp'))))"
    )
});

static CUT_OFF_ROUNDS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT t.id, r.round FROM task t JOIN task_round r ON r.task_id = t.id
         WHERE {OPEN} AND t.result_file IS NOT NULL AND t.worker_instance_id != ?1"
    )
});

pub(super) fn end_cut_off(tx: &Transaction, instance: &str) -> Result<()> {
    finish_each(
        tx,
        &CUT_OFF_ROUNDS,
        params![instance],
        "terminated",
        "the Rex server stopped while the run was still running",
    )
}

static LEFT_TO_SERVER_ROUNDS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT t.id, r.round FROM task t JOIN task_round r ON r.task_id = t.id
         WHERE t.worker_lineage = ?1 AND {OPEN} AND t.worker_instance_id IS NOT ?2
           AND r.created_at < (SELECT MIN(started_at) FROM epoch WHERE instance_id = ?2)
           AND NOT EXISTS (SELECT 1 FROM occupant o
                           WHERE o.block_lineage = ?1 AND o.instance_id = ?2
                             AND o.runtime = t.worker_runtime AND o.native_id = t.worker_native_id)"
    )
});

pub fn end_left_to_server(tx: &Transaction, instance: &str, block: &str) -> Result<()> {
    finish_each(
        tx,
        &LEFT_TO_SERVER_ROUNDS,
        params![block, instance],
        "terminated",
        "the Rex server restored the worker's pane with a shell, and its conversation was not \
         reopened there",
    )
}

fn on_worker_closed(tx: &Transaction, lineage: &str) -> Result<()> {
    finish_each(
        tx,
        &WORKER_ROUNDS,
        params![lineage],
        "worker_closed",
        "the worker's pane closed before it finished",
    )?;
    tx.execute(
        "UPDATE task SET pane_state = 'closed', pane_closed_at = ?1,
           pane_note = CASE WHEN pane_state = 'kept' THEN pane_note END
         WHERE worker_lineage = ?2 AND pane_state != 'closed'",
        params![now_ms(), lineage],
    )?;
    Ok(())
}

fn finish_each(
    tx: &Transaction,
    rounds: &str,
    params: impl Params,
    state: &str,
    note: &str,
) -> Result<()> {
    let open: Vec<(String, i64)> = tx
        .prepare(rounds)?
        .query_map(params, |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, round) in open {
        finish(tx, &id, round, state, None, Some(note), Queued::End)?;
    }
    Ok(())
}

pub fn block_gone(tx: &Transaction, lineage: &str) -> Result<()> {
    on_worker_closed(tx, lineage)?;
    store::forget_block(tx, lineage)
}

#[cfg(test)]
mod tests;
