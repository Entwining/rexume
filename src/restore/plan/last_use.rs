//! How one pane starts, by what it was last used for as recorded.

use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::{Plan, Start};
use crate::runtime::{self, Conversation, Runtime, Turn};
use crate::task;

/// One slot as planned before its insert.
pub(super) struct Candidate {
    pub start: Start,
    pub conversation: Option<(Runtime, String)>,
    pub plan: Plan,
}

pub(super) fn shell(reason: &str) -> Candidate {
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
}

fn occupant(conn: &Connection, block: &str) -> Result<Option<Occupant>> {
    Ok(conn
        .query_row(
            "SELECT o.runtime, o.native_id, c.transcript, c.settings, c.resumable,
               o.keeps_session_env, o.instance_id, o.pid, o.pid_name, o.own_program, o.reported_at
             FROM occupant o JOIN conversation c USING (runtime, native_id)
             WHERE o.block_lineage = ?1",
            params![block],
            |row| {
                Ok(Occupant {
                    runtime: row.get(0)?,
                    conversation: Conversation {
                        native_id: row.get(1)?,
                        transcript: row.get(2)?,
                        settings: row
                            .get::<_, Value>(3)?
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    },
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

/// Plans a pane by its last use as recorded: an exited terminal stays exited unless its own
/// program was, or may have been, the conversation, which reopens; a conversation still the
/// foreground program reopens; anything else gets a shell. The occupant counts only from the
/// instance the pane's record describes, where its PID names a process.
pub(super) fn candidate(
    conn: &Connection,
    instance: &str,
    block: &str,
    server_gone: bool,
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
    let occupant = occupant(conn, block)?.filter(|o| {
        o.instance_id
            .as_deref()
            .is_none_or(|i| i == seen.instance_id)
    });
    // Rex starts nothing in a terminal after its own program exits. A conversation reported
    // before version 3 may have been that program; it reopens from a shell.
    if let Some(code) = seen.last_exit {
        let occupant = match occupant.filter(|o| o.own_program != Some(false)) {
            Some(o) if !ran_elsewhere(conn, block, &o)? => Some(o),
            _ => None,
        };
        return match occupant {
            Some(mut occupant) => reopen(conn, instance, block, &mut occupant, None),
            None => Ok(Candidate {
                start: Start::Exited,
                conversation: None,
                plan: Plan {
                    own_program: true,
                    exit_code: Some(code),
                    reason: "the pane's program had exited".into(),
                    ..Plan::default()
                },
            }),
        };
    }
    let Some(mut occupant) = occupant else {
        return Ok(shell("no conversation recorded"));
    };
    if seen.fg_pid != Some(occupant.pid) {
        return Ok(shell("the conversation had already left the pane"));
    }
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
    let (start, reason) = match running {
        Some((seen, server_gone)) => continuation(conn, block, occupant, seen, server_gone)?,
        None => (
            Start::Agent,
            "the conversation's program had exited".to_owned(),
        ),
    };
    let prompt = (start == Start::Continue)
        .then(|| -> Result<String> {
            Ok(runtime::continuation_prompt(
                &format!("{instance}/{block}"),
                &task::unacknowledged(conn, occupant.runtime, &occupant.conversation.native_id)?,
            ))
        })
        .transpose()?;
    Ok(Candidate {
        start,
        conversation: Some((occupant.runtime, occupant.conversation.native_id.clone())),
        plan: Plan {
            argv: occupant.runtime.resume_argv(&occupant.conversation),
            prompt,
            transcript: occupant.conversation.transcript.clone(),
            keeps_session_env: occupant.keeps_session_env,
            own_program: occupant.own_program == Some(true),
            exit_code: None,
            reason,
        },
    })
}

/// A turn continues only when the server's end is what stopped it: the server was watched until
/// it went away, the pane's program was still this conversation's process and was working, and
/// the conversation's own record shows a prompt that nothing ended. A cancel or answer the
/// runtime recorded wins over a status event the server never delivered. A non-interactive run
/// does not continue: its task already ended with the server, and a prompt would not bring back
/// its result file.
fn continuation(
    conn: &Connection,
    block: &str,
    occupant: &Occupant,
    seen: &LastSeen,
    server_gone: bool,
) -> Result<(Start, String)> {
    let agent = |reason: &str| Ok((Start::Agent, reason.to_owned()));
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
    let Some(transcript) = &occupant.conversation.transcript else {
        return agent("no conversation record to check");
    };
    match runtime::last_turn(occupant.runtime, Path::new(transcript), 0) {
        Ok(Some(Turn::Open { .. })) => Ok((
            Start::Continue,
            "the server's end cut off a running turn".into(),
        )),
        Ok(Some(turn)) => agent(turn.describe()),
        Ok(None) => agent("the record has a line that is not JSON"),
        Err(e) => agent(&format!("the record could not be read: {e:#}")),
    }
}
