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

pub struct Report {
    pub native_id: String,
    pub transcript: Option<String>,
    pub resumable: bool,
    /// Only the settings this report carries; a later report merges into the recorded ones.
    pub settings: Map<String, Value>,
}

/// Reads the hook's JSON input. Claude Code and Codex send their hook input; the Pi extension sends
/// `session_id`, `session_file`, `mode`, `provider`, `model`, and `thinking`.
pub fn parse(runtime: Runtime, input: &Value) -> Result<Option<Report>> {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    // A subagent's session start describes the subagent, not the pane's conversation.
    if runtime == Runtime::Claude && input.get("agent_id").is_some_and(|v| !v.is_null()) {
        return Ok(None);
    }
    // Each runtime's resume command reads these keys (`runtime::*::resume_argv`).
    let settings_from = |pairs: &[(&str, &str)]| -> Map<String, Value> {
        pairs
            .iter()
            .filter_map(|(field, key)| Some(((*key).to_owned(), Value::from(text(field)?))))
            .collect()
    };
    let report = match runtime {
        Runtime::Claude | Runtime::Codex => Report {
            native_id: text("session_id").context("hook input without session_id")?,
            transcript: text("transcript_path"),
            resumable: true,
            settings: settings_from(&[("model", "model"), ("effort", "effort")]),
        },
        Runtime::Pi => {
            let file = text("session_file");
            Report {
                native_id: text("session_id")
                    .or_else(|| file.clone())
                    .context("hook input without session_id or session_file")?,
                resumable: text("mode").as_deref() == Some("tui") && file.is_some(),
                transcript: file,
                settings: settings_from(&[
                    ("provider", "provider"),
                    ("model", "model"),
                    ("thinking", "effort"),
                ]),
            }
        }
    };
    Ok(Some(report))
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
    let Some(report) = parse(runtime, &input)? else {
        return Ok(());
    };
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

/// Records the conversation and makes it the pane's occupant.
pub fn record(
    tx: &Transaction,
    lineage: &str,
    runtime: Runtime,
    report: &Report,
    process: &Process,
) -> Result<()> {
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
mod tests {
    use super::*;

    #[test]
    fn pi_print_runs_and_session_less_runs_are_not_resumable() -> Result<()> {
        let tui = json!({"session_id": "s", "session_file": "/p/s.jsonl", "mode": "tui"});
        assert!(parse(Runtime::Pi, &tui)?.unwrap().resumable);
        let print = json!({"session_id": "s", "session_file": "/p/s.jsonl", "mode": "print"});
        assert!(!parse(Runtime::Pi, &print)?.unwrap().resumable);
        let no_session = json!({"session_id": "s", "mode": "tui"});
        assert!(!parse(Runtime::Pi, &no_session)?.unwrap().resumable);
        Ok(())
    }

    #[test]
    fn a_resumed_claude_conversation_keeps_its_launch_model() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = Store::open(dir.path())?;
        let process = Process {
            transport: Map::new(),
            keeps_session_env: false,
            pid: 100,
            name: Some("claude".into()),
        };
        let model = |store: &mut Store, input: Value| -> Result<Option<String>> {
            let tx = store.write()?;
            let report = parse(Runtime::Claude, &input)?.unwrap();
            record(&tx, "b", Runtime::Claude, &report, &process)?;
            let model = tx.query_row(
                "SELECT c.settings ->> '$.model' FROM occupant o
                 JOIN conversation c USING (runtime, native_id) WHERE o.block_lineage = 'b'",
                [],
                |row| row.get(0),
            )?;
            tx.commit()?;
            Ok(model)
        };
        // Inputs as Claude Code sends them on startup and on resume.
        let startup = json!({"session_id": "s", "source": "startup", "model": "claude-haiku-5-5"});
        assert_eq!(
            model(&mut store, startup)?.as_deref(),
            Some("claude-haiku-5-5")
        );
        let resume = json!({"session_id": "s", "source": "resume"});
        assert_eq!(
            model(&mut store, resume)?.as_deref(),
            Some("claude-haiku-5-5")
        );
        let other = json!({"session_id": "t", "source": "resume"});
        assert_eq!(model(&mut store, other)?, None);
        Ok(())
    }

    #[test]
    fn claude_subagent_starts_do_not_replace_the_pane_conversation() -> Result<()> {
        let input = json!({"session_id": "s", "agent_id": "a1", "transcript_path": "/t"});
        assert!(parse(Runtime::Claude, &input)?.is_none());
        Ok(())
    }
}
