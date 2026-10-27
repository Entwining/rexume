//! Rounds whose worker's terminal is gone before its record showed how they ended: the pane
//! closed, or the server instance that ran it stopped.

use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Transaction, params};

use super::OPEN;
use super::outcome::finish;
use crate::store::{self, now_ms};

/// The rounds a closed worker pane leaves unfinished that no record can finish: a run's, and an
/// interactive worker's whose conversation is unbound or reported no record. The others take a
/// turn the record already shows, on the daemon's next pass.
pub(super) static WORKER_ROUNDS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT t.id, r.round FROM task t JOIN task_round r ON r.task_id = t.id
         LEFT JOIN conversation c ON c.runtime = t.worker_runtime AND c.native_id = t.worker_native_id
         WHERE t.worker_lineage = ?1 AND {OPEN} AND c.transcript IS NULL"
    )
});

/// The runs an earlier server instance left open: its terminals ended with it, and its records
/// did not show how.
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
        instance,
        "terminated",
        "the Rex server stopped while the run was still running",
    )
}

fn on_worker_closed(tx: &Transaction, lineage: &str) -> Result<()> {
    finish_each(
        tx,
        &WORKER_ROUNDS,
        lineage,
        "worker_closed",
        "the worker's pane closed before it finished",
    )?;
    // Whatever rexume meant to do with the pane, it is gone now.
    tx.execute(
        "UPDATE task SET pane_state = 'closed', pane_closed_at = ?1, pane_note = NULL
         WHERE worker_lineage = ?2 AND pane_state != 'closed'",
        params![now_ms(), lineage],
    )?;
    Ok(())
}

fn finish_each(tx: &Transaction, rounds: &str, param: &str, state: &str, note: &str) -> Result<()> {
    let open: Vec<(String, i64)> = tx
        .prepare(rounds)?
        .query_map(params![param], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, round) in open {
        finish(tx, &id, round, state, None, Some(note))?;
    }
    Ok(())
}

/// Records that a pane is closed for good: it leaves the saved layout, its worker tasks end, and
/// what was recorded about it goes. The close event and the daemon's own close both record it,
/// whichever comes first.
pub fn block_gone(tx: &Transaction, lineage: &str) -> Result<()> {
    crate::layout::forget_block(tx, lineage)?;
    on_worker_closed(tx, lineage)?;
    store::forget_block(tx, lineage)
}
