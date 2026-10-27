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

fn definitions(conn: &Connection) -> Result<Vec<(String, String, Option<String>)>> {
    Ok(conn
        .prepare("SELECT type, name, sql FROM sqlite_schema ORDER BY type, name")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

#[test]
fn open_upgrades_a_version_1_database_and_keeps_its_tasks() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = Connection::open(dir.path().join("state.db"))?;
    conn.execute_batch(include_str!("../../tests/fixtures/schema-v1.sql"))?;
    // Task `i` is interactive: its worker reported for round 2 only, which is running from
    // offset 250. Task `r` is a run whose acknowledged round left its pane being closed. Task `k`
    // keeps its pane, so its acknowledged round did not release it. Task `u` was rearmed before
    // its pane's next conversation reported.
    conn.execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0), ('claude', 'worker', 1, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, worker_pid,
           result_file, keep_pane, pane_state, created_at)
           VALUES ('i', 'x', 'claude', 'owner', 'b1', 7, NULL, 0, 'open', 1),
             ('r', 'y', 'claude', 'owner', 'b2', 8, '/r', 0, 'closing', 2),
             ('k', 'z', 'claude', 'owner', 'b3', 9, NULL, 1, 'open', 3),
             ('u', 'w', 'claude', 'owner', 'b4', 10, NULL, 0, 'open', 4);
         INSERT INTO task_round (task_id, round, worker_runtime, worker_native_id, cursor, state,
           finished_at, acked_at, created_at, result_ref, result)
           VALUES ('i', 1, NULL, NULL, 100, 'completed', 3, 4, 1, 'm1', 'first'),
             ('i', 2, 'claude', 'worker', 250, 'running', NULL, NULL, 5, NULL, NULL),
             ('r', 1, NULL, NULL, 0, 'completed', 6, 7, 2, '/r', 'done'),
             ('k', 1, 'claude', 'worker', 120, 'completed', 6, 7, 3, 'm2', 'kept'),
             ('u', 1, 'claude', 'worker', 300, 'completed', 6, 7, 4, 'm3', 'before'),
             ('u', 2, NULL, NULL, 300, 'running', NULL, NULL, 8, NULL, NULL);
         INSERT INTO notice (id, task_id, round, state, sent_at, delivered_at)
           VALUES ('n', 'i', 1, 'delivered', 3, 4);",
    )?;
    drop(conn);
    let refused = Store::open_read_only(dir.path()).err().unwrap().to_string();
    assert!(refused.contains("older schema"), "{refused}");

    let store = Store::open(dir.path())?;
    let (_fresh_dir, fresh) = self::store();
    assert_eq!(definitions(store.conn())?, definitions(fresh.conn())?);
    type Task = (String, Option<String>, i64, Option<i64>, String);
    let tasks: Vec<Task> = store
        .conn()
        .prepare(
            "SELECT id, worker_native_id, position, released_at, pane_state FROM task ORDER BY id",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        tasks,
        [
            ("i".into(), Some("worker".into()), 250, None, "open".into()),
            ("k".into(), Some("worker".into()), 120, None, "open".into()),
            ("r".into(), None, 0, Some(7), "closing".into()),
            ("u".into(), None, 0, None, "open".into())
        ]
    );
    let rounds: Vec<(String, i64, String, Option<String>)> = store
        .conn()
        .prepare("SELECT task_id, round, state, result FROM task_round ORDER BY task_id, round")?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(rounds.len(), 6);
    assert_eq!(rounds[0].3.as_deref(), Some("first"));
    let notice: String =
        store
            .conn()
            .query_row("SELECT state FROM notice WHERE id = 'n'", [], |row| {
                row.get(0)
            })?;
    assert_eq!(notice, "delivered");
    Ok(())
}

