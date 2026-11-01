//! Closing a released disposable worker's pane, only while nothing has happened in it since its
//! latest result. The daemon is the only closer: it claims the pane (`closing`) in a short
//! transaction before it calls Rex, so `add` cannot hand the pane new work meanwhile, and a
//! released task starts no more rounds.
//! Rex has no conditional close, so a change in the moment between the last check and the close
//! is not excluded.

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

/// Released disposable workers whose panes, as last recorded, still show the worker as its latest
/// result left it, with the task to close each by. Run before a later instance's records replace
/// the earlier one's; any other pane, including one the user took over after the result or one
/// whose worker's record cannot be read, stays.
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

/// Holds a retired worker's pane, which the server restored, for the restore to close: `add`
/// hands it no new work and the close pass leaves it alone. Returns whether it was held; a pane
/// working again is not.
pub fn hold(store: &mut Store, id: &str) -> Result<bool> {
    let tx = store.write()?;
    let held = tx.execute(&CLAIM, params![id])? == 1;
    tx.commit()?;
    Ok(held)
}

/// Lets go of every held pane, which the next pass checks again: a restore holds a pane for one
/// epoch, and a close attempt for one pass.
pub fn release_held(tx: &Transaction) -> Result<()> {
    tx.execute(
        "UPDATE task SET pane_state = 'open' WHERE pane_state = 'closing'",
        [],
    )?;
    Ok(())
}

/// The tasks whose worker's pane is held, with the pane. Only a restore holds a pane across
/// passes, and the daemon lets go of every one when it starts and when an epoch starts.
pub fn held(conn: &Connection) -> Result<Vec<(String, String)>> {
    Ok(conn
        .prepare("SELECT id, worker_lineage FROM task WHERE pane_state = 'closing'")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Gives up a held pane for good: someone uses it.
pub fn keep(tx: &Transaction, id: &str, reason: &str) -> Result<()> {
    tx.execute(
        "UPDATE task SET pane_state = 'kept', pane_note = ?1 WHERE id = ?2 AND pane_state = 'closing'",
        params![reason, id],
    )?;
    Ok(())
}

/// Released disposable workers whose panes Rexume may still close: no round of any task on the
/// same pane is unfinished.
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

/// Claims one due task's pane for a close attempt, unless a round of any task on the same pane
/// has not finished: that pane is working again.
pub(super) static CLAIM: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE task SET pane_state = 'closing'
         WHERE id = ?1 AND keep_pane = 0 AND pane_state = 'open' AND released_at IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM task t JOIN task_round r ON r.task_id = t.id
                           WHERE t.worker_lineage = task.worker_lineage AND {OPEN})"
    )
});

/// How a close attempt ended.
enum Check {
    /// The pane is no longer on the server, closed by this attempt or before it.
    Gone(String),
    /// Someone used the worker after its result; it is theirs now.
    Keep(String),
    /// Try again once the server's view changes.
    Later(String),
}

/// Closes a released disposable worker's pane only while it still shows the worker as the
/// result left it, as recorded and, for an interactive worker, as the server reports it now, and
/// is not the pane the user is looking at. A pane found in use is kept for good; a close put off
/// goes back to `open` with the reason in `pane_note`. Returns what happened, for the log.
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

/// Closes a worker's pane unless it is its session's focused pane, which the user may be looking
/// at. Returns `None` once the pane is gone, or why it is still there.
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
