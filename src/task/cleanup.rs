//! Closing a released disposable worker's pane, only while nothing has happened in it since its
//! latest result. The daemon is the only closer: it claims the pane (`closing`) in a short
//! transaction before it calls Rex, so `add` cannot hand the pane new work meanwhile, and a
//! released task starts no more rounds.
//! Rex has no conditional close, so a change in the moment between the last check and the close
//! is not excluded.

use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use super::{LATEST_ROUND, OPEN, block_gone};
use crate::rex::{CallError, Rex, items, root_status, str_at, time_at};
use crate::runtime::{Runtime, Turn, last_turn};
use crate::store::{Store, live_of};

/// Released disposable workers whose panes, as last recorded, still show the worker as its latest
/// result left it. A restore leaves these out; any other pane, including one the user took over
/// after the result, comes back.
pub fn retired_workers(conn: &Connection) -> Result<Vec<String>> {
    let mut retired = Vec::new();
    for id in due(conn)? {
        if let Untouched::Yes(worker) = untouched(conn, &id)? {
            retired.push(worker);
        }
    }
    Ok(retired)
}

/// Released disposable workers whose panes rexume may still close: no round of any task on the
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

/// What the latest round recorded when it finished, and where the task stands in its worker's
/// record.
struct Taken {
    interactive: bool,
    worker: String,
    worker_pid: Option<i64>,
    finished_at: Option<i64>,
    status_at: Option<i64>,
    generation: Option<i64>,
    position: i64,
    conversation: Option<(Runtime, String)>,
}

fn taken(conn: &Connection, id: &str) -> Result<Taken> {
    Ok(conn.query_row(
        &format!(
            "SELECT t.result_file IS NULL, t.worker_lineage, t.worker_pid,
               r.finished_at, r.finished_status_at, r.finished_generation, t.position,
               t.worker_runtime, t.worker_native_id
             FROM task t JOIN task_round r ON r.task_id = t.id WHERE t.id = ?1 AND {LATEST_ROUND}"
        ),
        params![id],
        |row| {
            let runtime: Option<Runtime> = row.get(7)?;
            let native_id: Option<String> = row.get(8)?;
            Ok(Taken {
                interactive: row.get(0)?,
                worker: row.get(1)?,
                worker_pid: row.get(2)?,
                finished_at: row.get(3)?,
                status_at: row.get(4)?,
                generation: row.get(5)?,
                position: row.get(6)?,
                conversation: runtime.zip(native_id),
            })
        },
    )?)
}

enum Untouched {
    Yes(String),
    No(String),
}

