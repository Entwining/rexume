//! A capture's files. In the task's snapshot directory, `snapshots/TASK_ID` in the state directory:
//! `ID.lock`, on which every process of the capture holds a `flock` and which names the capture's
//! temporary directory, and `ID`, the objects the capture writes, created only once space is
//! reserved for them. In the system's temporary directory, `.rexume-ID`: the copy of the index,
//! the objects copied from the repository for Git to read, and Git LFS's storage, which hold no
//! object the snapshot keeps and so take none of the space snapshots may take. The lock file is
//! created first and deleted last, so a lock that can be taken shows every file a capture left
//! whose processes all ended.

use std::ffi::OsString;
use std::fs::{DirBuilder, File, TryLockError};
use std::io::{ErrorKind, Read, Seek, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};

use super::command::Isolation;
use crate::store::new_id;

/// What a capture's lock file is named after its ID.
const LOCK: &str = ".lock";
/// What the name of a capture's temporary directory is, before its ID.
const TEMP_PREFIX: &str = ".rexume-";
/// The task's objects, in its snapshot directory, which each capture adds to.
const KEPT: &str = "objects";

pub(super) struct Scratch {
    pub id: String,
    /// The capture's objects directory, `ID` in the task's snapshot directory.
    dir: PathBuf,
    /// The task's snapshot directory, whose objects the capture's go to once it keeps them.
    task_dir: PathBuf,
    temp: PathBuf,
    /// Held while the capture runs, and inherited by every process it starts.
    lock: File,
    lock_path: PathBuf,
    /// Set once `keep` or `into_left` takes over the capture's files: what either leaves is for
    /// whoever finishes the capture by its row.
    ended: bool,
}

