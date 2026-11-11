use anyhow::Result;
use serde_json::{Value, json};

use super::*;
use crate::store::archive;

/// A database with an epoch and two tasks in use, `old` finished long ago and `kept` with an
/// unfinished round, after a sweep that archived `old`, and when it did.
fn store() -> Result<(tempfile::TempDir, Store, i64)> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    store.conn().execute_batch(
        "INSERT INTO epoch (id, instance_id, host_id, started_at) VALUES (4, 'i2', 'h', 1);
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 1);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at)
           VALUES ('old', 'x', 'claude', 'owner', 'b1', 0, 1),
             ('kept', 'y', 'claude', 'owner', 'b2', 1, 1);
         INSERT INTO task_round (task_id, round, state, finished_at, acked_at, created_at)
           VALUES ('old', 1, 'cancelled', 1, 1, 1), ('kept', 1, 'running', NULL, NULL, 1);",
    )?;
    let archived_at = now_ms();
    archive::sweep(&mut store, "i2", archived_at)?;
    Ok((dir, store, archived_at))
}

/// Finishes `kept`'s round long ago, so that the next sweep archives it.
fn finish_kept(store: &Store) -> Result<()> {
    store.conn().execute_batch(
        "UPDATE task_round SET state = 'cancelled', finished_at = 1, acked_at = 1
         WHERE task_id = 'kept'",
    )?;
    Ok(())
}

fn lines(output: &[u8]) -> Result<Vec<Value>> {
    std::str::from_utf8(output)?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn entity<'a>(lines: &'a [Value], id: &str) -> Vec<&'a Value> {
    lines
        .iter()
        .filter(|line| line["record"] == "entity" && line["id"] == id)
        .collect()
}

fn archived_data(store: &Store, id: &str) -> Result<Value> {
    let data: String = store.conn().query_row(
        "SELECT data FROM archive WHERE entity_type = 'task' AND entity_id = ?1",
        [id],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&data)?)
}

/// The export starts with its header and ends with the count of entities between; an archived
/// entity carries its archived document, and one in use the document the sweep then archives.
#[test]
fn export_prints_entities_in_use_and_archived_between_header_and_end() -> Result<()> {
    let (_dir, mut store, archived_at) = store()?;
    finish_kept(&store)?;
    let mut output = Vec::new();
    run(store.dir(), &mut output)?;
    let lines = lines(&output)?;
    let source_id: String = store
        .conn()
        .query_row("SELECT source_id FROM store", [], |row| row.get(0))?;
    let header = &lines[0];
    assert_eq!(
        (
            &header["record"],
            &header["version"],
            &header["schema_version"]
        ),
        (&json!("header"), &json!(1), &json!(SCHEMA_VERSION))
    );
    assert_eq!(header["source_id"], source_id);
    assert!(header["exported_at"].is_i64());
    assert_eq!(
        lines.last(),
        Some(&json!({ "record": "end", "entities": 3 }))
    );
    assert_eq!(lines.len(), 5);
    let old = entity(&lines, "old");
    assert_eq!(
        (old.len(), &old[0]["archived_at"]),
        (1, &json!(archived_at))
    );
    assert_eq!(old[0]["data"], archived_data(&store, "old")?);
    let epoch = entity(&lines, "4");
    assert_eq!(epoch[0]["data"]["epoch"]["host_id"], "h");
    let kept = entity(&lines, "kept");
    assert_eq!(kept[0]["archived_at"], Value::Null);
    archive::sweep(&mut store, "i2", now_ms())?;
    assert_eq!(archived_data(&store, "kept")?, kept[0]["data"]);
    Ok(())
}

/// An entity archived between the snapshot and its printing appears once, as it was in use.
#[test]
fn an_entity_archived_after_the_snapshot_appears_once() -> Result<()> {
    let (_dir, mut store, _) = store()?;
    let conn = Store::open_read_only(store.dir())?;
    let snapshot = snapshot(&conn)?;
    finish_kept(&store)?;
    archive::sweep(&mut store, "i2", now_ms())?;
    let mut output = Vec::new();
    print(&conn, &snapshot, &mut output)?;
    let lines = lines(&output)?;
    let kept = entity(&lines, "kept");
    assert_eq!((kept.len(), &kept[0]["archived_at"]), (1, &Value::Null));
    assert_eq!(lines.last().unwrap()["entities"], 3);
    Ok(())
}

/// An archived document gone by the time it is printed fails the export before its last line.
#[test]
fn a_missing_archived_document_leaves_the_export_without_its_end() -> Result<()> {
    let (_dir, store, _) = store()?;
    let conn = Store::open_read_only(store.dir())?;
    let snapshot = snapshot(&conn)?;
    store.conn().execute("DELETE FROM archive", [])?;
    let mut output = Vec::new();
    assert!(print(&conn, &snapshot, &mut output).is_err());
    assert!(lines(&output)?.iter().all(|line| line["record"] != "end"));
    Ok(())
}
