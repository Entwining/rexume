//! Blocked episodes: a round's own conversation that reports `blocked` from the worker's pane on
//! the observed server instance, without a break for `GRACE_MS`, puts the round in an episode,
//! which its owner learns of through one blocked notice. The episode ends when the daemon replaces
//! that status, when the round finishes, or when a pass finds the status no longer that
//! conversation's `blocked` on the observed instance.

use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Transaction, params};

use super::OPEN;
use super::evidence::Pane;
use super::notice::{self, Kind};
use crate::store::now_ms;

/// How long a pane reports `blocked` without a break before its round begins an episode, so that a
/// worker passing through a dialog the user answers at once tells its owner nothing.
const GRACE_MS: i64 = 1000;

/// Begins a running round's episode.
pub(super) const BEGIN: &str =
    "UPDATE task_round SET state = 'blocked', blocked_at = ?1 WHERE task_id = ?2 AND round = ?3";

/// Ends a blocked round's episode while the round goes on.
pub(super) const RESUME: &str =
    "UPDATE task_round SET state = 'running', blocked_at = NULL WHERE task_id = ?1 AND round = ?2";

/// The rounds in an episode whose worker is a given pane, through `task_round_open`, so that a
/// status event does not read the worker's finished rounds.
pub(super) static ON_PANE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT r.task_id, r.round FROM task t JOIN task_round r ON r.task_id = t.id
         WHERE t.worker_lineage = ?1 AND {OPEN} AND r.state = 'blocked'"
    )
});

/// Ends the episodes of the rounds whose worker is the pane `lineage`, in the transaction that
/// replaces the pane's status with one that is not `blocked`. A pass sees only the latest status,
/// so two reports recorded before it would otherwise hide one episode's end and the next one's
/// start, and the first episode's notice would tell of the second.
pub fn end_blocked_episodes(tx: &Transaction, lineage: &str) -> Result<()> {
    let rounds: Vec<(String, i64)> = tx
        .prepare_cached(&ON_PANE)?
        .query_map(params![lineage], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, round) in rounds {
        resume(tx, &id, round)?;
    }
    Ok(())
}

/// Moves an unfinished round to how its worker's pane stands, given whether the round is in an
/// episode and the pid and report time of the task's conversation while it is the pane's occupant.
/// Returns when to read the round again while the grace runs.
pub(super) fn follow(
    tx: &Transaction,
    id: &str,
    round: i64,
    blocked: bool,
    pane: &Pane,
    process: Option<(i64, i64)>,
) -> Result<Option<i64>> {
    // A status recorded on an earlier instance, or reported by another program in the pane, is
    // not the task's conversation waiting on a dialog now.
    let reported = pane.current
        && pane.status.as_deref() == Some("blocked")
        && process.is_some_and(|(pid, _)| pane.status_pid == Some(pid));
    match (blocked, reported) {
        (true, false) => resume(tx, id, round)?,
        (false, true) => {
            let now = now_ms();
            // The grace runs from the latest report, so a pane that left `blocked` and came back
            // starts it again.
            if let Some(due) = pane.status_at.map(|at| at + GRACE_MS)
                && now < due
            {
                return Ok(Some(due));
            }
            tx.prepare_cached(BEGIN)?.execute(params![now, id, round])?;
            notice::write(tx, Kind::Blocked, id, round, now)?;
        }
        _ => {}
    }
    Ok(None)
}

fn resume(tx: &Transaction, id: &str, round: i64) -> Result<()> {
    tx.prepare_cached(RESUME)?.execute(params![id, round])?;
    end(tx, id, round)
}

/// Ends the round's episode, if it has one, as the round goes on or finishes: a blocked notice not
/// yet sent no longer tells the owner anything true, so it is never sent. One that may have been
/// sent stays as it is.
pub(super) fn end(tx: &Transaction, id: &str, round: i64) -> Result<()> {
    notice::supersede(
        tx,
        Kind::Blocked,
        id,
        round,
        "the block ended before it was sent",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
