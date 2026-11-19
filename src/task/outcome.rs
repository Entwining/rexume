use std::sync::LazyLock;

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, Transaction, params};

use super::evidence::{Evidence, Reading, RunProcess, Take};
use super::notice::{self, Kind};
use super::queue::{self, Queued};
use super::{OPEN, request};
use crate::runtime::Turn;
use crate::store::now_ms;

pub(super) static ROUND: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT t.worker_pid, t.result_file, t.pane_state = 'closed', t.worker_instance_id
         FROM task t JOIN task_round r ON r.task_id = t.id
         WHERE t.id = ?1 AND r.round = ?2 AND {OPEN}"
    )
});

pub(super) fn evaluate(tx: &Transaction, id: &str, round: i64, evidence: Evidence) -> Result<()> {
    type Row = (Option<i64>, Option<String>, bool, Option<String>);
    let row: Option<Row> = tx
        .prepare_cached(&ROUND)?
        .query_row(params![id, round], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .optional()?;
    let Some((worker_pid, result_file, closed, worker_instance)) = row else {
        return Ok(());
    };
    match evidence {
        Evidence::Turn(Err(note)) => {
            finish(tx, id, round, "failed", None, Some(&note), Queued::End)
        }
        Evidence::Turn(Ok(reading)) => interactive_outcome(tx, id, round, closed, reading),
        Evidence::Run { pane, file } => {
            let ended = match file {
                Some(Ok(result)) => {
                    let reference = result_file.unwrap_or_default();
                    Some(("completed", Some(Finished { result, reference }), None))
                }
                Some(Err(note)) => Some(("failed", None, Some(note))),
                None => {
                    let run = RunProcess {
                        instance: worker_instance.as_deref(),
                        pid: worker_pid,
                    };
                    match (pane.ended(&run), pane.last_exit) {
                        (true, Some(-1)) => {
                            Some(("terminated", None, Some("ended by a signal".into())))
                        }
                        (true, Some(code)) if code != 0 => {
                            Some(("failed", None, Some(format!("exited {code}"))))
                        }
                        _ => None,
                    }
                }
            };
            match ended {
                Some((state, done, note)) => {
                    finish(tx, id, round, state, done, note.as_deref(), Queued::End)
                }
                None => Ok(()),
            }
        }
    }
}

fn interactive_outcome(
    tx: &Transaction,
    id: &str,
    round: i64,
    closed: bool,
    reading: Reading,
) -> Result<()> {
    if reading.received {
        tx.prepare_cached(
            "UPDATE request SET state = 'delivered', delivered_at = ?3
             WHERE task_id = ?1 AND round = ?2 AND state IN ('sent', 'accepted', 'unknown')",
        )?
        .execute(params![id, round, now_ms()])?;
    }
    if reading.replaced {
        let note = "the worker's record was replaced or cut, so its turns after the task's \
                    position can no longer be told apart";
        return finish(tx, id, round, "failed", None, Some(note), Queued::End);
    }
    if let Some(take) = reading.chosen {
        return take_turn(tx, id, round, take);
    }
    if closed {
        let note = "the worker's pane closed before it finished";
        finish(
            tx,
            id,
            round,
            "worker_closed",
            None,
            Some(note),
            Queued::End,
        )?;
    }
    Ok(())
}

fn take_turn(tx: &Transaction, id: &str, round: i64, take: Take) -> Result<()> {
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
            Queued::Start,
        )?,
        Turn::Cancelled => finish(
            tx,
            id,
            round,
            "cancelled",
            None,
            Some("the worker's turn was cancelled"),
            Queued::Start,
        )?,
        Turn::Failed { text } => finish(tx, id, round, "failed", None, Some(&text), Queued::Start)?,
        Turn::Open | Turn::None => bail!("a round can take only a turn that ended"),
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

pub(super) struct Finished {
    result: String,
    reference: String,
}

pub(super) fn finish(
    tx: &Transaction,
    id: &str,
    round: i64,
    state: &str,
    done: Option<Finished>,
    note: Option<&str>,
    queued: Queued,
) -> Result<()> {
    end(tx, id, round, state, done, note)?;
    queue::advance(tx, id, round, state, queued)
}

pub(super) fn end(
    tx: &Transaction,
    id: &str,
    round: i64,
    state: &str,
    done: Option<Finished>,
    note: Option<&str>,
) -> Result<()> {
    let (instance, status_at, fg_changes): (Option<String>, Option<i64>, Option<i64>) = tx
        .query_row(
            "SELECT b.instance_id, b.status_at, b.fg_changes
             FROM task t LEFT JOIN block b ON b.lineage = t.worker_lineage WHERE t.id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    let (result, reference) = done.map(|d| (d.result, d.reference)).unzip();
    let now = now_ms();
    tx.execute(
        "UPDATE task_round SET state = ?1, result = ?2, result_ref = ?3, finished_at = ?4,
           finished_instance_id = ?5, finished_status_at = ?6, finished_fg_changes = ?7, note = ?8
         WHERE task_id = ?9 AND round = ?10",
        params![
            state, result, reference, now, instance, status_at, fg_changes, note, id, round
        ],
    )?;
    notice::write(tx, Kind::Finished, id, Some(round), now, None)?;
    tx.execute(
        "UPDATE request SET state = 'superseded', note = ?3
         WHERE task_id = ?1 AND round = ?2 AND state = 'pending'",
        params![id, round, request::ENDED],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
