use std::io::Write as _;

use super::*;
use crate::task::tests::{ANSWER, DURATION, PROMPT, record, task_world};

/// Task `t`'s disposable worker in pane `b`, its round acknowledged and recorded exactly as its
/// result left it, and the task released.
fn released_worker(dir: &Path, transcript: &Path) -> Result<Store> {
    let store = task_world(dir, transcript, None)?;
    let position = i64::try_from(std::fs::metadata(transcript)?.len())?;
    store.conn().execute_batch(
        "UPDATE block SET status = 'idle', status_at = 90 WHERE lineage = 'b';
         UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = 'm1', finished_at = 100,
           finished_instance_id = 'i1', finished_status_at = 90, finished_generation = 5, turn_start = 0,
           acked_at = 120;",
    )?;
    store.conn().execute(
        "UPDATE task SET position = ?1, released_at = 120",
        params![position],
    )?;
    Ok(store)
}

/// Task `t`'s non-interactive run in pane `b`, which exited 0 with its result; the round is
/// acknowledged and the task released.
fn released_run(dir: &Path) -> Result<Store> {
    let store = task_world(dir, &dir.join("unused.jsonl"), Some("/r"))?;
    store.conn().execute_batch(
        "UPDATE block SET fg_pid = NULL, last_exit = 0 WHERE lineage = 'b';
         UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = '/r', finished_at = 100,
           finished_instance_id = 'i1', finished_generation = 5, acked_at = 120;
         UPDATE task SET released_at = 120;",
    )?;
    Ok(store)
}

/// The server can report the run's pane without a foreground after the exit, once the round has
/// its result; that moves the generation but runs nothing. A pane restored on a later instance
/// is a new terminal and may be anyone's, also once its own program exits.
#[test]
fn a_pane_whose_run_exited_is_retired_until_it_is_restored() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = released_run(dir.path())?;
    store
        .conn()
        .execute_batch("UPDATE block SET fg_generation = 6 WHERE lineage = 'b'")?;
    assert_eq!(retired_workers(store.conn())?, ["b"]);
    store.conn().execute_batch(
        "UPDATE block SET instance_id = 'i2', fg_pid = 300, last_exit = NULL WHERE lineage = 'b'",
    )?;
    assert!(retired_workers(store.conn())?.is_empty());
    store
        .conn()
        .execute_batch("UPDATE block SET fg_pid = NULL, last_exit = 0 WHERE lineage = 'b'")?;
    assert!(retired_workers(store.conn())?.is_empty());
    // A round from before version 3 that the upgrade could not date.
    store
        .conn()
        .execute_batch("UPDATE task_round SET finished_instance_id = NULL")?;
    assert!(retired_workers(store.conn())?.is_empty());
    Ok(())
}

/// A restore on the current instance brings the pane back as a new terminal, which the pane's
/// record describes only once the server reports it. A restore that can no longer bind the pane
/// leaves it gone.
#[test]
fn a_pane_being_restored_is_not_the_recorded_one() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = released_run(dir.path())?;
    let conn = store.conn();
    conn.execute_batch(
        "INSERT INTO restore_op (id, instance_id, session_lineage, state, doc, created_at)
           VALUES ('o', 'i2', 's', 'sent', '{}', 130);
         INSERT INTO restore_slot (op_id, slot, instance_id, block_lineage, start, plan)
           VALUES ('o', 0, 'i2', 'b', 'shell', '{}');",
    )?;
    assert_eq!(live(conn, "i2", "b")?, Live::Restoring);
    conn.execute_batch("UPDATE restore_op SET state = 'failed'")?;
    assert_eq!(live(conn, "i2", "b")?, Live::Gone);
    conn.execute_batch(
        "INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'block:new', 'b')",
    )?;
    assert_eq!(live(conn, "i2", "b")?, Live::Restoring);
    conn.execute_batch("UPDATE block SET instance_id = 'i2' WHERE lineage = 'b'")?;
    assert_eq!(live(conn, "i2", "b")?, Live::Block("block:new".into()));
    Ok(())
}

/// A close waits for a restore that may still bring the pane back, here through a floating
/// layer whose reply was lost, without claiming the pane; the pass after the gate's late claim
/// looks again.
#[test]
fn a_close_waits_for_the_restore_and_looks_again_after_the_claim() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = released_run(dir.path())?;
    store.conn().execute_batch(
        "INSERT INTO restore_op (id, instance_id, session_lineage, state, doc, created_at)
           VALUES ('o', 'i2', 's', 'partial', '{}', 130);
         INSERT INTO restore_slot (op_id, slot, instance_id, block_lineage, start, plan, layer_sent_at)
           VALUES ('o', 0, 'i2', 'b', 'shell', '{}', 140);",
    )?;
    let mut rex = Rex::Cli(crate::rex::Cli {
        bin: dir.path().join("rex"),
        endpoint: String::new(),
    });
    let mut follow = crate::task::Follow::default();
    let pane = |store: &Store| -> Result<(String, Option<String>)> {
        Ok(store
            .conn()
            .query_row("SELECT pane_state, pane_note FROM task", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?)
    };
    follow.pass(&mut store, Some((&mut rex, "i2")))?;
    assert_eq!(pane(&store)?, ("open".into(), None));
    store.conn().execute_batch(
        "INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'block:new', 'b');
         UPDATE block SET instance_id = 'i2', fg_pid = 300, last_exit = NULL WHERE lineage = 'b';",
    )?;
    follow.pass(&mut store, Some((&mut rex, "i2")))?;
    assert_eq!(
        pane(&store)?,
        (
            "kept".into(),
            Some("the worker's pane was restored after its result".into())
        )
    );
    Ok(())
}

