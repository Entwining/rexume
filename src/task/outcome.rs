//! How an unfinished round stands, judged from its evidence, and the transitions that finish a
//! round.

use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{OptionalExtension, Transaction, params};

use super::OPEN;
use super::evidence::{Evidence, Pane};
use crate::runtime::Turn;
use crate::store::{self, new_id, now_ms};

/// Claude Code can write the end of a turn after its idle status, so an idle status ends a prompt
/// the record leaves open only once the record still shows it open this long after the status.
const IDLE_GRACE_MS: i64 = 3000;

/// One round, if it is still open, with what `evaluate` needs to judge it.
pub(super) static ROUND: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT t.worker_lineage, t.worker_pid, t.result_file, r.worker_runtime, r.worker_native_id
         FROM task t JOIN task_round r ON r.task_id = t.id
         WHERE t.id = ?1 AND r.round = ?2 AND {OPEN}"
    )
});

/// Re-reads one round that has not finished and finishes it when the evidence shows how it
/// ended. Returns when to read it again, in Unix milliseconds, when only time can settle it.
pub(super) fn evaluate(
    tx: &Transaction,
    id: &str,
    round: i64,
    evidence: Evidence,
) -> Result<Option<i64>> {
    type Row = (
        String,
        Option<i64>,
        Option<String>,
        Option<(String, String)>,
    );
    let row: Option<Row> = tx
        .prepare_cached(&ROUND)?
        .query_row(params![id, round], |row| {
            let runtime: Option<String> = row.get(3)?;
            let native_id: Option<String> = row.get(4)?;
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                runtime.zip(native_id),
            ))
        })
        .optional()?;
    let Some((worker, worker_pid, result_file, bound)) = row else {
        return Ok(None);
    };
    match evidence {
        Evidence::Turn { turn, end } => {
            let pane = Pane::read(tx, &worker)?;
            let pid = match bound {
                Some((runtime, native_id)) => tx
                    .prepare_cached(
                        "SELECT pid, reported_at FROM occupant
                         WHERE block_lineage = ?1 AND runtime = ?2 AND native_id = ?3",
                    )?
                    .query_row(params![worker, runtime, native_id], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })
                    .optional()?,
                None => None,
            };
            interactive_outcome(tx, id, round, &pane, pid, turn, end)
        }
        Evidence::Run {
            file: Some(read), ..
        } => {
            match read {
                Ok(result) => finish(
                    tx,
                    id,
                    round,
                    "completed",
                    Some(Finished {
                        result,
                        reference: result_file.unwrap_or_default(),
                    }),
                    None,
                )?,
                Err(note) => finish(tx, id, round, "failed", None, Some(&note))?,
            }
            Ok(None)
        }
        Evidence::Run { pane, file: None } => {
            match (pane.ended(worker_pid), pane.last_exit) {
                (true, Some(-1)) => {
                    finish(tx, id, round, "terminated", None, Some("ended by a signal"))?;
                }
                (true, Some(code)) if code != 0 => finish(
                    tx,
                    id,
                    round,
                    "failed",
                    None,
                    Some(&format!("exited {code}")),
                )?,
                _ => {}
            }
            Ok(None)
        }
    }
}

/// Whether the process that accepted the prompt recorded at `since` went idle after it. A process
/// started after the prompt, such as one reopening the conversation after a restart, reports idle
/// on its own start, which does not end the earlier turn.
fn idle_after(pane: &Pane, (pid, reported_at): (i64, i64), since: i64) -> bool {
    pane.status.as_deref() == Some("idle")
        && pane.status_pid == Some(pid)
        && reported_at < since
        && pane.status_at.is_some_and(|at| at > since)
}

