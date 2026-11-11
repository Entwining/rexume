use std::io::Write as _;

use rusqlite::OptionalExtension as _;

use super::*;
use crate::store::Store;
use crate::task::follow::Follow;
use crate::task::gone::block_gone;
use crate::task::tests::{ANSWER, PROMPT, record, round, task_world};

/// The worker's pane `b` reports `status`, last at `at`.
fn report(store: &Store, status: &str, at: i64) -> Result<()> {
    store.conn().execute(
        "UPDATE block SET status = ?1, status_at = ?2 WHERE lineage = 'b'",
        params![status, at],
    )?;
    Ok(())
}

/// A report old enough that its grace has passed.
fn long_ago() -> i64 {
    now_ms() - GRACE_MS - 1
}

/// Task `t`'s notices in the order they were written, with kind, state, and note.
fn notices(store: &Store) -> Result<Vec<(String, String, Option<String>)>> {
    Ok(store
        .conn()
        .prepare("SELECT kind, state, note FROM notice WHERE task_id = 't' ORDER BY rowid")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn pending(kind: &str) -> (String, String, Option<String>) {
    (kind.into(), "pending".into(), None)
}

fn ended() -> (String, String, Option<String>) {
    (
        "blocked".into(),
        "superseded".into(),
        Some("the block ended before it was sent".into()),
    )
}

/// When the round's open episode began.
fn open_since(store: &Store) -> Result<Option<i64>> {
    Ok(store
        .conn()
        .query_row(
            "SELECT started_at FROM blocked_episode WHERE ended_at IS NULL",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

/// Why each of the round's episodes ended, in the order they began.
fn ends(store: &Store) -> Result<Vec<Option<String>>> {
    Ok(store
        .conn()
        .prepare("SELECT end_reason FROM blocked_episode ORDER BY started_at, rowid")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn ended_by(reason: &str) -> Option<String> {
    Some(reason.into())
}

/// Whether every superseded notice records when it was superseded.
fn stamped(store: &Store) -> Result<bool> {
    Ok(store.conn().query_row(
        "SELECT NOT EXISTS (SELECT 1 FROM notice WHERE state = 'superseded' AND superseded_at IS NULL)",
        [],
        |row| row.get(0),
    )?)
}

/// When task `t`'s notice of `kind` was written.
fn written_at(store: &Store, kind: &str) -> Result<i64> {
    Ok(store.conn().query_row(
        "SELECT created_at FROM notice WHERE kind = ?1",
        params![kind],
        |row| row.get(0),
    )?)
}

#[test]
fn a_round_blocks_once_its_pane_has_reported_blocked_for_the_grace() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    report(&store, "blocked", now_ms() - GRACE_MS / 2)?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    assert!(follow.deadline().is_some());
    // A dialog answered within the grace makes no episode, and the next one starts the grace again.
    report(&store, "working", now_ms())?;
    follow.pass(&mut store, None)?;
    report(&store, "blocked", now_ms())?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    assert!(notices(&store)?.is_empty());

    let since = long_ago();
    report(&store, "blocked", since)?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "blocked");
    assert_eq!(open_since(&store)?, Some(written_at(&store, "blocked")?));
    let reported_at: i64 =
        store
            .conn()
            .query_row("SELECT status_at FROM blocked_episode", [], |row| {
                row.get(0)
            })?;
    assert_eq!(reported_at, since);
    follow.pass(&mut store, None)?;
    assert_eq!(notices(&store)?, [pending("blocked")]);
    Ok(())
}

#[test]
fn an_episode_that_ends_drops_only_an_unsent_notice_and_the_next_tells_again() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    report(&store, "blocked", long_ago())?;
    follow.pass(&mut store, None)?;
    report(&store, "working", now_ms())?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    assert_eq!(open_since(&store)?, None);
    assert_eq!(ends(&store)?, [ended_by("status_changed")]);
    assert_eq!(notices(&store)?, [ended()]);
    assert!(stamped(&store)?);

    report(&store, "blocked", long_ago())?;
    follow.pass(&mut store, None)?;
    assert_eq!(notices(&store)?, [ended(), pending("blocked")]);
    let another =
        "INSERT INTO blocked_episode (id, task_id, round, started_at) VALUES ('x', 't', 1, 0)";
    assert!(
        store.conn().execute(another, []).is_err(),
        "two open episodes"
    );
    store
        .conn()
        .execute_batch("UPDATE notice SET state = 'sent' WHERE state = 'pending'")?;
    report(&store, "working", now_ms())?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    assert_eq!(
        notices(&store)?,
        [ended(), ("blocked".into(), "sent".into(), None)]
    );
    assert_eq!(
        ends(&store)?,
        [ended_by("status_changed"), ended_by("status_changed")]
    );
    Ok(())
}

/// A status from another process in the pane, such as a program started there after the worker
/// exited or another conversation that took the pane over, is that program's, so it neither begins
/// an episode nor keeps the worker's.
#[test]
fn a_block_another_process_in_the_pane_reports_is_not_the_worker_s() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    let other = "UPDATE block SET status_pid = 300 WHERE lineage = 'b'";
    report(&store, "blocked", long_ago())?;
    store.conn().execute_batch(other)?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    assert!(notices(&store)?.is_empty());
    store
        .conn()
        .execute_batch("UPDATE block SET status_pid = 100 WHERE lineage = 'b'")?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "blocked");
    store.conn().execute_batch(other)?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    assert_eq!(notices(&store)?, [ended()]);
    assert_eq!(ends(&store)?, [ended_by("occupant_changed")]);
    assert!(stamped(&store)?);
    store.conn().execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'other', 1, 300);
         UPDATE occupant SET native_id = 'other', pid = 300, reported_at = 300
           WHERE block_lineage = 'b';",
    )?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    assert_eq!(notices(&store)?, [ended()]);
    Ok(())
}

