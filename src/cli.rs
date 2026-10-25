//! The command line.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::runtime::Runtime;
use crate::{daemon, gate, install, report, rex, status, store, task};

#[derive(Parser)]
#[command(version, about, long_about = None, after_help = EXAMPLES)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

const EXAMPLES: &str = "\
Examples:
  rexume install
  rexume status
  rexume task add --worker \"$WORKER_BLOCK\" --title 'review the parser'
  rexume task show 0192f3a0-7c1e-7a2b-9c3d-1e2f3a4b5c6d";

const TASK_EXAMPLES: &str = "\
Examples:
  rexume task add --worker \"$WORKER_BLOCK\" --title 'review the parser'
  rexume task add --worker \"$WORKER_BLOCK\" --title 'run the suite' --result-file /tmp/suite.md
  rexume task wait ID --timeout 300
  rexume task show ID --round 2
  rexume task ack ID";

/// The exit status of `task wait` when its timeout passes first, as coreutils `timeout` uses.
const TIMED_OUT: u8 = 124;

#[derive(clap::Args)]
struct StateDir {
    /// The directory holding the state database
    ///
    /// Defaults to `$HOME/.local/state/rexume`.
    #[arg(long, value_name = "DIR", global = true)]
    state_dir: Option<PathBuf>,
}

impl StateDir {
    fn resolve(self) -> Result<PathBuf> {
        self.state_dir.map_or_else(store::default_dir, Ok)
    }
}

#[derive(clap::Args)]
struct Server {
    /// The rex executable
    ///
    /// Defaults to the first `rex` on PATH.
    #[arg(long, value_name = "PATH")]
    rex: Option<PathBuf>,
    /// The server address
    ///
    /// Defaults to REX_SERVER, then to the socket of the server the Rex app starts.
    #[arg(long, value_name = "ADDRESS")]
    server: Option<String>,
    /// The foreground color restored terminals start with, as `#rrggbb`
    #[arg(long, value_name = "COLOR", requires = "theme_background")]
    theme_foreground: Option<String>,
    /// The background color restored terminals start with, as `#rrggbb`
    #[arg(long, value_name = "COLOR", requires = "theme_foreground")]
    theme_background: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Install the session hooks, the Pi extension, and the LaunchAgent that runs the daemon
    ///
    /// Writes a SessionStart hook into `~/.claude/settings.json` and `~/.codex/hooks.json`, the
    /// extension into `~/.pi/agent/extensions/rexume.ts`, and the LaunchAgent
    /// `io.github.entwining.rexume`, and prints what changed as JSON. A runtime whose directory
    /// does not exist is skipped. Running it again changes only what is missing or different.
    Install(Server),
    /// Remove what install wrote, keeping the state directory and this executable
    ///
    /// Removes the hooks, the Pi extension when install wrote it, and the LaunchAgent, which stops
    /// the daemon. `~/.local/state/rexume` keeps the saved layouts, conversations, and task
    /// results; delete it to remove them too.
    Uninstall,
    /// Follow the Rex server, saving its sessions and restoring them after it restarts
    ///
    /// The LaunchAgent runs this. It runs until stopped, logs to stderr, and exits at once when
    /// another daemon holds the same state directory. Set `RUST_LOG=rexume=debug` to log every
    /// call it makes through its adapter.
    Daemon {
        #[command(flatten)]
        server: Server,
        #[command(flatten)]
        state: StateDir,
    },
    /// Show restores that did not finish and notices not yet confirmed
    ///
    /// Reads the state database without changing it.
    Status(StateDir),
    /// Record the conversation starting in this pane; the session-start hooks run this
    ///
    /// Reads the hook's JSON on stdin. Outside a Rex pane it does nothing. A failure is printed on
    /// stderr with exit status 0, so a hook never holds up or fails the session it reports.
    Report {
        /// The runtime whose hook is running
        #[arg(value_name = "RUNTIME")]
        runtime: Runtime,
        #[command(flatten)]
        state: StateDir,
    },
    /// Hand work to a worker pane and get its result back
    ///
    /// Run these from the orchestrator's own Rex pane: they find the orchestrator through
    /// REX_BLOCK and the conversation last recorded there, and refuse a task another conversation
    /// owns.
    #[command(after_help = TASK_EXAMPLES)]
    Task {
        #[command(subcommand)]
        command: TaskCommand,
        #[command(flatten)]
        state: StateDir,
    },
    /// Run as the first program of a restored pane: claim its slot, then start its program
    #[command(hide = true)]
    Gate {
        /// The restore operation
        #[arg(value_name = "OP")]
        op: String,
        /// The pane's slot in it
        #[arg(value_name = "SLOT")]
        slot: i64,
        #[command(flatten)]
        state: StateDir,
    },
    /// Carry the daemon's connection for the script `rex do` runs
    #[command(hide = true)]
    Relay {
        /// The daemon's socket
        #[arg(value_name = "SOCKET")]
        socket: PathBuf,
        /// The line sent first, which the daemon checks
        #[arg(value_name = "TOKEN")]
        token: String,
    },
}

#[derive(clap::Args)]
struct TaskId {
    /// The task ID that `task add` printed
    #[arg(value_name = "ID")]
    id: String,
}

