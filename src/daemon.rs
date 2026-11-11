mod block;
mod connection;
mod epoch;
#[cfg(test)]
mod tests;
mod view;

use std::collections::{HashMap, HashSet};
use std::fs::{File, Permissions, TryLockError};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum as _;
use nix::sys::signal::{SigSet, Signal};
use rusqlite::params;

use crate::event::{Queue, Token};
use crate::install;
use crate::rex::Cli;
use crate::runtime::Runtime;
use crate::store::{self, Store, now_ms};
use crate::task;

pub struct Options {
    pub state_dir: PathBuf,
    pub cli: Cli,
    pub exe: PathBuf,
    pub home: PathBuf,
}

const LISTEN: Token = 1;
const PIPE: Token = 2;
const ADAPTER: Token = 3;
const WAL: Token = 4;
const SERVER_DIR: Token = 5;
const WAITER_EXIT: Token = 6;
const EXE: Token = 7;
const STOP: Token = 8;
const FIRST_FILE: Token = 1000;

const STOP_SIGNALS: [Signal; 2] = [Signal::SIGTERM, Signal::SIGINT];

pub fn run(options: &Options) -> Result<()> {
    // A signalfd reads only blocked signals, and kqueue reports blocked ones too. Children inherit
    // the blocked set.
    let mut stop = SigSet::empty();
    for signal in STOP_SIGNALS {
        stop.add(signal);
    }
    stop.thread_block()?;
    // `register` can follow a record not written yet only through an existing directory.
    for runtime in Runtime::value_variants() {
        if let Some(records) = runtime.records(&options.state_dir) {
            store::create_private_dir(&records)?;
        }
    }
    // Earlier versions created the state directory open to other users; restoring works either
    // way, so a failure only warns.
    match std::fs::metadata(&options.state_dir) {
        Ok(meta) if meta.permissions().mode() & 0o077 == 0 => {}
        _ => match std::fs::set_permissions(&options.state_dir, Permissions::from_mode(0o700)) {
            Ok(()) => log::info!("restricted `{}` to its user", options.state_dir.display()),
            Err(e) => log::warn!(
                "failed to restrict `{}` to its user: {e}",
                options.state_dir.display()
            ),
        },
    }
    let lock = File::create(options.state_dir.join("daemon.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            bail!("another daemon holds `{}`", options.state_dir.display())
        }
        Err(TryLockError::Error(e)) => {
            return Err(e).with_context(|| {
                format!(
                    "failed to lock `{}`",
                    options.state_dir.join("daemon.lock").display()
                )
            });
        }
    }
    refresh_integrations(&options.home);
    let mut store = Store::open(&options.state_dir)?;
    {
        let tx = store.write()?;
        tx.execute(
            "UPDATE epoch SET ended_at = ?1, end_reason = 'disconnected' WHERE ended_at IS NULL",
            params![now_ms()],
        )?;
        task::release_held(&tx)?;
        tx.commit()?;
    }
    for (kind, id) in task::orphaned_sends(&mut store)? {
        log::warn!("{kind} {id}: left mid-send by the previous daemon, now unknown");
    }
    let socket = options.state_dir.join("daemon.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to listen on `{}`", socket.display()))?;
    listener.set_nonblocking(true)?;
    let mut queue = Queue::new()?;
    queue.read(listener.as_fd(), LISTEN)?;
    queue.file(&options.state_dir.join("state.db-wal"), WAL)?;
    queue.entry(&options.exe, EXE)?;
    for signal in STOP_SIGNALS {
        queue.signal(signal, STOP)?;
    }
    let mut daemon = Daemon {
        options,
        store,
        queue,
        listener,
        socket,
        files: HashMap::new(),
        via_dir: HashSet::new(),
        next_file: FIRST_FILE,
        pids: HashSet::new(),
    };
    match daemon.serve() {
        Err(e) => match e.downcast_ref::<Exit>() {
            Some(exit @ Exit::Replaced) => {
                log::info!("{exit}; exiting so that its job starts the new one");
                Ok(())
            }
            Some(exit @ Exit::Stopped) => {
                log::info!("{exit}");
                Ok(())
            }
            None => Err(e),
        },
        Ok(never) => match never {},
    }
}

/// A daemon starts after every upgrade, so this is where an upgrade's integration files reach
/// each runtime. A runtime missing one is no reason to stop restoring the others.
fn refresh_integrations(home: &Path) {
    for (runtime, refreshed) in install::refresh_extensions(home) {
        match refreshed {
            Ok("updated") => log::info!("updated the {runtime} extension"),
            Ok(_) => {}
            Err(e) => log::warn!("failed to refresh the {runtime} extension: {e:#}"),
        }
    }
    match install::refresh_claude_plugin(home) {
        Ok("updated") => log::info!("updated the Claude Code plugin"),
        Ok(_) => {}
        Err(e) => log::warn!("failed to refresh the Claude Code plugin: {e:#}"),
    }
    match install::refresh_skills(home) {
        Ok(refreshed) => {
            for (key, outcome) in refreshed {
                if outcome == "updated" {
                    log::info!("updated the skill stub ({key})");
                }
            }
        }
        Err(e) => log::warn!("failed to refresh the skill stubs: {e:#}"),
    }
    match install::refresh_claude_hooks(home) {
        Ok("created") => log::info!("added the Claude Code turn hooks"),
        Ok(_) => {}
        Err(e) => log::warn!("failed to add the Claude Code turn hooks: {e:#}"),
    }
}

#[derive(Debug, thiserror::Error)]
enum Exit {
    /// The job names the path this daemon was started from, so it starts the new executable once
    /// this one exits.
    #[error("the rexume executable was replaced")]
    Replaced,
    #[error("stopped by a signal")]
    Stopped,
}

struct Daemon<'a> {
    options: &'a Options,
    store: Store,
    queue: Queue,
    listener: UnixListener,
    socket: PathBuf,
    files: HashMap<PathBuf, Token>,
    via_dir: HashSet<Token>,
    next_file: Token,
    pids: HashSet<i64>,
}
