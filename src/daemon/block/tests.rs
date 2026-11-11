use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::params;
use serde_json::{Value, json};

use super::super::view::View;
use super::touch_block;
use crate::store::{Store, now_ms};
use crate::task::tests::{ANSWER, PROMPT, record, task_world};
use crate::task::{Follow, retired_workers};

/// Records, as its event does, that process `pid` in pane `block` of `instance` reports `status`,
/// an object with Rex's `state` and, for a `blocked` one, its `kind` and `msg`.
fn report(store: &mut Store, instance: &str, block: &str, status: Value, pid: i64) -> Result<()> {
    let now = chrono::DateTime::from_timestamp_millis(now_ms()).context("now is out of range")?;
    let mut record = status;
    record["owner"] = json!({ "pid": pid, "name": "claude" });
    record["updated_at"] = json!(now.to_rfc3339());
    let payload = json!({ "record": record });
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

/// An episode's kind and message, and why it ended.
type Episode = (Option<String>, Option<String>, Option<String>);

/// Task `t`'s blocked episodes in the order they began.
fn episodes(store: &Store) -> Result<Vec<Episode>> {
    Ok(store
        .conn()
        .prepare(
            "SELECT kind, message, end_reason FROM blocked_episode ORDER BY started_at, rowid",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn episode(kind: Option<&str>, message: Option<&str>, ended: Option<&str>) -> Episode {
    (
        kind.map(Into::into),
        message.map(Into::into),
        ended.map(Into::into),
    )
}

/// Pane `b`'s recorded status with its kind and message.
fn status(store: &Store) -> Result<(Option<String>, Option<String>, Option<String>)> {
    Ok(store.conn().query_row(
        "SELECT status, status_kind, status_message FROM block WHERE lineage = 'b'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

fn blocked_on(kind: &str, msg: &str) -> Value {
    json!({ "state": "blocked", "kind": kind, "msg": msg })
}

/// `task_world` with its worker in a blocked episode on instance `i1`, whose notice a `task wait`
/// took when `taken`.
fn blocked_world(dir: &Path, transcript: &Path, taken: bool) -> Result<(Store, Follow)> {
    let mut store = task_world(dir, transcript, None)?;
    store.conn().execute_batch(
        "UPDATE block SET status = 'blocked', status_kind = 'permission', status_message = 'Edit?',
           status_at = 1 WHERE lineage = 'b'",
    )?;
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
/// second begins its own once its grace passes, with its own notice and the kind and message
/// `blocked_on("question", "Which branch?")` reported.
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
    assert_eq!(
        episodes(store)?[1..],
        [episode(Some("question"), Some("Which branch?"), None)]
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
        report(&mut store, "i1", "b", json!({ "state": "working" }), 100)?;
        assert_eq!(status(&store)?, (Some("working".into()), None, None));
        assert_eq!(
            episodes(&store)?,
            [episode(
                Some("permission"),
                Some("Edit?"),
                Some("status_changed")
            )]
        );
        report(
            &mut store,
            "i1",
            "b",
            blocked_on("question", "Which branch?"),
            100,
        )?;
        let reported = (
            Some("blocked".into()),
            Some("question".into()),
            Some("Which branch?".into()),
        );
        assert_eq!(status(&store)?, reported);
        second_episode(&mut store, &mut follow, first(taken))?;
    }
    Ok(())
}

/// A blocked status that changes what it says while the worker stays blocked is the same episode,
/// which keeps what it began with and tells nothing new.
#[test]
fn a_worker_still_blocked_with_another_message_stays_in_its_episode() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let (mut store, mut follow) = blocked_world(dir.path(), transcript.path(), false)?;
    report(
        &mut store,
        "i1",
        "b",
        blocked_on("question", "Which branch?"),
        100,
    )?;
    follow.pass(&mut store, None)?;
    assert_eq!(
        observed(&store)?,
        ("blocked".into(), vec![("pending".into(), None)])
    );
    assert_eq!(
        episodes(&store)?,
        [episode(Some("permission"), Some("Edit?"), None)]
    );
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
    let tx = store.write()?;
    touch_block(&tx, "i2", "block:new")?;
    tx.commit()?;
    assert_eq!(status(&store)?, (None, None, None));
    assert_eq!(
        episodes(&store)?,
        [episode(
            Some("permission"),
            Some("Edit?"),
            Some("server_gone")
        )]
    );
    report(
        &mut store,
        "i2",
        "block:new",
        blocked_on("question", "Which branch?"),
        200,
    )?;
    second_episode(
        &mut store,
        &mut follow,
        superseded("the block ended before it was sent"),
    )
}

/// Records, as its event does, Rex's report `generation` that pane `b` of instance `i1` has the
/// foreground process `pid`, or none.
fn foreground(store: &mut Store, pid: Option<i64>, generation: i64) -> Result<()> {
    let foreground = pid.map(|pid| json!({ "pid": pid, "name": "claude" }));
    let payload = json!({ "foreground": foreground, "generation": generation });
    let event = json!({ "event": "process_changed", "block_id": "b", "payload": payload });
    View::new("i1").block_event(store, "process_changed", &event, &payload)?;
    Ok(())
}

/// Rex reports a pane again when its foreground process changes directory or a client starts
/// receiving the terminal's output, which is no use of a released worker's pane: the pane is
/// still closed. Another program that runs there is use, even once the worker's process is the
/// foreground again.
#[test]
fn only_another_foreground_process_keeps_a_released_workers_pane() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    report(&mut store, "i1", "b", json!({ "state": "done" }), 100)?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(observed(&store)?.0, "completed");
    store
        .conn()
        .execute_batch("UPDATE task_round SET acked_at = 1; UPDATE task SET released_at = 1;")?;
    let retired = [("t".to_owned(), "b".to_owned())];
    assert_eq!(retired_workers(store.conn(), store.dir())?, retired);
    foreground(&mut store, Some(100), 6)?;
    foreground(&mut store, Some(100), 7)?;
    assert_eq!(retired_workers(store.conn(), store.dir())?, retired);
    foreground(&mut store, Some(300), 8)?;
    foreground(&mut store, Some(100), 9)?;
    assert!(retired_workers(store.conn(), store.dir())?.is_empty());
    Ok(())
}

/// The pane's own program ending clears its foreground, a change of process like any other; an
/// empty foreground Rex reports after it is no further change.
#[test]
fn the_exit_of_the_panes_program_counts_as_a_foreground_change() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let payload = json!({ "exit_code": 0 });
    let event = json!({ "event": "child_exited", "block_id": "b", "payload": payload });
    View::new("i1").block_event(&mut store, "child_exited", &event, &payload)?;
    let pane = |store: &Store| -> Result<(Option<i64>, Option<i64>, i64)> {
        Ok(store.conn().query_row(
            "SELECT fg_pid, last_exit, fg_changes FROM block WHERE lineage = 'b'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    };
    assert_eq!(pane(&store)?, (None, Some(0), 1));
    foreground(&mut store, None, 6)?;
    assert_eq!(pane(&store)?, (None, Some(0), 1));
    Ok(())
}
