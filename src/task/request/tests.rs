use std::io::Write as _;
use std::path::Path;

use super::*;
use crate::task::follow::Follow;
use crate::task::gone::block_gone;
use crate::task::notice::orphaned_sends;
use crate::task::register::{Kind, start_round};
use crate::task::tests::{record, task_world};

/// Task `t` with a Pi worker `w` in pane `b` whose first round has finished, and a request round
/// asking it to "go". Returns the request's ID.
fn request_world(dir: &Path, transcript: &Path) -> Result<(Store, String)> {
    let mut store = task_world(dir, transcript, None)?;
    store.conn().execute(
        "INSERT INTO conversation (runtime, native_id, transcript, resumable, reported_at)
         VALUES ('pi', 'w', ?1, 1, 50)",
        params![transcript.to_string_lossy()],
    )?;
    store.conn().execute_batch(
        "UPDATE occupant SET runtime = 'pi', native_id = 'w' WHERE block_lineage = 'b';
         UPDATE task SET worker_runtime = 'pi', worker_native_id = 'w';
         UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = 'a0', finished_at = 1;",
    )?;
    let tx = store.write()?;
    start_round(&tx, "t", &Kind::Request("go"))?;
    tx.commit()?;
    let id = store
        .conn()
        .query_row("SELECT id FROM request", [], |row| row.get(0))?;
    Ok((store, id))
}

fn request(store: &Store) -> Result<(String, Option<String>)> {
    Ok(store
        .conn()
        .query_row("SELECT state, note FROM request", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?)
}

#[test]
fn a_request_is_sent_once_its_worker_is_idle_in_its_pane_and_settled_by_its_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[])?;
    let (mut store, id) = request_world(dir.path(), transcript.path())?;
    let inbox = pi::inbox(store.dir(), "w");
    let mut interest = Interest::default();
    send_due(&mut store, &mut interest)?;
    assert_eq!(
        request(&store)?,
        (
            "pending".into(),
            Some("waiting for the worker to end its turn".into())
        )
    );
    // A pass that finds the same reason writes nothing, so it does not wake the daemon again.
    let hook = rusqlite::Connection::open(dir.path().join("state.db"))?;
    hook.execute_batch("BEGIN IMMEDIATE")?;
    send_due(&mut store, &mut interest)?;
    hook.execute_batch("ROLLBACK")?;
    store
        .conn()
        .execute_batch("UPDATE block SET status = 'idle', fg_pid = 300 WHERE lineage = 'b'")?;
    send_due(&mut store, &mut interest)?;
    assert_eq!(
        request(&store)?.1.as_deref(),
        Some("waiting for the worker's conversation to run in its pane")
    );
    assert!(!inbox.exists());
    // A PID names a process only on the server instance it was reported on.
    store.conn().execute_batch(
        "UPDATE block SET fg_pid = 100 WHERE lineage = 'b';
         UPDATE occupant SET instance_id = 'i0' WHERE block_lineage = 'b';",
    )?;
    send_due(&mut store, &mut interest)?;
    assert!(!inbox.exists());
    store
        .conn()
        .execute_batch("UPDATE occupant SET instance_id = 'i1' WHERE block_lineage = 'b'")?;
    send_due(&mut store, &mut interest)?;
    assert_eq!(request(&store)?, ("sent".into(), None));
    assert!(interest.files.contains(&inbox));
    let sent: Value = serde_json::from_slice(&std::fs::read(inbox.join(format!("{id}.json")))?)?;
    assert_eq!(sent["text"], format!("{} go", runtime::token(&id)));
    // The extension takes the message from the inbox.
    std::fs::rename(
        inbox.join(format!("{id}.json")),
        inbox.join(format!("{id}.accepted")),
    )?;
    send_due(&mut store, &mut interest)?;
    assert_eq!(request(&store)?.0, "accepted");
    writeln!(
        transcript,
        r#"{{"type":"custom_message","id":"n1","customType":"rexume","content":"{} go","display":true}}"#,
        runtime::token(&id)
    )?;
    writeln!(
        transcript,
        r#"{{"type":"message","id":"a1","message":{{"role":"assistant","stopReason":"stop","content":[{{"type":"text","text":"Here."}}]}}}}"#
    )?;
    transcript.flush()?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(request(&store)?.0, "delivered");
    let result: Option<String> =
        store
            .conn()
            .query_row("SELECT result FROM task_round WHERE round = 2", [], |row| {
                row.get(0)
            })?;
    assert_eq!(result.as_deref(), Some("Here."));
    Ok(())
}

#[test]
fn a_request_left_mid_send_becomes_unknown_and_is_not_sent_again() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[])?;
    let (mut store, id) = request_world(dir.path(), transcript.path())?;
    store.conn().execute_batch(
        "UPDATE request SET state = 'sending';
         UPDATE block SET status = 'idle' WHERE lineage = 'b';",
    )?;
    assert_eq!(orphaned_sends(&mut store)?, [("request", id.clone())]);
    send_due(&mut store, &mut Interest::default())?;
    let (state, note) = request(&store)?;
    assert_eq!(state, "unknown");
    assert!(note.is_some_and(|n| n.contains("daemon stopped")));
    assert!(!pi::inbox(store.dir(), "w").exists());
    Ok(())
}

#[test]
fn a_request_whose_round_ended_is_never_sent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[])?;
    let (mut store, id) = request_world(dir.path(), transcript.path())?;
    let tx = store.write()?;
    block_gone(&tx, "b")?;
    tx.commit()?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(
        request(&store)?,
        (
            "superseded".into(),
            Some("the round ended before it was sent".into())
        )
    );
    let tx = store.write()?;
    assert_eq!(tx.execute(&CLAIM, params![id])?, 0);
    Ok(())
}
