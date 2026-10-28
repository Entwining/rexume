//! The outbox of notices to task owners. A notice leaves `pending` through exactly one claimant:
//! the running daemon, which marks it `sending` before it sends, or the owner's own `task wait`
//! or acknowledgement. A send that may have happened is never repeated; only the owner's record
//! settles it.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value};

use super::Interest;
use crate::proc;
use crate::runtime::{Runtime, Sent, Target};
use crate::store::{Store, new_id, now_ms};

pub(super) mod receipt;
mod text;

/// How long after the round finished, or began its blocked episode, the daemon waits before it
/// sends the notice, so a `task wait` running in the owner's turn can take it first.
const GRACE_MS: i64 = 3000;

/// What a notice tells its owner.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    /// The round finished.
    Finished,
    /// The round began a blocked episode.
    Blocked,
}

/// A round's finished notice as `n`: the predicate of the partial index `notice_finished`,
/// written once so that every query relying on the index matches it.
const FINISHED: &str = "n.kind = 'finished'";
/// A round's blocked notice as `n` that has not been sent: the predicate of the partial index
/// `notice_blocked_unsent`.
const UNSENT_BLOCKED: &str = "n.kind = 'blocked' AND n.state IN ('pending', 'sending')";

/// Drops a round's pending finished notice, with `?3` saying why.
pub(super) static SUPERSEDE_FINISHED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE notice AS n SET state = 'superseded', note = ?3
         WHERE n.task_id = ?1 AND n.round = ?2 AND {FINISHED} AND n.state = 'pending'"
    )
});

/// Drops a round's pending blocked notice, with `?3` saying why.
pub(super) static SUPERSEDE_BLOCKED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE notice AS n SET state = 'superseded', note = ?3
         WHERE n.task_id = ?1 AND n.round = ?2 AND {UNSENT_BLOCKED} AND n.state = 'pending'"
    )
});

/// Writes a round's notice at `now`, the time its transaction gives the round, which the daemon
/// sends once it is due.
pub(super) fn write(tx: &Transaction, kind: Kind, id: &str, round: i64, now: i64) -> Result<()> {
    let kind = match kind {
        Kind::Finished => "finished",
        Kind::Blocked => "blocked",
    };
    tx.execute(
        "INSERT INTO notice (id, task_id, round, kind, state, created_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
        params![new_id(), id, round, kind, now],
    )?;
    Ok(())
}

/// Drops a round's notice while it is still pending, so that it is never sent, with `note` saying
/// why. Returns whether there was one.
pub(super) fn supersede(
    tx: &Transaction,
    kind: Kind,
    id: &str,
    round: i64,
    note: &str,
) -> Result<bool> {
    let statement = match kind {
        Kind::Finished => &SUPERSEDE_FINISHED,
        Kind::Blocked => &SUPERSEDE_BLOCKED,
    };
    Ok(tx
        .prepare_cached(statement)?
        .execute(params![id, round, note])?
        == 1)
}

/// Pending notices with the round each tells of, the worker pane's Rex block ID on the instance
/// the daemon observes last, and the state of the round's request.
pub(super) const PENDING: &str = "SELECT n.id, n.kind = 'blocked', t.id, t.title, n.round,
    r.state, r.note, r.waiter_pid, n.created_at, l.live_id, q.state
  FROM notice n JOIN task t ON t.id = n.task_id
  JOIN task_round r ON r.task_id = n.task_id AND r.round = n.round
  LEFT JOIN request q ON q.task_id = n.task_id AND q.round = n.round
  LEFT JOIN epoch e ON e.id = (SELECT MAX(id) FROM epoch)
  LEFT JOIN live l ON l.instance_id = e.instance_id AND l.lineage = t.worker_lineage
  WHERE n.state = 'pending'";

struct Due {
    notice: String,
    blocked: bool,
    task: String,
    title: String,
    round: i64,
    round_state: String,
    note: Option<String>,
    waiter: Option<i64>,
    /// When the notice was written.
    since: i64,
    worker: Option<String>,
    request: Option<String>,
}

