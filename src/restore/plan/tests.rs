use std::path::Path;

use super::*;
use crate::task::tests::turn_record;

const PROMPT: &str = r#"{"event":"prompt","prompt_id":"p1","token":null}"#;

fn conversation(store: &Store, block: &str, runtime: &str, transcript: &Path) -> Result<()> {
    if runtime == "claude" {
        turn_record(store.dir(), &format!("{block}-conversation"), transcript)?;
    }
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

fn ended(store: &Store, reason: &str) -> Result<()> {
    store.conn().execute(
        "INSERT INTO epoch (instance_id, started_at, ended_at, end_reason) VALUES ('i1', 0, 1, ?1)",
        params![reason],
    )?;
    Ok(())
}

fn end_reason(server_gone: bool) -> &'static str {
    if server_gone {
        "server_gone"
    } else {
        "disconnected"
    }
}

#[test]
fn only_a_turn_the_servers_end_cut_off_continues() -> Result<()> {
    const ANSWER: &str = r#"{"event":"stop","prompt_id":"p1","id":"s1","answer":"Done."}"#;
    const FAILURE: &str = r#"{"event":"failure","prompt_id":"p1","text":"API Error"}"#;
    let start_from =
        |transcript: &Path, status: &str, status_pid: i64, server_gone: bool| -> Result<Start> {
            let store = Store::open(transcript.parent().unwrap())?;
            conversation(&store, "b", "claude", transcript)?;
            store.conn().execute(
                "INSERT INTO block (lineage, instance_id, fg_pid, status, status_pid, status_name)
             VALUES ('b', 'i1', 10, ?1, ?2, 'claude')",
                params![status, status_pid],
            )?;
            ended(&store, end_reason(server_gone))?;
            let planned = candidate(store.conn(), store.dir(), "i2", "b")?;
            if planned.start == Start::Continue {
                assert_eq!(
                    planned.plan.record_length,
                    Some(std::fs::metadata(transcript)?.len())
                );
            }
            Ok(planned.start)
        };
    let start =
        |records: &[&str], status: &str, status_pid: i64, server_gone: bool| -> Result<Start> {
            let dir = tempfile::tempdir()?;
            let transcript = dir.path().join("t.jsonl");
            std::fs::write(&transcript, records.join("\n") + "\n")?;
            start_from(&transcript, status, status_pid, server_gone)
        };
    assert_eq!(start(&[PROMPT], "working", 10, true)?, Start::Continue);
    assert_eq!(start(&[PROMPT], "working", 10, false)?, Start::Agent);
    assert_eq!(start(&[PROMPT], "blocked", 10, true)?, Start::Agent);
    assert_eq!(start(&[PROMPT, ANSWER], "idle", 10, true)?, Start::Agent);
    assert_eq!(
        start(&[PROMPT, FAILURE], "working", 10, true)?,
        Start::Agent
    );
    assert_eq!(
        start(&[PROMPT, ANSWER], "working", 10, true)?,
        Start::Continue
    );
    assert_eq!(start(&[PROMPT], "working", 11, true)?, Start::Agent);
    assert_eq!(start(&[PROMPT, "{cut"], "working", 10, true)?, Start::Agent);
    let dir = tempfile::tempdir()?;
    let unreadable = dir.path().join("t.jsonl");
    std::fs::create_dir(&unreadable)?;
    assert_eq!(start_from(&unreadable, "working", 10, true)?, Start::Agent);
    Ok(())
}

#[test]
fn a_codex_answer_wins_over_a_working_status() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(
        &transcript,
        concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"u1","last_agent_message":"Done."}}"#,
            "\n"
        ),
    )?;
    let store = Store::open(dir.path())?;
    conversation(&store, "b", "codex", &transcript)?;
    store.conn().execute(
        "INSERT INTO block (lineage, instance_id, fg_pid, status, status_pid, status_name)
         VALUES ('b', 'i1', 10, 'working', 10, 'codex')",
        [],
    )?;
    ended(&store, "server_gone")?;
    assert_eq!(
        candidate(store.conn(), store.dir(), "i2", "b")?.start,
        Start::Agent
    );
    Ok(())
}

