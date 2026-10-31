//! The command line.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::runtime::Runtime;
use crate::{daemon, gate, install, report, rex, status, store};

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
    /// Defaults to REX_SERVER, then to the socket of the server the Rex app starts on macOS, or
    /// `$XDG_RUNTIME_DIR/rex/server.sock` on Linux.
    #[arg(long, value_name = "ADDRESS")]
    server: Option<String>,
    /// Ignored, accepted because a job that an earlier install wrote still passes it
    #[arg(long, hide = true, value_name = "COLOR")]
    theme_foreground: Option<String>,
    /// Ignored, as `--theme-foreground`
    #[arg(long, hide = true, value_name = "COLOR")]
    theme_background: Option<String>,
}

impl Server {
    fn warn_ignored(&self) {
        if self.theme_foreground.is_some() || self.theme_background.is_some() {
            log::warn!(
                "--theme-foreground and --theme-background are ignored: Rexume no longer creates \
                 terminals; run `rexume install` again to drop them from the job"
            );
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Install the session hooks, the Pi extension, and the job that runs the daemon
    ///
    /// Writes a SessionStart hook into `~/.claude/settings.json` and `~/.codex/hooks.json`, the
    /// extension into `~/.pi/agent/extensions/rexume.ts`, and the job: the LaunchAgent
    /// `io.github.entwining.rexume` on macOS, the systemd user service `rexume.service` on Linux.
    /// Prints what changed as JSON. A runtime whose directory does not exist is skipped. Running it
    /// again changes only what is missing or different.
    Install(Server),
    /// Remove what install wrote, keeping the state directory and this executable
    ///
    /// Removes the hooks, the Pi extension when install wrote it, and the job, which stops the
    /// daemon. `~/.local/state/rexume` keeps the recorded panes, conversations, and task results;
    /// delete it to remove them too.
    Uninstall,
    /// Follow the Rex server and reopen the conversations of the panes it restores
    ///
    /// The job install writes runs this. It runs until stopped, logs to stderr, and exits at once
    /// when another daemon holds the same state directory. Set `RUST_LOG=rexume=debug` to log
    /// every call it makes through its adapter.
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
    #[command(after_help = task::EXAMPLES)]
    Task {
        #[command(subcommand)]
        command: task::Command,
        #[command(flatten)]
        state: StateDir,
    },
    /// Run in the shell of a restored pane: claim its restore, then start its conversation
    #[command(hide = true)]
    Gate {
        /// The restore
        #[arg(value_name = "ID")]
        id: String,
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
        Command::Gate { id, state } => match state.resolve() {
            Ok(dir) => return ExitCode::from(gate::run(&dir, &id)),
            Err(e) => Err(e),
        },
        command => run(command),
    };
    let printed = result.and_then(|printed| {
        let Some((value, status)) = printed else {
            return Ok(ExitCode::SUCCESS);
        };
        writeln!(std::io::stdout().lock(), "{value:#}").context("failed to write to stdout")?;
        Ok(ExitCode::from(status))
    });
    printed.unwrap_or_else(|e| {
        eprintln!("rexume: {e:#}");
        ExitCode::FAILURE
    })
}

/// Runs a command, returning what it prints and its exit status.
fn run(command: Command) -> Result<Option<(Value, u8)>> {
    match command {
        Command::Install(server) => {
            server.warn_ignored();
            let options = install::Install {
                exe: exe()?,
                rex: rex_path(server.rex)?,
                server: server_address(server.server)?,
                path: std::env::var("PATH").context("PATH is not set")?,
            };
            install::install(&home()?, &options).map(|value| Some((value, 0)))
        }
        Command::Uninstall => install::uninstall(&home()?).map(|value| Some((value, 0))),
        Command::Daemon { server, state } => {
            server.warn_ignored();
            let options = daemon::Options {
                state_dir: state.resolve()?,
                cli: rex::Cli {
                    bin: rex_path(server.rex)?,
                    endpoint: server_address(server.server)?,
                },
                exe: exe()?,
                home: home()?,
            };
            daemon::run(&options).map(|()| None)
        }
        Command::Status(state) => status::run(&state.resolve()?).map(|value| Some((value, 0))),
        Command::Task { command, state } => task::run(command, &state.resolve()?).map(Some),
        Command::Relay { socket, token } => rex::relay::run(&socket, &token).map(|()| None),
        Command::Report { .. } | Command::Gate { .. } => unreachable!("handled in main"),
    }
}

fn home() -> Result<PathBuf> {
    std::env::home_dir().context("HOME is not set")
}

/// This executable by the path it was started with, so that commands written into the job
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
    let socket = default_socket()?;
    let path = socket.to_string_lossy();
    Ok(format!(
        "unix://{}",
        percent_encoding::utf8_percent_encode(&path, &PATH_ESCAPES)
    ))
}

/// The socket of the server the Rex app starts.
#[cfg(target_os = "macos")]
fn default_socket() -> Result<PathBuf> {
    Ok(home()?.join("Library/Application Support/rex/server.sock"))
}

/// The socket of the user's own Rex server. Without a runtime directory Rex puts it elsewhere, in
/// a place that has changed between Rex releases, so it is not guessed.
#[cfg(target_os = "linux")]
fn default_socket() -> Result<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("rex/server.sock"))
        .context("XDG_RUNTIME_DIR is not set to an absolute path; pass --server or set REX_SERVER")
}

/// The bytes a `unix://` address escapes in its path.
const PATH_ESCAPES: percent_encoding::AsciiSet = percent_encoding::CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b'?')
    .add(b'#');

mod task;
#[cfg(test)]
mod tests;
