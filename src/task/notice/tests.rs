use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::net::UnixListener;
use std::path::Path;

use serde_json::json;

use super::receipt::settle;
use super::*;
use crate::task::tests::{ANSWER, ANSWER_2, DURATION, PROMPT, PROMPT_2, task_world};

/// Task `t`'s completed first round and its notice `d` in `state`, to the owner idle in pane
/// `a`, whose record is `transcript` and whose messaging socket is the returned listener.
fn notice_world(dir: &Path, transcript: &Path, state: &str) -> Result<(Store, UnixListener)> {
    let socket = dir.join("owner.sock");
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let store = task_world(dir, &dir.join("worker.jsonl"), None)?;
    store.conn().execute(
        "UPDATE conversation SET transcript = ?1 WHERE native_id = 'owner'",
        params![transcript.to_string_lossy()],
    )?;
    store.conn().execute(
        "UPDATE occupant SET transport = json_object('messaging_socket', ?1) WHERE block_lineage = 'a'",
        params![socket.to_string_lossy()],
    )?;
    store.conn().execute(
        "UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = 'm1', finished_at = 0",
        [],
    )?;
    store.conn().execute(
        "INSERT INTO notice (id, task_id, round, state) VALUES ('d', 't', 1, ?1)",
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

/// The record line of Claude Code taking in notice `id` from another session.
fn receipt(id: &str) -> String {
    json!({ "type": "user", "origin": { "kind": "peer" }, "message": { "role": "user",
        "content": format!("Another Claude session sent a message:\n{} Worker task t finished.", runtime::token(id)) } })
    .to_string()
}

/// What the next daemon does with the notice on its start and through its first pass.
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

/// An owner's record that cannot be read, here a directory, fails only its own check and is not
/// watched, and its receipts are found once it can be read.
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
    assert!(!interest.files.contains(&record));
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
fn a_notice_goes_to_its_idle_owner_once_its_grace_after_the_round_has_passed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = notice_world(dir.path(), transcript.path(), "pending")?;
    // The round took the worker's first turn; a second finished after it.
    let worker = [PROMPT, ANSWER, DURATION, PROMPT_2, ANSWER_2, DURATION].join("\n") + "\n";
    std::fs::write(dir.path().join("worker.jsonl"), worker)?;
    store
        .conn()
        .execute_batch("UPDATE task_round SET turn_start = 0")?;
    store
        .conn()
        .execute("UPDATE task_round SET finished_at = ?1", params![now_ms()])?;
    let mut interest = Interest::default();
    assert!(send_due(&mut store, &mut interest)?.is_some());
    assert!(
        listener.accept().is_err(),
        "the notice was sent within its grace"
    );
    store.conn().execute(
        "UPDATE task_round SET finished_at = ?1",
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
    // A waiter that registered after the daemon saw a dead one, or none, also holds it.
    let tx = store.write()?;
    assert!(!claim(&tx, "d", None)?);
    assert!(!claim(&tx, "d", Some(alive + 1))?);
    assert!(claim(&tx, "d", Some(alive))?);
    Ok(())
}
