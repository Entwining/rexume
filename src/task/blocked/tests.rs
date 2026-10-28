use std::io::Write as _;

use super::*;
use crate::store::Store;
use crate::task::follow::Follow;
use crate::task::gone::block_gone;
use crate::task::tests::{ANSWER, DURATION, PROMPT, record, round, task_world};

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

fn blocked_at(store: &Store) -> Result<Option<i64>> {
    Ok(store
        .conn()
        .query_row("SELECT blocked_at FROM task_round", [], |row| row.get(0))?)
}

/// When task `t`'s notice of `kind` was written.
fn written_at(store: &Store, kind: &str) -> Result<Option<i64>> {
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

    report(&store, "blocked", long_ago())?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "blocked");
    assert!(blocked_at(&store)?.is_some());
    assert_eq!(written_at(&store, "blocked")?, blocked_at(&store)?);
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
    assert_eq!(blocked_at(&store)?, None);
    assert_eq!(notices(&store)?, [ended()]);

    report(&store, "blocked", long_ago())?;
    follow.pass(&mut store, None)?;
    assert_eq!(notices(&store)?, [ended(), pending("blocked")]);
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
        } else {
            assert_eq!(round(&store)?.0, "running");
            assert_eq!(notices(&store)?, [ended()]);
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

/// A worker whose user dismissed the dialog goes idle with its turn still open. The record may
/// still end that turn within the idle grace, but the block is over at once, so its notice is not
/// sent meanwhile.
#[test]
fn a_worker_idle_after_its_dialog_ends_the_episode_within_the_idle_grace() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    report(&store, "blocked", long_ago())?;
    follow.pass(&mut store, None)?;
    report(&store, "idle", now_ms())?;
    follow.pass(&mut store, None)?;
    assert_eq!(round(&store)?.0, "running");
    assert!(follow.deadline().is_some());
    assert_eq!(notices(&store)?, [ended()]);
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
            writeln!(transcript, "{ANSWER}\n{DURATION}")?;
            transcript.flush()?;
        }
        follow.pass(&mut store, None)?;
        let state = if closes { "worker_closed" } else { "completed" };
        assert_eq!(round(&store)?.0, state);
        assert_eq!(blocked_at(&store)?, None);
        let finished_at =
            store
                .conn()
                .query_row("SELECT finished_at FROM task_round", [], |row| row.get(0))?;
        assert_eq!(written_at(&store, "finished")?, finished_at);
        assert_eq!(notices(&store)?, [ended(), pending("finished")], "{state}");
    }
    Ok(())
}
