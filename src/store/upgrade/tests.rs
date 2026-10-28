use anyhow::Result;
use rusqlite::{Connection, params};

use crate::store::Store;

/// The definitions a new database gets.
fn fresh() -> Result<Vec<(String, String, Option<String>)>> {
    let dir = tempfile::tempdir()?;
    definitions(Store::open(dir.path())?.conn())
}

fn definitions(conn: &Connection) -> Result<Vec<(String, String, Option<String>)>> {
    Ok(conn
        .prepare("SELECT type, name, sql FROM sqlite_schema ORDER BY type, name")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// A notice's kind, state, and when it was written.
fn notice_of(store: &Store, id: &str) -> Result<(String, String, i64)> {
    Ok(store.conn().query_row(
        "SELECT kind, state, created_at FROM notice WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

#[test]
fn open_upgrades_a_version_1_database_and_keeps_its_tasks() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = Connection::open(dir.path().join("state.db"))?;
    conn.execute_batch(include_str!("../../../tests/fixtures/schema-v1.sql"))?;
    // Task `i` is interactive: its worker reported for round 2 only, which is blocked at offset
    // 250. Task `r` is a run whose acknowledged round left its pane being closed. Task `k`
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
             ('i', 2, 'claude', 'worker', 250, 'blocked', NULL, NULL, 5, NULL, NULL),
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
    assert_eq!(definitions(store.conn())?, fresh()?);
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
    assert_eq!(rounds[1].2, "running");
    assert_eq!(
        notice_of(&store, "n")?,
        ("finished".into(), "delivered".into(), 3)
    );
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
    conn.execute_batch(include_str!("../../../tests/fixtures/schema-v2.sql"))?;
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
             ('b', 2, 'running', NULL, 50), ('c', 1, 'blocked', NULL, 30);
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
    assert_eq!(definitions(store.conn())?, fresh()?);
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
    assert_eq!(
        notice_of(&store, "n")?,
        ("finished".into(), "delivered".into(), 48)
    );
    let blocked: (String, Option<i64>) = store.conn().query_row(
        "SELECT state, blocked_at FROM task_round WHERE task_id = 'c'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(blocked, ("running".into(), None));
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

/// Version 4 adds where each occupant's record started; an occupant reported before keeps its
/// columns and starts at 0.
#[test]
fn open_upgrades_a_version_3_database_and_keeps_its_occupants() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = Connection::open(dir.path().join("state.db"))?;
    conn.execute_batch(include_str!("../../../tests/fixtures/schema-v3.sql"))?;
    conn.execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'worker', 1, 0);
         INSERT INTO occupant (block_lineage, runtime, native_id, instance_id, pid, pid_name,
           own_program, keeps_session_env, transport, reported_at)
           VALUES ('b', 'claude', 'worker', 'i1', 7, 'claude', 1, 0, '{}', 5);",
    )?;
    drop(conn);
    let store = Store::open(dir.path())?;
    assert_eq!(definitions(store.conn())?, fresh()?);
    let occupant: (String, i64, Option<bool>, i64, i64) = store.conn().query_row(
        "SELECT instance_id, pid, own_program, reported_at, start_position FROM occupant",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    assert_eq!(occupant, ("i1".into(), 7, Some(true), 5, 0));
    Ok(())
}

/// Version 5 adds blocked episodes. A blocked round has none until the daemon sees its pane blocked
/// again, and every earlier notice told of a finished round.
#[test]
fn open_upgrades_versions_3_and_4_and_keeps_rounds_notices_and_requests() -> Result<()> {
    for fixture in [
        include_str!("../../../tests/fixtures/schema-v3.sql"),
        include_str!("../../../tests/fixtures/schema-v4.sql"),
    ] {
        let dir = tempfile::tempdir()?;
        let conn = Connection::open(dir.path().join("state.db"))?;
        conn.execute_batch(fixture)?;
        // Task `i` finished round 1, whose notice was delivered, and its request round 2 is
        // blocked with the request still unsent.
        conn.execute_batch(
            "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
               VALUES ('claude', 'owner', 1, 0), ('claude', 'worker', 1, 0);
             INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage,
               keep_pane, worker_runtime, worker_native_id, position, created_at)
               VALUES ('i', 'x', 'claude', 'owner', 'b1', 0, 'claude', 'worker', 300, 1);
             INSERT INTO task_round (task_id, round, state, finished_at, acked_at, created_at,
               turn_start, result_ref, result)
               VALUES ('i', 1, 'completed', 3, 4, 1, 0, 'm1', 'first'),
                 ('i', 2, 'blocked', NULL, NULL, 5, NULL, NULL, NULL);
             INSERT INTO notice (id, task_id, round, state) VALUES ('n', 'i', 1, 'delivered');
             INSERT INTO request (id, task_id, round, text) VALUES ('q', 'i', 2, 'go');",
        )?;
        drop(conn);
        let store = Store::open(dir.path())?;
        assert_eq!(definitions(store.conn())?, fresh()?);
        type Round = (i64, String, Option<i64>, Option<i64>, Option<String>);
        let rounds: Vec<Round> = store
            .conn()
            .prepare(
                "SELECT round, state, blocked_at, turn_start, result FROM task_round
                 ORDER BY round",
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
            rounds,
            [
                (1, "completed".into(), None, Some(0), Some("first".into())),
                (2, "running".into(), None, None, None)
            ]
        );
        assert_eq!(
            notice_of(&store, "n")?,
            ("finished".into(), "delivered".into(), 3)
        );
        let request: (String, String) = store.conn().query_row(
            "SELECT state, text FROM request WHERE id = 'q'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(request, ("pending".into(), "go".into()));
    }
    Ok(())
}
