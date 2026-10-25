//! Records which conversation a pane runs, from the runtime's session-start hook. The report is
//! accepted only from the pane's own foreground program, so a conversation the agent starts in a
//! subprocess, such as a non-interactive run from a tool call, does not replace it.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Transaction, params};
use serde_json::{Map, Value, json};

use crate::proc;
use crate::rex::{Cli, Rex, require_str, str_at};
use crate::runtime::Runtime;
use crate::store::{Store, ensure_lineage, now_ms};

/// What a session start says about the session's place in the agent process that reports it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The agent's own conversation.
    Own,
    /// A session the agent runs inside its process for its own purposes.
    Inner,
    /// Not marked either way: the agent's own conversation when its process does not hold the
    /// pane yet, and an inner session of the process that does.
    Unmarked,
}

pub struct Report {
    pub native_id: String,
    pub role: Role,
    pub transcript: Option<String>,
    pub resumable: bool,
    /// Only the settings this report carries; a later report merges into the recorded ones.
    pub settings: Map<String, Value>,
}

/// Reads the hook's JSON input. Claude Code and Codex send their hook input; the Pi extension sends
/// `session_id`, `session_file`, `mode`, `provider`, `model`, and `thinking`.
pub fn parse(runtime: Runtime, input: &Value) -> Result<Report> {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    // The agent also runs sessions inside its own process, which pass the foreground check, so
    // the runtime's marks decide. Claude Code marks a subagent's or in-process teammate's start
    // with `agent_id`. Codex records every interactive conversation, and none of its guardian
    // reviews or side conversations, nor an ephemeral `codex exec` run. Pi runs the pane's
    // interactive session in `tui` mode, and an SDK session an extension creates starts in
    // `print` mode, as `pi -p` does.
    let role = match runtime {
        Runtime::Claude if input.get("agent_id").is_some_and(|v| !v.is_null()) => Role::Inner,
        Runtime::Codex if text("transcript_path").is_none() => Role::Unmarked,
        Runtime::Pi if text("mode").as_deref() != Some("tui") => Role::Unmarked,
        _ => Role::Own,
    };
    // Each runtime's resume command reads these keys (`runtime::*::resume_argv`).
    let settings_from = |pairs: &[(&str, &str)]| -> Map<String, Value> {
        pairs
            .iter()
            .filter_map(|(field, key)| Some(((*key).to_owned(), Value::from(text(field)?))))
            .collect()
    };
    Ok(match runtime {
        Runtime::Claude | Runtime::Codex => Report {
            native_id: text("session_id").context("hook input without session_id")?,
            role,
            transcript: text("transcript_path"),
            resumable: role == Role::Own,
            settings: settings_from(&[("model", "model"), ("effort", "effort")]),
        },
        Runtime::Pi => {
            let file = text("session_file");
            Report {
                native_id: text("session_id")
                    .or_else(|| file.clone())
                    .context("hook input without session_id or session_file")?,
                role,
                resumable: role == Role::Own && file.is_some(),
                transcript: file,
                settings: settings_from(&[
                    ("provider", "provider"),
                    ("model", "model"),
                    ("thinking", "effort"),
                ]),
            }
        }
    })
}

pub fn run(runtime: Runtime, state_dir: &Path) -> Result<()> {
    let Some(block) = std::env::var_os("REX_BLOCK") else {
        return Ok(());
    };
    let block = block.to_string_lossy();
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("failed to read stdin")?;
    let input: Value = serde_json::from_str(&input).context("failed to parse the hook input")?;
    let report = parse(runtime, &input)?;
    let mut rex = Rex::Cli(Cli::from_env()?);
    // Workers run without `REX_SESSION`.
    let session = match std::env::var_os("REX_SESSION") {
        Some(session) => session.to_string_lossy().into_owned(),
        None => rex
            .block_session(&block)?
            .with_context(|| format!("pane {block} is not on the server"))?,
    };
    let process = rex.block_method(&session, &block, "process")?;
    let program = proc::program_above(std::process::id());
    let Some(foreground) = process
        .pointer("/foreground/pid")
        .and_then(Value::as_u64)
        .filter(|pid| program.map(u64::from) == Some(*pid))
    else {
        bail!(
            "this {runtime} process is not the pane's foreground program, so nothing was recorded"
        );
    };
    let instance = require_str(&rex.call("server.status", &json!({}))?, "/instance_id")?;
    let transport: Map<String, Value> = std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET")
        .map(|socket| {
            (
                "messaging_socket".to_owned(),
                socket.to_string_lossy().into(),
            )
        })
        .into_iter()
        .collect();
    let mut store = Store::open(state_dir)?;
    let tx = store.write()?;
    let lineage = ensure_lineage(&tx, &instance, &block)?;
    record(
        &tx,
        &lineage,
        runtime,
        &report,
        &Process {
            transport,
            keeps_session_env: std::env::var_os("REX_SESSION").is_some(),
            pid: i64::try_from(foreground)?,
            name: str_at(&process, "/foreground/name").map(str::to_owned),
        },
    )?;
    tx.commit()?;
    Ok(())
}

pub struct Process {
    /// How a notice reaches this process (`occupant.transport`).
    pub transport: Map<String, Value>,
    pub keeps_session_env: bool,
    pub pid: i64,
    pub name: Option<String>,
}

/// Records the conversation and makes it the pane's occupant, unless it is an inner session of
/// the agent process.
pub fn record(
    tx: &Transaction,
    lineage: &str,
    runtime: Runtime,
    report: &Report,
    process: &Process,
) -> Result<()> {
    let inner = match report.role {
        Role::Own => false,
        Role::Inner => true,
        Role::Unmarked => tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM occupant
               WHERE block_lineage = ?1 AND pid = ?2 AND pid_name IS ?3)",
            params![lineage, process.pid, process.name],
            |row| row.get(0),
        )?,
    };
    if inner {
        return Ok(());
    }
    let now = now_ms();
    tx.execute(
        "INSERT INTO conversation (runtime, native_id, transcript, resumable, settings, reported_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (runtime, native_id) DO UPDATE SET
           transcript = COALESCE(excluded.transcript, transcript), resumable = excluded.resumable,
           settings = json_patch(settings, excluded.settings), reported_at = excluded.reported_at",
        params![
            runtime.as_str(),
            report.native_id,
            report.transcript,
            report.resumable,
            serde_json::to_string(&report.settings)?,
            now,
        ],
    )?;
    let transport = (!process.transport.is_empty())
        .then(|| serde_json::to_string(&process.transport))
        .transpose()?;
    tx.execute(
        "INSERT INTO occupant (block_lineage, runtime, native_id, pid, pid_name, keeps_session_env,
           transport, reported_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (block_lineage) DO UPDATE SET runtime = excluded.runtime,
           native_id = excluded.native_id, pid = excluded.pid, pid_name = excluded.pid_name,
           keeps_session_env = excluded.keeps_session_env, transport = excluded.transport,
           reported_at = excluded.reported_at",
        params![
            lineage,
            runtime.as_str(),
            report.native_id,
            process.pid,
            process.name,
            process.keeps_session_env,
            transport,
            now,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
