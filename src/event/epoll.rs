use std::collections::HashMap;
use std::fs::Metadata;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use nix::errno::Errno;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use rustix::process::{Pid, PidfdFlags, pidfd_open};

use super::{Fired, Token};

const INOTIFY: u64 = u64::MAX;
const SIGNALS: u64 = u64::MAX - 1;
/// A pidfd's epoll data is `EXIT` plus its process ID, so a token passed to `read` stays below it.
const EXIT: u64 = 1 << 62;

/// One mask per kind of inode: inotify keeps one watch per inode, and a second `add_watch` on it
/// replaces the first's mask.
const DIR_MASK: AddWatchFlags = AddWatchFlags::IN_CREATE
    .union(AddWatchFlags::IN_DELETE)
    .union(AddWatchFlags::IN_MOVED_FROM)
    .union(AddWatchFlags::IN_MOVED_TO)
    .union(AddWatchFlags::IN_DELETE_SELF)
    .union(AddWatchFlags::IN_MOVE_SELF);

/// `IN_ATTRIB` reports a file's unlinking, as its link count changes; on a directory it and
/// `IN_MODIFY` would also report changes to every file in it.
const FILE_MASK: AddWatchFlags = DIR_MASK
    .union(AddWatchFlags::IN_MODIFY)
    .union(AddWatchFlags::IN_ATTRIB);

const MAYBE_GONE: AddWatchFlags = AddWatchFlags::IN_ATTRIB
    .union(AddWatchFlags::IN_MOVE_SELF)
    .union(AddWatchFlags::IN_DELETE_SELF)
    .union(AddWatchFlags::IN_IGNORED);

const GONE: AddWatchFlags = AddWatchFlags::IN_DELETE_SELF.union(AddWatchFlags::IN_IGNORED);

struct Watch {
    wd: WatchDescriptor,
    path: PathBuf,
    link: bool,
    inode: (u64, u64),
}

impl Watch {
    /// A file replaced or removed while still open or mapped, as the daemon's own executable is,
    /// gets its `IN_DELETE_SELF` only once the last holder lets go, so the path is what tells.
    fn replaced(&self) -> bool {
        metadata(&self.path, self.link)
            .ok()
            .flatten()
            .is_none_or(|meta| (meta.dev(), meta.ino()) != self.inode)
    }
}