/// Sends each due notice to its owner once the owner is running and between turns, unless a live
/// `task wait` will take it, and returns when the next notice comes due.
pub(super) fn send_due(store: &mut Store, interest: &mut Interest) -> Result<Option<Instant>> {
    let pending: Vec<Due> = store
        .conn()
        .prepare_cached(PENDING)?
        .query_map([], |row| {
            Ok(Due {
                notice: row.get(0)?,
                blocked: row.get(1)?,
                task: row.get(2)?,
                title: row.get(3)?,
                round: row.get(4)?,
                round_state: row.get(5)?,
                note: row.get(6)?,
                waiter: row.get(7)?,
                since: row.get(8)?,
                worker: row.get(9)?,
                request: row.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let now = now_ms();
    let mut next = None;
    for due in pending {
        let due_at = due.since + GRACE_MS;
        if due_at > now {
            let at = Instant::now() + Duration::from_millis(u64::try_from(due_at - now)?);
            next = Some(next.map_or(at, |n: Instant| n.min(at)));
            continue;
        }
        if let Some(pid) = due.waiter.filter(|pid| proc::alive(*pid)) {
            interest.pids.insert(pid);
            continue;
        }
        let Some(owner) = ready_owner(store.conn(), &due.task)? else {
            continue;
        };
        let text = if due.blocked {
            text::blocked(&due)
        } else {
            text::finished(&due, super::unread(store.conn(), &due.task)?)
        };
        let tx = store.write()?;
        let claimed = claim(&tx, &due.notice, due.waiter)?;
        tx.commit()?;
        if !claimed {
            continue;
        }
        let target = Target {
            native_id: &owner.native_id,
            transport: &owner.transport,
        };
        let (state, note) =
            Sent::recorded(owner.runtime.send(&target, store.dir(), &due.notice, &text));
        log::info!("notice {} for task {}: {state}", due.notice, due.task);
        let tx = store.write()?;
        tx.execute(
            "UPDATE notice SET state = ?1, note = ?2 WHERE id = ?3",
            params![state, note, due.notice],
        )?;
        tx.commit()?;
    }
    Ok(next)
}

/// Claims a pending notice for sending, unless a `task wait` registered since `?2`, the dead
/// waiter (or none) the daemon saw before the transaction.
pub(super) const CLAIM: &str =
    "UPDATE notice SET state = 'sending' WHERE id = ?1 AND state = 'pending'
  AND NOT EXISTS (SELECT 1 FROM task_round r
                  WHERE r.task_id = notice.task_id AND r.round = notice.round
                    AND r.waiter_pid IS NOT NULL AND r.waiter_pid IS NOT ?2)";

pub(super) fn claim(tx: &Transaction, notice: &str, seen_waiter: Option<i64>) -> Result<bool> {
    Ok(tx.execute(CLAIM, params![notice, seen_waiter])? == 1)
}

/// Notices and requests a stopped daemon left mid-send. Only the daemon holding the state
/// directory's lock claims either for sending, so once the next one holds it, one still marked as
/// sending may or may not have reached its receiver. It becomes unknown: never sent again, settled
/// only by the receiver's own record. Returns each one's kind and ID.
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

/// The task's owner when its conversation runs as the foreground program of a pane on the
/// instance the running daemon observes, and is between turns.
pub(super) const READY_OWNER: &str = "SELECT o.runtime, o.native_id, o.transport
  FROM task t
  JOIN occupant o ON o.runtime = t.owner_runtime AND o.native_id = t.owner_native_id
  JOIN block b ON b.lineage = o.block_lineage
  JOIN epoch e ON e.id = (SELECT MAX(id) FROM epoch)
  WHERE t.id = ?1 AND e.ended_at IS NULL AND e.instance_id = b.instance_id AND b.fg_pid = o.pid
    AND (o.instance_id IS NULL OR o.instance_id = b.instance_id)
    AND (b.status IS NULL OR b.status NOT IN ('working', 'blocked'))
  ORDER BY o.reported_at DESC LIMIT 1";

struct Owner {
    runtime: Runtime,
    native_id: String,
    transport: Map<String, Value>,
}

fn ready_owner(conn: &Connection, task: &str) -> Result<Option<Owner>> {
    Ok(conn
        .prepare_cached(READY_OWNER)?
        .query_row(params![task], |row| {
            let transport: Option<Value> = row.get(2)?;
            Ok(Owner {
                runtime: row.get(0)?,
                native_id: row.get(1)?,
                transport: transport
                    .and_then(|t| t.as_object().cloned())
                    .unwrap_or_default(),
            })
        })
        .optional()?)
}

/// Notices not yet confirmed as delivered, with the round each tells of. A `pending` notice
/// with a note is one whose send failed, which keeps failing until its cause is fixed.
const UNDELIVERED: &str = "SELECT n.id, n.kind, n.task_id, n.round, r.state, n.state, n.note
  FROM notice n JOIN task t ON t.id = n.task_id
  JOIN task_round r ON r.task_id = n.task_id AND r.round = n.round
  WHERE (n.state IN ('unknown', 'sending', 'sent', 'accepted')
         OR (n.state = 'pending' AND n.note IS NOT NULL))";

/// The NOTICE objects of `docs/cli.md` not yet confirmed as delivered: one owner's, or all.
pub fn undelivered(conn: &Connection, owner: Option<(Runtime, &str)>) -> Result<Vec<Value>> {
    let notice = |row: &rusqlite::Row| -> rusqlite::Result<Value> {
        Ok(serde_json::json!({
            "id": row.get::<_, String>(0)?,
            "kind": row.get::<_, String>(1)?,
            "task_id": row.get::<_, String>(2)?,
            "round": row.get::<_, i64>(3)?,
            "round_state": row.get::<_, String>(4)?,
            "state": row.get::<_, String>(5)?,
            "note": row.get::<_, Option<String>>(6)?,
        }))
    };
    Ok(match owner {
        Some((runtime, native_id)) => conn
            .prepare(&format!(
                "{UNDELIVERED} AND t.owner_runtime = ?1 AND t.owner_native_id = ?2"
            ))?
            .query_map(params![runtime.as_str(), native_id], notice)?
            .collect::<rusqlite::Result<_>>()?,
        None => conn
            .prepare(UNDELIVERED)?
            .query_map([], notice)?
            .collect::<rusqlite::Result<_>>()?,
    })
}

#[cfg(test)]
mod tests;