/// Whether the worker's pane, as recorded, is unchanged since its result: the same foreground
/// generation (or none observed on either side) and status, the task's conversation still the
/// pane's occupant and not reported since, and no turn in its record after the task's position.
fn untouched(conn: &Connection, id: &str) -> Result<Untouched> {
    let taken = taken(conn, id)?;
    let no = |reason: &str| Ok(Untouched::No(reason.to_owned()));
    let Some(finished_at) = taken.finished_at else {
        return no("the task has no result");
    };
    let pane: Option<(Option<i64>, Option<i64>)> = conn
        .query_row(
            "SELECT status_at, fg_generation FROM block WHERE lineage = ?1",
            params![taken.worker],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((status_at, generation)) = pane else {
        return no("the worker's pane is not recorded");
    };
    if generation != taken.generation {
        return no("another program ran in the worker's pane after its result");
    }
    if !taken.interactive {
        return Ok(Untouched::Yes(taken.worker));
    }
    if status_at != taken.status_at {
        return no("the worker's status changed after its result");
    }
    let Some((runtime, native_id)) = taken.conversation else {
        return no("the worker's conversation is not recorded");
    };
    let occupant: Option<(Runtime, String, i64, Option<String>)> = conn
        .query_row(
            "SELECT o.runtime, o.native_id, o.reported_at, c.transcript FROM occupant o
             JOIN conversation c ON c.runtime = o.runtime AND c.native_id = o.native_id
             WHERE o.block_lineage = ?1",
            params![taken.worker],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((o_runtime, o_native_id, reported_at, transcript)) = occupant else {
        return no("the worker's conversation is not recorded");
    };
    if o_runtime != runtime || o_native_id != native_id || reported_at > finished_at {
        return no("a conversation started in the worker's pane after its result");
    }
    if let Some(transcript) = transcript {
        let turn = last_turn(
            runtime,
            Path::new(&transcript),
            u64::try_from(taken.position)?,
        )?;
        if turn != Some(Turn::None) {
            return no("the worker has a turn after its result");
        }
    }
    Ok(Untouched::Yes(taken.worker))
}

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
/// result left it, both as recorded and as the server reports it now, and is not the pane the
/// user is looking at. A pane found in use is kept for good; a close put off goes back to `open`
/// with the reason in `pane_note`. Returns what happened, for the log.
pub(super) fn close(rex: &mut Rex, store: &mut Store, instance: &str, id: &str) -> Result<String> {
    let tx = store.write()?;
    let claimed = tx.execute(&CLAIM, params![id])?;
    tx.commit()?;
    if claimed == 0 {
        return Ok("not closed: its pane is working again or no longer due".into());
    }
    let check =
        attempt(rex, store.conn(), instance, id).unwrap_or_else(|e| Check::Later(format!("{e:#}")));
    let tx = store.write()?;
    let outcome = match check {
        // The saved layout loses the pane now, so a restore that starts before the close event
        // arrives does not bring it back.
        Check::Gone(lineage) => {
            block_gone(&tx, &lineage)?;
            "closed".to_owned()
        }
        Check::Keep(reason) => {
            tx.execute(
                "UPDATE task SET pane_state = 'kept', pane_note = ?1 WHERE id = ?2 AND pane_state = 'closing'",
                params![reason, id],
            )?;
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

fn attempt(rex: &mut Rex, conn: &Connection, instance: &str, id: &str) -> Result<Check> {
    let worker = match untouched(conn, id)? {
        Untouched::Yes(worker) => worker,
        Untouched::No(reason) => return Ok(Check::Keep(reason)),
    };
    // A restore leaves an untouched worker out, so on a later server instance it has no pane.
    let Some(block) = live_of(conn, instance, &worker)? else {
        return Ok(Check::Gone(worker));
    };
    let Some(session) = rex.block_session(&block)? else {
        return Ok(Check::Gone(worker));
    };
    let taken = taken(conn, id)?;
    let process = rex.block_method(&session, &block, "process")?;
    let foreground = process
        .pointer("/foreground/pid")
        .and_then(serde_json::Value::as_i64);
    if taken.interactive {
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
        if root_status(&status).and_then(|r| time_at(r, "/updated_at")) != taken.status_at {
            return Ok(Check::Keep(
                "the worker's status changed after its result".into(),
            ));
        }
    } else if foreground.is_some() && foreground != taken.worker_pid {
        return Ok(Check::Keep(
            "another program runs in the worker's pane".into(),
        ));
    }
    let view = rex.call("session.view", &json!({ "session_id": session }))?;
    let active = str_at(&view, "/active_window_id");
    let focused = items(&view, "/windows")
        .iter()
        .find(|w| str_at(w, "/window_id") == active)
        .and_then(|w| str_at(w, "/focused_block_id"));
    if focused == Some(block.as_str()) {
        return Ok(Check::Later("the worker's pane has focus".into()));
    }
    Ok(
        match rex.call(
            "block.close",
            &json!({ "session_id": session, "block_id": block }),
        ) {
            Ok(_) => Check::Gone(worker),
            Err(e) if e.not_found() => Check::Gone(worker),
            Err(e @ (CallError::Unsent(_) | CallError::Rejected { .. })) => {
                Check::Later(format!("the close failed: {e}"))
            }
            // Whether a close without an answer took effect shows in the next check.
            Err(e @ CallError::Unknown(_)) => {
                Check::Later(format!("the close was not answered: {e}"))
            }
        },
    )
}

#[cfg(test)]
mod tests;
