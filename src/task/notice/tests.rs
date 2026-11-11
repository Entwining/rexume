use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::net::UnixListener;
use std::path::Path;

use serde_json::json;

use super::receipt::settle;
use super::*;
use crate::runtime;
use crate::runtime::claude;
use crate::task::tests::{ANSWER, ANSWER_2, PROMPT, PROMPT_2, task_world, turn_record};

fn notice_world(dir: &Path, transcript: &Path, state: &str) -> Result<(Store, UnixListener)> {
    let socket = dir.join("owner.sock");
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let store = task_world(dir, &dir.join("worker.jsonl"), None)?;
    turn_record(dir, "owner", transcript)?;
    store.conn().execute(
        "UPDATE occupant SET transport = json_object('messaging_socket', ?1) WHERE block_lineage = 'a'",
        params![socket.to_string_lossy()],
    )?;
    store.conn().execute(
        "UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = 'm1', finished_at = 0",
        [],
    )?;
    store.conn().execute(
        "INSERT INTO notice (id, task_id, round, kind, state, created_at)
         VALUES ('d', 't', 1, 'finished', ?1, 0)",
        params![state],
    )?;
    Ok((store, listener))
}

fn notice(store: &Store) -> Result<(String, Option<String>)> {
    Ok(store
        .conn()
        .query_row("SELECT state, note FROM notice WHERE id = 'd'", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?)
}

fn receipt(id: &str) -> String {
    let prompt = json!({ "event": "prompt", "prompt_id": "p9", "token": runtime::token(id) });
    let stop = json!({ "event": "stop", "prompt_id": "p9", "id": "s9", "answer": "Noted." });
    format!("{prompt}\n{stop}")
}

fn restart_daemon(store: &mut Store) -> Result<(String, Option<String>)> {
    assert_eq!(orphaned_sends(store)?, [("notice", "d".to_owned())]);
    let mut interest = Interest::default();
    send_due(store, &mut interest)?;
    settle(store, &mut HashMap::new(), &mut interest)?;
    notice(store)
}

#[test]
fn a_notice_left_mid_send_settles_from_the_owners_receipt() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = tempfile::NamedTempFile::new()?;
    writeln!(transcript, "{}", receipt("d"))?;
    transcript.flush()?;
    let (mut store, listener) = notice_world(dir.path(), transcript.path(), "sending")?;
    assert_eq!(restart_daemon(&mut store)?.0, "delivered");
    assert!(listener.accept().is_err(), "the notice was sent again");
    Ok(())
}

#[test]
fn an_owner_record_that_cannot_be_read_is_checked_again() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let record = dir.path().join("owner.jsonl");
    std::fs::create_dir(&record)?;
    let (mut store, _listener) = notice_world(dir.path(), &record, "sent")?;
    let mut receipts = HashMap::new();
    let mut interest = Interest::default();
    settle(&mut store, &mut receipts, &mut interest)?;
    assert_eq!(notice(&store)?.0, "sent");
    assert!(
        !interest
            .files
            .contains(&claude::record(dir.path(), "owner"))
    );
    std::fs::remove_dir(&record)?;
    std::fs::write(&record, receipt("d") + "\n")?;
    settle(&mut store, &mut receipts, &mut Interest::default())?;
    assert_eq!(notice(&store)?.0, "delivered");
    Ok(())
}

#[test]
fn a_notice_left_mid_send_without_a_receipt_stays_unknown_and_unsent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = notice_world(dir.path(), transcript.path(), "sending")?;
    let (state, note) = restart_daemon(&mut store)?;
    assert_eq!(state, "unknown");
    assert!(note.is_some_and(|n| n.contains("daemon stopped")));
    assert!(listener.accept().is_err(), "the notice was sent again");
    Ok(())
}

#[test]
fn a_notice_waits_for_its_owner_to_report_on_the_running_instance() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = notice_world(dir.path(), transcript.path(), "pending")?;
    store
        .conn()
        .execute_batch("UPDATE occupant SET instance_id = 'i0' WHERE block_lineage = 'a'")?;
    send_due(&mut store, &mut Interest::default())?;
    assert_eq!(notice(&store)?.0, "pending");
    assert!(listener.accept().is_err(), "the notice was sent");
    Ok(())
}

#[test]
fn a_notice_goes_to_its_idle_owner_once_its_grace_has_passed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = notice_world(dir.path(), transcript.path(), "pending")?;
    let worker = [PROMPT, ANSWER, PROMPT_2, ANSWER_2].join("\n") + "\n";
    std::fs::write(dir.path().join("worker.jsonl"), worker)?;
    store
        .conn()
        .execute_batch("UPDATE task_round SET turn_start = 0")?;
    store
        .conn()
        .execute("UPDATE notice SET created_at = ?1", params![now_ms()])?;
    let mut interest = Interest::default();
    assert!(send_due(&mut store, &mut interest)?.is_some());
    assert!(
        listener.accept().is_err(),
        "the notice was sent within its grace"
    );
    store.conn().execute(
        "UPDATE notice SET created_at = ?1",
        params![now_ms() - GRACE_MS - 1],
    )?;
    assert!(send_due(&mut store, &mut interest)?.is_none());
    send_due(&mut store, &mut interest)?;
    assert_eq!(notice(&store)?.0, "sent");
    let mut sent = String::new();
    std::io::Read::read_to_string(&mut listener.accept()?.0, &mut sent)?;
    assert!(sent.contains("The worker has 1 unread turn"), "{sent}");
    assert!(listener.accept().is_err(), "the notice was sent twice");
    Ok(())
}

