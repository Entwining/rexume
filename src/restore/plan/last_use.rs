//! How one pane starts, by what it was last used for as recorded.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::{Plan, Start};
use crate::runtime::{self, Conversation, Runtime, Turn};
use crate::{report, task};

/// How one pane starts, as planned before its insert: a conversation exactly when it reopens one.
pub(super) struct Candidate {
    pub start: Start,
    pub conversation: Option<(Runtime, String)>,
    pub plan: Plan,
}

fn shell(reason: &str) -> Candidate {
    Candidate {
        start: Start::Shell,
        conversation: None,
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
    keeps_session_env: bool,
    instance_id: Option<String>,
    pid: i64,
    pid_name: Option<String>,
    own_program: Option<bool>,
    reported_at: i64,
    /// The record its turns are read from (`Runtime::record`).
    record: Option<PathBuf>,
}

fn occupant(conn: &Connection, state_dir: &Path, block: &str) -> Result<Option<Occupant>> {
    Ok(conn
        .query_row(
            "SELECT o.runtime, o.native_id, c.transcript, c.settings, c.resumable,
               o.keeps_session_env, o.instance_id, o.pid, o.pid_name, o.own_program, o.reported_at
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
                    keeps_session_env: row.get(5)?,
                    instance_id: row.get(6)?,
                    pid: row.get(7)?,
                    pid_name: row.get(8)?,
                    own_program: row.get(9)?,
                    reported_at: row.get(10)?,
                })
            },
        )
        .optional()?)
}

struct LastSeen {
    instance_id: String,
    fg_pid: Option<i64>,
    last_exit: Option<i64>,
    status: Option<String>,
    status_pid: Option<i64>,
    status_name: Option<String>,
}

