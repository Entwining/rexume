//! The daemon: one long-running process that observes a Rex server, keeps the panes' states,
//! reopens conversations in the panes a new server instance restored, and moves tasks along. It
//! waits on one event queue and wakes only for an event, a deadline, a signal, a process exit, or
//! a file it follows.

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
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use nix::sys::signal::{SigSet, Signal};
use rusqlite::params;

use crate::event::{Queue, Token};
use crate::install;
use crate::rex::Cli;
use crate::runtime::claude;
use crate::store::{self, Store, now_ms};
use crate::task;

pub struct Options {
    pub state_dir: PathBuf,
    pub cli: Cli,
    /// This executable, which restored panes run as their gate and the adapter runs as its relay.
    pub exe: PathBuf,
    /// The home directory whose Pi extension the daemon refreshes when it starts.
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
/// Files the task module follows get tokens from here up.
const FIRST_FILE: Token = 1000;

const STOP_SIGNALS: [Signal; 2] = [Signal::SIGTERM, Signal::SIGINT];

pub fn run(options: &Options) -> Result<()> {
    // Blocked, as a signalfd reads only blocked signals and kqueue reports them too. Every child
    // inherits the blocked set, so the daemon ends `rex do` with SIGKILL instead of passing them on.
    let mut stop = SigSet::empty();
    for signal in STOP_SIGNALS {
        stop.add(signal);
    }
    stop.thread_block()?;
    // With the directory of Claude Code's turn records, a record not written yet is followed
    // through it until it appears.
    let records = claude::records(&options.state_dir);
    store::create_private_dir(&records)?;
    // A daemon starts after every upgrade, so this closes a state directory an earlier version
    // created open to other users. The daemon works the same either way, so a failure is no reason
    // to stop restoring.
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
    // A daemon starts after every upgrade, so this is where a new extension reaches Pi; Pi without
    // it only misses reports and notices, which is no reason to stop restoring.
    match install::refresh_pi_extension(&options.home) {
        Ok("updated") => log::info!("updated the Pi extension"),
        Ok(_) => {}
        Err(e) => log::warn!("failed to refresh the Pi extension: {e:#}"),
    }
    // Claude Code rounds finish only from the turn record its turn hooks write.
    match install::refresh_claude_hooks(&options.home) {
        Ok("created") => log::info!("added the Claude Code turn hooks"),
        Ok(_) => {}
        Err(e) => log::warn!("failed to add the Claude Code turn hooks: {e:#}"),
    }
    let mut store = Store::open(&options.state_dir)?;
    // Holding the lock, an epoch still open belongs to a daemon that died. Whether its server
    // stopped afterwards is unknown, so nothing it saw counts as the server's final state. A pane
    // it held for a close goes back to `open`; the next pass checks it again.
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
    // Holding the lock, this process owns the socket path.
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

/// Why the daemon stops serving without an error. The child it runs is ended on the way out.
#[derive(Debug, thiserror::Error)]
enum Exit {
    /// The executable at the path this daemon was started from was replaced or removed, as an
    /// upgrade does. The job names that path, so it starts the new executable once this one
    /// exits; every restore and notice is safe to pick up from its saved state.
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
    /// Tokens of files followed through their directory because they did not exist yet.
    via_dir: HashSet<Token>,
    next_file: Token,
    pids: HashSet<i64>,
}
