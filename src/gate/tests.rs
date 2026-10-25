use super::*;

const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"Write a story"}}"#;
const ANSWER: &str = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Done."}]}}"#;

/// A store with one restore of session `s` on instance `i2` whose slot 0 continues pane `b`.
fn planned(dir: &Path, transcript: &Path) -> Result<Store> {
    let mut store = Store::open(dir)?;
    let plan = Plan {
        argv: vec!["claude".into(), "--resume".into(), "c".into()],
        prompt: Some("go on".into()),
        transcript: Some(transcript.to_string_lossy().into_owned()),
        ..Plan::default()
    };
    let tx = store.write()?;
    tx.execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'c', 1, 0);
         INSERT INTO restore_op (id, instance_id, session_lineage, state, doc, created_at)
           VALUES ('op', 'i2', 's', 'sent', '{}', 100);",
    )?;
    tx.execute(
        "INSERT INTO restore_slot (op_id, slot, instance_id, block_lineage, start, runtime,
           native_id, plan) VALUES ('op', 0, 'i2', 'b', 'continue', 'claude', 'c', ?1)",
        params![serde_json::to_string(&plan)?],
    )?;
    tx.commit()?;
    Ok(store)
}

fn started(store: &Store) -> Result<(Option<String>, bool)> {
    Ok(store.conn().query_row(
        "SELECT started, claimed_at IS NOT NULL FROM restore_slot WHERE op_id = 'op'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

#[test]
fn a_claim_continues_only_a_turn_still_open_and_binds_the_pane_and_session() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let mut store = planned(dir.path(), &transcript)?;
    let (start, _) = claim(&mut store, "op", 0, "block:new", "session:new")?;
    assert_eq!(start, Start::Continue);
    assert_eq!(started(&store)?, (Some("continue".into()), true));
    let tx = store.write()?;
    assert_eq!(crate::store::ensure_lineage(&tx, "i2", "block:new")?, "b");
    assert_eq!(crate::store::ensure_lineage(&tx, "i2", "session:new")?, "s");
    drop(tx);
    let again = claim(&mut store, "op", 0, "block:new", "session:new");
    assert!(again.unwrap_err().to_string().contains("already claimed"));
    Ok(())
}

#[test]
fn a_claim_takes_over_a_pane_and_session_the_daemon_recorded_first() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let mut store = planned(dir.path(), &transcript)?;
    let doc = |block: &str| {
        format!(
            r#"{{"label":"s","active":0,"windows":[{{"lineage":"w","label":null,"focused":"{block}",
                "zoomed":null,"tiled":{{"block":{{"lineage":"{block}","label":null}}}},"floating":[]}}]}}"#
        )
    };
    // A create whose reply was lost: the daemon saw the new session and pane before the gate.
    store.conn().execute_batch(&format!(
        "INSERT INTO block (lineage, instance_id, cwd) VALUES ('b', 'i1', '/old');
         INSERT INTO session (lineage, instance_id, position, doc, updated_at)
           VALUES ('s', 'i1', 0, '{}', 1);
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'block:new', 'block:new'),
           ('i2', 'session:new', 'session:new');
         INSERT INTO block (lineage, instance_id, fg_pid) VALUES ('block:new', 'i2', 7);
         INSERT INTO session (lineage, instance_id, position, doc, updated_at)
           VALUES ('session:new', 'i2', 0, '{}', 2);",
        doc("b"),
        doc("block:new")
    ))?;
    let (start, _) = claim(&mut store, "op", 0, "block:new", "session:new")?;
    assert_eq!(start, Start::Continue);
    let conn = store.conn();
    let lineages: Vec<String> = conn
        .prepare("SELECT lineage FROM live ORDER BY live_id")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(lineages, ["b", "s"]);
    let block: (String, Option<i64>) = conn.query_row(
        "SELECT instance_id, fg_pid FROM block WHERE lineage = 'b'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(block, ("i2".into(), Some(7)));
    let sessions: Vec<(String, String, String)> = conn
        .prepare("SELECT lineage, instance_id, doc FROM session")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let [(lineage, instance, saved)] = sessions.as_slice() else {
        panic!("one saved layout expected, found {sessions:?}");
    };
    assert_eq!((lineage.as_str(), instance.as_str()), ("s", "i2"));
    let saved: crate::layout::SessionDoc = serde_json::from_str(saved)?;
    assert_eq!(saved.blocks(), ["b"]);
    assert_eq!(saved.windows[0].focused.as_deref(), Some("b"));
    Ok(())
}

#[test]
fn a_turn_that_ended_after_the_plan_reopens_without_the_prompt() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n{ANSWER}\n"))?;
    let mut store = planned(dir.path(), &transcript)?;
    let (start, _) = claim(&mut store, "op", 0, "block:new", "session:new")?;
    assert_eq!(start, Start::Agent);
    assert_eq!(started(&store)?, (Some("agent".into()), true));
    Ok(())
}

#[test]
fn a_conversation_reported_elsewhere_since_the_plan_starts_a_shell() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let mut store = planned(dir.path(), &transcript)?;
    // Reported in the same millisecond as the plan: counted as later.
    store.conn().execute(
        "INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env,
           reported_at) VALUES ('other', 'claude', 'c', 1, 0, 100)",
        [],
    )?;
    let claimed = claim(&mut store, "op", 0, "block:new", "session:new");
    assert!(claimed.unwrap_err().to_string().contains("another pane"));
    assert_eq!(started(&store)?, (Some("shell".into()), true));
    Ok(())
}

#[test]
fn a_claim_keeps_a_pane_record_it_has_nothing_to_replace_with() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let mut store = planned(dir.path(), &transcript)?;
    // The daemon saved the new session's layout, binding the pane, before any of its events.
    store.conn().execute_batch(
        "INSERT INTO block (lineage, instance_id, cwd) VALUES ('b', 'i1', '/old');
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'block:new', 'block:new');",
    )?;
    claim(&mut store, "op", 0, "block:new", "session:new")?;
    let cwd: String =
        store
            .conn()
            .query_row("SELECT cwd FROM block WHERE lineage = 'b'", [], |row| {
                row.get(0)
            })?;
    assert_eq!(cwd, "/old");
    Ok(())
}

#[test]
fn a_claim_refuses_a_pane_bound_to_another_restored_lineage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let mut store = planned(dir.path(), &transcript)?;
    store.conn().execute(
        "INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'block:new', 'other')",
        [],
    )?;
    let claimed = claim(&mut store, "op", 0, "block:new", "session:new");
    assert!(claimed.unwrap_err().to_string().contains("bound to other"));
    assert_eq!(started(&store)?, (None, false));
    Ok(())
}
