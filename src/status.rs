//! `rexume status`: what a person checks after a restart, read without changing anything.

use std::path::Path;

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::store::Store;
use crate::task;

pub fn run(state_dir: &Path) -> Result<Value> {
    let conn = Store::open_read_only(state_dir)?;
    let restores: Vec<Value> = conn
        .prepare(
            "SELECT id, doc ->> '$.label', state, note, created_at FROM restore_op
             WHERE state != 'completed' ORDER BY created_at",
        )?
        .query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "session": row.get::<_, Option<String>>(1)?,
                "state": row.get::<_, String>(2)?,
                "note": row.get::<_, Option<String>>(3)?,
                "created_at": row.get::<_, i64>(4)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let notices = task::undelivered(&conn, None)?;
    let epoch_error: Option<Value> = conn
        .query_row(
            "SELECT error, started_at, ended_at FROM epoch
             WHERE id = (SELECT MAX(id) FROM epoch) AND error IS NOT NULL",
            [],
            |row| {
                Ok(json!({
                    "error": row.get::<_, String>(0)?,
                    "started_at": row.get::<_, i64>(1)?,
                    "ended_at": row.get::<_, i64>(2)?,
                }))
            },
        )
        .optional()?;
    Ok(json!({
        "version": 1,
        "restores": restores,
        "undelivered_notices": notices,
        "epoch_error": epoch_error,
    }))
}

#[cfg(test)]
mod tests;
