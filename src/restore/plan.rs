//! Deciding how each pane of a saved session starts: a shell, the reopen command typed, the
//! conversation reopened, or reopened with the one prompt to continue a turn the server's end
//! cut off.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::create;
use crate::runtime::{self, Conversation, Runtime, Turn};
use crate::store::{Store, new_id, now_ms};
use crate::task;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// A login shell, as a new pane gets.
    Shell,
    /// The shell with the reopen command typed but not entered.
    Typed,
    /// The conversation reopened.
    Agent,
    /// The conversation reopened with the one continuation prompt.
    Continue,
}

impl Start {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Typed => "typed",
            Self::Agent => "agent",
            Self::Continue => "continue",
        }
    }

    pub fn runs_conversation(self) -> bool {
        matches!(self, Self::Agent | Self::Continue)
    }
}

impl FromStr for Start {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        Ok(match text {
            "shell" => Self::Shell,
            "typed" => Self::Typed,
            "agent" => Self::Agent,
            "continue" => Self::Continue,
            _ => bail!("unknown start {text}"),
        })
    }
}

impl fmt::Display for Start {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the gate needs to start a slot, stored whole in `restore_slot.plan`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Plan {
    /// The reopen command, without the continuation prompt.
    pub argv: Vec<String>,
    /// The continuation prompt, added to `argv` only when the gate still finds the turn open.
    pub prompt: Option<String>,
    /// The conversation's record, which the gate reads again before it continues a turn.
    pub transcript: Option<String>,
    /// Whether the conversation ran with `REX_SESSION`. Workers start without it, so Rex does not
    /// ask them to label the session, and reopen without it too.
    pub keeps_session_env: bool,
    /// Why the slot starts the way it does, logged with the plan.
    pub reason: String,
}

/// One slot as planned before its insert.
struct Candidate {
    start: Start,
    conversation: Option<(Runtime, String)>,
    plan: Plan,
}

/// Plans one session's restore and returns the operation, or `None` when nothing is left to show.
/// `retired` are the worker panes left out. The conversations' records are read before the
/// transaction, which only decides and writes.
pub fn plan(
    store: &mut Store,
    instance: &str,
    session: &str,
    retired: &[String],
) -> Result<Option<String>> {
    // Taken before the records are read: a report that arrives meanwhile is later than the plan.
    let planned_at = now_ms();
    let conn = store.conn();
    let Some(saved) = create::saved(conn, session, retired)? else {
        return Ok(None);
    };
    let ended: Option<String> = conn
        .query_row(
            "SELECT end_reason FROM epoch WHERE instance_id = ?1 ORDER BY id DESC LIMIT 1",
            params![saved.instance],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let server_gone = ended.as_deref() == Some("server_gone");
    let candidates = saved
        .blocks
        .iter()
        .map(|block| candidate(conn, instance, block, server_gone))
        .collect::<Result<Vec<_>>>()?;
    let tx = store.write()?;
    let planned: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM restore_op WHERE instance_id = ?1 AND session_lineage = ?2)",
        params![instance, session],
        |row| row.get(0),
    )?;
    if planned {
        return Ok(None);
    }
    let op = new_id();
    tx.execute(
        "INSERT INTO restore_op (id, instance_id, session_lineage, state, doc, created_at)
         VALUES (?1, ?2, ?3, 'planned', ?4, ?5)",
        params![op, instance, session, saved.doc, planned_at],
    )?;
    let mut starts = Vec::new();
    for (slot, (block, mut candidate)) in saved.blocks.iter().zip(candidates).enumerate() {
        let slot = i64::try_from(slot)?;
        let insert = |candidate: &Candidate| -> Result<usize> {
            let (runtime, native_id) = candidate
                .conversation
                .as_ref()
                .map(|(r, id)| (r.as_str(), id.as_str()))
                .unzip();
            Ok(tx.execute(
                "INSERT INTO restore_slot (op_id, slot, instance_id, block_lineage, start, runtime,
                   native_id, plan) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (instance_id, runtime, native_id) WHERE start IN ('agent', 'continue')
                 DO NOTHING",
                params![
                    op,
                    slot,
                    instance,
                    block,
                    candidate.start.as_str(),
                    runtime,
                    native_id,
                    serde_json::to_string(&candidate.plan)?
                ],
            )?)
        };
        // Another slot on this instance already reopens the conversation: one process runs it.
        if insert(&candidate)? == 0 {
            candidate.start = Start::Typed;
            candidate.plan.prompt = None;
            candidate.plan.reason = "another pane reopens this conversation".into();
            insert(&candidate)?;
        }
        starts.push((block, candidate.start, candidate.plan.reason));
    }
    tx.commit()?;
    for (block, start, reason) in starts {
        log::info!("restore {op}: pane {block}: {}: {reason}", start.as_str());
    }
    Ok(Some(op))
}

