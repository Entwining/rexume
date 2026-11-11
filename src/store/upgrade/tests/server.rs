use anyhow::Result;
use rusqlite::Connection;

use super::{definitions, fresh};
use crate::store::Store;

/// Version 6 adds `epoch.error`. A version 5 database keeps its epochs, the newest of which
/// `status` reads.
#[test]
fn open_upgrades_a_version_5_database_and_keeps_its_epochs() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = Connection::open(dir.path().join("state.db"))?;
    conn.execute_batch(include_str!("../../../../tests/fixtures/schema-v5.sql"))?;
    conn.execute_batch(
        "INSERT INTO epoch (id, instance_id, started_at, ended_at, end_reason)
           VALUES (7, 'i1', 1, 2, 'server_gone');",
    )?;
    drop(conn);
    let store = Store::open(dir.path())?;
    assert_eq!(definitions(store.conn())?, fresh()?);
    let kept: (String, Option<String>) = store.conn().query_row(
        "SELECT end_reason, error FROM epoch WHERE id = 7",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(kept, ("server_gone".into(), None));
    Ok(())
}

/// Version 7 drops the saved layouts, the restores of recreated sessions, and the pane columns
/// only they read; a version 6 pane keeps the rest of its record.
#[test]
fn open_upgrades_a_version_6_database_and_keeps_its_panes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = Connection::open(dir.path().join("state.db"))?;
    conn.execute_batch(include_str!("../../../../tests/fixtures/schema-v6.sql"))?;
    conn.execute_batch(
        "INSERT INTO block (lineage, instance_id, cwd, columns, rows, fg_pid, status)
           VALUES ('b', 'i1', '/work', 80, 24, 10, 'working');
         INSERT INTO session (lineage, instance_id, position, doc, updated_at)
           VALUES ('s', 'i1', 0, '{}', 1);",
    )?;
    drop(conn);
    let store = Store::open(dir.path())?;
    assert_eq!(definitions(store.conn())?, fresh()?);
    let kept: (String, Option<i64>, Option<String>) = store.conn().query_row(
        "SELECT instance_id, fg_pid, status FROM block WHERE lineage = 'b'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(kept, ("i1".into(), Some(10), Some("working".into())));
    Ok(())
}
