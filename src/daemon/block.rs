//! The daemon's writes to the `block` table: each pane's last reported state on this instance.

use anyhow::Result;
use rusqlite::{Transaction, params};
use serde_json::Value;

use crate::rex::{str_at, time_at};
use crate::store::ensure_lineage;

/// Makes sure a block has a row on this instance and returns its lineage. A lineage on a new
/// instance is a new pane: its processes, their statuses and exit, and the generation that orders
/// their reports, start again.
pub(super) fn touch_block(tx: &Transaction, instance: &str, block: &str) -> Result<String> {
    let lineage = ensure_lineage(tx, instance, block)?;
    tx.execute(
        "INSERT INTO block (lineage, instance_id) VALUES (?1, ?2)
         ON CONFLICT (lineage) DO UPDATE SET fg_pid = NULL, fg_generation = NULL, last_exit = NULL,
           status = NULL, status_pid = NULL, status_name = NULL, status_at = NULL,
           instance_id = excluded.instance_id
         WHERE instance_id != excluded.instance_id",
        params![lineage, instance],
    )?;
    Ok(lineage)
}

/// Records the foreground process, ignoring a report older than one already recorded.
pub(super) fn set_foreground(
    tx: &Transaction,
    lineage: &str,
    foreground: Option<&Value>,
    generation: Option<i64>,
) -> Result<()> {
    if let Some(generation) = generation {
        let stored: Option<i64> = tx.query_row(
            "SELECT fg_generation FROM block WHERE lineage = ?1",
            params![lineage],
            |row| row.get(0),
        )?;
        if stored.is_some_and(|s| s >= generation) {
            return Ok(());
        }
    }
    tx.execute(
        "UPDATE block SET fg_pid = ?1, cwd = COALESCE(?2, cwd),
           fg_generation = COALESCE(?3, fg_generation) WHERE lineage = ?4",
        params![
            foreground
                .and_then(|f| f.get("pid"))
                .and_then(Value::as_i64),
            foreground.and_then(|f| str_at(f, "/cwd")),
            generation,
            lineage
        ],
    )?;
    Ok(())
}

pub(super) fn set_size(tx: &Transaction, lineage: &str, size: &Value) -> Result<()> {
    let (Some(columns), Some(rows)) = (
        size.get("columns").and_then(Value::as_i64),
        size.get("rows").and_then(Value::as_i64),
    ) else {
        return Ok(());
    };
    tx.execute(
        "UPDATE block SET columns = ?1, rows = ?2 WHERE lineage = ?3",
        params![columns, rows, lineage],
    )?;
    Ok(())
}

pub(super) fn set_status(tx: &Transaction, lineage: &str, record: Option<&Value>) -> Result<()> {
    tx.execute(
        "UPDATE block SET status = ?1, status_pid = ?2, status_name = ?3, status_at = ?4
         WHERE lineage = ?5",
        params![
            record.and_then(|r| str_at(r, "/state")),
            record
                .and_then(|r| r.pointer("/owner/pid"))
                .and_then(Value::as_i64),
            record.and_then(|r| str_at(r, "/owner/name")),
            record.and_then(|r| time_at(r, "/updated_at")),
            lineage
        ],
    )?;
    Ok(())
}
