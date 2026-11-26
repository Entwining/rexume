use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::{Plan, Start};
use crate::runtime::{self, Conversation, Runtime};
use crate::task;
use continuation::continuation;
use reported_first::reported_first;

pub(super) struct Candidate {
    pub start: Start,
    pub conversation: Option<(Runtime, String)>,
    /// The plan reopens the conversation the pane ran on the last instance, so the pane's open
    /// rounds go on from its record.
    pub reopens: bool,
    pub plan: Plan,
}

fn shell(reason: &str) -> Candidate {
    Candidate {
        start: Start::Shell,
        conversation: None,
        reopens: false,
        plan: Plan {
            reason: reason.to_owned(),
            ..Plan::default()
        },
    }
}

struct Occupant {
    runtime: Runtime,
    conversation: Conversation,
    resumable: bool,
    instance_id: Option<String>,
    pid: i64,
    pid_name: Option<String>,
    previous_pid: Option<i64>,
    own_program: Option<bool>,
    start_position: i64,
    reported_at: i64,
    left_fg_changes: Option<i64>,
    record: Option<PathBuf>,
}

fn occupant(conn: &Connection, state_dir: &Path, block: &str) -> Result<Option<Occupant>> {
    Ok(conn
        .query_row(
            "SELECT o.runtime, o.native_id, c.transcript, c.settings, c.resumable,
               o.instance_id, o.pid, o.pid_name, o.previous_pid, o.own_program, o.start_position,
               o.reported_at, o.left_fg_changes
             FROM occupant o JOIN conversation c USING (runtime, native_id)
             WHERE o.block_lineage = ?1",
            params![block],
            |row| {
                let runtime: Runtime = row.get(0)?;
                let conversation = Conversation {
                    native_id: row.get(1)?,
                    transcript: row.get(2)?,
                    settings: row
                        .get::<_, Value>(3)?
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                };
                Ok(Occupant {
                    runtime,
                    record: runtime.record(
                        state_dir,
                        &conversation.native_id,
                        conversation.transcript.as_deref(),
                    ),
                    conversation,
                    resumable: row.get(4)?,
                    instance_id: row.get(5)?,
                    pid: row.get(6)?,
                    pid_name: row.get(7)?,
                    previous_pid: row.get(8)?,
                    own_program: row.get(9)?,
                    start_position: row.get(10)?,
                    reported_at: row.get(11)?,
                    left_fg_changes: row.get(12)?,
                })
            },
        )
        .optional()?)
}

struct LastSeen {
    instance_id: String,
    fg_pid: Option<i64>,
    fg_changes: i64,
    last_exit: Option<i64>,
    status: Option<String>,
    status_pid: Option<i64>,
    status_name: Option<String>,
}

pub(super) fn candidate(
    conn: &Connection,
    state_dir: &Path,
    instance: &str,
    block: &str,
) -> Result<Candidate> {
    let seen: Option<LastSeen> = conn
        .query_row(
            "SELECT instance_id, fg_pid, fg_changes, last_exit, status, status_pid, status_name
             FROM block WHERE lineage = ?1",
            params![block],
            |row| {
                Ok(LastSeen {
                    instance_id: row.get(0)?,
                    fg_pid: row.get(1)?,
                    fg_changes: row.get(2)?,
                    last_exit: row.get(3)?,
                    status: row.get(4)?,
                    status_pid: row.get(5)?,
                    status_name: row.get(6)?,
                })
            },
        )
        .optional()?;
    let Some(seen) = seen else {
        return Ok(shell("the pane was never recorded"));
    };
    if seen.instance_id == instance {
        return Ok(shell("the pane was recorded on this server instance"));
    }
    let occupant = match occupant(conn, state_dir, block)? {
        Some(o) if o.instance_id.as_deref() == Some(instance) => {
            return reported_first(conn, instance, block, &seen, o);
        }
        occupant => occupant,
    };
    let occupant = occupant.filter(|o| {
        o.instance_id
            .as_deref()
            .is_none_or(|i| i == seen.instance_id)
    });
    let Some(mut occupant) = occupant else {
        return Ok(shell(if seen.last_exit.is_some() {
            "the pane's program had exited"
        } else {
            "no conversation recorded"
        }));
    };
    if seen.last_exit.is_none() && seen.fg_pid == Some(occupant.pid) {
        let server_gone = server_gone(conn, &seen.instance_id)?;
        return reopen(
            conn,
            instance,
            block,
            &mut occupant,
            Stood::Foreground(&seen, server_gone),
        );
    }
    // Rex starts nothing in a terminal after its own program exits.
    let exited = if seen.last_exit.is_some() && occupant.own_program != Some(false) {
        "the conversation's program had exited"
    } else {
        match killed(state_dir, &seen, &occupant) {
            Ok(true) => {
                "a signal ended the conversation's program and no other process took the pane since"
            }
            Ok(false) if seen.last_exit.is_some() => {
                return Ok(shell("the pane's program had exited"));
            }
            Ok(false) => {
                return Ok(shell(&format!(
                    "the conversation had already left the pane (its process {}, the foreground {})",
                    occupant.pid,
                    seen.fg_pid.map_or("none".to_owned(), |pid| pid.to_string())
                )));
            }
            Err(e) => {
                return Ok(shell(&format!(
                    "the conversation had already left the pane, and its record could not be read: \
                     {e:#}"
                )));
            }
        }
    };
    if ran_elsewhere(conn, block, &occupant)? {
        return Ok(shell("the conversation ran in another pane since"));
    }
    reopen(conn, instance, block, &mut occupant, Stood::Ended(exited))
}