struct Occupant {
    runtime: Runtime,
    conversation: Conversation,
    resumable: bool,
    keeps_session_env: bool,
    pid: i64,
    pid_name: Option<String>,
}

fn occupant(conn: &Connection, block: &str) -> Result<Option<Occupant>> {
    let occupant = conn
        .query_row(
            "SELECT o.runtime, o.native_id, c.transcript, c.settings, c.resumable,
               o.keeps_session_env, o.pid, o.pid_name
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
                    pid: row.get(6)?,
                    pid_name: row.get(7)?,
                })
            },
        )
        .optional()?;
    let Some(mut occupant) = occupant else {
        return Ok(None);
    };
    occupant.runtime.settings(&mut occupant.conversation)?;
    Ok(Some(occupant))
}

struct LastSeen {
    fg_pid: Option<i64>,
    status: Option<String>,
    status_pid: Option<i64>,
    status_name: Option<String>,
}

fn candidate(
    conn: &Connection,
    instance: &str,
    block: &str,
    server_gone: bool,
) -> Result<Candidate> {
    let shell = |reason: &str| Candidate {
        start: Start::Shell,
        conversation: None,
        plan: Plan {
            reason: reason.to_owned(),
            ..Plan::default()
        },
    };
    let Some(occupant) = occupant(conn, block)? else {
        return Ok(shell("no conversation recorded"));
    };
    if !occupant.resumable {
        return Ok(shell("the recorded program cannot be reopened"));
    }
    if !occupant.runtime.can_reopen(&occupant.conversation) {
        return Ok(shell("the conversation has no record to reopen yet"));
    }
    let seen: Option<LastSeen> = conn
        .query_row(
            "SELECT fg_pid, status, status_pid, status_name FROM block WHERE lineage = ?1",
            params![block],
            |row| {
                Ok(LastSeen {
                    fg_pid: row.get(0)?,
                    status: row.get(1)?,
                    status_pid: row.get(2)?,
                    status_name: row.get(3)?,
                })
            },
        )
        .optional()?;
    let mut candidate = Candidate {
        start: Start::Typed,
        conversation: Some((occupant.runtime, occupant.conversation.native_id.clone())),
        plan: Plan {
            argv: occupant.runtime.resume_argv(&occupant.conversation),
            prompt: None,
            transcript: occupant.conversation.transcript.clone(),
            keeps_session_env: occupant.keeps_session_env,
            reason: String::new(),
        },
    };
    let Some(seen) = seen.as_ref().filter(|s| s.fg_pid == Some(occupant.pid)) else {
        candidate.plan.reason = "the conversation had already left the pane".into();
        return Ok(candidate);
    };
    let (start, reason) = continuation(&occupant, seen, server_gone)?;
    candidate.start = start;
    candidate.plan.reason = reason;
    if start == Start::Continue {
        candidate.plan.prompt = Some(runtime::continuation_prompt(
            &format!("{instance}/{block}"),
            &task::unacknowledged(conn, occupant.runtime, &occupant.conversation.native_id)?,
        ));
    }
    Ok(candidate)
}

/// A turn continues only when the server's end is what stopped it: the server was watched until
/// it went away, the pane's program was still this conversation's process and was working, and
/// the conversation's own record shows a prompt that nothing ended. A cancel or answer the
/// runtime recorded wins over a status event the server never delivered.
fn continuation(
    occupant: &Occupant,
    seen: &LastSeen,
    server_gone: bool,
) -> Result<(Start, String)> {
    let agent = |reason: &str| Ok((Start::Agent, reason.to_owned()));
    if !server_gone {
        return agent("the server's end was not observed");
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
    match runtime::last_turn(occupant.runtime, Path::new(transcript), 0)? {
        Some(Turn::Open { .. }) => Ok((
            Start::Continue,
            "the server's end cut off a running turn".into(),
        )),
        Some(turn) => agent(turn.describe()),
        None => agent("the record has a line that is not JSON"),
    }
}

#[cfg(test)]
mod tests;
