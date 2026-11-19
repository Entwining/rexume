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
    ServerGone,
    OccupantChanged,
    TaskReleased,
    PaneClosed,
}

impl EndReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::StatusChanged => "status_changed",
            Self::ServerGone => "server_gone",
            Self::OccupantChanged => "occupant_changed",
            Self::TaskReleased => "task_released",
            Self::PaneClosed => "pane_closed",
        }
    }
}

/// The predicate of the partial index `blocked_episode_open`.
pub(super) const OPEN_EPISODE: &str = "e.ended_at IS NULL";

/// The state a round of task `?1` starts in.
pub(super) static START_STATE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "CASE WHEN EXISTS (SELECT 1 FROM blocked_episode e WHERE e.task_id = ?1 AND {OPEN_EPISODE})
           THEN 'blocked' ELSE 'running' END"
    )
});

pub(super) static BEGIN: LazyLock<String> = LazyLock::new(|| {
    format!("UPDATE task_round AS r SET state = 'blocked' WHERE r.task_id = ?1 AND {OPEN}")
});

pub(super) static END: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE blocked_episode AS e SET ended_at = ?2, end_reason = ?3
         WHERE e.task_id = ?1 AND {OPEN_EPISODE}"
    )
});

pub(super) static RESUME: LazyLock<String> = LazyLock::new(|| {
    format!("UPDATE task_round AS r SET state = 'running' WHERE r.task_id = ?1 AND {OPEN}")
});

pub(super) static ON_PANE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT t.id FROM task t JOIN blocked_episode e ON e.task_id = t.id
         WHERE t.worker_lineage = ?1 AND {OPEN_EPISODE}"
    )
});

pub fn end_blocked_episodes(tx: &Transaction, lineage: &str, reason: EndReason) -> Result<()> {
    let tasks: Vec<String> = tx
        .prepare_cached(&ON_PANE)?
        .query_map(params![lineage], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in tasks {
        end(tx, &id, now_ms(), reason)?;
    }
    Ok(())
}

/// Whether the task's conversation, `process`, reports `blocked` from `pane` on the observed
/// instance.
pub(super) fn reported(pane: &Pane, process: Option<i64>) -> bool {
    pane.current
        && pane.status.as_deref() == Some("blocked")
        && process.is_some_and(|pid| pane.status_pid == Some(pid))
}

/// `blocked`: the task has an episode that has not ended; `round`: its open round; `process`: the
/// task's conversation's process in `pane`. Returns when a status's grace passes.
pub(super) fn follow(
    tx: &Transaction,
    id: &str,
    round: Option<i64>,
    blocked: bool,
    pane: &Pane,
    process: Option<i64>,
) -> Result<Option<i64>> {
    match (blocked, reported(pane, process)) {
        (true, false) => {
            let reason = if !pane.current {
                EndReason::ServerGone
            } else if pane.status.as_deref() == Some("blocked") {
                EndReason::OccupantChanged
            } else {
                EndReason::StatusChanged
            };
            end(tx, id, now_ms(), reason)?;
        }
        (false, true) => {
            let now = now_ms();
            if let Some(due) = pane.status_at.map(|at| at + GRACE_MS)
                && now < due
            {
                return Ok(Some(due));
            }
            tx.prepare_cached(&BEGIN)?.execute(params![id])?;
            let episode = new_id();
            tx.prepare_cached(
                "INSERT INTO blocked_episode (id, task_id, kind, message, status_at, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                episode,
                id,
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

pub(super) const ENDED: &str = "the block ended before it was sent";

pub(super) fn end(tx: &Transaction, id: &str, now: i64, reason: EndReason) -> Result<()> {
    if tx
        .prepare_cached(&END)?
        .execute(params![id, now, reason.as_str()])?
        == 0
    {
        return Ok(());
    }
    tx.prepare_cached(&RESUME)?.execute(params![id])?;
    notice::supersede_blocked(tx, id, ENDED, now)?;
    Ok(())
}

#[cfg(test)]
mod tests;
