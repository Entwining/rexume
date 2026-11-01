use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::{Value, json};

use crate::common::{REXUME, Server, claude, db, query, rex_bin, wait_until};

mod abandon;
mod blocked;
mod orchestration;
mod run;

use orchestration::Orchestration;

const WAIT: Duration = Duration::from_secs(20);

/// `rexume task ...` as the orchestrator in `block` would run it.
fn task_command(server: &Server, state: &Path, block: &str, args: &[&str]) -> Command {
    let rex_dir = rex_bin().parent().unwrap().to_owned();
    let mut command = Command::new(REXUME);
    command
        .arg("task")
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .env_clear()
        .env("REX_BLOCK", block)
        .env("REX_SERVER", server.endpoint())
        .env("PATH", format!("{}:/usr/bin:/bin", rex_dir.display()));
    command
}

fn run_task(server: &Server, state: &Path, block: &str, args: &[&str]) -> Output {
    task_command(server, state, block, args).output().unwrap()
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

/// The content of the next message a listening messaging socket receives.
fn message(listener: &UnixListener, what: &str) -> String {
    let stream = wait_until(what, WAIT, || {
        listener.accept().ok().map(|(stream, _)| stream)
    });
    stream.set_nonblocking(false).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let message: Value = serde_json::from_str(&line).unwrap();
    message["message"]["content"].as_str().unwrap().to_owned()
}

/// The acknowledged worker takes a new request: Rexume delivers it to the worker's socket, and the
/// round's result is the turn that took it in.
fn next_request_round(
    server: &Server,
    state: &Path,
    owner: &str,
    id: &str,
    worker_listener: &UnixListener,
) {
    let sent = task(
        server,
        state,
        owner,
        &["send", id, "--message", "Fix the nit"],
    );
    assert_eq!(sent["round"], 2);
    let request = sent["request"]["id"].as_str().unwrap();
    let text = message(worker_listener, "the request");
    assert_eq!(text, format!("[rexume {request}] Fix the nit"));
    claude::turn(state, "worker", "p2", &text, Some("Fixed."));
    let waited = task(server, state, owner, &["wait", id, "--timeout", "20"]);
    assert_eq!(
        (
            waited["result"].as_str(),
            waited["request"]["state"].as_str(),
            waited["unread"].as_u64()
        ),
        (Some("Fixed."), Some("delivered"), Some(0))
    );
}

#[test]
#[ignore = "needs a Rex server"]
fn a_worker_answers_each_round_tells_its_owner_once_and_closes_after_release() {
    let world = Orchestration::start();
    let Orchestration {
        server,
        state,
        session,
        owner,
        worker,
        id,
        listener,
        worker_listener,
        ..
    } = &world;

    // Before the worker answers: a wait times out with the task, an ack is refused, and another
    // conversation cannot read the task.
    let waited = run_task(server, state, owner, &["wait", id, "--timeout", "1"]);
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
        let (code, stderr) = failure(&run_task(server, state, block, &args));
        assert_eq!(code, Some(1), "{args:?}");
        assert!(
            stderr.starts_with("rexume: ") && stderr.contains(id.as_str()),
            "{stderr}"
        );
    }

    claude::turn(
        state,
        "worker",
        "p1",
        "Review the diff",
        Some("Looks good."),
    );

    // The notice reaches the owner's message socket once, after the grace a waiting owner gets.
    let notice = message(listener, "the notice");
    let text = notice.as_str();
    assert!(text.contains(id.as_str()), "{text}");
    let shown = task(server, state, owner, &["show", id]);
    assert_eq!(
        (shown["state"].as_str(), shown["result"].as_str()),
        (Some("completed"), Some("Looks good."))
    );

    claude::turn(state, "owner", "o1", text, Some("Noted."));
    wait_until("the owner's receipt to settle the notice", WAIT, || {
        query::<String>(state, "SELECT state FROM notice").filter(|s| s == "delivered")
    });
    assert!(listener.accept().is_err(), "the notice was sent twice");

    let acked = task(server, state, owner, &["ack", id]);
    assert!(acked["acked_at"].is_i64());
    next_request_round(server, state, owner, id, worker_listener);
    // A turn the user had the worker finish is unread, so the worker is not released before
    // `task next` takes it.
    claude::turn(state, "worker", "p3", "And the docs?", Some("Docs too."));
    let (code, stderr) = failure(&run_task(server, state, owner, &["release", id]));
    assert_eq!(code, Some(1));
    assert!(stderr.contains("unread turns (1)"), "{stderr}");
    assert_eq!(task(server, state, owner, &["next", id])["round"], 3);
    let waited = task(server, state, owner, &["wait", id, "--timeout", "20"]);
    assert_eq!(waited["result"].as_str(), Some("Docs too."));
    let released = task(server, state, owner, &["release", id]);
    assert!(released["released_at"].is_i64());
    // The daemon, the only closer, closes the unused disposable worker once it is released.
    wait_until("the daemon to close the worker's pane", WAIT, || {
        query::<String>(
            state,
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
