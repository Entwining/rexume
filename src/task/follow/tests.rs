use std::io::Write as _;
use std::path::Path;

use anyhow::Result;
use rusqlite::params;

use super::*;
use crate::runtime::claude;
use crate::task::tests::{
    ANSWER, ANSWER_2, PROMPT, PROMPT_2, record, round, task_world, turn_record,
};
use crate::task::unread;

fn bound(store: &Store) -> Result<Option<String>> {
    Ok(store.conn().query_row(
        "SELECT worker_native_id FROM task WHERE id = 't'",
        [],
        |row| row.get(0),
    )?)
}

/// A turn the user interrupted records no end, so its round waits, the worker idle meanwhile,
/// until the worker takes other input: the round then ends as cancelled, and the turn that input
/// starts is unread once it finishes.
#[test]
fn a_round_whose_turn_was_interrupted_ends_cancelled_at_the_next_prompt() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute(
        "UPDATE block SET status = 'idle', status_pid = 100, status_at = 2 WHERE lineage = 'b'",
        [],
    )?;
    let mut follow = Follow::default();
    let interest = follow.pass(&mut store, None)?;
    assert!(
        interest
            .files
            .contains(&claude::record(dir.path(), "worker"))
    );
    assert_eq!(round(&store)?.0, "running");
    writeln!(transcript, "{PROMPT_2}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "cancelled");
    writeln!(transcript, "{ANSWER_2}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    assert_eq!(unread(store.conn(), store.dir(), "t")?, Some(1));
    Ok(())
}

#[test]
fn a_bound_task_keeps_its_conversation_when_another_takes_over_the_pane() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT])?;
    let other = record(&[
        PROMPT,
        r#"{"event":"stop","prompt_id":"p1","id":"m2","answer":"Other."}"#,
    ])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    turn_record(dir.path(), "other", other.path())?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    store.conn().execute(
        "INSERT INTO conversation (runtime, native_id, transcript, resumable, reported_at)
         VALUES ('claude', 'other', ?1, 1, 300)",
        params![other.path().to_string_lossy()],
    )?;
    store.conn().execute_batch(
        "UPDATE occupant SET native_id = 'other', pid = 200, reported_at = 300 WHERE block_lineage = 'b';
         UPDATE block SET fg_pid = 200, fg_generation = 6 WHERE lineage = 'b';",
    )?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    // The worker's own answer still counts, though its process no longer runs in the pane.
    writeln!(transcript, "{ANSWER}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    assert_eq!(
        round(&store)?,
        ("completed".into(), Some("Done.".into()), None)
    );
    Ok(())
}

#[test]
fn the_daemon_binds_a_task_registered_before_its_worker_reported_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    // An earlier conversation is the occupant at registration: its process had the foreground's
    // PID, but on an earlier server instance.
    store.conn().execute_batch(
        "UPDATE task SET worker_runtime = NULL, worker_native_id = NULL, created_at = 100;
         INSERT INTO conversation (runtime, native_id, resumable, reported_at) VALUES ('claude', 'old', 1, 40);
         UPDATE occupant SET native_id = 'old', instance_id = 'i0', reported_at = 40 WHERE block_lineage = 'b';",
    )?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    assert_eq!(bound(&store)?, None);
    store.conn().execute(
        "UPDATE occupant SET native_id = 'worker', instance_id = 'i1', reported_at = 150
         WHERE block_lineage = 'b'",
        [],
    )?;
    follow.pass(&mut store, None)?;
    assert_eq!(bound(&store)?.as_deref(), Some("worker"));
    store.conn().execute(
        "UPDATE occupant SET native_id = 'old', pid = 90, reported_at = 300 WHERE block_lineage = 'b'",
        [],
    )?;
    follow.pass(&mut store, None)?;
    assert_eq!(bound(&store)?.as_deref(), Some("worker"));
    Ok(())
}

/// A worker that resumed a conversation reports where its record ended, and its task's first
/// round takes no turn from before that, also when the task was registered first and the daemon
/// restarts after binding it.
#[test]
fn a_resumed_workers_first_round_starts_where_its_report_found_the_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let earlier = format!("{PROMPT}\n{ANSWER}\n");
    let mut transcript = record(&[PROMPT, ANSWER, PROMPT_2])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute(
        "UPDATE task SET worker_runtime = NULL, worker_native_id = NULL, created_at = 100",
        [],
    )?;
    store.conn().execute(
        "UPDATE occupant SET start_position = ?1, reported_at = 150 WHERE block_lineage = 'b'",
        params![i64::try_from(earlier.len())?],
    )?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(bound(&store)?.as_deref(), Some("worker"));
    writeln!(transcript, "{ANSWER_2}")?;
    transcript.flush()?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(
        round(&store)?,
        ("completed".into(), Some("Again.".into()), None)
    );
    assert_eq!(unread(store.conn(), store.dir(), "t")?, Some(0));
    Ok(())
}