#[derive(clap::Args)]
struct Round {
    /// The round, as a notice names it, instead of the latest
    #[arg(long, value_name = "N")]
    round: Option<i64>,
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Register a worker pane working on a task for this conversation
    ///
    /// Without --result-file the worker is an interactive conversation, and its next finished
    /// turn is the result; with it, the worker is a non-interactive run, and the file is the
    /// result once the run exits 0. Register a worker right after starting it; before prompting
    /// it again, use `task rearm`.
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
        /// Keep the worker's pane after its result is acknowledged
        #[arg(long)]
        keep_pane: bool,
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
    /// Wait until the task's latest round finishes, then show it
    ///
    /// Exits 124 when the timeout passes first; the task is still printed, with `timed_out` true.
    Wait {
        #[command(flatten)]
        task: TaskId,
        /// How long to wait before giving up
        #[arg(long, value_name = "SECONDS", default_value_t = 600)]
        timeout: u64,
    },
    /// Acknowledge a finished round's result, so the daemon closes an unused disposable worker
    ///
    /// Fails while the round is still running or blocked. A notice not yet sent for the round is
    /// dropped, since its owner has the result.
    Ack {
        #[command(flatten)]
        task: TaskId,
        #[command(flatten)]
        round: Round,
    },
    /// Start the next round with the same interactive worker
    ///
    /// Only a turn the worker finishes after now counts as the new round's result.
    Rearm(TaskId),
}

fn absolute_path(text: &str) -> Result<PathBuf, &'static str> {
    let path = PathBuf::from(text);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("not an absolute path")
    }
}

pub fn main() -> ExitCode {
    let args = Args::parse();
    let level = if matches!(args.command, Command::Daemon { .. }) {
        "info"
    } else {
        "warn"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();
    let result = match args.command {
        Command::Report { runtime, state } => {
            // A hook must not hold up or fail the session it reports.
            if let Err(e) = state
                .resolve()
                .and_then(|dir| report::run(runtime, &dir))
                .with_context(|| format!("report {runtime}"))
            {
                eprintln!("rexume: {e:#}");
            }
            return ExitCode::SUCCESS;
        }
        Command::Gate { op, slot, state } => match state.resolve() {
            Ok(dir) => gate::run(&dir, &op, slot),
            Err(e) => Err(e),
        },
        command => run(command),
    };
    let printed = result.and_then(|value| {
        let Some(value) = value else {
            return Ok(ExitCode::SUCCESS);
        };
        let code = if value.get("timed_out") == Some(&json!(true)) {
            ExitCode::from(TIMED_OUT)
        } else {
            ExitCode::SUCCESS
        };
        writeln!(std::io::stdout().lock(), "{value:#}").context("failed to write to stdout")?;
        Ok(code)
    });
    printed.unwrap_or_else(|e| {
        eprintln!("rexume: {e:#}");
        ExitCode::FAILURE
    })
}

fn run(command: Command) -> Result<Option<Value>> {
    match command {
        Command::Install(server) => {
            let options = install::Install {
                exe: exe()?,
                rex: rex_path(server.rex)?,
                server: server_address(server.server)?,
                theme: server.theme_foreground.zip(server.theme_background),
                path: std::env::var("PATH").context("PATH is not set")?,
            };
            install::install(&home()?, &options).map(Some)
        }
        Command::Uninstall => install::uninstall(&home()?).map(Some),
        Command::Daemon { server, state } => {
            let options = daemon::Options {
                state_dir: state.resolve()?,
                cli: rex::Cli {
                    bin: rex_path(server.rex)?,
                    endpoint: server_address(server.server)?,
                },
                exe: exe()?,
                theme: server
                    .theme_foreground
                    .zip(server.theme_background)
                    .map(|(fg, bg)| json!({ "foreground": fg, "background": bg })),
            };
            daemon::run(&options).map(|()| None)
        }
        Command::Status(state) => status::run(&state.resolve()?).map(Some),
        Command::Task { command, state } => run_task(command, &state.resolve()?).map(Some),
        Command::Relay { socket, token } => rex::relay::run(&socket, &token).map(|()| None),
        Command::Report { .. } | Command::Gate { .. } => unreachable!("handled in main"),
    }
}

fn run_task(command: TaskCommand, dir: &std::path::Path) -> Result<Value> {
    match command {
        TaskCommand::Add {
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
        TaskCommand::List => task::list(dir),
        TaskCommand::Show { task, round } => task::show(dir, &task.id, round.round),
        TaskCommand::Wait { task, timeout } => {
            task::wait(dir, &task.id, Duration::from_secs(timeout))
        }
        TaskCommand::Ack { task, round } => task::ack(dir, &task.id, round.round),
        TaskCommand::Rearm(task) => task::rearm(dir, &task.id),
    }
}

fn home() -> Result<PathBuf> {
    std::env::home_dir().context("HOME is not set")
}

/// This executable by the path it was started with, so that commands written into the plist
/// and restored panes keep working after a package manager replaces the file behind it.
fn exe() -> Result<PathBuf> {
    let argv0 = PathBuf::from(std::env::args_os().next().unwrap_or_default());
    if argv0.is_absolute() {
        return Ok(argv0);
    }
    if argv0.components().count() == 1
        && let Ok(found) = which::which(&argv0)
    {
        return Ok(found);
    }
    std::env::current_exe().context("failed to find this executable")
}

fn rex_path(given: Option<PathBuf>) -> Result<PathBuf> {
    match given {
        Some(path) => Ok(path),
        None => which::which("rex").context("no `rex` on PATH; pass --rex"),
    }
}

fn server_address(given: Option<String>) -> Result<String> {
    if let Some(address) = given.or_else(|| std::env::var("REX_SERVER").ok()) {
        return Ok(address);
    }
    let socket = home()?.join("Library/Application Support/rex/server.sock");
    let path = socket.to_string_lossy();
    Ok(format!(
        "unix://{}",
        percent_encoding::utf8_percent_encode(&path, &PATH_ESCAPES)
    ))
}

/// The bytes a `unix://` address escapes in its path.
const PATH_ESCAPES: percent_encoding::AsciiSet = percent_encoding::CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b'?')
    .add(b'#');

#[cfg(test)]
mod tests;
