use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value};

use super::{Interest, OPEN};
use crate::runtime::{self, Runtime, Sent, Target, inbox};
use crate::store::{Store, now_ms};

/// SQLite keeps the left table of a `CROSS JOIN` as the outer loop, so `request` is not scanned.
pub(super) static PENDING: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT q.id, q.task_id, q.note, q.text FROM task_round r
         CROSS JOIN request q ON q.task_id = r.task_id AND q.round = r.round
         WHERE {OPEN} AND q.state = 'pending'"
    )
});

pub(super) const WORKER: &str = "SELECT t.worker_runtime, t.worker_native_id, o.transport,
    e.ended_at IS NULL AND e.instance_id = b.instance_id AND b.fg_pid = o.pid
      AND (o.instance_id IS NULL OR o.instance_id = b.instance_id),
    b.status IN ('working', 'blocked')
  FROM task t
  JOIN occupant o ON o.block_lineage = t.worker_lineage AND o.runtime = t.worker_runtime
    AND o.native_id = t.worker_native_id
  JOIN block b ON b.lineage = t.worker_lineage
  JOIN epoch e ON e.id = (SELECT MAX(id) FROM epoch)
  WHERE t.id = ?1";

pub(super) static CLAIM: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE request SET state = 'sending', sent_at = ?2, attempts = attempts + 1
         WHERE id = ?1 AND state = 'pending'
           AND EXISTS (SELECT 1 FROM task_round r
                       WHERE r.task_id = request.task_id AND r.round = request.round AND {OPEN})"
    )
});

pub(super) const ENDED: &str = "the round ended before it was sent";

pub(super) static ROUND_OPEN: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {OPEN} FROM request q
         JOIN task_round r ON r.task_id = q.task_id AND r.round = q.round WHERE q.id = ?1"
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

fn record_send(tx: &Transaction, id: &str, sent: (&str, Option<String>)) -> Result<()> {
    let (mut state, mut note) = sent;
    if state == "pending" && !tx.query_row(&ROUND_OPEN, params![id], |row| row.get::<_, bool>(0))? {
        state = "superseded";
        note = Some(ENDED.to_owned());
    }
    tx.execute(
        "UPDATE request SET state = ?1, note = ?2,
           sent_at = CASE WHEN ?1 IN ('pending', 'superseded') THEN NULL ELSE sent_at END
         WHERE id = ?3 AND state = 'sending'",
        params![state, note, id],
    )?;
    Ok(())
}

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
        let claimed = tx.execute(&CLAIM, params![id, now_ms()])? == 1;
        tx.commit()?;
        if !claimed {
            continue;
        }
        let target = Target {
            native_id: &worker.native_id,
            transport: &worker.transport,
        };
        let message = format!("{} {text}", runtime::token(&id));
        let sent = Sent::recorded(worker.runtime.send(&target, store.dir(), &id, &message));
        log::info!("request {id} for task {task}: {}", sent.0);
        let tx = store.write()?;
        record_send(&tx, &id, sent)?;
        tx.commit()?;
        if let Some(inbox) = worker.runtime.inbox(store.dir(), &worker.native_id) {
            interest.files.insert(inbox);
        }
    }
    settle_inbox(store, interest)
}

pub(super) static SENT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT q.id, t.worker_runtime, t.worker_native_id FROM task_round r
         JOIN task t ON t.id = r.task_id
         CROSS JOIN request q ON q.task_id = r.task_id AND q.round = r.round
         WHERE {OPEN} AND q.state IN ('sent', 'unknown')"
    )
});

fn settle_inbox(store: &mut Store, interest: &mut Interest) -> Result<()> {
    let sent: Vec<(String, Runtime, String)> = store
        .conn()
        .prepare_cached(&SENT)?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, runtime, native_id) in sent {
        let Some(inbox) = runtime.inbox(store.dir(), &native_id) else {
            continue;
        };
        let accepted = inbox::accepted(&inbox, &id);
        interest.files.insert(inbox);
        if accepted {
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
