//! The `task` commands an orchestrator runs.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::Value;

use crate::task;

/// The exit status of `task wait` when it returns because the round's worker is blocked.
const BLOCKED: u8 = 3;
/// The exit status of `task wait` when its timeout passes first, as coreutils `timeout` uses.
const TIMED_OUT: u8 = 124;

pub(super) const EXAMPLES: &str = "\
Examples:
  rexume task add --worker \"$WORKER_BLOCK\" --title 'review the parser'
  rexume task add --worker \"$WORKER_BLOCK\" --title 'run the suite' --result-file /tmp/suite.md
  rexume task wait ID --timeout 300
  rexume task show ID --round 2
  rexume task send ID --message-file /tmp/next-step.md
  rexume task next ID
  rexume task release ID";

#[derive(clap::Args)]
pub(super) struct TaskId {
    /// The task ID that `task add` printed
    #[arg(value_name = "ID")]
    id: String,
}

#[derive(clap::Args)]
pub(super) struct Round {
    /// The round, as a notice names it, instead of the latest
    #[arg(long, value_name = "N")]
    round: Option<i64>,
}

#[derive(Subcommand)]
pub(super) enum Command {
    /// Register a worker pane working on a task for this conversation
    ///
    /// Without --result-file the worker is an interactive conversation, and the first turn it
    /// finishes is the result; with it, the worker is a non-interactive run, and the file is the
    /// result once the run exits 0. Register a worker right after starting it, and prompt it again
    /// only through `task send`.
    Add {
        /// The Rex block ID of the worker's pane, as REX_BLOCK gives it there
        #[arg(long, value_name = "BLOCK")]
        worker: String,
        /// A short name for the task, shown in notices and in `task list`
        #[arg(long, value_name = "TEXT")]
        title: String,
        /// Read the result from this file when the worker exits (an absolute path)
        #[arg(long, value_name = "PATH", value_parser = absolute_path)]
        result_file: Option<PathBuf>,
        /// Keep the worker's pane after the task is released
        #[arg(long)]
        keep_pane: bool,
    },
    /// Start the next round with the earliest finished turn no round has taken
    ///
    /// For a worker that ends turns on its own, such as interim reports while it waits for CI;
    /// a turn that finished before this command counts. Acknowledges the previous round.
    Next(TaskId),
    /// Start the next round with a new request to the interactive worker
    ///
    /// rexume delivers the message to the worker's conversation once it is idle, without typing
    /// into its pane; the round's result is the turn that takes it in. Acknowledges the previous
    /// round.
    Send {
        #[command(flatten)]
        task: TaskId,
        #[command(flatten)]
        message: Message,
    },
    /// List this conversation's tasks with their latest round
    List,
    /// Show one task with a round's result
    Show {
        #[command(flatten)]
        task: TaskId,
        #[command(flatten)]
        round: Round,
    },
    /// Wait until the task's latest round finishes or its worker blocks, then show it
    ///
    /// Exits 3 when the worker is blocked on a dialog or a question that no notice or earlier wait
    /// reported, and 124 when the timeout passes first; the task is printed either way, with
    /// `timed_out`.
    Wait {
        #[command(flatten)]
        task: TaskId,
        /// How long to wait before giving up
        #[arg(long, value_name = "SECONDS", default_value_t = 600)]
        timeout: u64,
    },
    /// Acknowledge a finished round's result
    ///
    /// Fails while the round is still running or blocked. A notice not yet sent for the round is
    /// dropped, since its owner has the result.
    Ack {
        #[command(flatten)]
        task: TaskId,
        #[command(flatten)]
        round: Round,
    },
    /// Release the worker, so the daemon closes an unused disposable worker's pane
    ///
    /// Acknowledges the latest round. Fails while a round is unfinished or turns are unread; a
    /// released task starts no more rounds.
    Release(TaskId),
}

#[derive(clap::Args)]
#[group(required = true, multiple = false)]
pub(super) struct Message {
    /// The message to send
    #[arg(long, value_name = "TEXT")]
    message: Option<String>,
    /// Send the contents of this file
    #[arg(long, value_name = "PATH")]
    message_file: Option<PathBuf>,
}

impl Message {
    fn read(self) -> Result<String> {
        match (self.message, self.message_file) {
            (Some(text), _) => Ok(text),
            // One byte past the limit is enough to refuse it, also where it cuts a character.
            (None, Some(path)) => {
                let mut bytes = Vec::new();
                std::fs::File::open(&path)
                    .and_then(|file| file.take(task::MESSAGE_LIMIT + 1).read_to_end(&mut bytes))
                    .with_context(|| format!("failed to read `{}`", path.display()))?;
                task::refuse_too_large(bytes.len())?;
                String::from_utf8(bytes)
                    .with_context(|| format!("`{}` is not UTF-8 text", path.display()))
            }
            (None, None) => unreachable!("clap requires one of them"),
        }
    }
}

fn absolute_path(text: &str) -> Result<PathBuf, &'static str> {
    let path = PathBuf::from(text);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("not an absolute path")
    }
}

/// Runs a task command, returning what it prints and its exit status.
pub(super) fn run(command: Command, dir: &Path) -> Result<(Value, u8)> {
    let value = match command {
        Command::Add {
            worker,
            title,
            result_file,
            keep_pane,
        } => task::add(
            dir,
            &task::NewTask {
                worker: &worker,
                title: &title,
                result_file: result_file.as_deref(),
                keep_pane,
            },
        ),
        Command::List => task::list(dir),
        Command::Show { task, round } => task::show(dir, &task.id, round.round),
        Command::Wait { task, timeout } => {
            let (value, returned) = task::wait(dir, &task.id, Duration::from_secs(timeout))?;
            return Ok((value, exit_status(returned)));
        }
        Command::Ack { task, round } => task::ack(dir, &task.id, round.round),
        Command::Next(task) => task::next(dir, &task.id),
        Command::Send { task, message } => task::send(dir, &task.id, &message.read()?),
        Command::Release(task) => task::release(dir, &task.id),
    }?;
    Ok((value, 0))
}

fn exit_status(returned: task::Returned) -> u8 {
    match returned {
        task::Returned::Finished => 0,
        task::Returned::Blocked => BLOCKED,
        task::Returned::TimedOut => TIMED_OUT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_wait_exits_by_why_it_returned() {
        for (returned, status) in [
            (task::Returned::Finished, 0),
            (task::Returned::Blocked, 3),
            (task::Returned::TimedOut, 124),
        ] {
            assert_eq!(exit_status(returned), status, "{returned:?}");
        }
    }

    /// A message over the limit is refused before anything is recorded, as text or as a file whose
    /// limit falls inside a character.
    #[test]
    fn a_message_over_the_limit_is_refused() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let limit = usize::try_from(task::MESSAGE_LIMIT)?;
        let state = dir.path().join("state");
        let error = task::send(&state, "t", &"x".repeat(limit + 1))
            .err()
            .map(|e| e.to_string());
        assert_eq!(error.as_deref(), Some("the message is larger than 256 KiB"));
        assert!(!state.exists());
        let file = dir.path().join("message.md");
        std::fs::write(&file, "é".repeat(limit))?;
        let error = Message {
            message: None,
            message_file: Some(file),
        }
        .read()
        .err()
        .map(|e| e.to_string());
        assert_eq!(error.as_deref(), Some("the message is larger than 256 KiB"));
        Ok(())
    }
}
