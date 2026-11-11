use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::Value;

mod space;

use super::CURRENT_ROUND;
use crate::git;
use crate::store::{RETENTION, Store, now_ms};

pub(super) struct Taken {
    tree: Option<String>,
    note: Option<String>,
    created_at: i64,
}

fn task_dir(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join("snapshots").join(id)
}

pub(super) fn worktree(process: &Value) -> (Option<String>, Option<String>) {
    let Some(cwd) = process.pointer("/foreground/cwd").and_then(Value::as_str) else {
        let why = "Rex reported no working directory of the pane's foreground program";
        return (None, Some(why.to_owned()));
    };
    match git::worktree(Path::new(cwd)) {
        Ok(top) => (Some(top.to_string_lossy().into_owned()), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    }
}

pub(super) fn take(store: &mut Store, id: &str, worktree: &str) -> Taken {
    let created_at = now_ms();
    let (tree, note) = match capture(store, id, Path::new(worktree)) {
        Ok(tree) => (Some(tree), None),
        Err(e) => {
            if let Some(git::Stopped(signal)) = e.downcast_ref() {
                // On macOS the signal was delivered once unblocked; a signalfd took it on Linux.
                let _ = nix::sys::signal::raise(*signal);
            }
            log::debug!("task {id}: no snapshot: {e:#}");
            (None, Some(format!("{e:#}")))
        }
    };
    Taken {
        tree,
        note,
        created_at,
    }
}

fn capture(store: &mut Store, id: &str, worktree: &Path) -> Result<String> {
    let _blocked = git::Blocked::new()?;
    space::recover(store, id)?;
    let mut capture = git::Capture::list(worktree, &task_dir(store.dir(), id), git::BUDGET)?;
    let scratch = capture.id().to_owned();
    space::reserve(store, id, &scratch, capture.estimate())?;
    let staged = capture.stage().and_then(|(tree, bytes)| {
        space::settle(store, id, &scratch, bytes)?;
        Ok(tree)
    });
    match staged {
        Ok(tree) => capture.keep().map(|()| tree),
        Err(e) => {
            if let Err(left) = space::finish(store, id, capture.into_left()?) {
                log::warn!("task {id}: capture {scratch} left files: {left:#}");
            }
            Err(e)
        }
    }
}

pub(super) fn discard(store: &mut Store, id: &str) -> Result<()> {
    let dir = task_dir(store.dir(), id);
    finish_left(store, id, &dir, true)?;
    if dir.try_exists()? {
        return Ok(());
    }
    let tx = store.write()?;
    space::forget(&tx, id)?;
    tx.commit()?;
    Ok(())
}

/// Checked in the transaction that starts the round, since the round can finish, or another start
/// and finish, while a snapshot is taken.
pub(super) fn due(conn: &Connection, id: &str, taken: Option<&Taken>) -> Result<Option<String>> {
    let (worktree, finished_at): (Option<String>, Option<i64>) = conn.query_row(
        &format!(
            "SELECT t.worktree, r.finished_at
             FROM task t JOIN task_round r ON r.task_id = t.id AND r.round = {current}
             WHERE t.id = ?1",
            current = *CURRENT_ROUND
        ),
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let stale = taken.is_none_or(|taken| finished_at.is_some_and(|at| taken.created_at < at));
    Ok(worktree.filter(|_| stale))
}

pub(super) fn after(store: &mut Store, id: &str) -> Result<()> {
    let worktree: Option<String> = store
        .conn()
        .query_row(
            &format!(
                "SELECT t.worktree
                 FROM task t JOIN task_round r ON r.task_id = t.id AND r.round = {current}
                 WHERE t.id = ?1 AND r.finished_at IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM snapshot s WHERE s.task_id = r.task_id
                                   AND s.round = r.round AND s.side = 'after')",
                current = *CURRENT_ROUND
            ),
            params![id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    if let Some(worktree) = worktree {
        let taken = take(store, id, &worktree);
        let tx = store.write()?;
        record_after(&tx, id, &taken)?;
        tx.commit()?;
    }
    Ok(())
}

pub(super) fn record_after(tx: &Transaction, id: &str, taken: &Taken) -> Result<()> {
    tx.execute(
        &format!(
            "INSERT INTO snapshot (task_id, round, side, tree, note, created_at)
             SELECT r.task_id, r.round, 'after', ?2, ?3, ?4
             FROM task t JOIN task_round r ON r.task_id = t.id AND r.round = {current}
             WHERE t.id = ?1 AND r.finished_at <= ?4
             ON CONFLICT DO NOTHING",
            current = *CURRENT_ROUND
        ),
        params![id, taken.tree, taken.note, taken.created_at],
    )?;
    Ok(())
}

pub(super) fn record_before(tx: &Transaction, id: &str, round: i64, taken: &Taken) -> Result<()> {
    tx.execute(
        "INSERT INTO snapshot (task_id, round, side, tree, note, created_at)
         VALUES (?1, ?2, 'before', ?3, ?4, ?5)",
        params![id, round, taken.tree, taken.note, taken.created_at],
    )?;
    Ok(())
}

#[derive(Default, Serialize)]
pub(super) struct Snapshots {
    worktree: Option<String>,
    note: Option<String>,
    object_directory: Option<PathBuf>,
    before: Option<Snapshot>,
    after: Option<Snapshot>,
}

#[derive(Serialize)]
struct Snapshot {
    tree: Option<String>,
    note: Option<String>,
    created_at: i64,
}

pub(super) fn shown(
    conn: &Connection,
    state_dir: &Path,
    id: &str,
    round: i64,
) -> Result<Snapshots> {
    let side = |row: &rusqlite::Row, at: usize| -> rusqlite::Result<Option<Snapshot>> {
        row.get::<_, Option<i64>>(at + 2)?
            .map(|created_at| {
                Ok(Snapshot {
                    tree: row.get(at)?,
                    note: row.get(at + 1)?,
                    created_at,
                })
            })
            .transpose()
    };
    Ok(conn.query_row(
        "SELECT t.worktree, t.worktree_note, b.tree, b.note, b.created_at, a.tree, a.note,
           a.created_at
         FROM task t
         LEFT JOIN snapshot b ON b.task_id = t.id AND b.round = ?2 AND b.side = 'before'
         LEFT JOIN snapshot a ON a.task_id = t.id AND a.round = ?2 AND a.side = 'after'
         WHERE t.id = ?1",
        params![id, round],
        |row| {
            let worktree: Option<String> = row.get(0)?;
            Ok(Snapshots {
                object_directory: worktree
                    .as_ref()
                    .map(|_| git::object_directory(&task_dir(state_dir, id))),
                worktree,
                note: row.get(1)?,
                before: side(row, 2)?,
                after: side(row, 5)?,
            })
        },
    )?)
}

pub fn sweep(store: &mut Store) -> Result<()> {
    let dir = store.dir().join("snapshots");
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            let dirs = entries
                .map(|entry| Ok(entry?.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            delete_old(store, dirs)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("failed to read `{}`", dir.display())),
    }
    let counted: Vec<String> = store
        .conn()
        .prepare("SELECT DISTINCT task_id FROM snapshot_space")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let gone: Vec<String> = counted
        .into_iter()
        .filter(|id| matches!(dir.join(id).try_exists(), Ok(false)))
        .collect();
    if !gone.is_empty() {
        let tx = store.write()?;
        for id in &gone {
            space::forget(&tx, id)?;
        }
        tx.commit()?;
    }
    Ok(())
}

fn delete_old(store: &mut Store, dirs: Vec<PathBuf>) -> Result<()> {
    let cutoff = SystemTime::now() - RETENTION;
    for path in dirs {
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // Read before finishing what captures left, which writes to the directory.
        let written = std::fs::metadata(&path).and_then(|meta| meta.modified());
        // A registration still running may have a directory and no task row yet, which only the
        // directory's age tells apart from one a killed registration left.
        let old = written.is_ok_and(|at| at < cutoff)
            && !store.conn().query_row(
                "SELECT EXISTS (SELECT 1 FROM task WHERE id = ?1)",
                params![id],
                |row| row.get(0),
            )?;
        if let Err(e) = finish_left(store, id, &path, old) {
            log::warn!("failed to clean up `{}`: {e:#}", path.display());
        }
    }
    Ok(())
}

fn finish_left(store: &mut Store, id: &str, path: &Path, delete: bool) -> Result<()> {
    for left in git::left(path)? {
        let capture = left.id().to_owned();
        if let Err(e) = space::finish(store, id, left) {
            log::warn!("failed to finish what capture {capture} of task {id} left: {e:#}");
        }
    }
    // `delete` is set only for a task gone, in whose directory no capture starts.
    if delete && !git::captured(path)? {
        match std::fs::remove_dir_all(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("failed to delete `{}`", path.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
