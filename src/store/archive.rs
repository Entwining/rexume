//! Keeps what the retention sweep removes: each finished task, restore, and epoch past
//! `RETENTION` becomes one `archive` row, inserted in the transaction that deletes its rows
//! (`docs/design.md`, "What is saved"). `export` describes an entity still in use with the same
//! document.

#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Transaction, params};
use serde_json::{Map, Value};

use super::{RETENTION, SCHEMA_VERSION, Store};

/// The largest document the sweep archives, in bytes of JSON; a larger entity stays in use. Larger
/// archives more entities, but the daemon holds the whole document in memory and writes it in one
/// transaction.
const DOCUMENT_LIMIT: usize = 64 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EntityType {
    Restore,
    Task,
    Epoch,
}

impl EntityType {
    pub const ALL: [Self; 3] = [Self::Restore, Self::Task, Self::Epoch];

    /// The entity's table, whose name is also its `entity_type`.
    pub fn table(self) -> &'static str {
        match self {
            Self::Restore => "restore",
            Self::Task => "task",
            Self::Epoch => "epoch",
        }
    }

    /// The rows of the entity's table that the sweep archives, with the current instance as `?1`
    /// and the retention cutoff as `?2`. A task's notices do not extend its retention
    /// (`docs/design.md`, "What is saved").
    fn expired(self) -> &'static str {
        match self {
            Self::Restore => "instance_id != ?1 AND created_at < ?2",
            // Every round finished, and was acknowledged before the cutoff, or finished before it
            // if never acknowledged.
            Self::Task => {
                "(SELECT COUNT(finished_at) = COUNT(*) AND MAX(COALESCE(acked_at, finished_at)) < ?2
                  FROM task_round WHERE task_id = task.id)"
            }
            // A pane still recorded on the epoch's instance is restored by how the epoch ended.
            Self::Epoch => "ended_at < ?2 AND instance_id NOT IN (SELECT instance_id FROM block)",
        }
    }
}

/// The entity as `archive.data` holds it: its row and, for a task, the rows of its rounds,
/// requests, and notices, keyed by table name.
pub fn document(conn: &Connection, entity_type: EntityType, id: &str) -> Result<Value> {
    let table = entity_type.table();
    let own = rows(conn, &format!("SELECT * FROM {table} WHERE id = ?1"), id)?
        .pop()
        .with_context(|| format!("no {table} {id}"))?;
    let mut document = Map::from_iter([(table.to_owned(), own)]);
    if entity_type == EntityType::Task {
        for (child, key) in [("task_round", "round"), ("request", "id"), ("notice", "id")] {
            let sql = format!("SELECT * FROM {child} WHERE task_id = ?1 ORDER BY {key}");
            document.insert(child.to_owned(), Value::Array(rows(conn, &sql, id)?));
        }
    }
    Ok(Value::Object(document))
}

/// Each row `sql` returns for `id`, as an object of its columns by name.
fn rows(conn: &Connection, sql: &str, id: &str) -> Result<Vec<Value>> {
    let mut statement = conn.prepare_cached(sql)?;
    let names: Vec<String> = statement.column_names().map(str::to_owned).collect();
    let mut rows = statement.query(params![id])?;
    let mut found = Vec::new();
    while let Some(row) = rows.next()? {
        let mut object = Map::new();
        for (i, name) in names.iter().enumerate() {
            let value = match row.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => n.into(),
                ValueRef::Text(text) => std::str::from_utf8(text)?.into(),
                ValueRef::Real(_) | ValueRef::Blob(_) => {
                    bail!("`{name}` holds neither an integer nor text")
                }
            };
            object.insert(name.clone(), value);
        }
        found.push(Value::Object(object));
    }
    Ok(found)
}

/// Archives every expired task, restore, and epoch, each in a transaction of its own, so that one
/// task's results do not hold the write lock, or grow the write-ahead log, through every other's;
/// then deletes the conversations nothing refers to any more. The daemon runs it when it starts
/// observing an instance, after its first restores, so what the sweep finds is what expired since
/// the previous epoch began. An entity whose document exceeds `DOCUMENT_LIMIT` stays in use, with
/// a warning at each sweep.
pub fn sweep(store: &mut Store, instance: &str, now: i64) -> Result<()> {
    let cutoff = now - i64::try_from(RETENTION.as_millis())?;
    for entity_type in EntityType::ALL {
        let ids: Vec<String> = store
            .conn()
            .prepare(&format!(
                "SELECT CAST(id AS TEXT) FROM {} WHERE {}",
                entity_type.table(),
                entity_type.expired()
            ))?
            .query_map(params![instance, cutoff], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in ids {
            let tx = store.write()?;
            let too_large = archive(&tx, entity_type, &id, instance, cutoff, now)?;
            tx.commit()?;
            if let Some(size) = too_large {
                log::warn!(
                    "{} {id} stays in use: its document is {size} bytes, over the \
                     {DOCUMENT_LIMIT} bytes the sweep archives",
                    entity_type.table()
                );
            }
        }
    }
    let tx = store.write()?;
    tx.execute(
        "DELETE FROM conversation
          WHERE reported_at < ?1
            AND NOT EXISTS (SELECT 1 FROM occupant o
                            WHERE o.runtime = conversation.runtime
                              AND o.native_id = conversation.native_id)
            AND NOT EXISTS (SELECT 1 FROM task t
                            WHERE t.owner_runtime = conversation.runtime
                              AND t.owner_native_id = conversation.native_id)
            AND NOT EXISTS (SELECT 1 FROM restore r
                            WHERE r.runtime = conversation.runtime
                              AND r.native_id = conversation.native_id)
            AND NOT EXISTS (SELECT 1 FROM task t
                            WHERE t.worker_runtime = conversation.runtime
                              AND t.worker_native_id = conversation.native_id)",
        params![cutoff],
    )?;
    tx.commit()?;
    Ok(())
}

/// Archives one entity the sweep listed, if it is still expired: a task may have taken a new
/// round since. Deleting its row deletes a task's rounds, requests, and notices with it. Returns
/// the size of a document too large to archive, whose entity stays as it is.
fn archive(
    tx: &Transaction,
    entity_type: EntityType,
    id: &str,
    instance: &str,
    cutoff: i64,
    now: i64,
) -> Result<Option<usize>> {
    let table = entity_type.table();
    let expired: bool = tx.query_row(
        &format!(
            "SELECT EXISTS (SELECT 1 FROM {table} WHERE id = ?3 AND {})",
            entity_type.expired()
        ),
        params![instance, cutoff, id],
        |row| row.get(0),
    )?;
    if !expired {
        return Ok(None);
    }
    let data = document(tx, entity_type, id)?.to_string();
    if data.len() > DOCUMENT_LIMIT {
        return Ok(Some(data.len()));
    }
    tx.execute(
        "INSERT INTO archive (entity_type, entity_id, version, archived_at, data)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![table, id, SCHEMA_VERSION, now, data],
    )?;
    tx.execute(&format!("DELETE FROM {table} WHERE id = ?1"), params![id])?;
    Ok(None)
}
