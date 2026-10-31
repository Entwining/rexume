//! Deciding whether a pane the server restored reopens its conversation, by what the pane was last
//! used for on the earlier instance: the conversation reopened, or reopened with the one prompt to
//! continue a turn the server's end cut off; any other pane keeps the shell the server started.

use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::store::{Store, new_id, now_ms};
use crate::task;
use last_use::candidate;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// The shell the server started, left as it is.
    Shell,
    /// The conversation reopened.
    Agent,
    /// The conversation reopened with the one continuation prompt.
    Continue,
}

impl Start {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
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

/// What the gate needs to start a pane's conversation, stored whole in `restore.plan`.
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
    /// Why the pane starts the way it does, logged with the plan.
    pub reason: String,
}

/// Plans the restore of a pane the server restored, as recorded on an earlier instance, and returns
/// it, or `None` when the pane keeps its shell or has a plan on this instance already. A pane that
/// keeps its shell ends the rounds the earlier instance left open there, since nothing takes them
/// up again, unless another pane reopens its conversation, whose record finishes them there. The
/// conversation's record is read before the transaction, which only decides and writes.
pub fn plan(
    store: &mut Store,
    instance: &str,
    block: &str,
    session_label: Option<&str>,
) -> Result<Option<String>> {
    // Taken before the record is read: a report that arrives meanwhile is later than the plan.
    let planned_at = now_ms();
    let candidate = candidate(store.conn(), instance, block)?;
    let tx = store.write()?;
    let planned: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM restore WHERE instance_id = ?1 AND block_lineage = ?2)",
        params![instance, block],
        |row| row.get(0),
    )?;
    if planned {
        return Ok(None);
    }
    let mut reason = candidate.plan.reason.clone();
    let mut id = None;
    if let Some((runtime, native_id)) = &candidate.conversation {
        let new = new_id();
        // A pane that shows the turn the server's end cut off takes over the plan of one planned
        // first without it, so that the turn continues wherever it ran.
        let planned: Option<String> = tx
            .query_row(
                "INSERT INTO restore (id, instance_id, block_lineage, session_label, start, runtime,
                   native_id, plan, state, created_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'planned', ?9)
                 ON CONFLICT (instance_id, runtime, native_id) DO UPDATE SET
                   block_lineage = excluded.block_lineage, session_label = excluded.session_label,
                   start = excluded.start, plan = excluded.plan, created_at = excluded.created_at
                 WHERE restore.state = 'planned' AND restore.start = 'agent'
                   AND excluded.start = 'continue'
                 RETURNING id",
                params![
                    new,
                    instance,
                    block,
                    session_label,
                    candidate.start.as_str(),
                    runtime.as_str(),
                    native_id,
                    serde_json::to_string(&candidate.plan)?,
                    planned_at
                ],
                |row| row.get(0),
            )
            .optional()?;
        match planned {
            Some(planned) => {
                if planned != new {
                    reason += "; the plan moves here from a pane that showed the conversation idle";
                }
                id = Some(planned);
            }
            None => reason = "another pane reopens this conversation".into(),
        }
    }
    if candidate.conversation.is_none() {
        task::end_left_to_server(&tx, instance, block)?;
    }
    let start = if id.is_some() {
        candidate.start
    } else {
        Start::Shell
    };
    tx.commit()?;
    log::info!("pane {block}: {start}: {reason}");
    Ok(id)
}

mod last_use;
#[cfg(test)]
mod tests;
