use super::*;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

#[test]
fn open_creates_the_schema_once_and_reopens_without_writing() -> Result<()> {
    let (dir, store) = store();
    let before = store.data_version()?;
    let again = Store::open(dir.path())?;
    let version: i64 = again
        .conn()
        .pragma_query_value(None, "user_version", |row| row.get(0))?;
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(store.data_version()?, before);
    Ok(())
}

#[test]
fn open_refuses_a_newer_schema_and_a_foreign_file() -> Result<()> {
    let (dir, store) = store();
    store.conn().pragma_update(None, "user_version", 2)?;
    drop(store);
    let newer = Store::open(dir.path()).err().unwrap().to_string();
    assert!(newer.contains("newer rexume"), "{newer}");
    assert!(Store::open_read_only(dir.path()).is_err());

    let other = tempfile::tempdir()?;
    let conn = Connection::open(other.path().join("state.db"))?;
    conn.execute_batch("CREATE TABLE notes (body TEXT)")?;
    drop(conn);
    let foreign = Store::open(other.path()).err().unwrap().to_string();
    assert!(foreign.contains("not a state database"), "{foreign}");
    Ok(())
}

#[test]
fn open_read_only_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    assert!(Store::open_read_only(&state).is_err());
    assert!(!state.exists());
}

#[test]
fn bind_live_is_idempotent_and_refuses_a_second_lineage() -> Result<()> {
    let (_dir, mut store) = store();
    let tx = store.write()?;
    bind_live(&tx, "i2", "block:new", "block:old")?;
    bind_live(&tx, "i2", "block:new", "block:old")?;
    assert!(bind_live(&tx, "i2", "block:new", "block:other").is_err());
    assert!(bind_live(&tx, "i2", "block:another", "block:old").is_err());
    // The same live ID on another instance is a different resource.
    bind_live(&tx, "i3", "block:new", "block:third")?;
    Ok(())
}

#[test]
fn ensure_lineage_keeps_a_binding_made_by_a_gate() -> Result<()> {
    let (_dir, mut store) = store();
    let tx = store.write()?;
    bind_live(&tx, "i2", "block:new", "block:old")?;
    assert_eq!(ensure_lineage(&tx, "i2", "block:new")?, "block:old");
    assert_eq!(ensure_lineage(&tx, "i2", "block:other")?, "block:other");
    assert_eq!(ensure_lineage(&tx, "i2", "block:other")?, "block:other");
    Ok(())
}

#[test]
fn sweep_keeps_what_a_layout_or_an_open_task_still_names() -> Result<()> {
    let (_dir, mut store) = store();
    let old = now_ms() - i64::try_from(RETENTION.as_millis())? - 1;
    let tx = store.write()?;
    tx.execute_batch(&format!(
        "INSERT INTO epoch (instance_id, started_at, ended_at, end_reason)
           VALUES ('i1', 0, {old}, 'server_gone'), ('i0', 0, {old}, 'server_gone');
         INSERT INTO session (lineage, instance_id, position, doc, updated_at)
           VALUES ('s', 'i1', 0, '{{\"windows\":[{{\"tiled\":{{\"block\":{{\"lineage\":\"b-saved\"}}}}}}]}}', 0);
         INSERT INTO block (lineage, instance_id) VALUES ('b-saved', 'i1'), ('b-task', 'i1'),
           ('b-gone', 'i1'), ('b-now', 'i2'), ('b-layer', 'i1');
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i1', 'x', 'x'), ('i2', 'y', 'y');
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0), ('claude', 'unused', 1, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at) VALUES ('t', 'x', 'claude', 'owner', 'b-task', 0, 0);
         INSERT INTO restore_op (id, instance_id, session_lineage, state, doc, created_at)
           VALUES ('op-stuck', 'i1', 's', 'created', '{{}}', {old}),
             ('op-recent', 'i1', 's2', 'unknown', '{{}}', {now}),
             ('op-current', 'i2', 's', 'unknown', '{{}}', {old});
         -- A floating layer the current instance's restore has not created yet.
         INSERT INTO restore_slot (op_id, slot, instance_id, block_lineage, start, plan)
           VALUES ('op-current', 0, 'i2', 'b-layer', 'shell', '{{}}');",
        now = now_ms()
    ))?;
    sweep(&tx, "i2", now_ms())?;
    let names = |sql: &str| -> Vec<String> {
        let mut statement = tx.prepare(sql).unwrap();
        let rows = statement.query_map([], |row| row.get(0)).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    };
    assert_eq!(
        names("SELECT lineage FROM block ORDER BY lineage"),
        ["b-layer", "b-now", "b-saved", "b-task"]
    );
    assert_eq!(names("SELECT live_id FROM live"), ["y"]);
    assert_eq!(names("SELECT instance_id FROM epoch"), ["i1"]);
    assert_eq!(names("SELECT native_id FROM conversation"), ["owner"]);
    assert_eq!(
        names("SELECT id FROM restore_op ORDER BY id"),
        ["op-current", "op-recent"]
    );
    Ok(())
}
