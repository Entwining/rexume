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

/// Pane `b` as recorded on instance `i1`, with the foreground and exit given, and a Claude Code
/// conversation that reported from it as process 10 as `occupant` (its instance and whether it
/// was the terminal's own program) with a prompt still open in its record. `run` registered a
/// non-interactive run in the pane on the process given, if any, and `elsewhere` reported the
/// conversation from pane `c` at that time, where it was still the foreground program or not.
struct Pane<'a> {
    fg_pid: Option<i64>,
    last_exit: Option<i64>,
    occupant: Option<(Option<&'a str>, Option<bool>)>,
    status: &'a str,
    run: Option<Option<i64>>,
    elsewhere: Option<(i64, bool)>,
}

fn planned(pane: &Pane) -> Result<(Start, bool, Option<i64>)> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(
        &transcript,
        r#"{"type":"user","message":{"role":"user","content":"Go"}}"#.to_owned() + "\n",
    )?;
    let store = Store::open(dir.path())?;
    let conn = store.conn();
    conn.execute(
        "INSERT INTO block (lineage, instance_id, fg_pid, last_exit, status, status_pid, status_name)
         VALUES ('b', 'i1', ?1, ?2, ?3, 10, 'claude')",
        params![pane.fg_pid, pane.last_exit, pane.status],
    )?;
    if let Some((instance, own_program)) = pane.occupant {
        conversation(&store, "b", "claude", &transcript)?;
        conn.execute(
            "UPDATE occupant SET instance_id = ?1, own_program = ?2",
            params![instance, own_program],
        )?;
    }
    if let Some(pid) = pane.run {
        conn.execute(
            "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage,
               worker_pid, worker_instance_id, result_file, keep_pane, created_at)
             VALUES ('t', 'x', 'claude', 'b-conversation', 'b', ?1, 'i1', '/r', 0, 0)",
            params![pid],
        )?;
    }
    if let Some((reported_at, running)) = pane.elsewhere {
        conn.execute(
            "INSERT INTO occupant (block_lineage, runtime, native_id, instance_id, pid,
               keeps_session_env, reported_at) VALUES ('c', 'claude', 'b-conversation', 'i1', 20, 0, ?1)",
            params![reported_at],
        )?;
        conn.execute(
            "INSERT INTO block (lineage, instance_id, fg_pid) VALUES ('c', 'i1', ?1)",
            params![running.then_some(20)],
        )?;
    }
    let candidate = candidate(conn, "i2", "b", true)?;
    Ok((
        candidate.start,
        candidate.plan.own_program,
        candidate.plan.exit_code,
    ))
}

#[test]
fn a_pane_restores_as_it_was_last_used() -> Result<()> {
    let pane = |fg_pid, last_exit, occupant| Pane {
        fg_pid,
        last_exit,
        occupant,
        status: "idle",
        run: None,
        elsewhere: None,
    };
    let here = Some("i1");
    // The conversation was the terminal's own program and exited: it reopens, again as the
    // terminal's own program. One reported before version 3 may have been; it reopens from a shell.
    assert_eq!(
        planned(&pane(None, Some(0), Some((here, Some(true)))))?,
        (Start::Agent, true, None)
    );
    assert_eq!(
        planned(&pane(None, Some(0), Some((None, None))))?,
        (Start::Agent, false, None)
    );
    // The conversation ran in another pane after it exited here, so that pane reopens it.
    let elsewhere = |reported_at, running| Pane {
        elsewhere: Some((reported_at, running)),
        ..pane(None, Some(0), Some((here, Some(true))))
    };
    assert_eq!(
        planned(&elsewhere(1, false))?,
        (Start::Exited, true, Some(0))
    );
    assert_eq!(
        planned(&elsewhere(-1, true))?,
        (Start::Exited, true, Some(0))
    );
    assert_eq!(planned(&elsewhere(-1, false))?, (Start::Agent, true, None));
    // Another program was the terminal's own, such as the shell an agent ran from, or the record
    // is of an earlier terminal, as after a restore whose agent failed to start: the exit shows.
    assert_eq!(
        planned(&pane(None, Some(2), None))?,
        (Start::Exited, true, Some(2))
    );
    assert_eq!(
        planned(&pane(None, Some(-1), Some((here, Some(false)))))?,
        (Start::Exited, true, Some(-1))
    );
    assert_eq!(
        planned(&pane(None, Some(127), Some((Some("i0"), Some(true)))))?,
        (Start::Exited, true, Some(127))
    );
    // A conversation still in the foreground reopens as it ran; one that left gives a shell.
    assert_eq!(
        planned(&pane(Some(10), None, Some((here, Some(true)))))?,
        (Start::Agent, true, None)
    );
    assert_eq!(
        planned(&pane(Some(10), None, Some((here, Some(false)))))?,
        (Start::Agent, false, None)
    );
    assert_eq!(
        planned(&pane(Some(30), None, Some((here, Some(false)))))?,
        (Start::Shell, false, None)
    );
    // A PID names a process only on its own instance; a report from before instances were
    // recorded counts as the pane's.
    assert_eq!(
        planned(&pane(Some(10), None, Some((Some("i0"), Some(false)))))?,
        (Start::Shell, false, None)
    );
    assert_eq!(
        planned(&pane(Some(10), None, Some((None, None))))?,
        (Start::Agent, false, None)
    );
    // A run cut off while working reopens its conversation without continuing it; a run the pane
    // held earlier is not this conversation.
    let working = |run| Pane {
        status: "working",
        run,
        ..pane(Some(10), None, Some((here, Some(true))))
    };
    assert_eq!(planned(&working(None))?.0, Start::Continue);
    // A copy started later in another pane that exited does not take it from the one running.
    let copied = Pane {
        elsewhere: Some((1, false)),
        ..working(None)
    };
    assert_eq!(planned(&copied)?.0, Start::Continue);
    assert_eq!(
        planned(&working(Some(Some(10))))?,
        (Start::Agent, true, None)
    );
    assert_eq!(planned(&working(Some(None)))?.0, Start::Agent);
    assert_eq!(planned(&working(Some(Some(9))))?.0, Start::Continue);
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
    assert_eq!(starts[1].0, "shell");
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
        store.conn().execute_batch(
            "INSERT INTO block (lineage, instance_id, fg_pid, status) VALUES ('b', 'i1', 10, 'idle')",
        )?;
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
