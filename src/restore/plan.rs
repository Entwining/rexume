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
    Shell,
    Agent,
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Plan {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    pub prompt: Option<String>,
    pub transcript: Option<String>,
    #[serde(default)]
    pub record_length: Option<u64>,
    /// Workers run without `REX_SESSION` so that Rex does not ask them to label the session.
    pub keeps_session_env: bool,
    pub reason: String,
}

pub fn plan(
    store: &mut Store,
    instance: &str,
    block: &str,
    session_label: Option<&str>,
) -> Result<Option<String>> {
    // Before the record is read, so a report that arrives meanwhile counts as after the plan.
    let planned_at = now_ms();
    let candidate = candidate(store.conn(), store.dir(), instance, block)?;
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
        // A `continue` plan replaces an `agent` plan another pane of the conversation made first,
        // so the cut-off turn continues in the pane it ran in.
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
    // When another pane reopens the conversation, this pane's open rounds go on from its record.
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