impl Scratch {
    /// Creates a capture's lock file in `task_dir`, a task's snapshot directory, and its temporary
    /// directory.
    pub fn create(task_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(object_directory(task_dir))?;
        // Absolute, as Git runs in the working tree and the lock file records it for whoever next
        // takes the lock.
        let temp_dir = std::path::absolute(std::env::temp_dir())?;
        loop {
            let id = new_id();
            let lock_path = task_dir.join(format!("{id}{LOCK}"));
            let lock = File::create_new(&lock_path)?;
            match lock.try_lock() {
                Ok(()) => {}
                // A reclaimer took the new lock file for one a killed capture left.
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
            // A reclaimer may have taken it, and deleted it, before the lock was taken.
            match std::fs::metadata(&lock_path) {
                Ok(found) if found.ino() == lock.metadata()?.ino() => {}
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
                _ => continue,
            }
            let mut scratch = Self {
                dir: task_dir.join(&id),
                temp: temp_dir.join(format!("{TEMP_PREFIX}{id}")),
                id,
                task_dir: task_dir.to_path_buf(),
                lock,
                lock_path,
                ended: false,
            };
            let flags = FdFlag::from_bits_retain(fcntl(scratch.lock.as_fd(), FcntlArg::F_GETFD)?);
            fcntl(
                scratch.lock.as_fd(),
                FcntlArg::F_SETFD(flags.difference(FdFlag::FD_CLOEXEC)),
            )?;
            // Recorded before it is created, so that no temporary directory goes unrecorded.
            let temp = scratch.temp.as_os_str().as_encoded_bytes().to_vec();
            scratch.lock.write_all(&temp)?;
            DirBuilder::new().mode(0o700).create(&scratch.temp)?;
            std::fs::create_dir(scratch.seed())?;
            return Ok(scratch);
        }
    }

    pub fn index(&self) -> PathBuf {
        self.temp.join("index")
    }

    pub fn seed(&self) -> PathBuf {
        self.temp.join("seed")
    }

    /// The size of the objects copied from the repository so far.
    pub fn seeded(&self) -> Result<u64> {
        size(&self.seed())
    }

    /// Where Git finds objects while the changed files are listed: in the seed, where any object
    /// it wrote would go, with the task's kept objects as alternates.
    pub fn reading(&self) -> Isolation {
        Isolation {
            index: self.index(),
            objects: self.seed(),
            alternates: object_directory(&self.task_dir).into_os_string(),
        }
    }

    /// Creates the capture's objects directory, once space is reserved for what it writes: Git
    /// writes the capture's objects there, reading the task's kept objects and the seed as
    /// alternates.
    pub fn staging(&self) -> Result<Isolation> {
        std::fs::create_dir(&self.dir)?;
        Ok(Isolation {
            index: self.index(),
            objects: self.dir.clone(),
            alternates: std::env::join_paths([object_directory(&self.task_dir), self.seed()])?,
        })
    }

    /// The setting that gives Git LFS a directory for its storage in the temporary directory.
    pub fn lfs(&self) -> Result<OsString> {
        let dir = self.temp.join("lfs");
        std::fs::create_dir(&dir)?;
        let mut setting = OsString::from("lfs.storage=");
        setting.push(&dir);
        Ok(setting)
    }

    /// The size of the objects the capture wrote.
    pub fn staged(&self) -> Result<u64> {
        size(&self.dir)
    }

    /// Moves the capture's objects to the task's, then deletes its files. Called once their space
    /// is recorded as kept: should a move fail, the objects stay in the capture's directory for
    /// whoever next takes its lock to move.
    pub fn keep(mut self) -> Result<()> {
        self.ended = true;
        move_objects(&self.dir, &object_directory(&self.task_dir))?;
        delete(&self.dir)?;
        delete(&self.temp)?;
        delete_file(&self.lock_path)
    }

    /// The capture as one left, holding its lock, for a capture that failed once it reserved space
    /// to be finished by its row as a killed one is.
    pub fn into_left(mut self) -> Result<Left> {
        self.ended = true;
        Ok(Left {
            id: self.id.clone(),
            task_dir: self.task_dir.clone(),
            lock: Some(self.lock.try_clone()?),
        })
    }

    fn delete_files(&self) -> Result<()> {
        delete(&self.temp)?;
        delete(&self.dir)?;
        delete_file(&self.lock_path)
    }
}

impl Drop for Scratch {
    /// Deletes the files of a capture that ended before it reserved space, or by a panic.
    fn drop(&mut self) {
        if !self.ended
            && let Err(e) = self.delete_files()
        {
            log::warn!("failed to delete what capture {} wrote: {e:#}", self.id);
        }
    }
}

/// A capture none of whose processes runs, whose lock this holds: its lock file found gone is the
/// end of one that a capture or a reclaimer killed while deleting it left.
pub struct Left {
    id: String,
    task_dir: PathBuf,
    lock: Option<File>,
}

/// Capture `id` of `task_dir`, a task's snapshot directory, unless one of its processes runs.
pub fn claim(task_dir: &Path, id: &str) -> Result<Option<Left>> {
    let mut left = Left {
        id: id.to_owned(),
        task_dir: task_dir.to_path_buf(),
        lock: None,
    };
    match File::open(left.lock_path()) {
        Ok(lock) => match lock.try_lock() {
            Ok(()) => left.lock = Some(lock),
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        },
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(Some(left))
}

/// The captures in `task_dir`, a task's snapshot directory, none of whose processes runs.
pub fn left(task_dir: &Path) -> Result<Vec<Left>> {
    let mut left = Vec::new();
    for id in captures(task_dir)? {
        left.extend(claim(task_dir, &id)?);
    }
    Ok(left)
}

/// Whether `task_dir` holds the lock file of any capture.
pub fn captured(task_dir: &Path) -> Result<bool> {
    Ok(!captures(task_dir)?.is_empty())
}

/// The ID of each capture whose lock file is in `task_dir`.
fn captures(task_dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(task_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        if let Some(id) = name.to_str().and_then(|name| name.strip_suffix(LOCK)) {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

impl Left {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Deletes the capture's objects and its temporary directory.
    pub fn delete(&mut self) -> Result<()> {
        self.delete_temp()?;
        delete(&self.task_dir.join(&self.id))
    }

    /// Moves the objects left in the capture's directory to the task's, then deletes the
    /// directory and the temporary directory.
    pub fn keep(&mut self) -> Result<()> {
        self.delete_temp()?;
        let dir = self.task_dir.join(&self.id);
        move_objects(&dir, &self.task_dir.join(KEPT))?;
        delete(&dir)
    }

    /// The size of the objects left in the capture's directory.
    pub fn size(&self) -> Result<u64> {
        size(&self.task_dir.join(&self.id))
    }

    /// Deletes the lock file, the capture's last file.
    pub fn forget(self) -> Result<()> {
        delete_file(&self.lock_path())
    }

    fn lock_path(&self) -> PathBuf {
        self.task_dir.join(format!("{}{LOCK}", self.id))
    }

    /// Deletes the temporary directory the lock file names, when it names the capture's: a
    /// capture killed while recording it had created none.
    fn delete_temp(&mut self) -> Result<()> {
        let Some(lock) = &mut self.lock else {
            return Ok(());
        };
        let mut recorded = Vec::new();
        lock.rewind()?;
        lock.read_to_end(&mut recorded)?;
        let temp = PathBuf::from(String::from_utf8_lossy(&recorded).into_owned());
        let named = temp.file_name().and_then(|name| name.to_str())
            == Some(format!("{TEMP_PREFIX}{}", self.id).as_str());
        if temp.is_absolute() && named {
            delete(&temp)?;
        }
        Ok(())
    }
}

/// Moves every object under `from`, a capture's objects directory, to the same path under `to`.
fn move_objects(from: &Path, to: &Path) -> Result<()> {
    for file in files(from)? {
        let kept = to.join(file.strip_prefix(from)?);
        kept.parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::rename(&file, &kept))
            .with_context(|| format!("failed to keep `{}`", file.display()))?;
    }
    Ok(())
}

/// The total size of the regular files under `dir`.
fn size(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for file in files(dir)? {
        total += std::fs::symlink_metadata(file)?.len();
    }
    Ok(total)
}

/// The regular files under `dir`, none once it is gone.
fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                found.push(entry.path());
            }
        }
    }
    Ok(found)
}

fn delete(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            Err(e).with_context(|| format!("failed to delete `{}`", dir.display()))
        }
        _ => Ok(()),
    }
}

fn delete_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            Err(e).with_context(|| format!("failed to delete `{}`", path.display()))
        }
        _ => Ok(()),
    }
}

/// The directory of the objects a task's captures kept, in `task_dir`, its snapshot directory.
pub fn object_directory(task_dir: &Path) -> PathBuf {
    task_dir.join(KEPT)
}
