//! The conversation a task command runs for, found through the pane the command runs in, and what
//! ties a task to it: the conversation owns the task, and the worker runs on its machine.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::rex::{Cli, Rex, items, require_str, str_at};
use crate::runtime::Runtime;
use crate::store::{Store, lineage_of};

pub(super) struct Caller {
    pub(super) rex: Rex,
    pub(super) instance: String,
    /// The machine and account the caller's server runs as.
    host: Option<String>,
    pub(super) block: String,
    pub(super) lineage: String,
    pub(super) runtime: Runtime,
    pub(super) native_id: String,
}

/// The conversation in the calling pane. Inside an agent's sandbox the process tree cannot be
/// read, so the pane's recorded conversation is the caller.
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
    let recorded = match lineage_of(conn, &instance, &block)? {
        Some(lineage) => conn
            .query_row(
                "SELECT runtime, native_id FROM occupant WHERE block_lineage = ?1",
                params![lineage],
                |row| Ok((lineage.clone(), row.get(0)?, row.get(1)?)),
            )
            .optional()?,
        None => None,
    };
    let (lineage, runtime, native_id) =
        recorded.with_context(|| format!("no conversation recorded for pane {block}"))?;
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
    /// Refuses a worker pane in `session` that runs on another machine. A task follows its worker
    /// through this machine's files and processes: the reports its hooks write here, its record or
    /// result file, and its process IDs.
    pub(super) fn refuse_remote(&mut self, session: &str, worker: &str) -> Result<()> {
        let list = self
            .rex
            .call("session.list_blocks", &json!({ "session_id": session }))?;
        on_host(&list, worker, self.host.as_deref())
    }
}

/// Whether `worker` in a `session.list_blocks` reply runs as `host`. Rex gives a block the
/// server's own `host_id` when it sees the server's files, and a server without a state directory
/// reports none, so only equal values, both absent included, are this machine.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_is_local_only_when_its_host_equals_the_servers() {
        let list = json!({ "blocks": [
            { "block_id": "block:here", "host_id": "h1" },
            { "block_id": "block:there", "host_id": "h2" },
            { "block_id": "block:unknown" },
        ] });
        assert!(on_host(&list, "block:here", Some("h1")).is_ok());
        assert!(on_host(&list, "block:unknown", None).is_ok());
        for (worker, host) in [
            ("block:there", Some("h1")),
            ("block:unknown", Some("h1")),
            ("block:here", None),
        ] {
            let refused = on_host(&list, worker, host).unwrap_err().to_string();
            assert!(refused.contains("another machine"), "{worker}: {refused}");
        }
        let missing = on_host(&list, "block:gone", Some("h1"))
            .unwrap_err()
            .to_string();
        assert_eq!(missing, "pane block:gone is not on the server");
    }
}
