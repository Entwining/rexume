//! The outbox of notices to task owners. A notice leaves `pending` through exactly one claimant:
//! the running daemon, which marks it `sending` before it sends, or the owner's own `task wait`
//! or `task ack`. A send that may have happened is never repeated; only the owner's record
//! settles it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value};

use super::Interest;
use crate::proc;
use crate::runtime::{self, Receipt, Runtime, Sent, Target, pi};
use crate::store::{Store, now_ms};

/// How long a finished round waits before the daemon sends its notice, so a `task wait` running
/// in the owner's turn can take the result first.
const GRACE_MS: i64 = 3000;

fn text(due: &Due) -> String {
    let Due {
        notice,
        task,
        title,
        round,
        round_state,
        note,
        ..
    } = due;
    let token = runtime::token(notice);
    if round_state == "completed" {
        format!(
            "{token} Worker task {task} \"{title}\" finished round {round}. Read its result with \
             `rexume task show {task} --round {round}`, then acknowledge it with \
             `rexume task ack {task} --round {round}`."
        )
    } else {
        let reason = note.as_ref().map_or(String::new(), |n| format!(" ({n})"));
        format!(
            "{token} Worker task {task} \"{title}\" ended round {round} without a result: \
             {round_state}{reason}. Check it with `rexume task show {task} --round {round}`; once \
             it is dealt with, acknowledge it with `rexume task ack {task} --round {round}`."
        )
    }
}

/// Pending notices with the round each tells of.
pub(super) const PENDING: &str = "SELECT n.id, t.id, t.title, n.round, r.state, r.note,
    r.waiter_pid, r.finished_at
  FROM notice n JOIN task t ON t.id = n.task_id
  JOIN task_round r ON r.task_id = n.task_id AND r.round = n.round
  WHERE n.state = 'pending'";

