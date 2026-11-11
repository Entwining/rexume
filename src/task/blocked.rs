use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Transaction, params};

use super::OPEN;
use super::evidence::Pane;
use super::notice::{self, Kind};
use crate::store::{new_id, now_ms};

const GRACE_MS: i64 = 1000;

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

/// The predicate of the partial index `blocked_episode_open`.
pub(super) const OPEN_EPISODE: &str = "e.ended_at IS NULL";

pub(super) const BEGIN: &str =
    "UPDATE task_round SET state = 'blocked' WHERE task_id = ?1 AND round = ?2";

pub(super) const RESUME: &str =
    "UPDATE task_round SET state = 'running' WHERE task_id = ?1 AND round = ?2";

pub(super) static END: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE blocked_episode AS e SET ended_at = ?3, end_reason = ?4
         WHERE e.task_id = ?1 AND e.round = ?2 AND {OPEN_EPISODE}"
    )
});

pub(super) static ON_PANE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT r.task_id, r.round FROM task t JOIN task_round r ON r.task_id = t.id
         WHERE t.worker_lineage = ?1 AND {OPEN} AND r.state = 'blocked'"
    )
});

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

pub(super) fn follow(
    tx: &Transaction,
    id: &str,
    round: i64,
    blocked: bool,
    pane: &Pane,
    process: Option<i64>,
) -> Result<Option<i64>> {
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
