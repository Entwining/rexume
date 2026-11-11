use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::super::CURRENT_ROUND;
use crate::runtime::{Runtime, Turn, last_turn};

pub(super) struct Taken {
    pub(super) interactive: bool,
    pub(super) worker: String,
    pub(super) worker_instance: Option<String>,
    pub(super) finished_at: Option<i64>,
    pub(super) finished_instance: Option<String>,
    pub(super) status_at: Option<i64>,
    pub(super) fg_changes: Option<i64>,
    pub(super) position: i64,
    pub(super) conversation: Option<(Runtime, String)>,
}

pub(super) fn taken(conn: &Connection, id: &str) -> Result<Taken> {
    Ok(conn.query_row(
        &format!(
            "SELECT t.result_file IS NULL, t.worker_lineage,
               r.finished_at, r.finished_status_at, r.finished_fg_changes, t.position,
               t.worker_runtime, t.worker_native_id, t.worker_instance_id, r.finished_instance_id
             FROM task t JOIN task_round r ON r.task_id = t.id
             WHERE t.id = ?1 AND r.round = {current}",
            current = *CURRENT_ROUND
        ),
        params![id],
        |row| {
            let runtime: Option<Runtime> = row.get(6)?;
            let native_id: Option<String> = row.get(7)?;
            Ok(Taken {
                interactive: row.get(0)?,
                worker: row.get(1)?,
                worker_instance: row.get(8)?,
                finished_at: row.get(2)?,
                finished_instance: row.get(9)?,
                status_at: row.get(3)?,
                fg_changes: row.get(4)?,
                position: row.get(5)?,
                conversation: runtime.zip(native_id),
            })
        },
    )?)
}

pub(super) enum Untouched {
    Yes(String),
    No(String),
    Unreadable(String),
}

pub(super) fn untouched(conn: &Connection, state_dir: &Path, id: &str) -> Result<Untouched> {
    let taken = taken(conn, id)?;
    let no = |reason: &str| Ok(Untouched::No(reason.to_owned()));
    let Some(finished_at) = taken.finished_at else {
        return no("the task has no result");
    };
    type Pane = (String, Option<String>, Option<i64>, i64, Option<i64>);
    let pane: Option<Pane> = conn
        .query_row(
            "SELECT instance_id, status, status_at, fg_changes, last_exit FROM block
             WHERE lineage = ?1",
            params![taken.worker],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((instance, status, status_at, fg_changes, last_exit)) = pane else {
        return no("the worker's pane is not recorded");
    };
    // A round the upgrade to version 3 could not date counts as finished on the task's instance,
    // which at worst keeps a pane it finished on a later one.
    let finished_on = taken.finished_instance.or(taken.worker_instance);
    if finished_on.is_some_and(|at| at != instance) {
        return no("the worker's pane was restored after its result");
    }
    // Rex starts nothing in a terminal whose program exited, so for a run the recorded exit alone
    // shows the pane unused.
    if !taken.interactive {
        if last_exit.is_none() {
            return no("the worker's pane was restored after its run");
        }
        return Ok(Untouched::Yes(taken.worker));
    }
    if taken.fg_changes != Some(fg_changes) {
        return no("another program ran in the worker's pane after its result");
    }
    if let Err(reason) = idle_since(status.as_deref(), status_at, taken.status_at) {
        return no(reason);
    }
    let Some((runtime, native_id)) = taken.conversation else {
        return no("the worker's conversation is not recorded");
    };
    let occupant: Option<(Runtime, String, i64, Option<String>)> = conn
        .query_row(
            "SELECT o.runtime, o.native_id, o.reported_at, c.transcript FROM occupant o
             JOIN conversation c ON c.runtime = o.runtime AND c.native_id = o.native_id
             WHERE o.block_lineage = ?1 AND (o.instance_id IS NULL OR o.instance_id = ?2)",
            params![taken.worker, instance],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((o_runtime, o_native_id, reported_at, transcript)) = occupant else {
        return no("no conversation reported in the worker's pane on its instance");
    };
    if o_runtime != runtime || o_native_id != native_id || reported_at > finished_at {
        return no("a conversation started in the worker's pane after its result");
    }
    if let Some(record) = runtime.record(state_dir, &native_id, transcript.as_deref()) {
        match last_turn(runtime, &record, u64::try_from(taken.position)?) {
            Ok(Some(Turn::None)) => {}
            Ok(_) => return no("the worker has a turn after its result"),
            Err(e) => return Ok(Untouched::Unreadable(format!("{e:#}"))),
        }
    }
    Ok(Untouched::Yes(taken.worker))
}

/// Claude Code reports `done` after its Stop hooks, which can be after the round took the turn.
pub(super) fn idle_since(
    state: Option<&str>,
    at: Option<i64>,
    finished: Option<i64>,
) -> Result<(), &'static str> {
    match state {
        Some("working" | "blocked") => Err("the worker is still working after its result"),
        _ if at == finished => Ok(()),
        Some("done") => Ok(()),
        _ => Err("the worker's status changed after its result"),
    }
}
