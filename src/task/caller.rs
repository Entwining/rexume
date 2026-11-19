use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::rex::{Cli, Rex, items, require_str, str_at};
use crate::runtime::Runtime;
use crate::store::{Store, lineage_of};

pub(super) struct Caller {
    pub(super) rex: Rex,
    pub(super) instance: String,
    host: Option<String>,
    pub(super) block: String,
    pub(super) lineage: String,
    pub(super) runtime: Runtime,
    pub(super) native_id: String,
}

/// Inside an agent's sandbox the process tree cannot be read, so the pane's recorded conversation
/// is the caller.
pub(super) fn caller(store: &Store) -> Result<Caller> {
    let block = std::env::var_os("REX_BLOCK")
        .context("REX_BLOCK is not set; run this from the orchestrator's own Rex pane")?
        .to_string_lossy()
        .into_owned();
    let mut rex = Rex::Cli(Cli::from_env()?);
    let status = rex.call("server.status", &json!({}))?;
    let instance = require_str(&status, "/instance_id")?;
    let host = str_at(&status, "/host_id").map(str::to_owned);
    let conn = store.conn();
    let unrecorded = || format!("no conversation recorded for pane {block}");
    let lineage = lineage_of(conn, &instance, &block)?.with_context(unrecorded)?;
    let (runtime, native_id) = match std::env::var("OPENCODE_SESSION_ID") {
        Ok(session) => (Runtime::OpenCode, recorded_session(conn, session)?),
        Err(_) => conn
            .query_row(
                "SELECT runtime, native_id FROM occupant WHERE block_lineage = ?1",
                params![lineage],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .with_context(unrecorded)?,
    };
    Ok(Caller {
        rex,
        instance,
        host,
        block,
        lineage,
        runtime,
        native_id,
    })
}

impl Caller {
    pub(super) fn refuse_remote(&mut self, session: &str, worker: &str) -> Result<()> {
        let list = self
            .rex
            .call("session.list_blocks", &json!({ "session_id": session }))?;
        on_host(&list, worker, self.host.as_deref())
    }
}

fn on_host(list: &Value, worker: &str, host: Option<&str>) -> Result<()> {
    let block = items(list, "/blocks")
        .iter()
        .find(|b| str_at(b, "/block_id") == Some(worker))
        .with_context(|| format!("pane {worker} is not on the server"))?;
    if str_at(block, "/host_id") != host {
        bail!(
            "pane {worker} runs on another machine than this Rex server, and a task follows only a worker on this machine"
        );
    }
    Ok(())
}

pub(super) fn owned(conn: &Connection, id: &str, caller: &Caller) -> Result<()> {
    let owner: Option<(Runtime, String)> = conn
        .query_row(
            "SELECT owner_runtime, owner_native_id FROM task WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match owner {
        None => bail!("no task {id}"),
        Some((runtime, native_id))
            if runtime != caller.runtime || native_id != caller.native_id =>
        {
            bail!("task {id} belongs to another conversation")
        }
        Some(_) => Ok(()),
    }
}

/// OpenCode's service goes on running every session a TUI showed, after the TUI moved on or
/// exited, so the session that runs a command, which OpenCode's shell tool names in
/// `OPENCODE_SESSION_ID`, may not be the pane's conversation. Only a recorded session can own a
/// task; a subagent's never is.
fn recorded_session(conn: &Connection, session: String) -> Result<String> {
    let recorded = conn
        .query_row(
            "SELECT 1 FROM conversation WHERE runtime = 'opencode' AND native_id = ?1",
            params![session],
            |_| Ok(()),
        )
        .optional()?;
    if recorded.is_none() {
        bail!(
            "no conversation recorded for OpenCode session {session}; run task commands from a session a Rex pane shows"
        );
    }
    Ok(session)
}

#[cfg(test)]
mod tests;
