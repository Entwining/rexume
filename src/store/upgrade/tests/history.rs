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

/// `conn` holding a version 9 database in `dir`, with task `t` of owner `owner` on worker pane `b`.
fn version_9(dir: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(dir.join("state.db"))?;
    conn.execute_batch(include_str!("../../../../tests/fixtures/schema-v9.sql"))?;
    conn.execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at) VALUES ('t', 'x', 'claude', 'owner', 'b', 0, 0);",
    )?;
    Ok(conn)
}

/// A round blocked before version 10 keeps its episode open from when it began, and the episode
/// keeps the notice that told of it, so nothing is sent again; an earlier episode's notice, which
/// version 9 no longer needed, belongs to no episode, and the owner is owed what it was before.
#[test]
fn open_upgrades_a_version_9_blocked_round_into_its_open_episode() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = version_9(dir.path())?;
    conn.execute_batch(
        "INSERT INTO task_round (task_id, round, state, blocked_at, created_at)
           VALUES ('t', 1, 'blocked', 50, 0);
         INSERT INTO notice (id, task_id, round, kind, state, created_at, sent_at)
           VALUES ('earlier', 't', 1, 'blocked', 'sent', 20, 23),
             ('current', 't', 1, 'blocked', 'sent', 50, 53);",
    )?;
    drop(conn);
    let store = Store::open(dir.path())?;
    let episode: (String, i64, Option<i64>) = store.conn().query_row(
        "SELECT id, started_at, ended_at FROM blocked_episode WHERE task_id = 't' AND round = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!((episode.1, episode.2), (50, None));
    let notices: Vec<(String, String, Option<String>)> = store
        .conn()
        .prepare("SELECT id, state, blocked_episode_id FROM notice ORDER BY created_at")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        notices,
        [
            ("earlier".into(), "sent".into(), None),
            ("current".into(), "sent".into(), Some(episode.0))
        ]
    );
    let round: String = store
        .conn()
        .query_row("SELECT state FROM task_round", [], |row| row.get(0))?;
    assert_eq!(round, "blocked");
    let owed = crate::task::undelivered(store.conn(), None)?;
    assert_eq!(
        owed.iter().map(|n| &n["id"]).collect::<Vec<_>>(),
        ["current"]
    );
    Ok(())
}

/// Version 10's new columns start empty for what version 9 recorded: no meta, no attempt counted,
/// no delivery time, and a run without a conversation; a notice keeps when it was sent, and a
/// closed pane its state.
#[test]
fn open_upgrades_a_version_9_database_leaving_its_records_as_they_were() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let conn = version_9(dir.path())?;
    conn.execute_batch(
        "UPDATE task SET result_file = '/r.md', worker_pid = 7, pane_state = 'closed',
           pane_closed_at = 9;
         INSERT INTO task_round (task_id, round, state, finished_at, created_at, turn_start,
           result_ref, result) VALUES ('t', 1, 'completed', 8, 0, NULL, 'r', 'done');
         INSERT INTO request (id, task_id, round, state, text) VALUES ('q', 't', 1, 'delivered', 'go');
         INSERT INTO notice (id, task_id, round, kind, state, created_at, sent_at)
           VALUES ('n', 't', 1, 'finished', 'delivered', 8, 12);",
    )?;
    drop(conn);
    let store = Store::open(dir.path())?;
    let task: (String, Option<String>, String, Option<String>) = store.conn().query_row(
        "SELECT meta, worker_native_id, pane_state, pane_note FROM task",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    assert_eq!(task, ("{}".into(), None, "closed".into(), None));
    let request: (i64, Option<i64>, Option<i64>) = store.conn().query_row(
        "SELECT attempts, sent_at, delivered_at FROM request",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(request, (0, None, None));
    let notice: (i64, Option<i64>, Option<i64>, Option<i64>, Option<String>) =
        store.conn().query_row(
            "SELECT attempts, sent_at, delivered_at, superseded_at, blocked_episode_id FROM notice",
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
    assert_eq!(notice, (0, Some(12), None, None, None));
    Ok(())
}