/// An untouched released worker is not restored. One whose record cannot be read, here a
/// directory, may have a turn after its result, so it is, without holding back the restore.
#[test]
fn an_untouched_released_worker_is_retired_unless_its_record_cannot_be_read() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, DURATION])?;
    let store = released_worker(dir.path(), transcript.path())?;
    assert_eq!(retired_workers(store.conn())?, ["b"]);
    store.conn().execute(
        "UPDATE conversation SET transcript = ?1 WHERE native_id = 'worker'",
        params![dir.path().to_string_lossy()],
    )?;
    assert!(retired_workers(store.conn())?.is_empty());
    // Any other error ends the restore's planning instead of deciding it.
    store
        .conn()
        .execute_batch("UPDATE task SET position = -1")?;
    assert!(retired_workers(store.conn()).is_err());
    Ok(())
}

/// A conversation reported in the pane on an earlier server instance is not what the pane runs
/// now, as when the pane came back as a shell and the worker never reported again.
#[test]
fn a_worker_reported_on_another_instance_than_its_pane_is_not_retired() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, DURATION])?;
    let store = released_worker(dir.path(), transcript.path())?;
    store
        .conn()
        .execute_batch("UPDATE occupant SET instance_id = 'i0' WHERE block_lineage = 'b'")?;
    assert!(retired_workers(store.conn())?.is_empty());
    Ok(())
}

/// Acknowledging a result leaves the worker to the orchestrator, which may still start a round;
/// only releasing it gives up the pane.
#[test]
fn an_acknowledged_worker_is_neither_closed_nor_retired_until_released() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, DURATION])?;
    let store = released_worker(dir.path(), transcript.path())?;
    store
        .conn()
        .execute_batch("UPDATE task SET released_at = NULL")?;
    assert!(due(store.conn())?.is_empty());
    assert!(retired_workers(store.conn())?.is_empty());
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
        // A new conversation in the pane.
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at) VALUES ('claude', 'new', 1, 300);
         UPDATE occupant SET native_id = 'new', pid = 200, reported_at = 300 WHERE block_lineage = 'b';",
        "UPDATE block SET status = 'working', status_at = 200 WHERE lineage = 'b'",
        "UPDATE block SET fg_pid = 300, fg_generation = 6 WHERE lineage = 'b'",
        // The pane restored on a later instance, whose status and generation happen to match.
        "UPDATE block SET instance_id = 'i2' WHERE lineage = 'b'",
        // A pane an earlier close attempt found in use, whatever it shows now.
        "UPDATE task SET pane_state = 'kept', pane_note = 'the worker''s status changed after its result'",
        // Another round started and not yet acknowledged.
        "INSERT INTO task_round (task_id, round, state, created_at) VALUES ('t', 2, 'running', 130)",
    ] {
        let dir = tempfile::tempdir()?;
        let transcript = record(&[PROMPT, ANSWER, DURATION])?;
        let store = released_worker(dir.path(), transcript.path())?;
        store.conn().execute_batch(change)?;
        assert!(retired_workers(store.conn())?.is_empty(), "{change}");
    }
    Ok(())
}

#[test]
fn a_new_turn_in_the_same_worker_process_is_restored() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT, ANSWER, DURATION])?;
    let store = released_worker(dir.path(), transcript.path())?;
    // The prompt is in the record before the status event that would also show it.
    writeln!(transcript, "{PROMPT}")?;
    transcript.flush()?;
    assert!(retired_workers(store.conn())?.is_empty());
    Ok(())
}

#[test]
fn a_closed_worker_stays_closed_when_the_close_event_never_arrived() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, DURATION])?;
    let mut store = released_worker(dir.path(), transcript.path())?;
    let doc = r#"{"label":"s","active":0,"windows":[{"lineage":"w","label":null,"focused":null,"zoomed":null,
        "tiled":{"split":{"direction":"horizontal","ratio":0.5,"before":{"block":{"lineage":"a","label":null}},
        "after":{"block":{"lineage":"b","label":null}}}},"floating":[]}]}"#;
    store.conn().execute(
        "INSERT INTO session (lineage, instance_id, position, doc, updated_at) VALUES ('s', 'i1', 0, ?1, 0)",
        params![doc],
    )?;
    let tx = store.write()?;
    block_gone(&tx, "b")?;
    tx.commit()?;
    let pane_state: String = store
        .conn()
        .query_row("SELECT pane_state FROM task", [], |row| row.get(0))?;
    assert_eq!(pane_state, "closed");
    // The server and the daemon end before the close event; the next instance plans the restore.
    let retired = retired_workers(store.conn())?;
    let op = crate::restore::plan(&mut store, "i2", "s", &retired)?.expect("pane a is restored");
    let slots: Vec<String> = store
        .conn()
        .prepare("SELECT block_lineage FROM restore_slot WHERE op_id = ?1")?
        .query_map(params![op], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(slots, ["a"]);
    Ok(())
}

#[test]
fn a_pane_with_an_unfinished_round_is_neither_closed_nor_left_out_of_a_restore() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, DURATION])?;
    let mut store = released_worker(dir.path(), transcript.path())?;
    store.conn().execute_batch(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane, created_at)
           VALUES ('u', 'next', 'claude', 'owner', 'b', 0, 130);
         INSERT INTO task_round (task_id, round, state, created_at) VALUES ('u', 1, 'running', 130);",
    )?;
    let tx = store.write()?;
    assert!(retired_workers(&tx)?.is_empty());
    assert_eq!(tx.execute(&CLAIM, params!["t"])?, 0);
    tx.execute(
        "UPDATE task_round SET state = 'cancelled', finished_at = 140 WHERE task_id = 'u'",
        [],
    )?;
    assert_eq!(tx.execute(&CLAIM, params!["t"])?, 1);
    Ok(())
}
