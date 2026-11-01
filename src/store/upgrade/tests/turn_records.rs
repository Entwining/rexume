use anyhow::Result;
use rusqlite::Connection;
use serde_json::json;

use super::{definitions, fresh};
use crate::runtime::claude;
use crate::store::Store;
use crate::task::Follow;

/// Version 8 reads a Claude Code conversation's turns from its turn record, so a Claude Code
/// worker's task reads it from the start, its rounds keep their results without an offset, and
/// its occupant's record starts empty; a Codex worker keeps its place in its transcript.
#[test]
fn open_upgrades_a_version_7_database_and_restarts_claude_turn_offsets() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = Connection::open(dir.path().join("state.db"))?;
    conn.execute_batch(include_str!("../../../../tests/fixtures/schema-v7.sql"))?;
    conn.execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0), ('claude', 'cw', 1, 0), ('codex', 'xw', 1, 0);
         INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env,
           start_position, reported_at)
           VALUES ('b1', 'claude', 'cw', 7, 0, 90, 0), ('b2', 'codex', 'xw', 8, 0, 80, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           worker_runtime, worker_native_id, position, created_at)
           VALUES ('c', 'x', 'claude', 'owner', 'b1', 0, 'claude', 'cw', 300, 1),
             ('x', 'y', 'claude', 'owner', 'b2', 0, 'codex', 'xw', 200, 1);
         INSERT INTO task_round (task_id, round, state, finished_at, created_at, turn_start,
           result_ref, result)
           VALUES ('c', 1, 'completed', 3, 1, 100, 'm1', 'first'),
             ('x', 1, 'completed', 3, 1, 50, 'm2', 'other');",
    )?;
    drop(conn);
    let store = Store::open(dir.path())?;
    assert_eq!(definitions(store.conn())?, fresh()?);
    let tasks: Vec<(String, i64, Option<i64>, Option<String>)> = store
        .conn()
        .prepare(
            "SELECT t.id, t.position, r.turn_start, r.result FROM task t
             JOIN task_round r ON r.task_id = t.id ORDER BY t.id",
        )?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        tasks,
        [
            ("c".into(), 0, None, Some("first".into())),
            ("x".into(), 200, Some(50), Some("other".into()))
        ]
    );
    let starts: Vec<i64> = store
        .conn()
        .prepare("SELECT start_position FROM occupant ORDER BY block_lineage")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(starts, [0, 80]);
    Ok(())
}

/// A version 7 database whose Claude Code worker `w` runs request round 1 of task `t`, with the
/// request in `state`, opened at version 8.
fn request_in_flight(dir: &std::path::Path, state: &str) -> Result<Store> {
    let conn = Connection::open(dir.join("state.db"))?;
    conn.execute_batch(include_str!("../../../../tests/fixtures/schema-v7.sql"))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         INSERT INTO epoch (id, instance_id, started_at) VALUES (1, 'i1', 0);
         INSERT INTO block (lineage, instance_id, fg_pid, status, status_pid, status_at)
           VALUES ('b', 'i1', 100, 'working', 100, 1);
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0), ('claude', 'w', 1, 0);
         INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at)
           VALUES ('b', 'claude', 'w', 100, 0, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           worker_runtime, worker_native_id, position, created_at)
           VALUES ('t', 'x', 'claude', 'owner', 'b', 1, 'claude', 'w', 400, 0);
         INSERT INTO task_round (task_id, round, state, created_at) VALUES ('t', 1, 'running', 0);",
    )?;
    conn.execute(
        "INSERT INTO request (id, task_id, round, state, text) VALUES ('q', 't', 1, ?1, 'go')",
        [state],
    )?;
    drop(conn);
    let store = Store::open(dir)?;
    // The worker's hooks write its turn record only from the upgrade on: the request's prompt is
    // not in it, only the end of the turn that took it in.
    claude::append(
        dir,
        "w",
        &json!({ "event": "stop", "prompt_id": "p1", "id": "s1", "answer": "Done." }),
    )?;
    Ok(store)
}

fn round(store: &Store) -> Result<(String, Option<String>, String)> {
    Ok(store.conn().query_row(
        "SELECT r.state, r.result, q.state FROM task_round r JOIN request q USING (task_id, round)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

/// A request received before the upgrade is received before the record its round now reads, so
/// the round takes the first turn that record shows ending.
#[test]
fn a_request_received_before_the_upgrade_takes_the_next_turn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = request_in_flight(dir.path(), "delivered")?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(
        round(&store)?,
        ("completed".into(), Some("Done.".into()), "delivered".into())
    );
    Ok(())
}

/// A request whose receipt only the transcript before the upgrade shows is not known received:
/// its round waits until the owner abandons it, and the request is not sent again.
#[test]
fn a_request_sent_but_not_seen_received_before_the_upgrade_waits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = request_in_flight(dir.path(), "sent")?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(round(&store)?, ("running".into(), None, "sent".into()));
    Ok(())
}
