//! The one kqueue the daemon waits on: a readable socket, a process's exit, a change to a file
//! or directory, and a signal each fire the token they were registered with.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use nix::errno::Errno;
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue as Queue};
use nix::sys::signal::Signal;

pub type Token = u64;

pub struct Fired {
    pub token: Token,
    /// A watched file was deleted or renamed: what its path names now needs watching again.
    pub gone: bool,
}

pub struct Kqueue {
    queue: Queue,
    /// Open files of the vnode watches; closing one removes its watch.
    files: HashMap<Token, File>,
}

impl Kqueue {
    pub fn new() -> Result<Self> {
        Ok(Self {
            queue: Queue::new()?,
            files: HashMap::new(),
        })
    }

    fn change(
        &self,
        ident: usize,
        filter: EventFilter,
        flags: EvFlags,
        fflags: FilterFlag,
        token: Token,
    ) -> nix::Result<()> {
        let udata = isize::try_from(token).map_err(|_| Errno::EINVAL)?;
        let change = KEvent::new(ident, filter, flags, fflags, 0, udata);
        self.queue.kevent(&[change], &mut [], None).map(drop)
    }

    pub fn read(&self, fd: RawFd, token: Token) -> Result<()> {
        let fd = usize::try_from(fd)?;
        Ok(self.change(
            fd,
            EventFilter::EVFILT_READ,
            EvFlags::EV_ADD,
            FilterFlag::empty(),
            token,
        )?)
    }

    /// Fires when `signal` is sent to this process. The kqueue records the signal even while it
    /// is blocked, which is how the daemon takes it without a handler.
    pub fn signal(&self, signal: Signal, token: Token) -> Result<()> {
        let ident = usize::try_from(signal as i32)?;
        Ok(self.change(
            ident,
            EventFilter::EVFILT_SIGNAL,
            EvFlags::EV_ADD,
            FilterFlag::empty(),
            token,
        )?)
    }

    /// Watches for a process's exit; `false` when it has already exited.
    pub fn exit(&self, pid: i64, token: Token) -> Result<bool> {
        let Ok(ident) = usize::try_from(pid) else {
            return Ok(false);
        };
        match self.change(
            ident,
            EventFilter::EVFILT_PROC,
            EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
            FilterFlag::NOTE_EXIT,
            token,
        ) {
            Ok(()) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn forget_exit(&self, pid: i64) {
        if let Ok(ident) = usize::try_from(pid) {
            let _ = self.change(
                ident,
                EventFilter::EVFILT_PROC,
                EvFlags::EV_DELETE,
                FilterFlag::empty(),
                0,
            );
        }
    }

    /// Watches a file for writes, or a directory for entries added or removed, replacing an
    /// earlier watch under the same token; `false` when nothing exists at the path.
    pub fn file(&mut self, path: &Path, token: Token) -> Result<bool> {
        self.watch(path, token, libc::O_EVTONLY)
    }

    /// Like `file`, but a symbolic link at the path is watched itself, so replacing the link
    /// fires even though the file it pointed to is unchanged.
    pub fn entry(&mut self, path: &Path, token: Token) -> Result<bool> {
        self.watch(path, token, libc::O_EVTONLY | libc::O_SYMLINK)
    }

    fn watch(&mut self, path: &Path, token: Token, flags: i32) -> Result<bool> {
        self.files.remove(&token);
        let file = match OpenOptions::new().read(true).custom_flags(flags).open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let fflags = FilterFlag::NOTE_WRITE
            | FilterFlag::NOTE_EXTEND
            | FilterFlag::NOTE_DELETE
            | FilterFlag::NOTE_RENAME;
        let fd = usize::try_from(file.as_raw_fd())?;
        self.change(
            fd,
            EventFilter::EVFILT_VNODE,
            EvFlags::EV_ADD | EvFlags::EV_CLEAR,
            fflags,
            token,
        )?;
        self.files.insert(token, file);
        Ok(true)
    }

    pub fn forget_file(&mut self, token: Token) {
        self.files.remove(&token);
    }

    /// Waits for at least one registered event, or until `timeout` passes when one is given.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<Vec<Fired>> {
        let timeout = timeout.map(|t| libc::timespec {
            tv_sec: i64::try_from(t.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(t.subsec_nanos()),
        });
        let mut events = [KEvent::new(
            0,
            EventFilter::EVFILT_READ,
            EvFlags::empty(),
            FilterFlag::empty(),
            0,
            0,
        ); 64];
        let n = loop {
            match self.queue.kevent(&[], &mut events, timeout) {
                Ok(n) => break n,
                Err(Errno::EINTR) => {}
                Err(e) => return Err(e.into()),
            }
        };
        Ok(events[..n]
            .iter()
            .map(|event| Fired {
                token: Token::try_from(event.udata()).unwrap_or_default(),
                gone: event.filter().is_ok_and(|f| f == EventFilter::EVFILT_VNODE)
                    && event
                        .fflags()
                        .intersects(FilterFlag::NOTE_DELETE | FilterFlag::NOTE_RENAME),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn fires_for_appends_replacement_and_exit() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("t.jsonl");
        let mut kq = Kqueue::new()?;
        assert!(!kq.file(&path, 1)?);
        std::fs::write(&path, "a\n")?;
        assert!(kq.file(&path, 1)?);
        assert!(kq.wait(Some(Duration::ZERO))?.is_empty());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"b\n")?;
        let fired = kq.wait(Some(Duration::from_secs(1)))?;
        assert!(fired.iter().any(|f| f.token == 1 && !f.gone));
        std::fs::write(dir.path().join("new"), "c\n")?;
        std::fs::rename(dir.path().join("new"), &path)?;
        let fired = kq.wait(Some(Duration::from_secs(1)))?;
        assert!(fired.iter().any(|f| f.token == 1 && f.gone));

        let mut child = std::process::Command::new("/bin/sleep")
            .arg("0.1")
            .spawn()?;
        let pid = i64::from(child.id());
        assert!(kq.exit(pid, 2)?);
        let fired = kq.wait(Some(Duration::from_secs(5)))?;
        assert!(fired.iter().any(|f| f.token == 2));
        child.wait()?;
        assert!(!kq.exit(pid, 2)?);
        Ok(())
    }
}
