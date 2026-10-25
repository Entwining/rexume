use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use rusqlite::params;
use serde_json::{Value, json};

use crate::common::{
    Daemon, REXUME, Server, db, query, rex_bin, shell_block, shell_pids, wait_until,
};

const WAIT: Duration = Duration::from_secs(20);

/// Runs `rexume task ...` as the orchestrator in `block` would.
fn run_task(server: &Server, state: &Path, block: &str, args: &[&str]) -> Output {
    let rex_dir = rex_bin().parent().unwrap().to_owned();
    Command::new(REXUME)
        .arg("task")
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .env_clear()
        .env("REX_BLOCK", block)
        .env("REX_SERVER", server.endpoint())
        .env("PATH", format!("{}:/usr/bin:/bin", rex_dir.display()))
        .output()
        .unwrap()
}

fn task(server: &Server, state: &Path, block: &str, args: &[&str]) -> Value {
    let output = run_task(server, state, block, args);
    assert!(
        output.status.success(),
        "task {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A command's exit status and stderr.
fn failure(output: &Output) -> (Option<i32>, String) {
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn append(path: &Path, line: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

#[test]
#[ignore = "needs a Rex server"]
#[expect(
    clippy::too_many_lines,
    reason = "a scenario reads as one fixture, its calls, and its assertions"
)]
fn a_finished_worker_notifies_its_idle_owner_once_and_closes_after_ack() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (owner_record, worker_record) = (
        dir.path().join("owner.jsonl"),
        dir.path().join("worker.jsonl"),
    );
    std::fs::write(&owner_record, "").unwrap();
    std::fs::write(&worker_record, "").unwrap();
    let socket = dir.path().join("owner.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();

    let server = Server::start(dir.path(), None);
    let _daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let created = server.call(
        "session.create",
        &json!({ "label": "orchestrate", "initial_windows": [{ "layout": { "split": { "direction": "horizontal",
            "ratio": 0.5, "before": shell_block("owner", dir.path()), "after": shell_block("worker", dir.path()) } } }] }),
    );
    let session = created["session_id"].as_str().unwrap();
    let ids = created["initial_windows"][0]["block_ids"]
        .as_array()
        .unwrap();
    let (owner, worker) = (ids[0].as_str().unwrap(), ids[1].as_str().unwrap());
    server.call(
        "session.focus_block",
        &json!({ "session_id": session, "block_id": owner }),
    );
    let pids = shell_pids(&server, &state, session, &[owner, worker]);

    // What each agent's session-start hook would have reported.
    let conn = db(&state);
    for (native_id, record, block, pid, socket) in [
        (
            "owner",
            &owner_record,
            owner,
            pids[0],
            Some(socket.to_string_lossy().into_owned()),
        ),
        ("worker", &worker_record, worker, pids[1], None),
    ] {
        conn.execute(
            "INSERT INTO conversation (runtime, native_id, transcript, resumable, reported_at)
             VALUES ('claude', ?1, ?2, 1, 0)",
            params![native_id, record.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at, transport)
             VALUES (?1, 'claude', ?2, ?3, 1, 0,
                     CASE WHEN ?4 IS NOT NULL THEN json_object('messaging_socket', ?4) END)",
            params![block, native_id, pid, socket],
        )
        .unwrap();
    }

    let id = task(
        &server,
        &state,
        owner,
        &["add", "--worker", worker, "--title", "review"],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Before the worker answers: a wait times out with the task, an ack is refused, and another
    // conversation cannot read the task.
    let waited = run_task(&server, &state, owner, &["wait", &id, "--timeout", "1"]);
    assert_eq!(failure(&waited), (Some(124), String::new()));
    let waited: Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(
        (waited["timed_out"].as_bool(), waited["state"].as_str()),
        (Some(true), Some("running"))
    );
    for (block, args) in [
        (owner, ["ack", id.as_str()]),
        (worker, ["show", id.as_str()]),
    ] {
        let (code, stderr) = failure(&run_task(&server, &state, block, &args));
        assert_eq!(code, Some(1), "{args:?}");
        assert!(
            stderr.starts_with("rexume: ") && stderr.contains(&id),
            "{stderr}"
        );
    }

    append(
        &worker_record,
        r#"{"type":"user","message":{"role":"user","content":"Review the diff"}}"#,
    );
    append(
        &worker_record,
        r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Looks good."}]}}"#,
    );

    // The notice reaches the owner's message socket once, after the grace a waiting owner gets.
    let stream = wait_until("the notice", WAIT, || {
        listener.accept().ok().map(|(stream, _)| stream)
    });
    stream.set_nonblocking(false).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let notice: Value = serde_json::from_str(&line).unwrap();
    let text = notice["message"]["content"].as_str().unwrap();
    assert!(text.contains(&id), "{text}");
    let shown = task(&server, &state, owner, &["show", &id]);
    assert_eq!(
        (shown["state"].as_str(), shown["result"].as_str()),
        (Some("completed"), Some("Looks good."))
    );

    append(
        &owner_record,
        &json!({ "type": "user", "origin": { "kind": "peer" }, "message": { "role": "user", "content": text } }).to_string(),
    );
    wait_until("the owner's receipt to settle the notice", WAIT, || {
        query::<String>(&state, "SELECT state FROM notice").filter(|s| s == "delivered")
    });
    assert!(listener.accept().is_err(), "the notice was sent twice");

    let acked = task(&server, &state, owner, &["ack", &id]);
    assert!(acked["acked_at"].is_i64());
    // The daemon, the only closer, closes the unused disposable worker after the ack.
    wait_until("the daemon to close the worker's pane", WAIT, || {
        query::<String>(
            &state,
            &format!("SELECT pane_state FROM task WHERE id = '{id}'"),
        )
        .filter(|s| s == "closed")
    });
    assert!(
        server
            .try_call(
                "block.method",
                &json!({ "session_id": session, "block_id": worker, "method": "process" })
            )
            .is_err()
    );
}
