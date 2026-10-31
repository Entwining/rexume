//! The line the daemon types into the shell of a pane the server restored. It claims the pane's
//! restore, then becomes the reopened conversation, which runs as a child of the pane's shell and
//! leaves that shell at its prompt when it exits. A restore claimed already, or one whose
//! conversation runs elsewhere, starts nothing, and the shell's prompt comes back.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Map;

use crate::report::{self, Process, Report, Role};
use crate::restore::{self, Plan, Start};
use crate::runtime::{self, Runtime, Turn};
use crate::store::{Store, bind_live, now_ms};

/// The shell started when `SHELL` is unset; Linux systems often have no zsh.
#[cfg(target_os = "macos")]
const FALLBACK_SHELL: &str = "/bin/zsh";
#[cfg(target_os = "linux")]
const FALLBACK_SHELL: &str = "/bin/sh";

/// Claims the restore and starts its conversation. Returns, as the gate's exit status, only when
/// it started nothing: 0 when the conversation runs in another pane, 1 when the claim failed or
/// the conversation could not start.
pub fn run(state_dir: &Path, id: &str) -> u8 {
    let (started, plan) = match claim_here(state_dir, id) {
        Ok(claimed) => claimed,
        Err(e) => {
            eprintln!("rexume: {e:#}");
            return 1;
        }
    };
    if !started.runs_conversation() {
        eprintln!("rexume: the conversation was opened in another pane; it stays there");
        return 0;
    }
    if started == Start::Continue {
        eprintln!(
            "rexume: {}; reopening the conversation with one prompt to continue",
            plan.reason
        );
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| FALLBACK_SHELL.to_owned());
    let error = agent(&shell, &plan, started);
    eprintln!("rexume: failed to start `{shell}`: {error}");
    1
}

/// Runs the conversation from an interactive shell, as when the user starts it, so that a shell
/// alias or function of the program's name applies; `exec` would skip both. The shell replaces
/// this process, and the shell runs a lone command as itself rather than as a child, so the
/// conversation runs as the process the claim recorded; a function that wraps it runs it as that
/// process's child, which the daemon records once the pane shows it in the foreground.
fn agent(shell: &str, plan: &Plan, started: Start) -> std::io::Error {
    let prompt = plan
        .prompt
        .as_deref()
        .filter(|_| started == Start::Continue);
    let command = match command_line(plan, prompt) {
        Ok(command) => command,
        Err(e) => return std::io::Error::other(e.to_string()),
    };
    let mut process = Command::new(shell);
    process.args(["-i", "-c", &command]);
    if !plan.keeps_session_env {
        process.env_remove("REX_SESSION");
    }
    process.exec()
}

/// The reopen command as the shell reads it. The plan's variables are assignments before it,
/// which set them for that command alone: `env` would skip the user's alias or shell function of
/// the same name.
fn command_line(plan: &Plan, prompt: Option<&str>) -> Result<String, shlex::QuoteError> {
    let mut line = String::new();
    for (name, value) in &plan.env {
        line += &format!("{name}={} ", shlex::try_quote(value)?);
    }
    line += &shlex::try_join(plan.argv.iter().map(String::as_str).chain(prompt))?;
    Ok(line)
}

/// Claims the restore for the pane this process runs in.
fn claim_here(state_dir: &Path, id: &str) -> Result<(Start, Plan)> {
    let block = std::env::var("REX_BLOCK").context("REX_BLOCK is not set")?;
    let mut store = Store::open(state_dir)?;
    claim(&mut store, id, &block, std::process::id())
}

/// Claims the restore for `block` and returns what it starts. A restore is claimed once; a
/// conversation that reported itself from another pane since the restore was planned is already
/// running there and is not started, and a turn its record shows ended since is not continued.
/// The claim makes a conversation it starts the pane's occupant, as process `pid`.
fn claim(store: &mut Store, id: &str, block: &str, pid: u32) -> Result<(Start, Plan)> {
    type Row = (String, String, String, String, String, String);
    let (start, runtime, plan, instance, lineage, native_id): Row = store
        .conn()
        .query_row(
            "SELECT start, runtime, plan, instance_id, block_lineage, native_id FROM restore
             WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .with_context(|| format!("failed to read restore {id}"))?
        .with_context(|| format!("no restore {id}"))?;
    let runtime: Runtime = runtime.parse()?;
    let start: Start = start.parse()?;
    let plan: Plan = serde_json::from_str(&plan)?;
    let mut started = start;
    // Read before the transaction: no record is read while holding the write lock.
    if start == Start::Continue && !turn_still_open(runtime, plan.transcript.as_deref()) {
        started = Start::Agent;
    }
    let start_position = match plan.transcript.as_deref() {
        Some(path) => report::record_length(path)?,
        None => 0,
    };
    let tx = store.write()?;
    if restore::opened_elsewhere(&tx, id)? {
        started = Start::Shell;
    }
    let claimed = tx.execute(
        "UPDATE restore SET state = 'claimed', claimed_at = ?1, started = ?2, pid = ?3, note = NULL
         WHERE id = ?4 AND state IN ('typed', 'unknown')",
        params![now_ms(), started.as_str(), pid, id],
    )?;
    if claimed == 0 {
        bail!("this pane's restore has run already; nothing more to start");
    }
    bind_live(&tx, &instance, block, &lineage)?;
    if started.runs_conversation() {
        occupy(
            &tx,
            &lineage,
            &instance,
            (runtime, native_id),
            &plan,
            pid,
            start_position,
        )?;
    }
    tx.commit()?;
    Ok((started, plan))
}

/// Records the conversation the claim starts as the pane's occupant: `agent` replaces this process
/// with the shell and the shell with the conversation, which therefore runs as this process, or as
/// a child of it when a shell function wraps it, which the daemon then records instead. Its own report
/// may come much later, since Codex runs a resumed conversation's session-start hook only when its
/// first turn starts, and until then requests and notices would wait for input nobody gives. The
/// name of the process is not known before it runs; its own report adds it and keeps the start
/// measured here.
fn occupy(
    tx: &Transaction,
    lineage: &str,
    instance: &str,
    (runtime, native_id): (Runtime, String),
    plan: &Plan,
    pid: u32,
    start_position: u64,
) -> Result<()> {
    let report = Report {
        native_id,
        role: Role::Own,
        transcript: plan.transcript.clone(),
        resumable: true,
        settings: Map::new(),
        record_length: None,
    };
    let process = Process {
        transport: Map::new(),
        keeps_session_env: plan.keeps_session_env,
        instance: instance.to_owned(),
        pid: i64::from(pid),
        name: None,
        own_program: false,
        start_position,
    };
    report::record(tx, lineage, runtime, &report, &process)
}

/// Whether the conversation's record still ends with the turn the plan would continue.
fn turn_still_open(runtime: Runtime, transcript: Option<&str>) -> bool {
    transcript.is_some_and(|transcript| {
        matches!(
            runtime::last_turn(runtime, Path::new(transcript), 0),
            Ok(Some(Turn::Open { .. }))
        )
    })
}

#[cfg(test)]
mod tests;
