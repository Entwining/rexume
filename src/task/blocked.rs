//! Blocked episodes: a round's own conversation that reports `blocked` from the worker's pane on
//! the observed server instance, without a break for `GRACE_MS`, puts the round in an episode, a
//! `blocked_episode` row, which its owner learns of through one blocked notice. The episode ends
//! when the daemon replaces that status, when the round finishes, or when a pass finds the status
//! no longer that conversation's `blocked` on the observed instance.

use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Transaction, params};

use super::OPEN;
use super::evidence::Pane;
use super::notice::{self, Kind};
use crate::store::{new_id, now_ms};

/// How long a pane reports `blocked` without a break before its round begins an episode, so that a
/// worker passing through a dialog the user answers at once tells its owner nothing.
const GRACE_MS: i64 = 1000;

/// Why an episode ended, as `blocked_episode.end_reason` records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    StatusChanged,
    RoundFinished,
    ServerGone,
    OccupantChanged,
}

impl EndReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::StatusChanged => "status_changed",
            Self::RoundFinished => "round_finished",
            Self::ServerGone => "server_gone",
            Self::OccupantChanged => "occupant_changed",
        }
    }
}

/// An episode of `e` that has not ended: the predicate of the partial index
/// `blocked_episode_open`, written once so that every query relying on the index matches it.
pub(super) const OPEN_EPISODE: &str = "e.ended_at IS NULL";

/// Begins a running round's episode.
pub(super) const BEGIN: &str =
    "UPDATE task_round SET state = 'blocked' WHERE task_id = ?1 AND round = ?2";

/// Ends a blocked round's episode while the round goes on.
pub(super) const RESUME: &str =
    "UPDATE task_round SET state = 'running' WHERE task_id = ?1 AND round = ?2";

/// Ends a round's open episode at `?3`, for the reason `?4`.
pub(super) static END: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE blocked_episode AS e SET ended_at = ?3, end_reason = ?4
         WHERE e.task_id = ?1 AND e.round = ?2 AND {OPEN_EPISODE}"
    )
});

/// The rounds in an episode whose worker is a given pane, through `task_round_open`, so that a
/// status event does not read the worker's finished rounds.
pub(super) static ON_PANE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT r.task_id, r.round FROM task t JOIN task_round r ON r.task_id = t.id
         WHERE t.worker_lineage = ?1 AND {OPEN} AND r.state = 'blocked'"
    )
});

/// Ends the episodes of the rounds whose worker is the pane `lineage`, in the transaction that
/// replaces the pane's status with one that is not `blocked` or starts the pane again on a new
/// server instance. A pass sees only the latest status, so two reports recorded before it would
/// otherwise hide one episode's end and the next one's start, and the first episode's notice would
/// tell of the second.
pub fn end_blocked_episodes(tx: &Transaction, lineage: &str, reason: EndReason) -> Result<()> {
    let rounds: Vec<(String, i64)> = tx
        .prepare_cached(&ON_PANE)?
        .query_map(params![lineage], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, round) in rounds {
        resume(tx, &id, round, reason)?;
    }
    Ok(())
}

/// Moves an unfinished round to how its worker's pane stands, given whether the round is in an
/// episode and the pid of the task's conversation while it is the pane's occupant.
/// Returns when to read the round again while the grace runs.
pub(super) fn follow(
    tx: &Transaction,
    id: &str,
    round: i64,
    blocked: bool,
    pane: &Pane,
    process: Option<i64>,
) -> Result<Option<i64>> {
    // A status recorded on an earlier instance, or reported by another program in the pane, is
    // not the task's conversation waiting on a dialog now.
    let still = pane.status.as_deref() == Some("blocked");
    let reported = pane.current && still && process.is_some_and(|pid| pane.status_pid == Some(pid));
    match (blocked, reported) {
        (true, false) => {
            let reason = if !pane.current {
                EndReason::ServerGone
            } else if still {
                EndReason::OccupantChanged
            } else {
                EndReason::StatusChanged
            };
            resume(tx, id, round, reason)?;
        }
        (false, true) => {
            let now = now_ms();
            // The grace runs from the latest report, so a pane that left `blocked` and came back
            // starts it again.
            if let Some(due) = pane.status_at.map(|at| at + GRACE_MS)
                && now < due
            {
                return Ok(Some(due));
            }
            tx.prepare_cached(BEGIN)?.execute(params![id, round])?;
            let episode = new_id();
            tx.prepare_cached(
                "INSERT INTO blocked_episode (id, task_id, round, kind, message, status_at,
                   started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                episode,
                id,
                round,
                pane.status_kind,
                pane.status_message,
                pane.status_at,
                now
            ])?;
            notice::write(tx, Kind::Blocked, id, round, now, Some(&episode))?;
        }
        _ => {}
    }
    Ok(None)
}

fn resume(tx: &Transaction, id: &str, round: i64, reason: EndReason) -> Result<()> {
    tx.prepare_cached(RESUME)?.execute(params![id, round])?;
    end(tx, id, round, now_ms(), reason)
}

pub(super) const ENDED: &str = "the block ended before it was sent";

/// Ends the round's episode at `now`, if it has one, as the round goes on or finishes: a blocked
/// notice not yet sent no longer tells the owner anything true, so it is never sent. One that may
/// have been sent stays as it is.
pub(super) fn end(
    tx: &Transaction,
    id: &str,
    round: i64,
    now: i64,
    reason: EndReason,
) -> Result<()> {
    tx.prepare_cached(&END)?
        .execute(params![id, round, now, reason.as_str()])?;
    notice::supersede(tx, Kind::Blocked, id, round, ENDED, now)?;
    Ok(())
}

#[cfg(test)]
mod tests;
