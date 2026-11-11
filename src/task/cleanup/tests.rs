use std::io::Write as _;

use super::*;
use crate::runtime::claude;
use crate::task::tests::{ANSWER, PROMPT, record, task_world, turn_record};

fn released_worker(dir: &Path, transcript: &Path) -> Result<Store> {
    let store = task_world(dir, transcript, None)?;
    let position = i64::try_from(std::fs::metadata(transcript)?.len())?;
    store.conn().execute_batch(
        "UPDATE block SET status = 'idle', status_at = 90 WHERE lineage = 'b';
         UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = 'm1', finished_at = 100,
           finished_instance_id = 'i1', finished_status_at = 90, finished_fg_changes = 0, turn_start = 0,
           acked_at = 120;",
    )?;
    store.conn().execute(
        "UPDATE task SET position = ?1, released_at = 120",
        params![position],
    )?;
    Ok(store)
}

fn released_run(dir: &Path) -> Result<Store> {
    let store = task_world(dir, &dir.join("unused.jsonl"), Some("/r"))?;
    store.conn().execute_batch(
        "UPDATE block SET fg_pid = NULL, last_exit = 0 WHERE lineage = 'b';
         UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = '/r', finished_at = 100,
           finished_instance_id = 'i1', finished_fg_changes = 0, acked_at = 120;
         UPDATE task SET released_at = 120;",
    )?;
    Ok(store)
}

#[test]
fn a_pane_whose_run_exited_is_retired_until_it_is_restored() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = released_run(dir.path())?;
    store
        .conn()
        .execute_batch("UPDATE block SET fg_changes = 1 WHERE lineage = 'b'")?;
    assert_eq!(
        retired_workers(store.conn(), store.dir())?,
        [("t".into(), "b".into())]
    );
    store.conn().execute_batch(
        "UPDATE block SET instance_id = 'i2', fg_pid = 300, last_exit = NULL WHERE lineage = 'b'",
    )?;
    assert!(retired_workers(store.conn(), store.dir())?.is_empty());
    store
        .conn()
        .execute_batch("UPDATE block SET fg_pid = NULL, last_exit = 0 WHERE lineage = 'b'")?;
    assert!(retired_workers(store.conn(), store.dir())?.is_empty());
    store
        .conn()
        .execute_batch("UPDATE task_round SET finished_instance_id = NULL")?;
    assert!(retired_workers(store.conn(), store.dir())?.is_empty());
    Ok(())
}

#[test]
fn a_restored_pane_the_restore_did_not_hold_is_kept() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = released_run(dir.path())?;
    store.conn().execute_batch(
        "INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'b', 'b');
         UPDATE block SET instance_id = 'i2', fg_pid = 300, last_exit = NULL WHERE lineage = 'b';",
    )?;
    let mut rex = Rex::Cli(crate::rex::Cli {
        bin: dir.path().join("rex"),
        endpoint: String::new(),
    });
    crate::task::Follow::default().pass(&mut store, Some((&mut rex, "i2")))?;
    let pane: (String, Option<String>) =
        store
            .conn()
            .query_row("SELECT pane_state, pane_note FROM task", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
    assert_eq!(
        pane,
        (
            "kept".into(),
            Some("the worker's pane was restored after its result".into())
        )
    );
    Ok(())
}

#[test]
fn an_untouched_released_worker_is_retired_unless_its_record_cannot_be_read() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let store = released_worker(dir.path(), transcript.path())?;
    assert_eq!(
        retired_workers(store.conn(), store.dir())?,
        [("t".into(), "b".into())]
    );
    std::fs::remove_file(claude::record(dir.path(), "worker"))?;
    turn_record(dir.path(), "worker", dir.path())?;
    assert!(retired_workers(store.conn(), store.dir())?.is_empty());
    store
        .conn()
        .execute_batch("UPDATE task SET position = -1")?;
    assert!(retired_workers(store.conn(), store.dir()).is_err());
    Ok(())
}

#[test]
fn a_dropped_round_does_not_keep_an_untouched_worker() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let store = released_worker(dir.path(), transcript.path())?;
    store.conn().execute_batch(
        "INSERT INTO task_round (task_id, round, state, finished_at, finished_instance_id,
           finished_status_at, finished_fg_changes, acked_at, created_at)
         VALUES ('t', 2, 'dropped', 60, 'i1', 50, 3, 60, 40)",
    )?;
    assert_eq!(
        retired_workers(store.conn(), store.dir())?,
        [("t".into(), "b".into())]
    );
    Ok(())
}

