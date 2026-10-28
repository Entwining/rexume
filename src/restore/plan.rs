//! Deciding how each pane of a saved session starts, by what the pane was last used for: a shell,
//! the exit its program had shown, the conversation reopened, or reopened with the one prompt to
//! continue a turn the server's end cut off.

use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::create;
use crate::store::{Store, new_id, now_ms};
use last_use::{Candidate, candidate, shell};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// A login shell, as a new pane gets.
    Shell,
    /// The pane's program exits again with the status it had exited with.
    Exited,
    /// The conversation reopened.
    Agent,
    /// The conversation reopened with the one continuation prompt.
    Continue,
}

impl Start {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Exited => "exited",
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
            "exited" => Self::Exited,
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
    /// Variables the reopen command runs with, by name and value.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// The continuation prompt, added to `argv` only when the gate still finds the turn open.
    pub prompt: Option<String>,
    /// The conversation's record, which the gate reads again before it continues a turn.
    pub transcript: Option<String>,
    /// Whether the conversation ran with `REX_SESSION`. Workers start without it, so Rex does not
    /// ask them to label the session, and reopen without it too.
    pub keeps_session_env: bool,
    /// Whether the program was the terminal's own rather than one started from its shell. It is
    /// again, in a pane that stays open after it exits, so that the pane shows the exit and the
    /// next restore finds what this one did.
    #[serde(default)]
    pub own_program: bool,
    /// The status an `exited` slot exits with, as Rex reports it: -1 for a signal.
    pub exit_code: Option<i64>,
    /// Why the slot starts the way it does, logged with the plan.
    pub reason: String,
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
            candidate = shell("another pane reopens this conversation");
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

mod last_use;
#[cfg(test)]
mod tests;