fn server_gone(conn: &Connection, instance: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COALESCE((SELECT end_reason = 'server_gone' FROM epoch WHERE instance_id = ?1
                          ORDER BY id DESC LIMIT 1), 0)",
        params![instance],
        |row| row.get(0),
    )?)
}

/// A shutdown or logout ends the pane's programs before the server stops, and the server reports
/// each foreground change on the way: the conversation's record shows its process ended on a
/// signal after it reported, and the pane's foreground has not moved since it left that process,
/// except to nothing when the pane's own program ended too.
fn killed(state_dir: &Path, seen: &LastSeen, occupant: &Occupant) -> Result<bool> {
    let Some(left) = occupant.left_fg_changes else {
        return Ok(false);
    };
    let unused = seen.fg_changes == left
        || (seen.last_exit.is_some() && seen.fg_pid.is_none() && seen.fg_changes == left + 1);
    if !unused {
        return Ok(false);
    }
    Ok(occupant
        .runtime
        .killed_at(state_dir, &occupant.conversation, occupant.pid)?
        .is_some_and(|at| at >= occupant.reported_at))
}

/// How the conversation stood when the server's instance ended: still the pane's foreground, with
/// whether the server's end was observed, or ended for the reason given.
enum Stood<'a> {
    Foreground(&'a LastSeen, bool),
    Ended(&'a str),
}

/// A report from another pane moves the conversation's `reported_at` past this occupant's, and
/// that stays after the other pane closes; one from before may still be that pane's foreground.
fn ran_elsewhere(conn: &Connection, block: &str, occupant: &Occupant) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT (SELECT reported_at FROM conversation WHERE runtime = ?1 AND native_id = ?2) > ?4
           OR EXISTS (SELECT 1 FROM occupant o JOIN block b ON b.lineage = o.block_lineage
             WHERE o.runtime = ?1 AND o.native_id = ?2 AND o.block_lineage != ?3
               AND b.fg_pid = o.pid AND b.last_exit IS NULL
               AND (o.instance_id IS NULL OR o.instance_id = b.instance_id))",
        params![
            occupant.runtime.as_str(),
            occupant.conversation.native_id,
            block,
            occupant.reported_at
        ],
        |row| row.get(0),
    )?)
}

fn reopen(
    conn: &Connection,
    instance: &str,
    block: &str,
    occupant: &mut Occupant,
    stood: Stood,
) -> Result<Candidate> {
    if !occupant.resumable {
        return Ok(shell("the recorded program cannot be reopened"));
    }
    if !occupant.runtime.can_reopen(&occupant.conversation) {
        return Ok(shell("the conversation has no record to reopen yet"));
    }
    if let Some(error) = occupant.runtime.launch_error(&occupant.conversation) {
        return Ok(shell(&format!(
            "the conversation's launcher declared no command Rexume can run: {error}"
        )));
    }
    // With an unreadable record the conversation reopens with its reported settings; its runtime
    // reports the error if it cannot reopen it either.
    let mut read = occupant.conversation.clone();
    match occupant.runtime.settings(&mut read) {
        Ok(()) => occupant.conversation = read,
        Err(e) => log::warn!("pane {block}: reopens with the settings it reported: {e:#}"),
    }
    let (start, reason, record_length) = match stood {
        Stood::Foreground(seen, server_gone) => {
            continuation(conn, block, occupant, seen, server_gone)?
        }
        Stood::Ended(why) => (Start::Agent, why.to_owned(), None),
    };
    let prompt = (start == Start::Continue)
        .then(|| prompt(conn, instance, block, occupant))
        .transpose()?;
    Ok(Candidate {
        start,
        conversation: Some((occupant.runtime, occupant.conversation.native_id.clone())),
        reopens: true,
        plan: Plan {
            argv: occupant.runtime.resume_argv(&occupant.conversation),
            env: occupant.runtime.resume_env(&occupant.conversation),
            prompt,
            transcript: occupant.conversation.transcript.clone(),
            record_length,
            reason,
        },
    })
}

fn prompt(conn: &Connection, instance: &str, block: &str, occupant: &Occupant) -> Result<String> {
    Ok(runtime::continuation_prompt(
        instance,
        block,
        &task::unacknowledged(conn, occupant.runtime, &occupant.conversation.native_id)?,
    ))
}

mod continuation;
mod reported_first;
