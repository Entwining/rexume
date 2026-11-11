//! The space the snapshots' objects take in the state directory, as `snapshot_space` records it:
//! a capture reserves its estimate before it writes an object there and settles on what it wrote
//! before it keeps it. What a failed or killed capture left is finished here, by its row, once its
//! lock is held.

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, Transaction, params};

use super::task_dir;
use crate::git;
use crate::store::Store;

/// The most the snapshots of every task may take in one state directory.
pub const SPACE_LIMIT: i64 = 1 << 30;

/// Finishes what the killed captures of task `id` left, and those of any task whose space is still
/// reserved, so that space no capture uses counts no more; one whose processes still run is left
/// as it is.
pub(super) fn recover(store: &mut Store, id: &str) -> Result<()> {
    let reserved: Vec<(String, String)> = store
        .conn()
        .prepare("SELECT task_id, id FROM snapshot_space WHERE state = 'reserved'")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut left = Vec::new();
    for (task, capture) in reserved {
        if let Some(found) = git::claim(&task_dir(store.dir(), &task), &capture)? {
            left.push((task, found));
        }
    }
    // The captures claimed above are held, so they are not found again.
    for found in git::left(&task_dir(store.dir(), id))? {
        left.push((id.to_owned(), found));
    }
    for (task, found) in left {
        let capture = found.id().to_owned();
        if let Err(e) = finish(store, &task, found) {
            log::warn!("failed to finish what capture {capture} of task {task} left: {e:#}");
        }
    }
    Ok(())
}

/// Finishes capture `left` of task `id` by its row: moves the objects of one that kept them to the
/// task's, and deletes the files and then the row of any other. A reservation whose files are not
/// all deleted stays, counting what they take, for whoever next takes the lock.
pub(super) fn finish(store: &mut Store, id: &str, mut left: git::Left) -> Result<()> {
    let state: Option<String> = store
        .conn()
        .query_row(
            "SELECT state FROM snapshot_space WHERE task_id = ?1 AND id = ?2",
            params![id, left.id()],
            |row| row.get(0),
        )
        .optional()?;
    if state.as_deref() == Some("kept") {
        left.keep()?;
    } else if let Err(e) = left.delete() {
        if state.is_some() {
            hold(store, id, left.id(), left.size().ok())?;
        }
        return Err(e);
    } else if state.is_some() {
        release(store, id, left.id())?;
    }
    left.forget()
}

/// Reserves `estimate` bytes for capture `capture` of task `id`, and refuses when the snapshots
/// would take more than `SPACE_LIMIT`.
pub(super) fn reserve(store: &mut Store, id: &str, capture: &str, estimate: u64) -> Result<()> {
    let estimate = i64::try_from(estimate)?;
    let tx = store.write()?;
    refuse_over(&tx, None, estimate)?;
    tx.execute(
        "INSERT INTO snapshot_space (task_id, id, state, bytes) VALUES (?1, ?2, 'reserved', ?3)",
        params![id, capture, estimate],
    )?;
    tx.commit()?;
    Ok(())
}

/// Records that capture `capture` of task `id` keeps `bytes` bytes of objects, before it moves
/// them, and refuses when the snapshots would take more than `SPACE_LIMIT`, as when a file grew
/// after the capture reserved its space. A refused capture releases its reservation once it has
/// deleted what it wrote.
pub(super) fn settle(store: &mut Store, id: &str, capture: &str, bytes: u64) -> Result<()> {
    let bytes = i64::try_from(bytes)?;
    let tx = store.write()?;
    refuse_over(&tx, Some((id, capture)), bytes)?;
    if bytes == 0 {
        tx.execute(
            "DELETE FROM snapshot_space WHERE task_id = ?1 AND id = ?2",
            params![id, capture],
        )?;
    } else if tx.execute(
        "UPDATE snapshot_space SET state = 'kept', bytes = ?3 WHERE task_id = ?1 AND id = ?2",
        params![id, capture, bytes],
    )? == 0
    {
        bail!("the space reserved for the snapshot was released before it settled");
    }
    tx.commit()?;
    Ok(())
}

/// Records that capture `capture` of task `id` left `bytes` bytes it failed to delete, or, when they
/// could not be measured, all the space the snapshots may take, so that its reservation counts no
/// less than what it left. The estimate it reserved no longer applies, since its processes ended.
fn hold(store: &mut Store, id: &str, capture: &str, bytes: Option<u64>) -> Result<()> {
    let bytes = bytes.map_or(Ok(SPACE_LIMIT), i64::try_from)?;
    let tx = store.write()?;
    tx.execute(
        "UPDATE snapshot_space SET bytes = ?3 WHERE task_id = ?1 AND id = ?2 AND state = 'reserved'",
        params![id, capture, bytes],
    )?;
    tx.commit()?;
    Ok(())
}

/// Releases what capture `capture` of task `id` reserved, once it has deleted what it wrote. A
/// capture that settled keeps its row, since its objects are the task's.
fn release(store: &mut Store, id: &str, capture: &str) -> Result<()> {
    let tx = store.write()?;
    tx.execute(
        "DELETE FROM snapshot_space WHERE task_id = ?1 AND id = ?2 AND state = 'reserved'",
        params![id, capture],
    )?;
    tx.commit()?;
    Ok(())
}

/// Refuses `bytes` more when they and what every row but `except` records exceed `SPACE_LIMIT`.
fn refuse_over(tx: &Transaction, except: Option<(&str, &str)>, bytes: i64) -> Result<()> {
    let (task, capture) = except.unzip();
    let taken: i64 = tx.query_row(
        "SELECT COALESCE(SUM(bytes), 0) FROM snapshot_space
         WHERE NOT (task_id IS ?1 AND id IS ?2)",
        params![task, capture],
        |row| row.get(0),
    )?;
    if taken + bytes > SPACE_LIMIT {
        bail!(
            "snapshots take {} MiB of the {} MiB they may in the state directory, too little for \
             this one's {} MiB",
            taken >> 20,
            SPACE_LIMIT >> 20,
            bytes >> 20
        );
    }
    Ok(())
}

/// Forgets every row of task `id`, whose snapshot directory is deleted.
pub(super) fn forget(tx: &Transaction, id: &str) -> Result<()> {
    tx.execute("DELETE FROM snapshot_space WHERE task_id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests;
