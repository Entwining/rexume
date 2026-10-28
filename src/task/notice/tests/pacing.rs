use super::*;

/// `notice_world` with notice `e` of task `u`'s finished first round, due to the same owner before
/// `d` though written after it.
fn two_notices(dir: &Path, transcript: &Path) -> Result<(Store, UnixListener)> {
    let (store, listener) = notice_world(dir, transcript, "pending")?;
    store.conn().execute_batch(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at) VALUES ('u', 'other', 'claude', 'owner', 'b', 0, 0);
         INSERT INTO task_round (task_id, round, state, finished_at, created_at, result, result_ref)
           VALUES ('u', 1, 'completed', 0, 0, 'Done.', 'm1');
         UPDATE notice SET created_at = 1 WHERE id = 'd';
         INSERT INTO notice (id, task_id, round, kind, state, created_at)
           VALUES ('e', 'u', 1, 'finished', 'pending', 0);",
    )?;
    Ok((store, listener))
}

fn state_of(store: &Store, id: &str) -> Result<String> {
    Ok(store.conn().query_row(
        "SELECT state FROM notice WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?)
}

/// An idle owner gets its notices one at a time in due order: the next waits while the owner's
/// pane has reported nothing since the last was sent, and goes once it reports a status after the
/// send, whether or not the last one arrived, as one the runtime dropped never does. A pane whose
/// status was cleared, or never reported, holds nothing.
#[test]
fn an_owners_next_notice_waits_for_a_status_after_the_last_send() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = two_notices(dir.path(), transcript.path())?;
    store.conn().execute_batch(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at) VALUES ('v', 'third', 'claude', 'owner', 'b', 0, 0);
         INSERT INTO task_round (task_id, round, state, finished_at, created_at, result, result_ref)
           VALUES ('v', 1, 'completed', 0, 0, 'Done.', 'm1');
         INSERT INTO notice (id, task_id, round, kind, state, created_at)
           VALUES ('f', 'v', 1, 'finished', 'pending', 2);",
    )?;
    let mut interest = Interest::default();
    send_due(&mut store, &mut interest)?;
    send_due(&mut store, &mut interest)?;
    assert_eq!(
        (state_of(&store, "e")?, state_of(&store, "d")?),
        ("sent".into(), "pending".into())
    );
    listener.accept()?;
    assert!(listener.accept().is_err(), "both notices were sent at once");
    // The owner went through a turn and is idle again, and its record never showed the notice.
    // Both lie in the past, so that the next send comes after the status.
    store.conn().execute_batch(
        "UPDATE notice SET sent_at = sent_at - 10 WHERE id = 'e';
         UPDATE block SET status_at = (SELECT sent_at + 1 FROM notice WHERE id = 'e')
         WHERE lineage = 'a'",
    )?;
    send_due(&mut store, &mut interest)?;
    assert_eq!(
        (state_of(&store, "d")?, state_of(&store, "f")?),
        ("sent".into(), "pending".into())
    );
    listener.accept()?;
    // The pane's status was cleared, as when its program's status record is removed.
    store.conn().execute_batch(
        "UPDATE block SET status = NULL, status_pid = NULL, status_at = NULL WHERE lineage = 'a'",
    )?;
    send_due(&mut store, &mut interest)?;
    assert_eq!(state_of(&store, "f")?, "sent");
    listener.accept()?;
    Ok(())
}
