//! The notices `task list` and `status` show as undelivered.

use std::sync::LazyLock;

use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::Value;

use super::WANTED;
use crate::runtime::Runtime;

/// Unconfirmed notices their rounds still need, with each round's state. A `pending` notice with
/// a note has failed to send; other pending notices are still waiting for their first attempt.
static UNDELIVERED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT n.id, n.kind, n.task_id, n.round, r.state, n.state, n.note
         FROM notice n JOIN task t ON t.id = n.task_id
         JOIN task_round r ON r.task_id = n.task_id AND r.round = n.round
         WHERE {}
           AND (n.state IN ('unknown', 'sending', 'sent', 'accepted')
                OR (n.state = 'pending' AND n.note IS NOT NULL))",
        *WANTED
    )
});

/// The NOTICE objects of `docs/cli.md` that `UNDELIVERED` selects: one owner's, or all.
pub fn undelivered(conn: &Connection, owner: Option<(Runtime, &str)>) -> Result<Vec<Value>> {
    let notice = |row: &rusqlite::Row| -> rusqlite::Result<Value> {
        Ok(serde_json::json!({
            "id": row.get::<_, String>(0)?,
            "kind": row.get::<_, String>(1)?,
            "task_id": row.get::<_, String>(2)?,
            "round": row.get::<_, i64>(3)?,
            "round_state": row.get::<_, String>(4)?,
            "state": row.get::<_, String>(5)?,
            "note": row.get::<_, Option<String>>(6)?,
        }))
    };
    Ok(match owner {
        Some((runtime, native_id)) => conn
            .prepare(&format!(
                "{} AND t.owner_runtime = ?1 AND t.owner_native_id = ?2",
                *UNDELIVERED
            ))?
            .query_map(params![runtime.as_str(), native_id], notice)?
            .collect::<rusqlite::Result<_>>()?,
        None => conn
            .prepare(&UNDELIVERED)?
            .query_map([], notice)?
            .collect::<rusqlite::Result<_>>()?,
    })
}
