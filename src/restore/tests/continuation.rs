use std::io::Read as _;
use std::os::unix::net::UnixListener;
use std::thread::JoinHandle;

use anyhow::Result;
use serde_json::{Map, json};

use super::server::STARTUP;
use super::{new_restorer, restarted};
use crate::report::{self, Process};
use crate::rex::Rex;
use crate::runtime::Runtime;
use crate::store::Store;
use crate::task::tests::{PROMPT, PROMPT_2};

/// A Claude Code conversation cut off mid-turn in pane `b` on instance `i1`, whose messaging socket
/// on the next instance accepts one connection and hands back what it read.
fn cut_off(dir: &std::path::Path) -> Result<(Store, Rex, std::path::PathBuf, JoinHandle<String>)> {
    let (store, _server, rex) = restarted(dir, STARTUP, STARTUP)?;
    std::fs::write(dir.join("worker.jsonl"), format!("{PROMPT}\n"))?;
    store.conn().execute_batch(
        "UPDATE epoch SET end_reason = 'server_gone' WHERE instance_id = 'i1';
             UPDATE occupant SET instance_id = 'i1' WHERE block_lineage = 'b';",
    )?;
    let socket = dir.join("messaging.sock");
    let listener = UnixListener::bind(&socket)?;
    let received = std::thread::spawn(move || {
        let mut got = String::new();
        if let Ok((mut stream, _)) = listener.accept() {
            stream.read_to_string(&mut got).unwrap();
        }
        got
    });
    Ok((store, rex, socket, received))
}

/// The conversation Rex resumed in pane `b` reports on `i2`, with its socket, as the report of a
/// resumed Claude Code does.
fn resumed_report(
    store: &mut Store,
    dir: &std::path::Path,
    socket: &std::path::Path,
) -> Result<()> {
    let tx = store.write()?;
    let report = report::parse(Runtime::Claude, &json!({ "session_id": "worker" }))?;
    let mut transport = Map::new();
    transport.insert("messaging_socket".into(), socket.to_string_lossy().into());
    let process = Process {
        transport,
        instance: "i2".into(),
        pid: 300,
        agent_pid: None,
        name: Some("zsh".into()),
        own_program: false,
        start_position: std::fs::metadata(dir.join("worker.jsonl"))?.len(),
    };
    report::record(&tx, "b", Runtime::Claude, &report, &process)?;
    tx.commit()?;
    Ok(())
}

/// What the baseline records for pane `b` once the resumed program is its foreground.
fn shown(store: &Store, status: Option<&str>) -> Result<()> {
    store.conn().execute(
        "UPDATE block SET instance_id = 'i2', fg_pid = 300, fg_generation = fg_generation + 1,
           status = ?1, status_pid = 300 WHERE lineage = 'b'",
        [status],
    )?;
    Ok(())
}

