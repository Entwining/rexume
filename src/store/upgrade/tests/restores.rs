use anyhow::Result;
use rusqlite::Connection;

use super::{definitions, fresh};
use crate::store::Store;

/// Version 14 added a state to `restore`'s `CHECK`, which an upgrade rebuilds from each version that
/// has the table, by way of each of the upgrade's paths.
#[test]
fn open_upgrades_restores_of_versions_7_to_13_as_they_were() -> Result<()> {
    for fixture in [
        include_str!("../../../../tests/fixtures/schema-v7.sql"),
        include_str!("../../../../tests/fixtures/schema-v11.sql"),
        include_str!("../../../../tests/fixtures/schema-v13.sql"),
    ] {
        let dir = tempfile::tempdir()?;
        let conn = Connection::open(dir.path().join("state.db"))?;
        conn.execute_batch(fixture)?;
        conn.execute_batch(
            "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
               VALUES ('claude', 'c', 1, 0), ('codex', 'x', 1, 0);
             INSERT INTO restore (id, instance_id, block_lineage, session_label, start, runtime,
               native_id, plan, state, note, created_at, typed_at, claimed_at, started, pid)
               VALUES ('r1', 'i1', 'b1', 'work', 'continue', 'claude', 'c', '{}', 'claimed', NULL,
                 1, 2, 3, 'agent', 7),
                 ('r2', 'i1', 'b2', NULL, 'agent', 'codex', 'x', '{}', 'skipped', 'the pane closed',
                 4, NULL, NULL, NULL, NULL);",
        )?;
        drop(conn);
        let store = Store::open(dir.path())?;
        assert_eq!(definitions(store.conn())?, fresh()?);
        let restores: String = store.conn().query_row(
            "SELECT json_group_array(json_array(id, instance_id, block_lineage, session_label,
               start, runtime, native_id, plan, state, note, created_at, typed_at, claimed_at,
               started, pid)) FROM (SELECT * FROM restore ORDER BY id)",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            restores,
            r#"[["r1","i1","b1","work","continue","claude","c","{}","claimed",null,1,2,3,"agent",7],["r2","i1","b2",null,"agent","codex","x","{}","skipped","the pane closed",4,null,null,null,null]]"#
        );
    }
    Ok(())
}
