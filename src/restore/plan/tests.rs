use std::path::Path;

use super::*;

fn conversation(store: &Store, block: &str, runtime: &str, transcript: &Path) -> Result<()> {
    store.conn().execute(
        "INSERT INTO conversation (runtime, native_id, transcript, resumable, reported_at)
         VALUES (?1, ?2, ?3, 1, 0)",
        params![
            runtime,
            format!("{block}-conversation"),
            transcript.to_string_lossy()
        ],
    )?;
    store.conn().execute(
        "INSERT INTO occupant (block_lineage, runtime, native_id, pid, pid_name, keeps_session_env,
           reported_at) VALUES (?1, ?2, ?3, 10, ?2, 0, 0)",
        params![block, runtime, format!("{block}-conversation")],
    )?;
    Ok(())
}

#[test]
fn only_a_turn_the_servers_end_cut_off_continues() -> Result<()> {
    const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"Write a story"}}"#;
    const ANSWER: &str = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Done."}]}}"#;
    const CANCEL: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
    let start =
        |records: &[&str], status: &str, status_pid: i64, server_gone: bool| -> Result<Start> {
            let dir = tempfile::tempdir()?;
            let transcript = dir.path().join("t.jsonl");
            std::fs::write(&transcript, records.join("\n") + "\n")?;
            let store = Store::open(dir.path())?;
            conversation(&store, "b", "claude", &transcript)?;
            store.conn().execute(
                "INSERT INTO block (lineage, instance_id, fg_pid, status, status_pid, status_name)
             VALUES ('b', 'i1', 10, ?1, ?2, 'claude')",
                params![status, status_pid],
            )?;
            Ok(candidate(store.conn(), "i2", "b", server_gone)?.start)
        };
    assert_eq!(start(&[PROMPT], "working", 10, true)?, Start::Continue);
    // The server may have stopped on purpose or not at all; without seeing it go, nothing reruns.
    assert_eq!(start(&[PROMPT], "working", 10, false)?, Start::Agent);
    assert_eq!(start(&[PROMPT], "blocked", 10, true)?, Start::Agent);
    assert_eq!(start(&[PROMPT, ANSWER], "idle", 10, true)?, Start::Agent);
    // The runtime recorded the cancel or the answer before its status reached the server.
    assert_eq!(start(&[PROMPT, CANCEL], "working", 10, true)?, Start::Agent);
    assert_eq!(start(&[PROMPT, ANSWER], "working", 10, true)?, Start::Agent);
    // A status from another process, such as a nested agent, says nothing about this turn.
    assert_eq!(start(&[PROMPT], "working", 11, true)?, Start::Agent);
    // A line that is not JSON may be what ended the turn.
    assert_eq!(start(&[PROMPT, "{cut"], "working", 10, true)?, Start::Agent);
    Ok(())
}

#[test]
fn a_conversation_without_a_record_yet_reopens_only_in_pi() -> Result<()> {
    let start = |runtime: &str| -> Result<Start> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path())?;
        conversation(&store, "b", runtime, &dir.path().join("unwritten"))?;
        store.conn().execute(
            "INSERT INTO block (lineage, instance_id, fg_pid, status, status_pid, status_name)
             VALUES ('b', 'i1', 10, 'idle', 10, ?1)",
            params![runtime],
        )?;
        Ok(candidate(store.conn(), "i2", "b", true)?.start)
    };
    // Claude Code and Codex write their record with the first prompt and cannot resume before.
    assert_eq!(start("claude")?, Start::Shell);
    assert_eq!(start("codex")?, Start::Shell);
    // Pi opens the missing session file as a new session with the reported settings.
    assert_eq!(start("pi")?, Start::Agent);
    Ok(())
}

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
         INSERT INTO block (lineage, instance_id, fg_pid) VALUES ('a', 'i1', 10), ('b', 'i1', 20);",
    )?;
    let doc = r#"{"label":"s","active":0,"windows":[{"lineage":"w","label":null,"focused":null,"zoomed":null,
        "tiled":{"split":{"direction":"horizontal","ratio":0.5,"before":{"block":{"lineage":"a","label":null}},
        "after":{"block":{"lineage":"b","label":null}}}},"floating":[]}]}"#;
    store.conn().execute(
        "INSERT INTO session (lineage, instance_id, position, doc, updated_at)
         VALUES ('s', 'i1', 0, ?1, 0)",
        params![doc],
    )?;
    let op = plan(&mut store, "i2", "s", &[])?.expect("planned");
    let starts: Vec<(String, String)> = store
        .conn()
        .prepare(
            "SELECT start, plan ->> '$.reason' FROM restore_slot WHERE op_id = ?1 ORDER BY slot",
        )?
        .query_map(params![op], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(starts[0].0, "agent");
    assert_eq!(starts[1].0, "typed");
    assert_eq!(starts[1].1, "another pane reopens this conversation");
    // Planning again on the same instance plans nothing new.
    assert!(plan(&mut store, "i2", "s", &[])?.is_none());
    Ok(())
}

#[test]
fn a_claude_conversation_reopens_in_the_mode_its_record_last_shows() -> Result<()> {
    let argv = |records: &[&str]| -> Result<Vec<String>> {
        let dir = tempfile::tempdir()?;
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, records.join("\n") + "\n")?;
        let store = Store::open(dir.path())?;
        conversation(&store, "b", "claude", &transcript)?;
        Ok(candidate(store.conn(), "i2", "b", true)?.plan.argv)
    };
    let resume = |mode: &[&str]| -> Vec<String> {
        ["claude", "--resume", "b-conversation"]
            .iter()
            .chain(mode)
            .map(|s| (*s).to_owned())
            .collect()
    };
    // Claude Code's own resume would start this conversation in a new session's mode.
    assert_eq!(
        argv(&[
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s"}"#,
            r#"{"type":"user","permissionMode":"bypassPermissions","message":{"role":"user","content":"Go"}}"#,
        ])?,
        resume(&["--permission-mode", "bypassPermissions"])
    );
    // Switched to Manual after the last prompt; its resume would take a settings `defaultMode`.
    assert_eq!(
        argv(&[
            r#"{"type":"user","permissionMode":"auto","message":{"role":"user","content":"Go"}}"#,
            r#"{"type":"permission-mode","permissionMode":"default","sessionId":"s"}"#,
        ])?,
        resume(&["--permission-mode", "default"])
    );
    // A subagent's records in the same file, as Claude Code 2.1.85 wrote them, are not the
    // conversation's own.
    assert_eq!(
        argv(&[
            r#"{"type":"user","permissionMode":"default","message":{"role":"user","content":"Go"}}"#,
            r#"{"type":"user","isSidechain":true,"permissionMode":"acceptEdits","message":{"role":"user","content":"Look"}}"#,
        ])?,
        resume(&["--permission-mode", "default"])
    );
    // A record that shows no mode leaves it to Claude Code's resume.
    assert_eq!(
        argv(&[r#"{"type":"user","message":{"role":"user","content":"Go"}}"#])?,
        resume(&[])
    );
    Ok(())
}
