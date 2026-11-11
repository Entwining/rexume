#[cfg(test)]
mod tests;

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::store::archive::{self, EntityType};
use crate::store::{SCHEMA_VERSION, Store, now_ms};

/// Changes only when a field of the lines changes meaning or goes away.
const VERSION: i64 = 1;

struct Snapshot {
    header: Value,
    live: Vec<Value>,
    archived: Vec<Key>,
}

struct Key {
    entity_type: String,
    id: String,
    version: i64,
    archived_at: i64,
}

pub fn run(dir: &Path, out: &mut impl Write) -> Result<()> {
    let conn = Store::open_read_only(dir)?;
    let snapshot = snapshot(&conn)?;
    print(&conn, &snapshot, out)
}

/// Nothing is printed inside the transaction: a slow reader of the output would hold it open, and
/// with it the write-ahead log, which no writer could then reset.
fn snapshot(conn: &Connection) -> Result<Snapshot> {
    let tx = conn.unchecked_transaction()?;
    let source_id: String = tx.query_row("SELECT source_id FROM store", [], |row| row.get(0))?;
    let mut live = Vec::new();
    for entity_type in EntityType::ALL {
        let table = entity_type.table();
        let ids: Vec<String> = tx
            .prepare(&format!("SELECT CAST(id AS TEXT) FROM {table}"))?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in ids {
            let data = archive::document(&tx, entity_type, &id)?;
            live.push(json!({ "record": "entity", "type": table, "id": id,
                "schema_version": SCHEMA_VERSION, "archived_at": null, "data": data }));
        }
    }
    let archived = tx
        .prepare("SELECT entity_type, entity_id, version, archived_at FROM archive")?
        .query_map([], |row| {
            Ok(Key {
                entity_type: row.get(0)?,
                id: row.get(1)?,
                version: row.get(2)?,
                archived_at: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let header = json!({ "record": "header", "version": VERSION, "source_id": source_id,
        "schema_version": SCHEMA_VERSION, "exported_at": now_ms() });
    Ok(Snapshot {
        header,
        live,
        archived,
    })
}

fn print(conn: &Connection, snapshot: &Snapshot, out: &mut impl Write) -> Result<()> {
    let mut line = |value: &Value| writeln!(out, "{value}").context("failed to write to stdout");
    line(&snapshot.header)?;
    for entity in &snapshot.live {
        line(entity)?;
    }
    let mut statement =
        conn.prepare("SELECT data FROM archive WHERE entity_type = ?1 AND entity_id = ?2")?;
    for key in &snapshot.archived {
        let data: String = statement
            .query_row(params![key.entity_type, key.id], |row| row.get(0))
            .optional()?
            .with_context(|| {
                format!(
                    "the archived {} {} is gone; export again",
                    key.entity_type, key.id
                )
            })?;
        let data: Value = serde_json::from_str(&data)?;
        line(
            &json!({ "record": "entity", "type": key.entity_type, "id": key.id,
            "schema_version": key.version, "archived_at": key.archived_at, "data": data }),
        )?;
    }
    line(&json!({ "record": "end", "entities": snapshot.live.len() + snapshot.archived.len() }))?;
    out.flush().context("failed to write to stdout")
}