/// Plans a pane by its last use as recorded on an earlier instance: a conversation that was the
/// terminal's own program, or may have been, reopens after it exited; a conversation still the
/// foreground program reopens; anything else keeps the shell. The occupant counts only from the
/// instance the pane's record describes, where its PID names a process.
pub(super) fn candidate(
    conn: &Connection,
    state_dir: &Path,
    instance: &str,
    block: &str,
) -> Result<Candidate> {
    let seen: Option<LastSeen> = conn
        .query_row(
            "SELECT instance_id, fg_pid, last_exit, status, status_pid, status_name FROM block
             WHERE lineage = ?1",
            params![block],
            |row| {
                Ok(LastSeen {
                    instance_id: row.get(0)?,
                    fg_pid: row.get(1)?,
                    last_exit: row.get(2)?,
                    status: row.get(3)?,
                    status_pid: row.get(4)?,
                    status_name: row.get(5)?,
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
    let occupant = occupant(conn, state_dir, block)?.filter(|o| {
        o.instance_id
            .as_deref()
            .is_none_or(|i| i == seen.instance_id)
    });
    // Rex starts nothing in a terminal after its own program exits. A conversation reported
    // before version 3 may have been that program.
    if seen.last_exit.is_some() {
        return match occupant.filter(|o| o.own_program != Some(false)) {
            Some(mut o) if !ran_elsewhere(conn, block, &o)? => {
                reopen(conn, instance, block, &mut o, None)
            }
            _ => Ok(shell("the pane's program had exited")),
        };
    }
    let Some(mut occupant) = occupant else {
        return Ok(shell("no conversation recorded"));
    };
    if seen.fg_pid != Some(occupant.pid) {
        return Ok(shell("the conversation had already left the pane"));
    }
    // Only a server whose end was observed left its last records as its final state.
    let ended: Option<String> = conn
        .query_row(
            "SELECT end_reason FROM epoch WHERE instance_id = ?1 ORDER BY id DESC LIMIT 1",
            params![seen.instance_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let server_gone = ended.as_deref() == Some("server_gone");
    reopen(
        conn,
        instance,
        block,
        &mut occupant,
        Some((&seen, server_gone)),
    )
}

/// Whether the conversation, after it exited from `block`, ran in another pane: it reported from
/// there later, or it was still that pane's foreground program. That pane reopens it.
fn ran_elsewhere(conn: &Connection, block: &str, occupant: &Occupant) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM occupant o LEFT JOIN block b ON b.lineage = o.block_lineage
           WHERE o.runtime = ?1 AND o.native_id = ?2 AND o.block_lineage != ?3
             AND (o.reported_at > ?4 OR (b.fg_pid = o.pid AND b.last_exit IS NULL
                  AND (o.instance_id IS NULL OR o.instance_id = b.instance_id))))",
        params![
            occupant.runtime.as_str(),
            occupant.conversation.native_id,
            block,
            occupant.reported_at
        ],
        |row| row.get(0),
    )?)
}

/// Reopens the occupant's conversation, continuing it only when `running`, the pane's last record
/// with whether the server's end was observed, shows a turn the end cut off.
fn reopen(
    conn: &Connection,
    instance: &str,
    block: &str,
    occupant: &mut Occupant,
    running: Option<(&LastSeen, bool)>,
) -> Result<Candidate> {
    if !occupant.resumable {
        return Ok(shell("the recorded program cannot be reopened"));
    }
    if !occupant.runtime.can_reopen(&occupant.conversation) {
        return Ok(shell("the conversation has no record to reopen yet"));
    }
    // A record that cannot be read leaves the settings the conversation reported, as one that
    // shows none does; its runtime reports the error if it cannot reopen it either.
    let mut read = occupant.conversation.clone();
    match occupant.runtime.settings(&mut read) {
        Ok(()) => occupant.conversation = read,
        Err(e) => log::warn!("pane {block}: reopens with the settings it reported: {e:#}"),
    }
    let (start, reason, record_length) = match running {
        Some((seen, server_gone)) => continuation(conn, block, occupant, seen, server_gone)?,
        None => (
            Start::Agent,
            "the conversation's program had exited".to_owned(),
            None,
        ),
    };
    let prompt = (start == Start::Continue)
        .then(|| -> Result<String> {
            Ok(runtime::continuation_prompt(
                instance,
                block,
                &task::unacknowledged(conn, occupant.runtime, &occupant.conversation.native_id)?,
            ))
        })
        .transpose()?;
    Ok(Candidate {
        start,
        conversation: Some((occupant.runtime, occupant.conversation.native_id.clone())),
        plan: Plan {
            argv: occupant.runtime.resume_argv(&occupant.conversation),
            env: occupant.runtime.resume_env(&occupant.conversation),
            prompt,
            transcript: occupant.conversation.transcript.clone(),
            record_length,
            keeps_session_env: occupant.keeps_session_env,
            reason,
        },
    })
}

/// A turn continues only when the server's end is what stopped it: the server was watched until
/// it went away, the pane's program was still this conversation's process and was working, and
/// the conversation's record shows a prompt that nothing ended. An end the record shows wins over
/// a status event the server never delivered, except for a runtime that can work on after the end
/// its record shows (`Runtime::works_after_end`): there the working status wins, and a server that
/// stopped between that end and the status after it gets one prompt too many. A non-interactive
/// run does not continue: its task already ended with the server, and a prompt would not bring
/// back its result file. A continuation comes with the record's length as read, from which the
/// gate checks that no turn ended since.
fn continuation(
    conn: &Connection,
    block: &str,
    occupant: &Occupant,
    seen: &LastSeen,
    server_gone: bool,
) -> Result<(Start, String, Option<u64>)> {
    let agent = |reason: &str| Ok((Start::Agent, reason.to_owned(), None));
    if !server_gone {
        return agent("the server's end was not observed");
    }
    if task::is_run(conn, block, &seen.instance_id, occupant.pid)? {
        return agent("the conversation was a non-interactive run");
    }
    let same_process = seen.status_pid == Some(occupant.pid)
        && (occupant.pid_name.is_none() || seen.status_name == occupant.pid_name);
    if !same_process {
        return agent("the last status came from another process");
    }
    match seen.status.as_deref() {
        Some("working") => {}
        Some("blocked") => return agent("the conversation was waiting for the user"),
        other => return agent(&format!("the last status was {}", other.unwrap_or("none"))),
    }
    let Some(record) = &occupant.record else {
        return agent("no conversation record to check");
    };
    let length = match report::record_length(record) {
        Ok(length) => length,
        Err(e) => return agent(&format!("the record could not be read: {e:#}")),
    };
    let reason = match runtime::last_turn(occupant.runtime, record, 0) {
        Ok(Some(Turn::Open)) => "the server's end cut off a running turn",
        Ok(Some(Turn::Completed { .. })) if occupant.runtime.works_after_end() => {
            "the server's end cut off work that went on after the turn's end"
        }
        Ok(Some(turn)) => return agent(turn.describe()),
        Ok(None) => return agent("the record has a line that is not JSON"),
        Err(e) => return agent(&format!("the record could not be read: {e:#}")),
    };
    Ok((Start::Continue, reason.into(), Some(length)))
}
