//! The first program of every restored block. It claims its slot, then becomes the shell or the
//! reopened conversation. It never exits on its own: a block whose program exits closes, and a
//! close on a live server counts as the user's.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};

use crate::layout;
use crate::restore::{Plan, Start};
use crate::runtime::{self, Runtime, Turn};
use crate::store::{Store, now_ms};

#[expect(
    clippy::disallowed_methods,
    reason = "both shells failed to exec; there is no caller to return to"
)]
pub fn run(state_dir: &Path, op: &str, slot: i64) -> ! {
    let claimed = match claim_here(state_dir, op, slot) {
        Ok(claimed) => Some(claimed),
        Err(e) => {
            eprintln!("rexume: {e:#}; starting a shell instead");
            None
        }
    };
    let shell = std::env::var_os("SHELL").map_or_else(
        || "/bin/zsh".to_owned(),
        |shell| shell.to_string_lossy().into_owned(),
    );
    let error = match claimed {
        Some((started, plan)) if started.runs_conversation() => {
            if started == Start::Continue {
                eprintln!(
                    "rexume: {}; reopening the conversation with one prompt to continue",
                    plan.reason
                );
            }
            agent(&shell, &plan, started)
        }
        _ => login_shell(&shell),
    };
    eprintln!("rexume: failed to start `{shell}`: {error}");
    let error = login_shell("/bin/zsh");
    eprintln!("rexume: failed to start `/bin/zsh`: {error}");
    std::process::exit(1)
}

fn login_shell(shell: &str) -> std::io::Error {
    let name = Path::new(shell)
        .file_name()
        .map_or("zsh".into(), |n| n.to_string_lossy().into_owned());
    Command::new(shell).arg0(format!("-{name}")).exec()
}

/// Runs the conversation from an interactive shell, as when the user starts it, and leaves a login
/// shell in the pane when it ends. Without the trap, Ctrl+C ending the program would also end the
/// shell's command list, and the pane would close.
fn agent(shell: &str, plan: &Plan, started: Start) -> std::io::Error {
    let prompt = plan
        .prompt
        .as_deref()
        .filter(|_| started == Start::Continue);
    let argv = plan.argv.iter().map(String::as_str).chain(prompt);
    let command = match shlex::try_join(argv) {
        Ok(command) => command,
        Err(e) => return std::io::Error::other(e.to_string()),
    };
    let quoted_shell =
        shlex::try_quote(shell).map_or_else(|_| "/bin/zsh".into(), |s| s.into_owned());
    let script = format!("trap : INT; {command}; exec {quoted_shell} -l");
    let mut process = Command::new(shell);
    process.args(["-i", "-c", &script]);
    if !plan.keeps_session_env {
        process.env_remove("REX_SESSION");
    }
    process.exec()
}

/// Claims the slot for the pane this process runs in.
fn claim_here(state_dir: &Path, op: &str, slot: i64) -> Result<(Start, Plan)> {
    let block = std::env::var_os("REX_BLOCK").context("REX_BLOCK is not set")?;
    let session = std::env::var_os("REX_SESSION").context("REX_SESSION is not set")?;
    let mut store = Store::open(state_dir)?;
    claim(
        &mut store,
        op,
        slot,
        &block.to_string_lossy(),
        &session.to_string_lossy(),
    )
}

/// Claims the slot for `block` and returns what it starts. A slot is claimed once; a
/// conversation that reported itself from another pane since the restore was planned is already
/// running there and is not started, and a turn its record shows ended since is not continued.
/// The claim binds this block and its session to their lineages.
fn claim(
    store: &mut Store,
    op: &str,
    slot: i64,
    block: &str,
    session: &str,
) -> Result<(Start, Plan)> {
    let (start, runtime, plan): (String, Option<String>, String) = store
        .conn()
        .query_row(
            "SELECT start, runtime, plan FROM restore_slot WHERE op_id = ?1 AND slot = ?2",
            params![op, slot],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .with_context(|| format!("failed to read restore {op}"))?
        .with_context(|| format!("no slot {slot} in restore {op}"))?;
    let start: Start = start.parse()?;
    let plan: Plan = serde_json::from_str(&plan)?;
    let mut started = start;
    // Read before the transaction: no record is read while holding the write lock.
    if start == Start::Continue && !turn_still_open(runtime.as_deref(), plan.transcript.as_deref())
    {
        started = Start::Agent;
    }
    let tx = store.write()?;
    let (instance, lineage, session_lineage, native_id, planned_at): (
        String,
        String,
        String,
        Option<String>,
        i64,
    ) = tx.query_row(
        "SELECT s.instance_id, s.block_lineage, o.session_lineage, s.native_id, o.created_at
         FROM restore_slot s JOIN restore_op o ON o.id = s.op_id WHERE s.op_id = ?1 AND s.slot = ?2",
        params![op, slot],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )?;
    // A report in the same millisecond as the plan counts as later: starting a shell is the
    // safe side.
    let elsewhere = started.runs_conversation()
        && tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM occupant WHERE runtime = ?1 AND native_id = ?2
               AND reported_at >= ?3 AND block_lineage != ?4)",
            params![runtime, native_id, planned_at, lineage],
            |row| row.get(0),
        )?;
    if elsewhere {
        started = Start::Shell;
    }
    let claimed = tx.execute(
        "UPDATE restore_slot SET claimed_at = ?1, started = ?2
         WHERE op_id = ?3 AND slot = ?4 AND claimed_at IS NULL",
        params![now_ms(), started.as_str(), op, slot],
    )?;
    if claimed == 0 {
        bail!("slot {slot} of restore {op} was already claimed");
    }
    layout::bind_restored(&tx, &instance, block, &lineage)?;
    layout::bind_restored(&tx, &instance, session, &session_lineage)?;
    // A floating layer whose reply was lost reports here only after the restore gave up waiting.
    tx.execute(
        "UPDATE restore_op SET state = 'completed', note = NULL WHERE id = ?1 AND state = 'partial'
           AND NOT EXISTS (SELECT 1 FROM restore_slot s WHERE s.op_id = ?1
             AND NOT EXISTS (SELECT 1 FROM live l
                             WHERE l.instance_id = s.instance_id AND l.lineage = s.block_lineage))",
        params![op],
    )?;
    tx.commit()?;
    if elsewhere {
        bail!("the conversation was opened in another pane after the restore was planned");
    }
    Ok((started, plan))
}

/// Whether the conversation's record still ends with the turn the plan would continue.
fn turn_still_open(runtime: Option<&str>, transcript: Option<&str>) -> bool {
    let (Some(runtime), Some(transcript)) = (runtime, transcript) else {
        return false;
    };
    runtime.parse::<Runtime>().is_ok_and(|runtime| {
        matches!(
            runtime::last_turn(runtime, Path::new(transcript), 0),
            Ok(Some(Turn::Open { .. }))
        )
    })
}

#[cfg(test)]
mod tests;
