mod untouched;

use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;

use super::{OPEN, block_gone};
use crate::rex::{CallError, Rex, items, root_status, str_at, time_at};
use crate::runtime::Runtime;
use crate::store::{Store, live_of};
use untouched::{Untouched, idle_since, taken, untouched};

pub fn retired_workers(conn: &Connection, state_dir: &Path) -> Result<Vec<(String, String)>> {
    let mut retired: Vec<(String, String)> = Vec::new();
    for id in due(conn)? {
        match untouched(conn, state_dir, &id)? {
            Untouched::Yes(worker) if !retired.iter().any(|(_, w)| *w == worker) => {
                retired.push((id, worker));
            }
            Untouched::Yes(_) | Untouched::No(_) => {}
            Untouched::Unreadable(e) => log::warn!("task {id}: keeping its worker's pane: {e}"),
        }
    }
    Ok(retired)
}

pub fn hold(store: &mut Store, id: &str) -> Result<bool> {
    let tx = store.write()?;
    let held = tx.execute(&CLAIM, params![id])? == 1;
    tx.commit()?;
    Ok(held)
}

pub fn release_held(tx: &Transaction) -> Result<()> {
    tx.execute(
        "UPDATE task SET pane_state = 'open' WHERE pane_state = 'closing'",
        [],
    )?;
    Ok(())
}

pub fn held(conn: &Connection) -> Result<Vec<(String, String)>> {
    Ok(conn
        .prepare("SELECT id, worker_lineage FROM task WHERE pane_state = 'closing'")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn keep(tx: &Transaction, id: &str, reason: &str) -> Result<()> {
    tx.execute(
        "UPDATE task SET pane_state = 'kept', pane_note = ?1 WHERE id = ?2 AND pane_state = 'closing'",
        params![reason, id],
    )?;
    Ok(())
}

pub(super) static DUE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT t.id FROM task t
         WHERE t.keep_pane = 0 AND t.pane_state = 'open' AND t.released_at IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM task o JOIN task_round r ON r.task_id = o.id
                           WHERE o.worker_lineage = t.worker_lineage AND {OPEN})"
    )
});

pub(super) fn due(conn: &Connection) -> Result<Vec<String>> {
    Ok(conn
        .prepare_cached(&DUE)?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

pub(super) static CLAIM: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE task SET pane_state = 'closing'
         WHERE id = ?1 AND keep_pane = 0 AND pane_state = 'open' AND released_at IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM task t JOIN task_round r ON r.task_id = t.id
                           WHERE t.worker_lineage = task.worker_lineage AND {OPEN})"
    )
});

enum Check {
    Gone(String),
    Keep(String),
    Later(String),
}

pub(super) fn close(rex: &mut Rex, store: &mut Store, instance: &str, id: &str) -> Result<String> {
    let worker: String = store.conn().query_row(
        "SELECT worker_lineage FROM task WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    let block = live_of(store.conn(), instance, &worker)?;
    let tx = store.write()?;
    let claimed = tx.execute(&CLAIM, params![id])?;
    tx.commit()?;
    if claimed == 0 {
        return Ok("not closed: its pane is working again or no longer due".into());
    }
    let check = attempt(rex, store.conn(), store.dir(), id, block)
        .unwrap_or_else(|e| Check::Later(format!("{e:#}")));
    let tx = store.write()?;
    let outcome = match check {
        // Recorded now: the close event does not arrive when the server stops first.
        Check::Gone(lineage) => {
            block_gone(&tx, &lineage)?;
            "closed".to_owned()
        }
        Check::Keep(reason) => {
            keep(&tx, id, &reason)?;
            format!("kept: {reason}")
        }
        Check::Later(reason) => {
            tx.execute(
                "UPDATE task SET pane_state = 'open', pane_note = ?1 WHERE id = ?2 AND pane_state = 'closing'",
                params![reason, id],
            )?;
            format!("put off: {reason}")
        }
    };
    tx.commit()?;
    Ok(outcome)
}

fn attempt(
    rex: &mut Rex,
    conn: &Connection,
    state_dir: &Path,
    id: &str,
    block: Option<String>,
) -> Result<Check> {
    let worker = match untouched(conn, state_dir, id)? {
        Untouched::Yes(worker) => worker,
        Untouched::No(reason) => return Ok(Check::Keep(reason)),
        Untouched::Unreadable(reason) => return Ok(Check::Later(reason)),
    };
    let Some(block) = block else {
        return Ok(Check::Gone(worker));
    };
    let Some(session) = rex.block_session(&block)? else {
        return Ok(Check::Gone(worker));
    };
    let taken = taken(conn, id)?;
    if taken.interactive {
        let process = rex.block_method(&session, &block, "process")?;
        let foreground = process
            .pointer("/foreground/pid")
            .and_then(serde_json::Value::as_i64);
        let (runtime, native_id) = taken.conversation.unzip();
        let pid: Option<i64> = conn
            .query_row(
                "SELECT pid FROM occupant WHERE block_lineage = ?1 AND runtime = ?2 AND native_id = ?3",
                params![worker, runtime.map(Runtime::as_str), native_id],
                |row| row.get(0),
            )
            .optional()?;
        if foreground.is_none() || foreground != pid {
            return Ok(Check::Keep(
                "the worker's conversation no longer runs in its pane".into(),
            ));
        }
        let status = rex.block_method(&session, &block, "program_status")?;
        let root = root_status(&status);
        if let Err(reason) = idle_since(
            root.and_then(|r| str_at(r, "/state")),
            root.and_then(|r| time_at(r, "/updated_at")),
            taken.status_at,
        ) {
            return Ok(Check::Keep(reason.into()));
        }
    }
    Ok(match close_unfocused(rex, &session, &block)? {
        None => Check::Gone(worker),
        Some(reason) => Check::Later(reason),
    })
}

pub fn close_unfocused(rex: &mut Rex, session: &str, block: &str) -> Result<Option<String>> {
    let view = rex.call("session.view", &json!({ "session_id": session }))?;
    let active = str_at(&view, "/active_window_id");
    let focused = items(&view, "/windows")
        .iter()
        .find(|w| str_at(w, "/window_id") == active)
        .and_then(|w| str_at(w, "/focused_block_id"));
    if focused == Some(block) {
        return Ok(Some("the worker's pane has focus".into()));
    }
    // Rex has no conditional close, so the pane can change between the last check and this close.
    Ok(
        match rex.call(
            "block.close",
            &json!({ "session_id": session, "block_id": block }),
        ) {
            Ok(_) => None,
            Err(e) if e.not_found() => None,
            Err(e @ (CallError::Unsent(_) | CallError::Rejected { .. })) => {
                Some(format!("the close failed: {e}"))
            }
            // Whether a close without an answer took effect shows in the next check.
            Err(e @ CallError::Unknown(_)) => Some(format!("the close was not answered: {e}")),
        },
    )
}

#[cfg(test)]
mod tests;