/// Task `a` was registered, and pane `b1`'s occupant reported, while an epoch observed instance
/// `i1` until the server's end; task `b` while an epoch observed `i2` before the daemon
/// reconnected to it; task `c` and pane `b3`'s occupant while no daemon observed the server.
/// Two slots typed their reopen command, one of them claimed.
#[test]
fn open_upgrades_a_version_2_database_dating_each_process_by_its_epoch() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = Connection::open(dir.path().join("state.db"))?;
    conn.execute_batch(include_str!("../../tests/fixtures/schema-v2.sql"))?;
    conn.execute_batch(
        "INSERT INTO epoch (instance_id, started_at, ended_at, end_reason)
           VALUES ('i1', 10, 20, 'server_gone'), ('i2', 40, 60, 'disconnected'),
             ('i2', 61, 70, 'disconnected'), ('i3', 80, NULL, NULL);
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, worker_pid,
           result_file, keep_pane, created_at)
           VALUES ('a', 'x', 'claude', 'owner', 'b1', 7, '/r', 0, 15),
             ('b', 'y', 'claude', 'owner', 'b2', 8, NULL, 0, 45),
             ('c', 'z', 'claude', 'owner', 'b3', 9, NULL, 1, 30),
             ('d', 'w', 'claude', 'owner', 'b4', 10, '/r', 0, 65),
             ('e', 'v', 'claude', 'owner', 'b5', 11, '/r', 0, 85);
         INSERT INTO task_round (task_id, round, state, finished_at, created_at)
           VALUES ('a', 1, 'running', NULL, 15), ('b', 1, 'cancelled', 48, 45),
             ('b', 2, 'running', NULL, 50), ('c', 1, 'running', NULL, 30);
         INSERT INTO request (id, task_id, round, state, text) VALUES ('q', 'b', 2, 'sent', 'go');
         INSERT INTO notice (id, task_id, round, state) VALUES ('n', 'b', 1, 'delivered');
         INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at)
           VALUES ('b1', 'claude', 'owner', 7, 0, 15), ('b3', 'claude', 'owner', 9, 0, 30);
         INSERT INTO restore_op (id, instance_id, session_lineage, state, doc, created_at)
           VALUES ('o', 'i2', 's', 'created', '{}', 41);
         INSERT INTO restore_slot (op_id, slot, instance_id, block_lineage, start, runtime,
           native_id, plan, claimed_at, started)
           VALUES ('o', 0, 'i2', 'b1', 'agent', 'claude', 'owner', '{}', 42, 'agent'),
             ('o', 1, 'i2', 'b2', 'typed', 'claude', 'owner', '{}', 42, 'typed'),
             ('o', 2, 'i2', 'b3', 'typed', 'claude', 'owner', '{}', NULL, NULL);",
    )?;
    drop(conn);
    let store = Store::open(dir.path())?;
    let (_fresh_dir, fresh) = self::store();
    assert_eq!(definitions(store.conn())?, definitions(fresh.conn())?);
    let tasks: Vec<(String, Option<i64>, Option<String>)> = store
        .conn()
        .prepare("SELECT id, worker_pid, worker_instance_id FROM task ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    // Between epochs, and in an epoch whose end is not known: one closed by the daemon after a
    // daemon that died, as another instance's epoch follows, or one still open.
    assert_eq!(
        tasks,
        [
            ("a".into(), Some(7), Some("i1".into())),
            ("b".into(), Some(8), Some("i2".into())),
            ("c".into(), Some(9), None),
            ("d".into(), Some(10), None),
            ("e".into(), Some(11), None)
        ]
    );
    let kept: (i64, String, String, String) = store.conn().query_row(
        "SELECT (SELECT COUNT(*) FROM task_round),
           (SELECT finished_instance_id FROM task_round WHERE finished_at IS NOT NULL),
           (SELECT state FROM request WHERE id = 'q'), (SELECT state FROM notice WHERE id = 'n')",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    assert_eq!(kept, (4, "i2".into(), "sent".into(), "delivered".into()));
    let occupants: Vec<(Option<String>, Option<bool>)> = store
        .conn()
        .prepare("SELECT instance_id, own_program FROM occupant ORDER BY block_lineage")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(occupants, [(Some("i1".into()), None), (None, None)]);
    type Slot = (String, Option<String>, Option<String>);
    let slots: Vec<Slot> = store
        .conn()
        .prepare("SELECT start, runtime, started FROM restore_slot ORDER BY slot")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        slots,
        [
            ("agent".into(), Some("claude".into()), Some("agent".into())),
            ("shell".into(), None, Some("shell".into())),
            ("shell".into(), None, None)
        ]
    );
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