fn metadata(path: &Path, link: bool) -> std::io::Result<Option<Metadata>> {
    let meta = if link {
        std::fs::symlink_metadata(path)
    } else {
        std::fs::metadata(path)
    };
    match meta {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub struct Queue {
    epoll: Epoll,
    inotify: Inotify,
    watches: HashMap<Token, Watch>,
    exits: HashMap<i64, (OwnedFd, Token)>,
    signals: Option<SignalFd>,
    signal_tokens: HashMap<Signal, Token>,
}

impl Queue {
    pub fn new() -> Result<Self> {
        let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC)?;
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)?;
        epoll.add(&inotify, EpollEvent::new(EpollFlags::EPOLLIN, INOTIFY))?;
        Ok(Self {
            epoll,
            inotify,
            watches: HashMap::new(),
            exits: HashMap::new(),
            signals: None,
            signal_tokens: HashMap::new(),
        })
    }

    pub fn read(&self, fd: BorrowedFd<'_>, token: Token) -> Result<()> {
        Ok(self
            .epoll
            .add(fd, EpollEvent::new(EpollFlags::EPOLLIN, token))?)
    }

    /// A signalfd receives only signals the caller blocks.
    pub fn signal(&mut self, signal: Signal, token: Token) -> Result<()> {
        self.signal_tokens.insert(signal, token);
        let mask: SigSet = self.signal_tokens.keys().copied().collect();
        match &self.signals {
            Some(signals) => signals.set_mask(&mask)?,
            None => {
                let signals =
                    SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC)?;
                self.epoll
                    .add(&signals, EpollEvent::new(EpollFlags::EPOLLIN, SIGNALS))?;
                self.signals = Some(signals);
            }
        }
        Ok(())
    }

    pub fn exit(&mut self, pid: i64, token: Token) -> Result<bool> {
        self.forget_exit(pid);
        let Some(id) = i32::try_from(pid)
            .ok()
            .filter(|&id| id > 0)
            .and_then(Pid::from_raw)
        else {
            return Ok(false);
        };
        let fd = match pidfd_open(id, PidfdFlags::empty()) {
            Ok(fd) => fd,
            // Older kernels give `EINVAL` for a group leader reaped while its ID still names its
            // group or session.
            Err(rustix::io::Errno::SRCH | rustix::io::Errno::INVAL) => return Ok(false),
            // A kernel without pidfds cannot report another process's exit without polling.
            Err(rustix::io::Errno::NOSYS) => {
                bail!("rexume needs Linux 5.3 or later, for pidfd_open")
            }
            Err(e) => return Err(e.into()),
        };
        let data = EXIT + u64::from(id.as_raw_pid().unsigned_abs());
        self.epoll
            .add(&fd, EpollEvent::new(EpollFlags::EPOLLIN, data))?;
        self.exits.insert(pid, (fd, token));
        Ok(true)
    }

    pub fn forget_exit(&mut self, pid: i64) {
        if let Some((fd, _)) = self.exits.remove(&pid) {
            let _ = self.epoll.delete(&fd);
        }
    }

    pub fn file(&mut self, path: &Path, token: Token) -> Result<bool> {
        self.watch(path, token, false)
    }

    pub fn entry(&mut self, path: &Path, token: Token) -> Result<bool> {
        self.watch(path, token, true)
    }

    fn watch(&mut self, path: &Path, token: Token, link: bool) -> Result<bool> {
        self.forget_file(token);
        // Read before the watch is added, so that a replacement in between leaves the recorded
        // inode stale, which only reports the path as replaced once too often.
        let Some(meta) = metadata(path, link)? else {
            return Ok(false);
        };
        let mut mask = if meta.is_dir() { DIR_MASK } else { FILE_MASK };
        if link {
            mask |= AddWatchFlags::IN_DONT_FOLLOW;
        }
        let wd = match self.inotify.add_watch(path, mask) {
            Ok(wd) => wd,
            Err(Errno::ENOENT) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let path = path.to_owned();
        self.watches.insert(
            token,
            Watch {
                wd,
                path,
                link,
                inode: (meta.dev(), meta.ino()),
            },
        );
        Ok(true)
    }

    pub fn forget_file(&mut self, token: Token) {
        let Some(watch) = self.watches.remove(&token) else {
            return;
        };
        if !self.watches.values().any(|other| other.wd == watch.wd) {
            // Fails when the kernel has removed the watch already, with its inode.
            let _ = self.inotify.rm_watch(watch.wd);
        }
    }

    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<Vec<Fired>> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            // Rounded up, so that a deadline less than a millisecond away is not a busy wait.
            let timeout = deadline.map_or(EpollTimeout::NONE, |at| {
                let left = at.saturating_duration_since(Instant::now());
                EpollTimeout::try_from(left.as_nanos().div_ceil(1_000_000))
                    .unwrap_or(EpollTimeout::MAX)
            });
            let mut events = [EpollEvent::empty(); 64];
            let n = match self.epoll.wait(&mut events, timeout) {
                Ok(n) => n,
                Err(Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            };
            let mut fired = Vec::new();
            for event in &events[..n] {
                match event.data() {
                    INOTIFY => self.changes(&mut fired)?,
                    SIGNALS => self.signals(&mut fired)?,
                    data if data >= EXIT => {
                        let pid = i64::try_from(data - EXIT)?;
                        if let Some((fd, token)) = self.exits.remove(&pid) {
                            let _ = self.epoll.delete(&fd);
                            fire(&mut fired, token, false);
                        }
                    }
                    token => fire(&mut fired, token, false),
                }
            }
            // A wake can fire nothing: removing a watch queues a last event for it, which names no
            // watch any more.
            if !fired.is_empty() || deadline.is_some_and(|at| Instant::now() >= at) {
                return Ok(fired);
            }
        }
    }

    fn changes(&mut self, fired: &mut Vec<Fired>) -> Result<()> {
        let events = match self.inotify.read_events() {
            Ok(events) => events,
            Err(Errno::EAGAIN) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for event in events {
            if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                for (&token, watch) in &self.watches {
                    fire(fired, token, watch.replaced());
                }
                continue;
            }
            for (&token, watch) in &self.watches {
                if watch.wd == event.wd {
                    let gone = event.mask.intersects(MAYBE_GONE)
                        && (event.mask.intersects(GONE) || watch.replaced());
                    fire(fired, token, gone);
                }
            }
        }
        Ok(())
    }

    fn signals(&self, fired: &mut Vec<Fired>) -> Result<()> {
        let Some(signals) = &self.signals else {
            return Ok(());
        };
        while let Some(info) = signals.read_signal()? {
            let token = i32::try_from(info.ssi_signo)
                .ok()
                .and_then(|signo| Signal::try_from(signo).ok())
                .and_then(|signal| self.signal_tokens.get(&signal));
            if let Some(&token) = token {
                fire(fired, token, false);
            }
        }
        Ok(())
    }
}

fn fire(fired: &mut Vec<Fired>, token: Token, gone: bool) {
    match fired.iter_mut().find(|f| f.token == token) {
        Some(f) => f.gone |= gone,
        None => fired.push(Fired { token, gone }),
    }
}
