use super::*;
use crate::runtime::claude;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

#[test]
fn open_creates_the_schema_once_and_reopens_without_writing() -> Result<()> {
    let (dir, store) = store();
    let source_id = |store: &Store| -> Result<String> {
        Ok(store
            .conn()
            .query_row("SELECT source_id FROM store", [], |row| row.get(0))?)
    };
    let minted = source_id(&store)?;
    let before = store.data_version()?;
    let again = Store::open(dir.path())?;
    let version: i64 = again
        .conn()
        .pragma_query_value(None, "user_version", |row| row.get(0))?;
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(source_id(&again)?, minted);
    assert_eq!(store.data_version()?, before);
    Ok(())
}

#[test]
fn the_state_directory_is_created_for_its_user_alone() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode =
        |path: &Path| -> Result<u32> { Ok(std::fs::metadata(path)?.permissions().mode() & 0o777) };
    let dir = tempfile::tempdir()?;
    let hooked = dir.path().join("hooked/rexume");
    claude::append(&hooked, "s", &serde_json::json!({ "event": "prompt" }))?;
    let opened = dir.path().join("opened/rexume");
    Store::open(&opened)?;
    for path in [
        dir.path().join("hooked"),
        hooked.clone(),
        claude::records(&hooked),
        dir.path().join("opened"),
        opened,
    ] {
        assert_eq!(mode(&path)?, 0o700, "{}", path.display());
    }
    Ok(())
}

#[test]
fn open_refuses_a_newer_schema_and_a_foreign_file() -> Result<()> {
    let (dir, store) = store();
    store
        .conn()
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)?;
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
fn ensure_lineage_keeps_the_lineage_of_an_id_the_server_brought_back() -> Result<()> {
    let (_dir, mut store) = store();
    let tx = store.write()?;
    tx.execute_batch(
        "INSERT INTO epoch (instance_id, started_at) VALUES ('i0', 0), ('i1', 1), ('i2', 2);
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i0', 'block:kept', 'block:stale'),
           ('i1', 'block:kept', 'block:first'), ('i1', 'block:twin', 'block:twin'),
           ('i2', 'block:copy', 'block:twin');",
    )?;
    assert_eq!(ensure_lineage(&tx, "i2", "block:kept")?, "block:first");
    let twin = ensure_lineage(&tx, "i2", "block:twin")?;
    assert!(
        !["block:twin", "block:first"].contains(&twin.as_str()),
        "{twin}"
    );
    assert_eq!(ensure_lineage(&tx, "i2", "block:twin")?, twin);
    Ok(())
}

#[test]
fn sweep_keeps_the_pane_an_open_task_still_names() -> Result<()> {
    let (_dir, mut store) = store();
    let tx = store.write()?;
    tx.execute_batch(
        "INSERT INTO block (lineage, instance_id) VALUES ('b-task', 'i1'), ('b-gone', 'i1'),
           ('b-now', 'i2');
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i1', 'x', 'x'), ('i2', 'y', 'y');
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at) VALUES ('t', 'x', 'claude', 'owner', 'b-task', 0, 0);",
    )?;
    sweep(&tx, "i2")?;
    let names = |sql: &str| -> Vec<String> {
        let mut statement = tx.prepare(sql).unwrap();
        let rows = statement.query_map([], |row| row.get(0)).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    };
    assert_eq!(
        names("SELECT lineage FROM block ORDER BY lineage"),
        ["b-now", "b-task"]
    );
    assert_eq!(names("SELECT live_id FROM live"), ["y"]);
    Ok(())
}

#[test]
fn sweep_forgets_the_occupant_of_a_pane_it_forgets() -> Result<()> {
    let (_dir, mut store) = store();
    let old = now_ms() - i64::try_from(RETENTION.as_millis())? - 1;
    let tx = store.write()?;
    tx.execute_batch(&format!(
        "INSERT INTO block (lineage, instance_id) VALUES ('b-closed', 'i1');
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'c-closed', 1, {old}), ('claude', 'c-new', 1, {old});
         INSERT INTO occupant (block_lineage, runtime, native_id, instance_id, pid,
           keeps_session_env, reported_at)
           VALUES ('b-closed', 'claude', 'c-closed', 'i1', 1, 0, {old}),
             ('b-unseen', 'claude', 'c-new', 'i2', 2, 0, {old});"
    ))?;
    sweep(&tx, "i2")?;
    tx.commit()?;
    archive::sweep(&mut store, "i2", now_ms())?;
    let occupants: Vec<String> = store
        .conn()
        .prepare("SELECT block_lineage FROM occupant")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(occupants, ["b-unseen"]);
    let conversations: Vec<String> = store
        .conn()
        .prepare("SELECT native_id FROM conversation")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(conversations, ["c-new"]);
    Ok(())
}

/// The foreign key reaches the notices of a round the sweep deletes through an index.
#[test]
fn notices_are_found_by_task_and_round_through_an_index() -> Result<()> {
    let (_dir, store) = store();
    let mut plan = store
        .conn()
        .prepare("EXPLAIN QUERY PLAN SELECT 1 FROM notice WHERE task_id = ?1 AND round = ?2")?;
    let mut rows = plan.raw_query();
    while let Some(row) = rows.next()? {
        let detail: String = row.get(3)?;
        assert!(!detail.starts_with("SCAN"), "{detail}");
    }
    Ok(())
}

#[test]
fn settle_returns_after_another_connections_commit() -> Result<()> {
    let (dir, mut store) = store();
    let before = store.data_version()?;
    let (holding, held) = std::sync::mpsc::channel();
    let path = dir.path().to_owned();
    let writer = std::thread::spawn(move || -> Result<()> {
        let mut other = Store::open(&path)?;
        let tx = other.write()?;
        tx.execute("CREATE TABLE committed (id INTEGER) STRICT", [])?;
        holding.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        tx.commit()?;
        Ok(())
    });
    held.recv().unwrap();
    store.settle()?;
    assert_ne!(store.data_version()?, before);
    writer.join().unwrap()
}

#[test]
fn sweep_records_deletes_only_old_records_of_forgotten_conversations() -> Result<()> {
    let (dir, store) = store();
    store.conn().execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'known', 1, 0);",
    )?;
    let old = SystemTime::now() - RETENTION - Duration::from_secs(60);
    let left = |runtime: Runtime| -> Result<Vec<String>> {
        let records = runtime.records(dir.path()).context("no records")?;
        let mut left: Vec<String> = std::fs::read_dir(records)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_>>()?;
        left.sort();
        Ok(left)
    };
    for runtime in [Runtime::Claude, Runtime::Omp] {
        std::fs::create_dir_all(runtime.records(dir.path()).context("no records")?)?;
        for (native_id, modified) in [
            ("known", old),
            ("forgotten", old),
            ("recent", SystemTime::now()),
        ] {
            let record = runtime.record(dir.path(), native_id, None);
            std::fs::File::create(record.context("no record")?)?.set_modified(modified)?;
        }
    }
    sweep_records(&store)?;
    assert_eq!(left(Runtime::Claude)?, ["known.jsonl", "recent.jsonl"]);
    assert_eq!(left(Runtime::Omp)?, ["recent.jsonl"]);
    Ok(())
}
