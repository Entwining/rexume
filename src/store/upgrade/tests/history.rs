use anyhow::Result;
use rusqlite::Connection;

use super::{definitions, fresh};
use crate::store::Store;

/// Version 10 adds `epoch.host_id`, unknown for an earlier epoch, and the database's `source_id`;
/// an epoch keeps its ID and a new one follows the highest, and a task keeps its rounds, requests,
/// and notices.
#[test]
fn open_upgrades_a_version_9_database_and_keeps_its_epochs_and_tasks() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = Connection::open(dir.path().join("state.db"))?;
    conn.execute_batch(include_str!("../../../../tests/fixtures/schema-v9.sql"))?;
    conn.execute_batch(
        "INSERT INTO epoch (id, instance_id, started_at, ended_at, end_reason)
           VALUES (3, 'i1', 1, 2, 'server_gone'), (7, 'i2', 3, 4, 'disconnected');
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at) VALUES ('t', 'x', 'claude', 'owner', 'b', 0, 0);
         INSERT INTO task_round (task_id, round, state, created_at)
           VALUES ('t', 1, 'queued', 0);
         INSERT INTO request (id, task_id, round, text) VALUES ('q', 't', 1, 'go on');
         INSERT INTO notice (id, task_id, round, kind, state, created_at)
           VALUES ('n', 't', 1, 'blocked', 'pending', 0);",
    )?;
    drop(conn);
    let store = Store::open(dir.path())?;
    assert_eq!(definitions(store.conn())?, fresh()?);
    let count =
        |sql: &str| -> Result<i64> { Ok(store.conn().query_row(sql, [], |row| row.get(0))?) };
    assert_eq!(
        count("SELECT COUNT(*) FROM epoch WHERE host_id IS NULL AND id IN (3, 7)")?,
        2
    );
    assert_eq!(count("SELECT COUNT(*) FROM store")?, 1);
    assert_eq!(
        count(
            "SELECT (SELECT COUNT(*) FROM task_round WHERE state = 'queued')
                  + (SELECT COUNT(*) FROM request) + (SELECT COUNT(*) FROM notice)"
        )?,
        3
    );
    store.conn().execute(
        "INSERT INTO epoch (instance_id, started_at) VALUES ('i3', 5)",
        [],
    )?;
    assert_eq!(store.conn().last_insert_rowid(), 8);
    Ok(())
}
