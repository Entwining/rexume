use super::*;

/// `notice_world` with round 1 blocked since long ago and `d` its blocked notice, the worker's pane
/// `b` known on the observed instance as `block:w`.
fn blocked_world(dir: &Path, transcript: &Path, state: &str) -> Result<(Store, UnixListener)> {
    let (store, listener) = notice_world(dir, transcript, state)?;
    store.conn().execute_batch(
        "UPDATE task_round SET state = 'blocked', blocked_at = 0, result = NULL, result_ref = NULL,
           finished_at = NULL;
         UPDATE notice SET kind = 'blocked';
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i1', 'block:w', 'b');",
    )?;
    Ok((store, listener))
}

#[test]
fn a_blocked_notice_waits_for_a_live_waiter_and_goes_once_otherwise() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = blocked_world(dir.path(), transcript.path(), "pending")?;
    store.conn().execute_batch(
        "INSERT INTO request (id, task_id, round, text) VALUES ('q', 't', 1, 'go')",
    )?;
    let alive = i64::from(std::process::id());
    store
        .conn()
        .execute("UPDATE task_round SET waiter_pid = ?1", params![alive])?;
    let mut interest = Interest::default();
    send_due(&mut store, &mut interest)?;
    assert!(interest.pids.contains(&alive));
    assert!(listener.accept().is_err(), "sent past a live waiter");

    store
        .conn()
        .execute_batch("UPDATE task_round SET waiter_pid = NULL")?;
    send_due(&mut store, &mut interest)?;
    send_due(&mut store, &mut interest)?;
    assert_eq!(notice(&store)?.0, "sent");
    let mut line = String::new();
    std::io::Read::read_to_string(&mut listener.accept()?.0, &mut line)?;
    assert!(listener.accept().is_err(), "the notice was sent twice");
    let message = serde_json::from_str::<Value>(&line)?;
    let sent = message["message"]["content"].as_str().unwrap_or_default();
    for part in [
        runtime::token("d").as_str(),
        "\"count\" is blocked in round 1",
        "in pane block:w",
        "may have been answered since",
        "`rexume task show t --round 1`",
        "request is still pending",
    ] {
        assert!(sent.contains(part), "{part}: {sent}");
    }
    assert!(!sent.contains("task ack"), "{sent}");
    Ok(())
}

#[test]
fn a_blocked_notice_left_mid_send_stays_unknown_and_unsent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = blocked_world(dir.path(), transcript.path(), "sending")?;
    assert_eq!(restart_daemon(&mut store)?.0, "unknown");
    assert!(listener.accept().is_err(), "the notice was sent again");
    Ok(())
}

/// A blocked notice whose send fails before anything leaves returns to `pending` if its episode
/// continues, or becomes `superseded` if the episode ended during the send.
#[test]
fn a_failed_send_keeps_a_blocked_notice_only_while_its_episode_goes_on() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, _listener) = blocked_world(dir.path(), transcript.path(), "pending")?;
    let failed = ("pending", Some("failed to connect".to_owned()));
    let tx = store.write()?;
    assert!(claim(&tx, "d", None)?);
    record_send(&tx, "d", failed.clone())?;
    tx.commit()?;
    assert_eq!(
        notice(&store)?,
        ("pending".to_owned(), Some("failed to connect".to_owned()))
    );
    let tx = store.write()?;
    assert!(claim(&tx, "d", None)?);
    crate::task::blocked::end_blocked_episodes(&tx, "b")?;
    record_send(&tx, "d", failed)?;
    tx.commit()?;
    assert_eq!(
        notice(&store)?,
        (
            "superseded".to_owned(),
            Some(crate::task::blocked::ENDED.to_owned())
        )
    );
    Ok(())
}

/// Once its episode ends, a blocked notice tells the owner nothing it can act on: it leaves the list
/// and stays `sent`, also while a later episode of the same round lasts.
#[test]
fn a_blocked_notice_is_listed_only_while_its_episode_lasts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, _listener) = blocked_world(dir.path(), transcript.path(), "sent")?;
    assert_eq!(undelivered(store.conn(), None)?.len(), 1);
    let tx = store.write()?;
    crate::task::blocked::end_blocked_episodes(&tx, "b")?;
    tx.commit()?;
    assert!(undelivered(store.conn(), None)?.is_empty());
    assert_eq!(notice(&store)?.0, "sent");
    let tx = store.write()?;
    tx.execute(crate::task::blocked::BEGIN, params![5, "t", 1])?;
    crate::task::notice::write(&tx, Kind::Blocked, "t", 1, 5)?;
    tx.execute("UPDATE notice SET state = 'sent' WHERE id != 'd'", [])?;
    tx.commit()?;
    let listed = undelivered(store.conn(), None)?;
    assert_eq!(listed.len(), 1);
    assert_ne!(listed[0]["id"], "d");
    Ok(())
}