fn continuation_of(store: &Store) -> Result<(String, Option<String>, Option<String>)> {
    Ok(store.conn().query_row(
        "SELECT start, continuation_state, note FROM restore WHERE block_lineage = 'b'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

#[test]
fn a_cut_off_turn_rex_resumed_gets_one_continuation_whichever_comes_first() -> Result<()> {
    for report_first in [true, false] {
        let dir = tempfile::tempdir()?;
        let (mut store, mut rex, socket, received) = cut_off(dir.path())?;
        let mut restorer = new_restorer();
        if report_first {
            resumed_report(&mut store, dir.path(), &socket)?;
        }
        restorer.start(&mut rex, &mut store, "i2")?;
        restorer.reconcile(&mut rex, &mut store, "i2");
        if !report_first {
            resumed_report(&mut store, dir.path(), &socket)?;
        }
        restorer.reconcile(&mut rex, &mut store, "i2");
        let (_, continuation, note) = continuation_of(&store)?;
        assert_eq!(continuation.as_deref(), Some("pending"));
        assert!(note.is_some_and(|n| n.contains("foreground")));
        shown(&store, None)?;
        restorer.reconcile(&mut rex, &mut store, "i2");
        restorer.reconcile(&mut rex, &mut store, "i2");
        let (start, continuation, _) = continuation_of(&store)?;
        assert_eq!(
            (start.as_str(), continuation.as_deref()),
            ("continue", Some("sent")),
            "report first: {report_first}"
        );
        let got = received.join().unwrap();
        assert_eq!(got.lines().count(), 1, "{got}");
        assert!(got.contains("[rexume i2/b]"), "{got}");
        let state: String = store.conn().query_row(
            "SELECT state FROM restore WHERE block_lineage = 'b'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(state, "opened");
    }
    Ok(())
}

#[test]
fn a_continuation_waits_for_the_conversation_and_gives_up_on_a_new_prompt_or_the_deadline()
-> Result<()> {
    for case in ["prompt", "deadline", "no socket"] {
        let dir = tempfile::tempdir()?;
        let (mut store, mut rex, socket, _received) = cut_off(dir.path())?;
        let mut restorer = new_restorer();
        restorer.start(&mut rex, &mut store, "i2")?;
        resumed_report(&mut store, dir.path(), &socket)?;
        shown(&store, Some("working"))?;
        restorer.reconcile(&mut rex, &mut store, "i2");
        let (_, continuation, note) = continuation_of(&store)?;
        assert_eq!(continuation.as_deref(), Some("pending"), "{case}");
        assert!(note.is_some_and(|n| n.contains("running a turn")), "{case}");
        match case {
            "prompt" => {
                let record = format!("{PROMPT}\n{PROMPT_2}\n");
                std::fs::write(dir.path().join("worker.jsonl"), record)?;
                shown(&store, None)?;
            }
            "deadline" => {
                store
                    .conn()
                    .execute_batch("UPDATE restore SET created_at = created_at - 31000")?;
            }
            _ => {
                std::fs::remove_file(&socket)?;
                shown(&store, None)?;
            }
        }
        restorer.reconcile(&mut rex, &mut store, "i2");
        let (_, continuation, note) = continuation_of(&store)?;
        let expected = match case {
            "prompt" => ("skipped", "since the plan"),
            "deadline" => (
                "skipped",
                "not sent within 30 seconds: the conversation is running",
            ),
            _ => ("pending", "failed to connect"),
        };
        assert_eq!(continuation.as_deref(), Some(expected.0), "{case}");
        let note = note.unwrap_or_default();
        assert!(note.contains(expected.1), "{case}: {note}");
        if case == "no socket" {
            let tx = store.write()?;
            tx.execute(
                "UPDATE restore SET continuation_state = 'sending' WHERE block_lineage = 'b'",
                [],
            )?;
            tx.commit()?;
            let orphaned = crate::task::orphaned_sends(&mut store)?;
            assert_eq!(orphaned.first().map(|o| o.0), Some("restore"));
            assert_eq!(continuation_of(&store)?.1.as_deref(), Some("unknown"));
        }
    }
    Ok(())
}

#[test]
fn a_cut_off_turn_of_another_runtime_found_opened_gets_no_continuation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, mut rex, _socket, _received) = cut_off(dir.path())?;
    let mut restorer = new_restorer();
    restorer.start(&mut rex, &mut store, "i2")?;
    store.conn().execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('codex', 'worker', 1, 0);
         UPDATE restore SET runtime = 'codex' WHERE block_lineage = 'b';
         UPDATE occupant SET runtime = 'codex', instance_id = 'i2', pid = 300
           WHERE block_lineage = 'b';",
    )?;
    shown(&store, None)?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    let (start, continuation, note) = continuation_of(&store)?;
    assert_eq!(
        (start.as_str(), continuation.as_deref()),
        ("continue", Some("skipped"))
    );
    assert!(note.is_some_and(|n| n.contains("it did not start")));
    Ok(())
}

#[test]
fn a_conversation_that_had_left_the_pane_before_the_servers_end_gets_no_continuation() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let (mut store, mut rex, socket, _received) = cut_off(dir.path())?;
    store
        .conn()
        .execute("UPDATE block SET last_exit = 0 WHERE lineage = 'b'", [])?;
    resumed_report(&mut store, dir.path(), &socket)?;
    new_restorer().start(&mut rex, &mut store, "i2")?;
    assert_eq!(continuation_of(&store)?.0, "agent");
    Ok(())
}
