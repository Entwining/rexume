use super::*;

/// A conversation shown in two panes reopens in one, and the other keeps its shell; a round its
/// worker left open there goes on from the conversation's record, while one in a pane that had no
/// conversation ends.
#[test]
fn a_conversation_shown_in_two_panes_is_reopened_in_one() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, "")?;
    conversation(&store, "a", "codex", &transcript)?;
    // Pane b shows the same conversation, as after `codex resume` in a second pane.
    store.conn().execute_batch(
        "INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at)
           VALUES ('b', 'codex', 'a-conversation', 20, 0, 0);
         INSERT INTO block (lineage, instance_id, fg_pid)
           VALUES ('a', 'i1', 10), ('b', 'i1', 20), ('c', 'i1', 30);
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0);
         INSERT INTO epoch (instance_id, started_at) VALUES ('i2', 10);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           worker_instance_id, created_at)
           VALUES ('on-b', 'x', 'claude', 'owner', 'b', 0, 'i1', 0),
                  ('on-c', 'x', 'claude', 'owner', 'c', 0, 'i1', 0);
         INSERT INTO task_round (task_id, round, state, created_at)
           VALUES ('on-b', 1, 'running', 0), ('on-c', 1, 'running', 0);",
    )?;
    let a = plan(&mut store, "i2", "a", Some("s"))?.expect("planned");
    assert_eq!(plan(&mut store, "i2", "b", Some("s"))?, None);
    assert_eq!(plan(&mut store, "i2", "c", Some("s"))?, None);
    let rounds: Vec<(String, String)> = store
        .conn()
        .prepare("SELECT task_id, state FROM task_round ORDER BY task_id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        rounds,
        [
            ("on-b".to_owned(), "running".to_owned()),
            ("on-c".to_owned(), "terminated".to_owned())
        ]
    );
    let planned: Vec<(String, String, Option<String>)> = store
        .conn()
        .prepare("SELECT id, block_lineage, session_label FROM restore")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(planned, [(a, "a".into(), Some("s".into()))]);
    // Planning again on the same instance plans nothing new.
    assert_eq!(plan(&mut store, "i2", "a", Some("s"))?, None);
    Ok(())
}

/// A turn the server's end cut off continues in the pane it ran in, even when another pane that
/// showed the conversation idle was planned first.
#[test]
fn the_pane_whose_turn_was_cut_off_reopens_a_conversation_two_panes_showed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    conversation(&store, "b", "claude", &transcript)?;
    store.conn().execute_batch(
        "INSERT INTO occupant (block_lineage, runtime, native_id, pid, pid_name, keeps_session_env,
           reported_at) VALUES ('a', 'claude', 'b-conversation', 20, 'claude', 0, 0);
         INSERT INTO block (lineage, instance_id, fg_pid, status, status_pid, status_name)
           VALUES ('a', 'i1', 20, 'idle', 20, 'claude'), ('b', 'i1', 10, 'working', 10, 'claude');",
    )?;
    ended(&store, "server_gone")?;
    let id = plan(&mut store, "i2", "a", Some("s"))?.expect("planned");
    assert_eq!(plan(&mut store, "i2", "b", Some("s"))?, Some(id.clone()));
    let planned: Vec<(String, String, String)> = store
        .conn()
        .prepare("SELECT id, block_lineage, start FROM restore")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(planned, [(id, "b".into(), "continue".into())]);
    Ok(())
}

/// A pane that left the conversation to another pane's restore keeps its rounds only while that
/// restore may still start the conversation: once it is skipped, they end as its own do.
#[test]
fn a_round_left_to_another_panes_restore_ends_when_that_restore_is_skipped() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, "")?;
    conversation(&store, "a", "codex", &transcript)?;
    store.conn().execute_batch(
        "INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at)
           VALUES ('b', 'codex', 'a-conversation', 20, 0, 0);
         INSERT INTO block (lineage, instance_id, fg_pid) VALUES ('a', 'i1', 10), ('b', 'i1', 20);
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0);
         INSERT INTO epoch (instance_id, started_at) VALUES ('i2', 10);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           worker_instance_id, created_at)
           VALUES ('on-b', 'x', 'claude', 'owner', 'b', 0, 'i1', 0);
         INSERT INTO task_round (task_id, round, state, created_at) VALUES ('on-b', 1, 'running', 0);",
    )?;
    let id = plan(&mut store, "i2", "a", Some("s"))?.expect("planned");
    assert_eq!(plan(&mut store, "i2", "b", Some("s"))?, None);
    let restore = crate::restore::Open {
        id,
        block: "a".into(),
        typed_at: None,
        created_at: 0,
    };
    crate::restore::skip(&mut store, "i2", &restore, "the pane closed")?;
    let state: String = store
        .conn()
        .query_row("SELECT state FROM task_round", [], |row| row.get(0))?;
    assert_eq!(state, "terminated");
    Ok(())
}