struct Pane<'a> {
    fg_pid: Option<i64>,
    last_exit: Option<i64>,
    occupant: Option<(Option<&'a str>, Option<bool>)>,
    status: &'a str,
    run: Option<Option<i64>>,
    elsewhere: Option<(i64, bool)>,
}

fn planned(pane: &Pane) -> Result<Start> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let store = Store::open(dir.path())?;
    ended(&store, "server_gone")?;
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
    Ok(candidate(conn, store.dir(), "i2", "b")?.start)
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
    assert_eq!(
        planned(&pane(None, Some(0), Some((here, Some(true)))))?,
        Start::Agent
    );
    assert_eq!(
        planned(&pane(None, Some(0), Some((None, None))))?,
        Start::Agent
    );
    let elsewhere = |reported_at, running| Pane {
        elsewhere: Some((reported_at, running)),
        ..pane(None, Some(0), Some((here, Some(true))))
    };
    assert_eq!(planned(&elsewhere(1, false))?, Start::Shell);
    assert_eq!(planned(&elsewhere(-1, true))?, Start::Shell);
    assert_eq!(planned(&elsewhere(-1, false))?, Start::Agent);
    assert_eq!(planned(&pane(None, Some(2), None))?, Start::Shell);
    assert_eq!(
        planned(&pane(None, Some(-1), Some((here, Some(false)))))?,
        Start::Shell
    );
    assert_eq!(
        planned(&pane(None, Some(127), Some((Some("i0"), Some(true)))))?,
        Start::Shell
    );
    assert_eq!(
        planned(&pane(Some(10), None, Some((here, Some(true)))))?,
        Start::Agent
    );
    assert_eq!(
        planned(&pane(Some(10), None, Some((here, Some(false)))))?,
        Start::Agent
    );
    assert_eq!(
        planned(&pane(Some(30), None, Some((here, Some(false)))))?,
        Start::Shell
    );
    assert_eq!(
        planned(&pane(Some(10), None, Some((Some("i0"), Some(false)))))?,
        Start::Shell
    );
    assert_eq!(
        planned(&pane(Some(10), None, Some((None, None))))?,
        Start::Agent
    );
    let working = |run| Pane {
        status: "working",
        run,
        ..pane(Some(10), None, Some((here, Some(true))))
    };
    assert_eq!(planned(&working(None))?, Start::Continue);
    let copied = Pane {
        elsewhere: Some((1, false)),
        ..working(None)
    };
    assert_eq!(planned(&copied)?, Start::Continue);
    assert_eq!(planned(&working(Some(Some(10))))?, Start::Agent);
    assert_eq!(planned(&working(Some(None)))?, Start::Agent);
    assert_eq!(planned(&working(Some(Some(9))))?, Start::Continue);
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
        Ok(candidate(store.conn(), store.dir(), "i2", "b")?.start)
    };
    assert_eq!(start("claude")?, Start::Shell);
    assert_eq!(start("codex")?, Start::Shell);
    assert_eq!(start("pi")?, Start::Agent);
    Ok(())
}

#[test]
fn a_conversation_with_an_unusable_launch_declaration_keeps_the_shell() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let store = Store::open(dir.path())?;
    conversation(&store, "b", "claude", &transcript)?;
    store.conn().execute_batch(
        r#"UPDATE conversation SET settings = '{"launch_error":"no command"}';
         INSERT INTO block (lineage, instance_id, fg_pid, status) VALUES ('b', 'i1', 10, 'idle')"#,
    )?;
    let planned = candidate(store.conn(), store.dir(), "i2", "b")?;
    assert_eq!(planned.start, Start::Shell);
    assert!(
        planned.plan.reason.ends_with(": no command"),
        "{}",
        planned.plan.reason
    );
    Ok(())
}

mod mode;
mod model;
mod two_panes;