struct Due {
    notice: String,
    task: String,
    title: String,
    round: i64,
    round_state: String,
    note: Option<String>,
    waiter: Option<i64>,
    finished_at: i64,
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
                task: row.get(1)?,
                title: row.get(2)?,
                round: row.get(3)?,
                round_state: row.get(4)?,
                note: row.get(5)?,
                waiter: row.get(6)?,
                finished_at: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let now = now_ms();
    let mut next = None;
    for due in pending {
        let due_at = due.finished_at + GRACE_MS;
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
        // An error means nothing was sent, so the notice can be sent later.
        let (state, note) = match owner
            .runtime
            .send(&target, store.dir(), &due.notice, &text(&due))
        {
            Ok(Sent::Handed) => ("sent", None),
            Ok(Sent::Accepted(receipt)) => ("accepted", Some(receipt)),
            Ok(Sent::Unknown(why)) => ("unknown", Some(why)),
            Err(e) => ("pending", Some(format!("{e:#}"))),
        };
        log::info!("notice {} for task {}: {state}", due.notice, due.task);
        let tx = store.write()?;
        tx.execute(
            "UPDATE notice SET state = ?1, note = ?2,
               sent_at = CASE WHEN ?1 = 'pending' THEN NULL ELSE ?3 END
             WHERE id = ?4",
            params![state, note, now_ms(), due.notice],
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

/// Notices a stopped daemon left mid-send. Only the daemon holding the state directory's lock
/// claims a notice for sending, so once the next one holds it, a notice still marked as sending
/// may or may not have reached its owner. It becomes unknown: never sent again, settled only by
/// the owner's own record. Returns their IDs.
pub fn orphaned_sends(store: &mut Store) -> Result<Vec<String>> {
    let tx = store.write()?;
    let orphaned = tx
        .prepare(
            "UPDATE notice SET state = 'unknown',
               note = 'the daemon stopped while sending it, so it may or may not have arrived'
             WHERE state = 'sending' RETURNING id",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
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

/// Sent notices with the owner record each is looked for in.
pub(super) const SENT: &str = "SELECT n.id, n.state, c.runtime, c.native_id, c.transcript
  FROM notice n JOIN task t ON t.id = n.task_id
  JOIN conversation c ON c.runtime = t.owner_runtime AND c.native_id = t.owner_native_id
  WHERE n.state IN ('sent', 'accepted', 'unknown')";

/// The notices sent to one owner record.
struct OwnerRecord {
    runtime: Runtime,
    native_id: String,
    notices: Vec<(String, String)>,
}

/// Marks notices the owner's conversation has taken in, from its own record. Pi's extension
/// renaming a notice to `.accepted` shows only that Pi holds it.
pub(super) fn settle(
    store: &mut Store,
    receipts: &mut HashMap<PathBuf, Receipt>,
    interest: &mut Interest,
) -> Result<()> {
    let mut records: HashMap<PathBuf, OwnerRecord> = HashMap::new();
    let rows: Vec<(String, String, Runtime, String, Option<String>)> = store
        .conn()
        .prepare_cached(SENT)?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (notice, state, runtime, native_id, transcript) in rows {
        let Some(transcript) = transcript else {
            continue;
        };
        records
            .entry(PathBuf::from(transcript))
            .or_insert_with(|| OwnerRecord {
                runtime,
                native_id,
                notices: Vec::new(),
            })
            .notices
            .push((notice, state));
    }
    receipts.retain(|path, _| records.contains_key(path));
    let mut delivered = HashSet::new();
    let mut accepted = Vec::new();
    for (path, record) in records {
        let receipt = receipts
            .entry(path.clone())
            .or_insert_with(|| Receipt::new(record.runtime, &path));
        interest.files.insert(path);
        let ids: Vec<String> = record.notices.iter().map(|(id, _)| id.clone()).collect();
        delivered.extend(receipt.found(&ids)?);
        if record.runtime == Runtime::Pi {
            interest
                .files
                .insert(pi::inbox(store.dir(), &record.native_id));
            accepted.extend(record.notices.into_iter().filter(|(id, state)| {
                state != "accepted"
                    && !delivered.contains(id)
                    && pi::accepted(store.dir(), &record.native_id, id)
            }));
        }
    }
    if delivered.is_empty() && accepted.is_empty() {
        return Ok(());
    }
    let now = now_ms();
    let tx = store.write()?;
    for id in &delivered {
        tx.execute(
            "UPDATE notice SET state = 'delivered', delivered_at = ?1
             WHERE id = ?2 AND state IN ('sent', 'accepted', 'unknown')",
            params![now, id],
        )?;
    }
    for (id, _) in &accepted {
        tx.execute(
            "UPDATE notice SET state = 'accepted' WHERE id = ?1 AND state IN ('sent', 'unknown')",
            params![id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Notices not yet confirmed as delivered, with the round each tells of. A `pending` notice
/// with a note is one whose send failed, which keeps failing until its cause is fixed.
const UNDELIVERED: &str = "SELECT n.id, n.task_id, n.round, r.state, n.state, n.note
  FROM notice n JOIN task t ON t.id = n.task_id
  JOIN task_round r ON r.task_id = n.task_id AND r.round = n.round
  WHERE (n.state IN ('unknown', 'sending', 'sent', 'accepted')
         OR (n.state = 'pending' AND n.note IS NOT NULL))";

/// The NOTICE objects of `docs/cli.md` not yet confirmed as delivered: one owner's, or all.
pub fn undelivered(conn: &Connection, owner: Option<(Runtime, &str)>) -> Result<Vec<Value>> {
    let notice = |row: &rusqlite::Row| -> rusqlite::Result<Value> {
        Ok(serde_json::json!({
            "id": row.get::<_, String>(0)?,
            "task_id": row.get::<_, String>(1)?,
            "round": row.get::<_, i64>(2)?,
            "round_state": row.get::<_, String>(3)?,
            "state": row.get::<_, String>(4)?,
            "note": row.get::<_, Option<String>>(5)?,
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
