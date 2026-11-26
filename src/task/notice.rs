use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use rusqlite::{Transaction, params};

use super::OPEN;
use super::blocked::OPEN_EPISODE;
use crate::store::{Store, new_id};

pub(super) mod receipt;
pub(super) mod send;
mod undelivered;

pub(super) use send::send_due;
pub use undelivered::undelivered;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Finished,
    Blocked,
    Unread,
}

impl FromSql for Kind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "finished" => Ok(Self::Finished),
            "blocked" => Ok(Self::Blocked),
            "unread" => Ok(Self::Unread),
            other => Err(FromSqlError::Other(
                format!("no notice kind {other}").into(),
            )),
        }
    }
}

/// The predicate of the partial index `notice_finished`.
const FINISHED: &str = "n.kind = 'finished'";
/// The predicate of the partial index `notice_blocked_unsent`.
const UNSENT_BLOCKED: &str = "n.kind = 'blocked' AND n.state IN ('pending', 'sending')";
/// The predicate of the partial index `notice_unread_pending`.
const PENDING_UNREAD: &str = "n.kind = 'unread' AND n.state = 'pending'";

/// The round whose `task wait` takes notice `n` itself: a finished notice's own round and a blocked
/// one's open round. An unread notice is sent only while no round is open.
static HOLDER: LazyLock<String> = LazyLock::new(|| {
    format!(
        "CASE n.kind WHEN 'finished' THEN n.round
           WHEN 'blocked' THEN (SELECT r.round FROM task_round r
                                WHERE r.task_id = n.task_id AND {OPEN}) END"
    )
});

/// Whether notice `n`, its round `r`, and its task `t` still call for it.
static WANTED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "CASE n.kind WHEN 'finished' THEN r.acked_at IS NULL
           WHEN 'blocked' THEN EXISTS (SELECT 1 FROM blocked_episode e
                                       WHERE e.id = n.blocked_episode_id AND {OPEN_EPISODE})
           ELSE t.released_at IS NULL
             AND NOT EXISTS (SELECT 1 FROM task_round r WHERE r.task_id = n.task_id AND {OPEN}) END"
    )
});

pub(super) static SUPERSEDE_FINISHED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE notice AS n SET state = 'superseded', note = ?3, superseded_at = ?4
         WHERE n.task_id = ?1 AND n.round = ?2 AND {FINISHED} AND n.state = 'pending'"
    )
});

pub(super) static SUPERSEDE_BLOCKED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE notice AS n SET state = 'superseded', note = ?2, superseded_at = ?3
         WHERE n.task_id = ?1 AND {UNSENT_BLOCKED} AND n.state = 'pending'"
    )
});

pub(super) static SUPERSEDE_UNREAD: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE notice AS n SET state = 'superseded', note = ?2, superseded_at = ?3
         WHERE n.task_id = ?1 AND {PENDING_UNREAD}"
    )
});

pub(super) fn write(
    tx: &Transaction,
    kind: Kind,
    id: &str,
    round: Option<i64>,
    now: i64,
    episode: Option<&str>,
) -> Result<()> {
    let kind = match kind {
        Kind::Finished => "finished",
        Kind::Blocked => "blocked",
        Kind::Unread => "unread",
    };
    tx.prepare_cached(
        "INSERT INTO notice (id, task_id, round, kind, blocked_episode_id, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
    )?
    .execute(params![new_id(), id, round, kind, episode, now])?;
    Ok(())
}

pub(super) fn supersede_finished(
    tx: &Transaction,
    id: &str,
    round: i64,
    note: &str,
    now: i64,
) -> Result<bool> {
    Ok(tx
        .prepare_cached(&SUPERSEDE_FINISHED)?
        .execute(params![id, round, note, now])?
        == 1)
}

pub(super) fn supersede_blocked(tx: &Transaction, id: &str, note: &str, now: i64) -> Result<bool> {
    Ok(tx
        .prepare_cached(&SUPERSEDE_BLOCKED)?
        .execute(params![id, note, now])?
        == 1)
}

pub(super) fn supersede_unread(tx: &Transaction, id: &str, note: &str, now: i64) -> Result<()> {
    tx.prepare_cached(&SUPERSEDE_UNREAD)?
        .execute(params![id, note, now])?;
    Ok(())
}

/// Only the daemon holding the state directory's lock claims notices and requests for sending, so
/// a `sending` row its successor finds may or may not have reached its receiver.
pub fn orphaned_sends(store: &mut Store) -> Result<Vec<(&'static str, String)>> {
    let tx = store.write()?;
    let mut orphaned = Vec::new();
    for (kind, statement) in [
        (
            "notice",
            "UPDATE notice SET state = 'unknown', note = ?1 WHERE state = 'sending' RETURNING id",
        ),
        (
            "request",
            "UPDATE request SET state = 'unknown', note = ?1 WHERE state = 'sending' RETURNING id",
        ),
        (
            "restore",
            "UPDATE restore SET continuation_state = 'unknown', note = ?1
             WHERE continuation_state = 'sending' RETURNING id",
        ),
    ] {
        let ids = tx
            .prepare(statement)?
            .query_map(
                ["the daemon stopped while sending it, so it may or may not have arrived"],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        orphaned.extend(ids.into_iter().map(|id| (kind, id)));
    }
    tx.commit()?;
    Ok(orphaned)
}

#[cfg(test)]
mod tests;
