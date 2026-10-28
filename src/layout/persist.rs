//! The `session` table: one saved layout per session, keyed by lineage.

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Value;

use super::{SessionDoc, from_view};
use crate::store::{bind_live, ensure_lineage, lineage_of, move_block, now_ms, unbind_provisional};
use crate::task;

/// Saves a session's layout as the server shows it.
pub fn save_view(
    tx: &Transaction,
    instance: &str,
    live: &str,
    position: usize,
    view: &Value,
) -> Result<()> {
    let lineage = ensure_lineage(tx, instance, live)?;
    let doc = from_view(view, &mut |id| ensure_lineage(tx, instance, id))?;
    tx.execute(
        "INSERT INTO session (lineage, instance_id, position, doc, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (lineage) DO UPDATE SET instance_id = excluded.instance_id,
           position = excluded.position, doc = excluded.doc, updated_at = excluded.updated_at",
        params![lineage, instance, i64::try_from(position)?, serde_json::to_string(&doc)?, now_ms()],
    )?;
    Ok(())
}

pub fn forget_session(tx: &Transaction, instance: &str, live: &str) -> Result<()> {
    if let Some(lineage) = lineage_of(tx, instance, live)? {
        tx.execute("DELETE FROM session WHERE lineage = ?1", params![lineage])?;
    }
    Ok(())
}

/// Drops the saved sessions of this instance that the server no longer has.
pub fn forget_sessions_except(tx: &Transaction, instance: &str, kept: &[String]) -> Result<()> {
    let saved: Vec<String> = tx
        .prepare("SELECT lineage FROM session WHERE instance_id = ?1")?
        .query_map(params![instance], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for lineage in saved.iter().filter(|l| !kept.contains(l)) {
        tx.execute("DELETE FROM session WHERE lineage = ?1", params![lineage])?;
    }
    Ok(())
}

pub fn forget_window(tx: &Transaction, instance: &str, session: &str, window: &str) -> Result<()> {
    let (Some(session), Some(window)) = (
        lineage_of(tx, instance, session)?,
        lineage_of(tx, instance, window)?,
    ) else {
        return Ok(());
    };
    let doc: Option<String> = tx
        .query_row(
            "SELECT doc FROM session WHERE lineage = ?1",
            params![session],
            |row| row.get(0),
        )
        .optional()?;
    match doc {
        Some(doc) => edit(tx, &session, &doc, |doc| doc.remove_window(&window)),
        None => Ok(()),
    }
}

/// Drops a closed block from every saved layout.
pub fn forget_block(tx: &Transaction, lineage: &str) -> Result<()> {
    let sessions: Vec<(String, String)> = tx
        .prepare("SELECT lineage, doc FROM session")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (session, doc) in sessions {
        edit(tx, &session, &doc, |doc| doc.remove_block(lineage))?;
    }
    Ok(())
}

/// Binds a restored session, window, or pane to its lineage, the one path every create reply,
/// layer reply, adoption, and gate claim binds through. When a reply was lost, the daemon may have
/// seen the resource first and recorded it under its own ID; what it recorded there moves to the
/// restored lineage: a pane's record, a session's saved layout, and every saved layout's
/// references to the window or pane, through which floating layers and display settings are
/// found. Any other binding of the live ID is refused.
pub fn bind_restored(tx: &Transaction, instance: &str, live_id: &str, lineage: &str) -> Result<()> {
    if let Some(provisional) = unbind_provisional(tx, instance, live_id)? {
        // The merge gives the pane this instance's statuses, and a dialog its worker waited on
        // went with the earlier one. Ended first, so that a task registered on the provisional
        // pane keeps its own episode.
        task::end_blocked_episodes(tx, lineage)?;
        move_block(tx, &provisional, lineage)?;
        move_session(tx, &provisional, lineage)?;
        rename(tx, &provisional, lineage)?;
    }
    bind_live(tx, instance, live_id, lineage)
}

/// Renames a window or block in every saved layout.
fn rename(tx: &Transaction, from: &str, to: &str) -> Result<()> {
    let sessions: Vec<(String, String)> = tx
        .prepare("SELECT lineage, doc FROM session")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (session, doc) in sessions {
        edit(tx, &session, &doc, |doc| doc.rename(from, to))?;
    }
    Ok(())
}

/// Moves the layout saved under `from` to `to`, in place of the one `to` kept from an earlier
/// instance. Without a layout under `from`, the one under `to` stays.
fn move_session(tx: &Transaction, from: &str, to: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM session WHERE lineage = ?2 AND EXISTS (SELECT 1 FROM session WHERE lineage = ?1)",
        params![from, to],
    )?;
    tx.execute(
        "UPDATE session SET lineage = ?2 WHERE lineage = ?1",
        params![from, to],
    )?;
    Ok(())
}

/// Applies `change` to a session's saved layout and writes it back when it changed.
fn edit(
    tx: &Transaction,
    session: &str,
    doc: &str,
    change: impl FnOnce(&mut SessionDoc) -> bool,
) -> Result<()> {
    let mut doc: SessionDoc = serde_json::from_str(doc)
        .with_context(|| format!("failed to parse the saved layout of session {session}"))?;
    if change(&mut doc) {
        tx.execute(
            "UPDATE session SET doc = ?1, updated_at = ?2 WHERE lineage = ?3",
            params![serde_json::to_string(&doc)?, now_ms(), session],
        )?;
    }
    Ok(())
}