/// The worker's process and its dialog go with the server, so a pane last recorded on an earlier
/// instance neither keeps an episode nor begins one, while a daemon started again on the same
/// instance keeps it.
#[test]
fn an_episode_does_not_outlive_its_server_instance() -> Result<()> {
    for (instance, kept) in [("i1", true), ("i2", false)] {
        let dir = tempfile::tempdir()?;
        let transcript = record(&[PROMPT])?;
        let mut store = task_world(dir.path(), transcript.path(), None)?;
        let mut follow = Follow::default();
        report(&store, "blocked", long_ago())?;
        follow.pass(&mut store, None)?;
        store.conn().execute(
            "INSERT INTO epoch (id, instance_id, started_at) VALUES (2, ?1, ?2)",
            params![instance, now_ms()],
        )?;
        follow.pass(&mut store, None)?;
        if kept {
            assert_eq!(round(&store)?.0, "blocked");
            assert_eq!(notices(&store)?, [pending("blocked")]);
            assert_eq!(ends(&store)?, [None]);
        } else {
            assert_eq!(round(&store)?.0, "running");
            assert_eq!(notices(&store)?, [ended()]);
            assert_eq!(ends(&store)?, [ended_by("server_gone")]);
            assert!(stamped(&store)?);
            follow.pass(&mut store, None)?;
            assert_eq!(
                round(&store)?.0,
                "running",
                "began from an earlier instance"
            );
        }
    }
    Ok(())
}

/// A round that finishes while blocked, by its answer or by its pane closing, leaves its finished
/// notice as the only one to send.
#[test]
fn a_round_that_finishes_while_blocked_ends_its_episode() -> Result<()> {
    for closes in [false, true] {
        let dir = tempfile::tempdir()?;
        let mut transcript = record(&[PROMPT])?;
        let mut store = task_world(dir.path(), transcript.path(), None)?;
        let mut follow = Follow::default();
        report(&store, "blocked", long_ago())?;
        follow.pass(&mut store, None)?;
        if closes {
            let tx = store.write()?;
            block_gone(&tx, "b")?;
            tx.commit()?;
        } else {
            writeln!(transcript, "{ANSWER}")?;
            transcript.flush()?;
        }
        follow.pass(&mut store, None)?;
        let state = if closes { "worker_closed" } else { "completed" };
        assert_eq!(round(&store)?.0, state);
        assert_eq!(ends(&store)?, [ended_by("round_finished")], "{state}");
        let (finished_at, ended_at): (i64, i64) = store.conn().query_row(
            "SELECT finished_at, ended_at FROM task_round JOIN blocked_episode USING (task_id, round)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(written_at(&store, "finished")?, finished_at);
        assert_eq!(ended_at, finished_at);
        assert_eq!(notices(&store)?, [ended(), pending("finished")], "{state}");
        assert!(stamped(&store)?);
    }
    Ok(())
}