/// An interactive worker's result is the turn its conversation record shows, not its status: a
/// worker that went idle may have been cancelled or be waiting on a question. The status counts
/// only for a prompt the record leaves open, which the bound conversation's process going idle
/// after it ends unanswered. `process` is that process's pid and report time while it is the
/// pane's occupant.
fn interactive_outcome(
    tx: &Transaction,
    id: &str,
    round: i64,
    pane: &Pane,
    process: Option<(i64, i64)>,
    turn: Turn,
    end: u64,
) -> Result<Option<i64>> {
    match turn {
        Turn::Completed { reference, text } => finish(
            tx,
            id,
            round,
            "completed",
            Some(Finished {
                result: text,
                reference,
            }),
            None,
        )?,
        Turn::Cancelled => finish(
            tx,
            id,
            round,
            "cancelled",
            None,
            Some("the worker's turn was cancelled"),
        )?,
        Turn::Failed { text } => finish(tx, id, round, "failed", None, Some(&text))?,
        Turn::Open { since: Some(since) }
            if process.is_some_and(|process| idle_after(pane, process, since)) =>
        {
            let due = pane.status_at.unwrap_or(since) + IDLE_GRACE_MS;
            if now_ms() < due {
                return Ok(Some(due));
            }
            finish(
                tx,
                id,
                round,
                "cancelled",
                None,
                Some("the worker went idle without answering"),
            )?;
        }
        Turn::Open { .. } | Turn::None => {
            let state = if pane.status.as_deref() == Some("blocked") {
                "blocked"
            } else {
                "running"
            };
            tx.prepare_cached(
                "UPDATE task_round SET state = ?1 WHERE task_id = ?2 AND round = ?3 AND state != ?1",
            )?
            .execute(params![state, id, round])?;
            return Ok(None);
        }
    }
    // A turn after the cursor is activity in the worker's pane, so the turn that ended the round
    // must not count as one.
    tx.execute(
        "UPDATE task_round SET cursor = ?1 WHERE task_id = ?2 AND round = ?3",
        params![i64::try_from(end)?, id, round],
    )?;
    Ok(None)
}

/// A finished round's result, as the worker's record or result file gave it.
struct Finished {
    result: String,
    reference: String,
}

/// Ends a round and writes its notice in the same transaction. The worker pane's status time and
/// foreground generation are kept to tell later activity in the pane apart.
fn finish(
    tx: &Transaction,
    id: &str,
    round: i64,
    state: &str,
    done: Option<Finished>,
    note: Option<&str>,
) -> Result<()> {
    let (status_at, generation): (Option<i64>, Option<i64>) = tx.query_row(
        "SELECT b.status_at, b.fg_generation FROM task t LEFT JOIN block b ON b.lineage = t.worker_lineage
         WHERE t.id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (result, reference) = done.map(|d| (d.result, d.reference)).unzip();
    tx.execute(
        "UPDATE task_round SET state = ?1, result = ?2, result_ref = ?3, finished_at = ?4,
           finished_status_at = ?5, finished_generation = ?6, note = ?7
         WHERE task_id = ?8 AND round = ?9",
        params![
            state,
            result,
            reference,
            now_ms(),
            status_at,
            generation,
            note,
            id,
            round
        ],
    )?;
    tx.execute(
        "INSERT INTO notice (id, task_id, round, state) VALUES (?1, ?2, ?3, 'pending')",
        params![new_id(), id, round],
    )?;
    Ok(())
}

/// The rounds a closed worker pane leaves unfinished.
pub(super) static WORKER_ROUNDS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT t.id, r.round FROM task t JOIN task_round r ON r.task_id = t.id
         WHERE t.worker_lineage = ?1 AND {OPEN}"
    )
});

fn on_worker_closed(tx: &Transaction, lineage: &str) -> Result<()> {
    let open: Vec<(String, i64)> = tx
        .prepare(&WORKER_ROUNDS)?
        .query_map(params![lineage], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, round) in open {
        finish(
            tx,
            &id,
            round,
            "worker_closed",
            None,
            Some("the worker's pane closed before it finished"),
        )?;
    }
    // Whatever rexume meant to do with the pane, it is gone now.
    tx.execute(
        "UPDATE task SET pane_state = 'closed', pane_closed_at = ?1, pane_note = NULL
         WHERE worker_lineage = ?2 AND pane_state != 'closed'",
        params![now_ms(), lineage],
    )?;
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
