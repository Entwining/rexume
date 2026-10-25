//! The outbox of requests to workers. The daemon is the only sender: it claims a request as
//! `sending` before it sends, and a send that may have happened is never repeated. Only the
//! worker's own record settles a request, which the round's tracker reads.

use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};

use super::{Interest, OPEN};
use crate::runtime::{self, Runtime, Sent, Target, pi};
use crate::store::Store;

/// The pending requests of unfinished rounds, reached through the rounds so that the request table
/// is never scanned: SQLite keeps the left table of a `CROSS JOIN` as the outer loop.
pub(super) static PENDING: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT q.id, q.task_id, q.note, q.text FROM task_round r
         CROSS JOIN request q ON q.task_id = r.task_id AND q.round = r.round
         WHERE {OPEN} AND q.state = 'pending'"
    )
});

/// The task's worker conversation where it reported from the worker's pane, whether it is that
/// pane's foreground program on the instance the running daemon observes, and whether it is in a
/// turn.
pub(super) const WORKER: &str = "SELECT t.worker_runtime, t.worker_native_id, o.transport,
    e.ended_at IS NULL AND e.instance_id = b.instance_id AND b.fg_pid = o.pid,
    b.status IN ('working', 'blocked')
  FROM task t
  JOIN occupant o ON o.block_lineage = t.worker_lineage AND o.runtime = t.worker_runtime
    AND o.native_id = t.worker_native_id
  JOIN block b ON b.lineage = t.worker_lineage
  JOIN epoch e ON e.id = (SELECT MAX(id) FROM epoch)
  WHERE t.id = ?1";

/// Claims a pending request for sending while its round is still unfinished.
pub(super) static CLAIM: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE request SET state = 'sending' WHERE id = ?1 AND state = 'pending'
           AND EXISTS (SELECT 1 FROM task_round r
                       WHERE r.task_id = request.task_id AND r.round = request.round AND {OPEN})"
    )
});

struct Worker {
    runtime: Runtime,
    native_id: String,
    transport: Map<String, Value>,
    in_pane: bool,
    busy: bool,
}

fn worker(conn: &Connection, task: &str) -> Result<Option<Worker>> {
    Ok(conn
        .prepare_cached(WORKER)?
        .query_row(params![task], |row| {
            let transport: Option<Value> = row.get(2)?;
            Ok(Worker {
                runtime: row.get(0)?,
                native_id: row.get(1)?,
                transport: transport
                    .and_then(|t| t.as_object().cloned())
                    .unwrap_or_default(),
                in_pane: row.get::<_, Option<bool>>(3)?.unwrap_or(false),
                busy: row.get::<_, Option<bool>>(4)?.unwrap_or(false),
            })
        })
        .optional()?)
}

/// Sends each pending request once the worker's conversation is the foreground program of its pane
/// on the current server and between turns, so that it takes the request as a turn of its own.
/// Why a request still waits goes to its `note`, written only when it changes.
pub(super) fn send_due(store: &mut Store, interest: &mut Interest) -> Result<()> {
    let pending: Vec<(String, String, Option<String>, String)> = store
        .conn()
        .prepare_cached(&PENDING)?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, task, note, text) in pending {
        let worker = worker(store.conn(), &task)?;
        let waiting = match &worker {
            Some(w) if w.in_pane && !w.busy => None,
            Some(w) if w.in_pane => Some("waiting for the worker to end its turn"),
            _ => Some("waiting for the worker's conversation to run in its pane"),
        };
        let Some(worker) = worker.filter(|_| waiting.is_none()) else {
            if note.as_deref() != waiting {
                let tx = store.write()?;
                tx.execute(
                    "UPDATE request SET note = ?1 WHERE id = ?2 AND state = 'pending'",
                    params![waiting, id],
                )?;
                tx.commit()?;
            }
            continue;
        };
        let tx = store.write()?;
        let claimed = tx.execute(&CLAIM, params![id])? == 1;
        tx.commit()?;
        if !claimed {
            continue;
        }
        let target = Target {
            native_id: &worker.native_id,
            transport: &worker.transport,
        };
        let message = format!("{} {text}", runtime::token(&id));
        let (state, note) =
            Sent::recorded(worker.runtime.send(&target, store.dir(), &id, &message));
        log::info!("request {id} for task {task}: {state}");
        let tx = store.write()?;
        tx.execute(
            "UPDATE request SET state = ?1, note = ?2 WHERE id = ?3 AND state = 'sending'",
            params![state, note, id],
        )?;
        tx.commit()?;
        if worker.runtime == Runtime::Pi {
            interest
                .files
                .insert(pi::inbox(store.dir(), &worker.native_id));
        }
    }
    settle_pi(store, interest)
}

/// The requests of unfinished rounds sent to Pi workers that Pi has not yet been seen to hold,
/// reached through the rounds as in `PENDING`.
pub(super) static SENT_TO_PI: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT q.id, t.worker_native_id FROM task_round r
         JOIN task t ON t.id = r.task_id
         CROSS JOIN request q ON q.task_id = r.task_id AND q.round = r.round
         WHERE {OPEN} AND t.worker_runtime = 'pi' AND q.state IN ('sent', 'unknown')"
    )
});

/// Marks requests the Pi extension took from the worker's inbox as `accepted`: Pi holds them, but
/// the conversation may not have taken them in yet.
fn settle_pi(store: &mut Store, interest: &mut Interest) -> Result<()> {
    let sent: Vec<(String, String)> = store
        .conn()
        .prepare_cached(&SENT_TO_PI)?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, native_id) in sent {
        interest.files.insert(pi::inbox(store.dir(), &native_id));
        if pi::accepted(store.dir(), &native_id, &id) {
            let tx = store.write()?;
            tx.execute(
                "UPDATE request SET state = 'accepted' WHERE id = ?1 AND state IN ('sent', 'unknown')",
                params![id],
            )?;
            tx.commit()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
