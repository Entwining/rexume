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

const LOCK: &str = ".lock";
const TEMP_PREFIX: &str = ".rexume-";
const KEPT: &str = "objects";

pub(super) struct Scratch {
    pub id: String,
    dir: PathBuf,
    task_dir: PathBuf,
    temp: PathBuf,
    lock: File,
    lock_path: PathBuf,
    ended: bool,
}

impl Scratch {
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
            // Inherited by every process the capture starts, so that the lock stays held while any
            // of them runs.
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

    pub fn seeded(&self) -> Result<u64> {
        size(&self.seed())
    }

    /// Objects Git writes before space is reserved go to the seed, outside the state directory.
    pub fn reading(&self) -> Isolation {
        Isolation {
            index: self.index(),
            objects: self.seed(),
            alternates: object_directory(&self.task_dir).into_os_string(),
        }
    }

    pub fn staging(&self) -> Result<Isolation> {
        std::fs::create_dir(&self.dir)?;
        Ok(Isolation {
            index: self.index(),
            objects: self.dir.clone(),
            alternates: std::env::join_paths([object_directory(&self.task_dir), self.seed()])?,
        })
    }

    pub fn lfs(&self) -> Result<OsString> {
        let dir = self.temp.join("lfs");
        std::fs::create_dir(&dir)?;
        let mut setting = OsString::from("lfs.storage=");
        setting.push(&dir);
        Ok(setting)
    }

    pub fn staged(&self) -> Result<u64> {
        size(&self.dir)
    }

    /// Called once the objects' space is recorded as kept, so a failed move leaves them for
    /// whoever next takes the lock to move.
    pub fn keep(mut self) -> Result<()> {
        self.ended = true;
        move_objects(&self.dir, &object_directory(&self.task_dir))?;
        delete(&self.dir)?;
        delete(&self.temp)?;
        delete_file(&self.lock_path)
    }

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
    fn drop(&mut self) {
        if !self.ended
            && let Err(e) = self.delete_files()
        {
            log::warn!("failed to delete what capture {} wrote: {e:#}", self.id);
        }
    }
}

pub struct Left {
    id: String,
    task_dir: PathBuf,
    /// `None` for a lock file found gone: it is deleted last, so the capture's other files are gone
    /// too.
    lock: Option<File>,
}

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

pub fn left(task_dir: &Path) -> Result<Vec<Left>> {
    let mut left = Vec::new();
    for id in captures(task_dir)? {
        left.extend(claim(task_dir, &id)?);
    }
    Ok(left)
}

pub fn captured(task_dir: &Path) -> Result<bool> {
    Ok(!captures(task_dir)?.is_empty())
}

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

    pub fn delete(&mut self) -> Result<()> {
        self.delete_temp()?;
        delete(&self.task_dir.join(&self.id))
    }

    pub fn keep(&mut self) -> Result<()> {
        self.delete_temp()?;
        let dir = self.task_dir.join(&self.id);
        move_objects(&dir, &self.task_dir.join(KEPT))?;
        delete(&dir)
    }

    pub fn size(&self) -> Result<u64> {
        size(&self.task_dir.join(&self.id))
    }

    pub fn forget(self) -> Result<()> {
        delete_file(&self.lock_path())
    }

    fn lock_path(&self) -> PathBuf {
        self.task_dir.join(format!("{}{LOCK}", self.id))
    }

    /// A recorded path that is not the capture's own was cut short by a capture killed while
    /// writing it, before it created the directory.
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

fn size(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for file in files(dir)? {
        total += std::fs::symlink_metadata(file)?.len();
    }
    Ok(total)
}

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

pub fn object_directory(task_dir: &Path) -> PathBuf {
    task_dir.join(KEPT)
}
