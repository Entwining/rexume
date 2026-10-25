use std::io::Write as _;
use std::path::Path;

use anyhow::Result;
use rusqlite::params;

use super::*;
use crate::task::tests::{ANSWER, DURATION, PROMPT, record, round, task_world};

/// When `PROMPT` was recorded, in Unix milliseconds.
const PROMPTED_AT: i64 = 1_792_855_785_738;

fn bound(store: &Store) -> Result<Option<String>> {
    Ok(store.conn().query_row(
        "SELECT worker_native_id FROM task WHERE id = 't'",
        [],
        |row| row.get(0),
    )?)
}

#[test]
fn a_turn_end_written_after_its_status_is_still_read() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    // The worker's idle status arrives while the record holds only the prompt.
    store.conn().execute(
        "UPDATE block SET status = 'idle', status_pid = 100, status_at = ?1 WHERE lineage = 'b'",
        params![now_ms()],
    )?;
    let mut follow = Follow::default();
    let interest = follow.pass(&mut store, None)?;
    assert!(interest.files.contains(transcript.path()));
    assert_eq!(round(&store)?.0, "running");
    assert!(follow.deadline().is_some());
    writeln!(transcript, "{ANSWER}\n{DURATION}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    assert_eq!(
        round(&store)?,
        ("completed".into(), Some("Done.".into()), None)
    );
    let notices: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM notice WHERE state = 'pending'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(notices, 1);
    Ok(())
}

#[test]
fn a_prompt_left_open_ends_when_the_worker_goes_idle_after_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    // A process reopening the conversation after a restart reports idle on its own start.
    store.conn().execute_batch(&format!(
        "UPDATE occupant SET pid = 200, reported_at = {} WHERE block_lineage = 'b';
         UPDATE block SET status = 'idle', status_pid = 200, status_at = {} WHERE lineage = 'b';",
        PROMPTED_AT + 4000,
        PROMPTED_AT + 4500,
    ))?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    // The process that accepted the prompt goes idle without answering.
    store.conn().execute_batch(&format!(
        "UPDATE occupant SET pid = 100, reported_at = 50 WHERE block_lineage = 'b';
         UPDATE block SET status_pid = 100, status_at = {} WHERE lineage = 'b';",
        PROMPTED_AT + 3500,
    ))?;
    follow.pass(&mut store, None)?;
    let (state, _, note) = round(&store)?;
    assert_eq!(state, "cancelled");
    assert_eq!(
        note.as_deref(),
        Some("the worker went idle without answering")
    );
    // The position passes the turn taken while open, so it is no later turn of the worker's.
    let position: i64 = store
        .conn()
        .query_row("SELECT position FROM task", [], |row| row.get(0))?;
    assert_eq!(
        u64::try_from(position)?,
        std::fs::metadata(transcript.path())?.len()
    );
    Ok(())
}

#[test]
fn a_bound_task_keeps_its_conversation_when_another_takes_over_the_pane() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT])?;
    let other = record(&[
        PROMPT,
        r#"{"type":"assistant","message":{"id":"m2","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Other."}]}}"#,
    ])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
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
    writeln!(transcript, "{ANSWER}\n{DURATION}")?;
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
    // An earlier conversation, not the pane's foreground, is the occupant at registration.
    store.conn().execute_batch(
        "UPDATE task SET worker_runtime = NULL, worker_native_id = NULL, created_at = 100;
         INSERT INTO conversation (runtime, native_id, resumable, reported_at) VALUES ('claude', 'old', 1, 40);
         UPDATE occupant SET native_id = 'old', pid = 90, reported_at = 40 WHERE block_lineage = 'b';",
    )?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    assert_eq!(bound(&store)?, None);
    store.conn().execute(
        "UPDATE occupant SET native_id = 'worker', pid = 100, reported_at = 150 WHERE block_lineage = 'b'",
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
