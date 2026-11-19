use super::*;

const PROMPT_3: &str = r#"{"event":"prompt","prompt_id":"p3","token":null}"#;
const ANSWER_3: &str = r#"{"event":"stop","prompt_id":"p3","id":"m3","answer":"More."}"#;

fn unread_end(store: &Store, id: &str) -> Result<Option<i64>> {
    Ok(store.conn().query_row(
        "SELECT unread_end FROM notice WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?)
}

fn worker_end(dir: &Path) -> Result<i64> {
    Ok(i64::try_from(
        std::fs::metadata(dir.join("worker.jsonl"))?.len(),
    )?)
}

/// The owner reported a status after the last send, and a pending unread notice is due.
fn unread_due(store: &Store, id: &str) -> Result<()> {
    store.conn().execute(
        "UPDATE block SET status_at = ?1 WHERE lineage = 'a'",
        params![now_ms() + 1],
    )?;
    store.conn().execute(
        "INSERT INTO notice (id, task_id, kind, state, created_at)
         VALUES (?1, 't', 'unread', 'pending', 0)",
        params![id],
    )?;
    Ok(())
}

#[test]
fn an_unread_notice_goes_only_for_turns_no_notice_reported() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = tempfile::NamedTempFile::new()?;
    let (mut store, listener) = notice_world(dir.path(), transcript.path(), "pending")?;
    let worker = [PROMPT, ANSWER, PROMPT_2, ANSWER_2].join("\n") + "\n";
    std::fs::write(dir.path().join("worker.jsonl"), worker)?;
    store
        .conn()
        .execute_batch("UPDATE task_round SET turn_start = 0")?;
    send_due(&mut store, &mut Interest::default())?;
    assert!(read_notice(&listener)?.contains("The worker has 1 unread turn"));
    assert_eq!(unread_end(&store, "d")?, Some(worker_end(dir.path())?));

    unread_due(&store, "u1")?;
    send_due(&mut store, &mut Interest::default())?;
    let state: (String, Option<String>) = store.conn().query_row(
        "SELECT state, note FROM notice WHERE id = 'u1'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(state, ("superseded".into(), Some(REPORTED.into())));
    assert!(listener.accept().is_err(), "sent turns already reported");

    let mut more = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join("worker.jsonl"))?;
    writeln!(more, "{PROMPT_3}\n{ANSWER_3}")?;
    unread_due(&store, "u2")?;
    send_due(&mut store, &mut Interest::default())?;
    let sent = read_notice(&listener)?;
    for part in [
        runtime::token("u2").as_str(),
        "has 2 unread turns, finished while no round was open",
        "`rexume task next t` takes them in order",
    ] {
        assert!(sent.contains(part), "{part}: {sent}");
    }
    assert_eq!(unread_end(&store, "u2")?, Some(worker_end(dir.path())?));
    Ok(())
}
