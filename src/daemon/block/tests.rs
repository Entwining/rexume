use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::params;
use serde_json::json;

use super::super::view::View;
use crate::layout::bind_restored;
use crate::store::{Store, now_ms};
use crate::task::Follow;
use crate::task::tests::{PROMPT, record, task_world};

/// Records, as its event does, that process `pid` in pane `block` of `instance` reports `state`.
fn report(store: &mut Store, instance: &str, block: &str, state: &str, pid: i64) -> Result<()> {
    let now = chrono::DateTime::from_timestamp_millis(now_ms()).context("now is out of range")?;
    let payload = json!({ "record": { "state": state, "owner": { "pid": pid, "name": "claude" },
                                      "updated_at": now.to_rfc3339() } });
    let event = json!({ "event": "program_status_changed", "block_id": block, "payload": payload });
    View::new(instance).block_event(store, "program_status_changed", &event, &payload)?;
    Ok(())
}

/// A notice's state and note.
type Notice = (String, Option<String>);

/// Task `t`'s round state and its notices, in the order they were written.
fn observed(store: &Store) -> Result<(String, Vec<Notice>)> {
    let state = store
        .conn()
        .query_row("SELECT state FROM task_round", [], |row| row.get(0))?;
    let notices = store
        .conn()
        .prepare("SELECT state, note FROM notice ORDER BY rowid")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok((state, notices))
}

fn superseded(note: &str) -> Notice {
    ("superseded".into(), Some(note.into()))
}

/// `task_world` with its worker in a blocked episode on instance `i1`, whose notice a `task wait`
/// took when `taken`.
fn blocked_world(dir: &Path, transcript: &Path, taken: bool) -> Result<(Store, Follow)> {
    let mut store = task_world(dir, transcript, None)?;
    store
        .conn()
        .execute_batch("UPDATE block SET status = 'blocked', status_at = 1 WHERE lineage = 'b'")?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    if taken {
        store
            .conn()
            .execute_batch("UPDATE notice SET state = 'superseded', note = 'taken by task wait'")?;
    }
    Ok((store, follow))
}

/// How the first episode's notice ends: taken by a wait, or dropped unsent with the episode.
fn first(taken: bool) -> Notice {
    if taken {
        superseded("taken by task wait")
    } else {
        superseded("the block ended before it was sent")
    }
}

/// The task's pass after both reports sees only the second; the first ended the episode, so the
/// second begins its own once its grace passes, with its own notice.
fn second_episode(store: &mut Store, follow: &mut Follow, first: Notice) -> Result<()> {
    follow.pass(store, None)?;
    assert_eq!(observed(store)?, ("running".into(), vec![first.clone()]));
    assert!(follow.deadline().is_some());
    store.conn().execute(
        "UPDATE block SET status_at = ?1",
        params![now_ms() - 60_000],
    )?;
    follow.pass(store, None)?;
    assert_eq!(
        observed(store)?,
        ("blocked".into(), vec![first, ("pending".into(), None)])
    );
    Ok(())
}

/// A worker that answers one dialog and stops on the next between two task passes, as two
/// permission prompts in a row make it.
#[test]
fn a_worker_stopping_on_another_dialog_before_a_pass_tells_of_each() -> Result<()> {
    for taken in [false, true] {
        let dir = tempfile::tempdir()?;
        let transcript = record(&[PROMPT])?;
        let (mut store, mut follow) = blocked_world(dir.path(), transcript.path(), taken)?;
        report(&mut store, "i1", "b", "working", 100)?;
        report(&mut store, "i1", "b", "blocked", 100)?;
        second_episode(&mut store, &mut follow, first(taken))?;
    }
    Ok(())
}

/// A worker pane restored on a new server whose conversation, started again, stops on a dialog
/// before the task's first pass there: the earlier episode went with the earlier server.
#[test]
fn a_restored_worker_stopping_on_a_dialog_before_a_pass_begins_a_new_episode() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let (mut store, mut follow) = blocked_world(dir.path(), transcript.path(), false)?;
    store.conn().execute_batch(
        "INSERT INTO epoch (id, instance_id, started_at) VALUES (2, 'i2', 2);
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'block:new', 'b');
         UPDATE occupant SET pid = 200, reported_at = 2 WHERE block_lineage = 'b';",
    )?;
    report(&mut store, "i2", "block:new", "blocked", 200)?;
    second_episode(
        &mut store,
        &mut follow,
        superseded("the block ended before it was sent"),
    )
}

/// The same, with the restored pane seen under its own ID before the gate bound it, so that the
/// binding merges that record over the worker's pane already on the new instance.
#[test]
fn a_worker_pane_bound_after_it_was_seen_ends_the_earlier_episode() -> Result<()> {
    for taken in [false, true] {
        let dir = tempfile::tempdir()?;
        let transcript = record(&[PROMPT])?;
        let (mut store, mut follow) = blocked_world(dir.path(), transcript.path(), taken)?;
        store.conn().execute_batch(
            "INSERT INTO epoch (id, instance_id, started_at) VALUES (2, 'i2', 2);",
        )?;
        let size = json!({ "columns": 80, "rows": 24 });
        let event = json!({ "event": "size_changed", "block_id": "block:new", "payload": size });
        View::new("i2").block_event(&mut store, "size_changed", &event, &size)?;
        let tx = store.write()?;
        bind_restored(&tx, "i2", "block:new", "b")?;
        tx.commit()?;
        store.conn().execute_batch(
            "UPDATE occupant SET pid = 200, reported_at = 2 WHERE block_lineage = 'b';",
        )?;
        report(&mut store, "i2", "block:new", "blocked", 200)?;
        second_episode(&mut store, &mut follow, first(taken))?;
    }
    Ok(())
}