#[test]
fn a_finished_notice_names_no_unread_turns_while_a_later_round_is_open() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = notice_world(dir.path(), transcript.path(), "pending")?;
    let worker = [PROMPT, ANSWER, PROMPT_2, ANSWER_2].join("\n") + "\n";
    std::fs::write(dir.path().join("worker.jsonl"), worker)?;
    store.conn().execute(
        "UPDATE notice SET created_at = ?1",
        params![now_ms() - GRACE_MS - 1],
    )?;
    store.conn().execute_batch(
        "UPDATE task_round SET turn_start = 0;
         INSERT INTO task_round (task_id, round, state, created_at) VALUES ('t', 2, 'running', 1);",
    )?;
    send_due(&mut store, &mut Interest::default())?;
    let mut sent = String::new();
    std::io::Read::read_to_string(&mut listener.accept()?.0, &mut sent)?;
    assert!(sent.contains("finished round 1"), "{sent}");
    assert!(!sent.contains("unread"), "{sent}");
    Ok(())
}

#[test]
fn a_failed_send_stays_pending_and_is_listed_with_its_reason() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = notice_world(dir.path(), transcript.path(), "pending")?;
    assert!(undelivered(store.conn(), None)?.is_empty());
    drop(listener);
    std::fs::remove_file(dir.path().join("owner.sock"))?;
    send_due(&mut store, &mut Interest::default())?;
    let listed = undelivered(store.conn(), None)?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["state"], "pending");
    assert!(
        listed[0]["note"]
            .as_str()
            .is_some_and(|n| n.contains("failed to connect"))
    );
    Ok(())
}

#[test]
fn a_notice_records_each_attempt_and_when_it_was_delivered() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = notice_world(dir.path(), transcript.path(), "pending")?;
    let record = |store: &Store| -> Result<(String, i64, Option<i64>, Option<i64>)> {
        Ok(store.conn().query_row(
            "SELECT state, attempts, sent_at, delivered_at FROM notice WHERE id = 'd'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?)
    };
    drop(listener);
    let socket = dir.path().join("owner.sock");
    std::fs::remove_file(&socket)?;
    send_due(&mut store, &mut Interest::default())?;
    assert_eq!(record(&store)?, ("pending".into(), 1, None, None));
    let listener = UnixListener::bind(&socket)?;
    let before = now_ms();
    send_due(&mut store, &mut Interest::default())?;
    let (state, attempts, sent_at, delivered_at) = record(&store)?;
    assert_eq!((state.as_str(), attempts, delivered_at), ("sent", 2, None));
    assert!(sent_at.is_some_and(|at| at >= before), "{sent_at:?}");
    drop(listener);
    writeln!(transcript, "{}", receipt("d"))?;
    transcript.flush()?;
    settle(&mut store, &mut HashMap::new(), &mut Interest::default())?;
    let (state, attempts, kept, delivered_at) = record(&store)?;
    assert_eq!((state.as_str(), attempts, kept), ("delivered", 2, sent_at));
    assert!(delivered_at >= sent_at, "{delivered_at:?}");
    Ok(())
}

#[test]
fn a_notice_of_an_acknowledged_round_is_not_listed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, _listener) = notice_world(dir.path(), transcript.path(), "sent")?;
    let owner = Some((Runtime::Claude, "owner"));
    assert_eq!(undelivered(store.conn(), owner)?.len(), 1);
    let tx = store.write()?;
    crate::task::acknowledge::acknowledge(&tx, "t", 1, "taken by task ack")?;
    tx.commit()?;
    assert!(undelivered(store.conn(), owner)?.is_empty());
    assert!(undelivered(store.conn(), None)?.is_empty());
    assert_eq!(notice(&store)?.0, "sent");
    Ok(())
}

#[test]
fn a_failed_send_drops_a_notice_of_a_round_acknowledged_meanwhile() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, _listener) = notice_world(dir.path(), transcript.path(), "pending")?;
    let tx = store.write()?;
    assert!(claim(&tx, "d", None)?);
    crate::task::acknowledge::acknowledge(&tx, "t", 1, "taken by task ack")?;
    record_send(&tx, "d", ("pending", Some("failed to connect".to_owned())))?;
    tx.commit()?;
    assert_eq!(
        notice(&store)?,
        ("superseded".to_owned(), Some(ACKED.to_owned()))
    );
    let superseded_at: Option<i64> =
        store
            .conn()
            .query_row("SELECT superseded_at FROM notice", [], |row| row.get(0))?;
    assert!(superseded_at.is_some());
    Ok(())
}

#[test]
fn a_registered_waiter_keeps_the_notice_from_the_daemon() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = notice_world(dir.path(), transcript.path(), "pending")?;
    let alive = i64::from(std::process::id());
    store
        .conn()
        .execute("UPDATE task_round SET waiter_pid = ?1", params![alive])?;
    let mut interest = Interest::default();
    send_due(&mut store, &mut interest)?;
    assert!(interest.pids.contains(&alive));
    assert!(
        listener.accept().is_err(),
        "the notice was sent past a live waiter"
    );
    let tx = store.write()?;
    assert!(!claim(&tx, "d", None)?);
    assert!(!claim(&tx, "d", Some(alive + 1))?);
    assert!(claim(&tx, "d", Some(alive))?);
    Ok(())
}

mod blocked;
mod inbox;
mod pacing;