#[test]
fn a_pass_with_nothing_to_settle_takes_no_write_lock() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    // A round whose worker has not reported yet has nothing to bind or read.
    store.conn().execute_batch(
        "UPDATE task SET worker_runtime = NULL, worker_native_id = NULL, created_at = 100;
         INSERT INTO conversation (runtime, native_id, resumable, reported_at) VALUES ('claude', 'old', 1, 40);
         UPDATE occupant SET native_id = 'old', pid = 90, reported_at = 40 WHERE block_lineage = 'b';",
    )?;
    let hook = rusqlite::Connection::open(dir.path().join("state.db"))?;
    hook.execute_batch("BEGIN IMMEDIATE")?;
    Follow::default().pass(&mut store, None)?;
    hook.execute_batch("ROLLBACK")?;
    assert_eq!(bound(&store)?, None);
    Ok(())
}

#[test]
fn a_non_interactive_round_ends_by_its_exit_and_result_file() -> Result<()> {
    let too_large = "x".repeat(1 << 20 | 1);
    let cases = [
        (Some(100), None, Some("Report."), "running", ""),
        (None, Some(0), Some("Report."), "completed", ""),
        (None, Some(0), None, "failed", "does not exist"),
        (None, Some(0), Some(""), "failed", "is empty"),
        (
            None,
            Some(0),
            Some(too_large.as_str()),
            "failed",
            "is larger than",
        ),
        (None, Some(-1), None, "terminated", "ended by a signal"),
        (Some(300), Some(2), None, "failed", "exited 2"),
    ];
    for (fg_pid, last_exit, contents, state, note) in cases {
        let dir = tempfile::tempdir()?;
        let result_file = dir.path().join("result.md");
        if let Some(contents) = contents {
            std::fs::write(&result_file, contents)?;
        }
        let path = result_file.to_string_lossy();
        let mut store = task_world(dir.path(), Path::new("/unused"), Some(&path))?;
        store.conn().execute(
            "UPDATE block SET fg_pid = ?1, last_exit = ?2 WHERE lineage = 'b'",
            params![fg_pid, last_exit],
        )?;
        Follow::default().pass(&mut store, None)?;
        let (found, result, found_note) = round(&store)?;
        assert_eq!(found, state, "{fg_pid:?} {last_exit:?} {note}");
        assert!(found_note.unwrap_or_default().contains(note), "{note}");
        if state == "completed" {
            assert_eq!(result.as_deref(), contents);
        }
    }
    Ok(())
}

/// A new server instance first finishes a run by what the earlier instance recorded, then ends
/// the runs that instance left open. A run's own instance is the only one whose record shows it,
/// and a run from before instances were recorded is left to its record.
#[test]
fn a_new_instance_ends_the_runs_the_earlier_one_left_open() -> Result<()> {
    let cases = [
        (None, Some(0), Some("i1"), "completed", ""),
        (
            Some(100),
            None,
            Some("i1"),
            "terminated",
            "the Rex server stopped",
        ),
        (Some(100), None, None, "running", ""),
        (None, Some(0), Some("i2"), "running", ""),
    ];
    for (fg_pid, last_exit, ran_on, state, note) in cases {
        let dir = tempfile::tempdir()?;
        let result_file = dir.path().join("result.md");
        std::fs::write(&result_file, "Report.")?;
        let mut store = task_world(
            dir.path(),
            Path::new("/unused"),
            Some(&result_file.to_string_lossy()),
        )?;
        store.conn().execute(
            "UPDATE block SET fg_pid = ?1, last_exit = ?2 WHERE lineage = 'b';",
            params![fg_pid, last_exit],
        )?;
        store
            .conn()
            .execute("UPDATE task SET worker_instance_id = ?1", params![ran_on])?;
        Follow::default().instance_started(&mut store, "i2")?;
        let (found, _, found_note) = round(&store)?;
        assert_eq!(found, state, "{fg_pid:?} {last_exit:?} {ran_on:?}");
        assert!(found_note.unwrap_or_default().contains(note), "{note}");
    }
    Ok(())
}

/// A new server instance also finishes the interactive rounds the earlier one's records show
/// ending, so a released worker whose turn ended just before the server stopped is retired, not
/// restored. A worker's record that cannot be read, here a directory, fails only its own round.
#[test]
fn a_new_instance_settles_each_interactive_round_on_its_own_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute(
        "INSERT INTO conversation (runtime, native_id, transcript, resumable, reported_at)
         VALUES ('claude', 'lost', ?1, 1, 0)",
        params![dir.path().to_string_lossy()],
    )?;
    // A record that cannot be read: a directory.
    turn_record(dir.path(), "lost", dir.path())?;
    store.conn().execute_batch(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           worker_runtime, worker_native_id, created_at)
           VALUES ('u', 'chat', 'claude', 'owner', 'c', 0, 'claude', 'lost', 0);
         INSERT INTO task_round (task_id, round, state, created_at) VALUES ('u', 1, 'running', 0);",
    )?;
    Follow::default().instance_started(&mut store, "i2")?;
    assert_eq!(
        round(&store)?,
        ("completed".into(), Some("Done.".into()), None)
    );
    let (state, note): (String, String) = store.conn().query_row(
        "SELECT state, note FROM task_round WHERE task_id = 'u'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(state, "failed");
    assert!(note.contains("could not be read"), "{note}");
    Ok(())
}