#[test]
fn a_worker_reported_on_another_instance_than_its_pane_is_not_retired() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let store = released_worker(dir.path(), transcript.path())?;
    store
        .conn()
        .execute_batch("UPDATE occupant SET instance_id = 'i0' WHERE block_lineage = 'b'")?;
    assert!(retired_workers(store.conn(), store.dir())?.is_empty());
    Ok(())
}

#[test]
fn an_acknowledged_worker_is_neither_closed_nor_retired_until_released() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let store = released_worker(dir.path(), transcript.path())?;
    store
        .conn()
        .execute_batch("UPDATE task SET released_at = NULL")?;
    assert!(due(store.conn())?.is_empty());
    assert!(retired_workers(store.conn(), store.dir())?.is_empty());
    assert_eq!(store.conn().execute(&CLAIM, params!["t"])?, 0);
    store
        .conn()
        .execute_batch("UPDATE task SET released_at = 130")?;
    assert_eq!(due(store.conn())?, ["t"]);
    assert_eq!(store.conn().execute(&CLAIM, params!["t"])?, 1);
    Ok(())
}

#[test]
fn later_activity_in_the_worker_pane_keeps_it() -> Result<()> {
    for change in [
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at) VALUES ('claude', 'new', 1, 300);
         UPDATE occupant SET native_id = 'new', pid = 200, reported_at = 300 WHERE block_lineage = 'b';",
        "UPDATE block SET status = 'working', status_at = 200 WHERE lineage = 'b'",
        "UPDATE block SET fg_pid = 300, fg_changes = fg_changes + 1 WHERE lineage = 'b'",
        "UPDATE block SET instance_id = 'i2' WHERE lineage = 'b'",
        "UPDATE task SET pane_state = 'kept', pane_note = 'the worker''s status changed after its result'",
        "INSERT INTO task_round (task_id, round, state, created_at) VALUES ('t', 2, 'running', 130)",
    ] {
        let dir = tempfile::tempdir()?;
        let transcript = record(&[PROMPT, ANSWER])?;
        let store = released_worker(dir.path(), transcript.path())?;
        store.conn().execute_batch(change)?;
        assert!(retired_workers(store.conn(), store.dir())?.is_empty(), "{change}");
    }
    Ok(())
}

#[test]
fn only_the_status_that_ends_the_turn_leaves_the_worker_retired() -> Result<()> {
    for (status, at, retired) in [
        ("done", 95, true),
        ("working", 90, false),
        ("blocked", 90, false),
        ("idle", 95, false),
    ] {
        let dir = tempfile::tempdir()?;
        let transcript = record(&[PROMPT, ANSWER])?;
        let store = released_worker(dir.path(), transcript.path())?;
        store.conn().execute(
            "UPDATE block SET status = ?1, status_at = ?2 WHERE lineage = 'b'",
            params![status, at],
        )?;
        assert_eq!(
            !retired_workers(store.conn(), store.dir())?.is_empty(),
            retired,
            "{status}"
        );
    }
    Ok(())
}

#[test]
fn a_new_turn_in_the_same_worker_process_is_restored() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT, ANSWER])?;
    let store = released_worker(dir.path(), transcript.path())?;
    writeln!(transcript, "{PROMPT}")?;
    transcript.flush()?;
    assert!(retired_workers(store.conn(), store.dir())?.is_empty());
    Ok(())
}

#[test]
fn a_pane_with_an_unfinished_round_is_neither_closed_nor_left_out_of_a_restore() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let mut store = released_worker(dir.path(), transcript.path())?;
    store.conn().execute_batch(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane, created_at)
           VALUES ('u', 'next', 'claude', 'owner', 'b', 0, 130);
         INSERT INTO task_round (task_id, round, state, created_at) VALUES ('u', 1, 'running', 130);",
    )?;
    let tx = store.write()?;
    assert!(retired_workers(&tx, dir.path())?.is_empty());
    assert_eq!(tx.execute(&CLAIM, params!["t"])?, 0);
    tx.execute(
        "UPDATE task_round SET state = 'cancelled', finished_at = 140 WHERE task_id = 'u'",
        [],
    )?;
    assert_eq!(tx.execute(&CLAIM, params!["t"])?, 1);
    Ok(())
}
