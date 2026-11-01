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

/// The state directory, each parent it needed, and the directories under it are created for their
/// user alone, whether a turn hook or the database creates them first.
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

/// Rex's own restore brings sessions and panes back under their IDs. A pane it restored keeps the
/// lineage its ID had on the latest earlier instance that bound it, unless another resource of the
/// instance holds that lineage already, as a copy an earlier version restored beside Rex's holds
/// the original's.
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
fn sweep_keeps_what_an_open_task_or_a_recent_restore_still_names() -> Result<()> {
    let (_dir, mut store) = store();
    let old = now_ms() - i64::try_from(RETENTION.as_millis())? - 1;
    let tx = store.write()?;
    tx.execute_batch(&format!(
        "INSERT INTO epoch (instance_id, started_at, ended_at, end_reason)
           VALUES ('i1', 0, {old}, 'server_gone'), ('i0', 0, {old}, 'server_gone');
         INSERT INTO block (lineage, instance_id) VALUES ('b-task', 'i1'), ('b-gone', 'i1'),
           ('b-now', 'i2');
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i1', 'x', 'x'), ('i2', 'y', 'y');
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0), ('claude', 'unused', 1, 0), ('claude', 'restored', 1, 0),
             ('claude', 'expired', 1, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at) VALUES ('t', 'x', 'claude', 'owner', 'b-task', 0, 0);
         INSERT INTO restore (id, instance_id, block_lineage, start, runtime, native_id, plan, state,
           created_at)
           VALUES ('r-old', 'i1', 'b-gone', 'agent', 'claude', 'expired', '{{}}', 'skipped', {old}),
             ('r-recent', 'i1', 'b-other', 'agent', 'claude', 'restored', '{{}}', 'skipped', {now}),
             ('r-current', 'i2', 'b-now', 'agent', 'claude', 'expired', '{{}}', 'planned', {old});",
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
        ["b-now", "b-task"]
    );
    assert_eq!(names("SELECT live_id FROM live"), ["y"]);
    assert_eq!(names("SELECT instance_id FROM epoch"), ["i1"]);
    assert_eq!(
        names("SELECT native_id FROM conversation ORDER BY native_id"),
        ["expired", "owner", "restored"]
    );
    assert_eq!(
        names("SELECT id FROM restore ORDER BY id"),
        ["r-current", "r-recent"]
    );
    Ok(())
}

/// A pane the sweep forgets takes its occupant along, so the conversation's row can expire; an
/// occupant reported on the current instance before the daemon has seen its pane stays.
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
    sweep(&tx, "i2", now_ms())?;
    let occupants: Vec<String> = tx
        .prepare("SELECT block_lineage FROM occupant")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(occupants, ["b-unseen"]);
    let conversations: Vec<String> = tx
        .prepare("SELECT native_id FROM conversation")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(conversations, ["c-new"]);
    Ok(())
}

/// The sweep reaches a task's notices through an index, both its own check for notices that may
/// still reach their owner and the foreign key's lookup when a deleted task takes its rounds along.
#[test]
fn notices_are_found_by_task_and_round_through_an_index() -> Result<()> {
    let (_dir, store) = store();
    for sql in [
        "SELECT 1 FROM notice n WHERE n.task_id = ?1 AND n.state IN ('pending', 'sent')",
        "SELECT 1 FROM notice WHERE task_id = ?1 AND round = ?2",
    ] {
        let mut plan = store.conn().prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let mut rows = plan.raw_query();
        while let Some(row) = rows.next()? {
            let detail: String = row.get(3)?;
            assert!(!detail.starts_with("SCAN"), "{sql}: {detail}");
        }
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

/// A Claude Code turn record goes once its hooks have written nothing for the retention period
/// and no conversation row names it.
#[test]
fn sweep_records_deletes_only_old_records_of_forgotten_conversations() -> Result<()> {
    let (dir, store) = store();
    store.conn().execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'known', 1, 0);",
    )?;
    let old = SystemTime::now() - RETENTION - Duration::from_secs(60);
    std::fs::create_dir_all(claude::records(dir.path()))?;
    for (native_id, modified) in [
        ("known", old),
        ("forgotten", old),
        ("recent", SystemTime::now()),
    ] {
        std::fs::File::create(claude::record(dir.path(), native_id))?.set_modified(modified)?;
    }
    sweep_records(&store)?;
    let mut left: Vec<String> = std::fs::read_dir(claude::records(dir.path()))?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_>>()?;
    left.sort();
    assert_eq!(left, ["known.jsonl", "recent.jsonl"]);
    Ok(())
}
