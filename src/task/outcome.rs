//! How an unfinished round stands, judged from its evidence, and the transitions that finish a
//! round.

use std::sync::LazyLock;

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, Transaction, params};

use super::OPEN;
use super::evidence::{Evidence, Pane, Reading, RunProcess, Take};
use crate::runtime::Turn;
use crate::store::{new_id, now_ms};

/// Claude Code can write the end of a turn after its idle status, so an idle status ends a turn
/// the record leaves open only once the record still shows it open this long after the status.
const IDLE_GRACE_MS: i64 = 3000;

/// One round, if it is still open, with what `evaluate` needs to judge it.
pub(super) static ROUND: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT t.worker_lineage, t.worker_pid, t.result_file, t.worker_runtime, t.worker_native_id,
           t.pane_state = 'closed', t.worker_instance_id
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
        bool,
        Option<String>,
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
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .optional()?;
    let Some((worker, worker_pid, result_file, bound, closed, worker_instance)) = row else {
        return Ok(None);
    };
    match evidence {
        Evidence::Turn(Err(note)) => {
            finish(tx, id, round, "failed", None, Some(&note))?;
            Ok(None)
        }
        Evidence::Turn(Ok(reading)) => {
            let pane = Pane::read(tx, &worker)?;
            let process = match bound {
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
            let worker = Worker {
                closed,
                pane,
                process,
            };
            interactive_outcome(tx, id, round, &worker, reading)
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
            let run = RunProcess {
                instance: worker_instance.as_deref(),
                pid: worker_pid,
            };
            match (pane.ended(&run), pane.last_exit) {
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

/// An interactive worker as recorded: whether its pane closed, the pane, and the pid and report
/// time of the task's conversation while it is the pane's occupant.
struct Worker {
    closed: bool,
    pane: Pane,
    process: Option<(i64, i64)>,
}

/// Whether the process that wrote the turn started at `since` went idle after it. A process
/// started after it, such as one reopening the conversation after a restart, reports idle on its
/// own start, which does not end the earlier turn.
fn idle_after(pane: &Pane, (pid, reported_at): (i64, i64), since: i64) -> bool {
    pane.status.as_deref() == Some("idle")
        && pane.status_pid == Some(pid)
        && reported_at < since
        && pane.status_at.is_some_and(|at| at > since)
}

/// An interactive round's result is the turn its tracker chose from the worker's record, not the
/// worker's status: a worker that went idle may have been cancelled or be waiting on a question.
/// A turn the record leaves open is taken only once nothing more can come of it: the pane closed,
/// or the process that wrote it went idle after it.
fn interactive_outcome(
    tx: &Transaction,
    id: &str,
    round: i64,
    worker: &Worker,
    reading: Reading,
) -> Result<Option<i64>> {
    if reading.received {
        tx.prepare_cached(
            "UPDATE request SET state = 'delivered'
             WHERE task_id = ?1 AND round = ?2 AND state IN ('sent', 'accepted', 'unknown')",
        )?
        .execute(params![id, round])?;
    }
    if reading.replaced {
        let note = "the worker's record was replaced or cut, so its turns after the task's \
                    position can no longer be told apart";
        finish(tx, id, round, "failed", None, Some(note))?;
        return Ok(None);
    }
    if let Some(take) = reading.chosen {
        take_turn(tx, id, round, take, None)?;
        return Ok(None);
    }
    if let Some((take, since)) = reading.open {
        let answered = matches!(take.turn, Turn::Completed { .. });
        if worker.closed && answered {
            take_turn(tx, id, round, take, None)?;
            return Ok(None);
        }
        if let Some(since) = since
            && !worker.closed
            && worker
                .process
                .is_some_and(|process| idle_after(&worker.pane, process, since))
        {
            let due = worker.pane.status_at.unwrap_or(since) + IDLE_GRACE_MS;
            if now_ms() < due {
                return Ok(Some(due));
            }
            let note = (!answered).then_some("the worker went idle without answering");
            take_turn(tx, id, round, take, note)?;
            return Ok(None);
        }
    }
    if worker.closed {
        let note = "the worker's pane closed before it finished";
        finish(tx, id, round, "worker_closed", None, Some(note))?;
        return Ok(None);
    }
    let state = if worker.pane.status.as_deref() == Some("blocked") {
        "blocked"
    } else {
        "running"
    };
    tx.prepare_cached(
        "UPDATE task_round SET state = ?1 WHERE task_id = ?2 AND round = ?3 AND state != ?1",
    )?
    .execute(params![state, id, round])?;
    Ok(None)
}

/// Finishes a round with the turn it takes, records the turn as taken, and moves the task's
/// position, all in the transaction that writes the round's notice.
fn take_turn(tx: &Transaction, id: &str, round: i64, take: Take, note: Option<&str>) -> Result<()> {
    match take.turn {
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
            Some(note.unwrap_or("the worker's turn was cancelled")),
        )?,
        Turn::Failed { text } => finish(tx, id, round, "failed", None, Some(&text))?,
        Turn::Open { .. } | Turn::None => bail!("a round can take only a turn that ended"),
    }
    tx.execute(
        "UPDATE task_round SET turn_start = ?1 WHERE task_id = ?2 AND round = ?3",
        params![i64::try_from(take.start)?, id, round],
    )?;
    tx.execute(
        "UPDATE task SET position = ?1 WHERE id = ?2",
        params![i64::try_from(take.position)?, id],
    )?;
    Ok(())
}

/// A finished round's result, as the worker's record or result file gave it.
pub(super) struct Finished {
    result: String,
    reference: String,
}

/// Ends a round and writes its notice in the same transaction. The worker pane's status time and
/// foreground generation are kept to tell later activity in the pane apart.
pub(super) fn finish(
    tx: &Transaction,
    id: &str,
    round: i64,
    state: &str,
    done: Option<Finished>,
    note: Option<&str>,
) -> Result<()> {
    let (instance, status_at, generation): (Option<String>, Option<i64>, Option<i64>) = tx
        .query_row(
            "SELECT b.instance_id, b.status_at, b.fg_generation
             FROM task t LEFT JOIN block b ON b.lineage = t.worker_lineage WHERE t.id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    let (result, reference) = done.map(|d| (d.result, d.reference)).unzip();
    tx.execute(
        "UPDATE task_round SET state = ?1, result = ?2, result_ref = ?3, finished_at = ?4,
           finished_instance_id = ?5, finished_status_at = ?6, finished_generation = ?7, note = ?8
         WHERE task_id = ?9 AND round = ?10",
        params![
            state,
            result,
            reference,
            now_ms(),
            instance,
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
    tx.execute(
        "UPDATE request SET state = 'superseded', note = 'the round ended before it was sent'
         WHERE task_id = ?1 AND round = ?2 AND state = 'pending'",
        params![id, round],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
