use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};

use super::{WAIT, run_task, task, task_command};
use crate::common::{Daemon, REXUME, Server, db, poll, query, shell_block, shell_pids, wait_until};

#[test]
#[ignore = "needs a Rex server"]
fn a_run_reaches_its_owner_and_its_pane_closes_after_release() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (go, result) = (dir.path().join("go"), dir.path().join("result.md"));
    let script = format!(
        "while [ ! -e '{}' ]; do sleep 0.1; done; echo reviewing; printf Reviewed. > '{}'",
        go.display(),
        result.display()
    );
    let server = Server::start(dir.path(), None);
    let _daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let (session, owner, worker) = owner_and_run(&server, &state, dir.path(), &script);
    let (session, owner, worker) = (session.as_str(), owner.as_str(), worker.as_str());
    let added = task(
        &server,
        &state,
        owner,
        &[
            "add",
            "--worker",
            worker,
            "--title",
            "review",
            "--result-file",
            &result.to_string_lossy(),
        ],
    );
    let snapshot = &added["snapshot"];
    assert_eq!(snapshot["worktree"], Value::Null);
    assert!(
        snapshot["note"]
            .as_str()
            .unwrap()
            .contains("not a git repository")
    );
    let id = added["id"].as_str().unwrap().to_owned();
    std::fs::write(&go, "").unwrap();
    let waited = task(&server, &state, owner, &["wait", &id, "--timeout", "20"]);
    assert_eq!(
        (waited["state"].as_str(), waited["result"].as_str()),
        (Some("completed"), Some("Reviewed."))
    );
    // Rex has so far reported the pane once more within a second of the result; the pane closes
    // either way.
    let generation = || {
        query::<i64>(
            &state,
            &format!(
                "SELECT b.fg_generation FROM task t JOIN block b ON b.lineage = t.worker_lineage
                 WHERE t.id = '{id}'"
            ),
        )
    };
    let at_result = generation();
    poll(Duration::from_secs(5), || {
        generation().filter(|now| Some(*now) > at_result)
    });
    assert!(
        run_task(&server, &state, owner, &["release", &id])
            .status
            .success()
    );
    let pane = wait_until("the daemon to settle the worker's pane", WAIT, || {
        query::<String>(
            &state,
            &format!(
                "SELECT pane_state || ': ' || COALESCE(pane_note, '') FROM task
                 WHERE id = '{id}' AND pane_state IN ('closed', 'kept')"
            ),
        )
    });
    assert_eq!(pane, "closed: ");
    assert!(
        server
            .try_call(
                "block.method",
                &json!({ "session_id": session, "block_id": worker, "method": "process" })
            )
            .is_err()
    );
}

#[test]
#[ignore = "needs a Rex server"]
fn a_run_that_reported_its_conversation_is_bound_when_registered() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let agent = dir.path().join("agent.pl");
    std::fs::write(
        &agent,
        format!(
            "open(my $r, '|-', '{REXUME}', 'report', 'claude', '--state-dir', '{}') or die $!;
             print $r '{{\"session_id\":\"run\"}}'; close $r; sleep 600;",
            state.display()
        ),
    )
    .unwrap();
    let server = Server::start(dir.path(), None);
    let mut daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let script = format!("exec /usr/bin/perl '{}'", agent.display());
    let (_, owner, worker) = owner_and_run(&server, &state, dir.path(), &script);
    wait_until("the run to report its conversation", WAIT, || {
        query::<i64>(
            &state,
            &format!(
                "SELECT 1 FROM occupant WHERE block_lineage = '{worker}' AND native_id = 'run'"
            ),
        )
    });
    daemon.stop();
    let result = dir.path().join("result.md");
    let meta = r#"{"eval": "e1"}"#;
    let added = task_command(
        &server,
        &state,
        &owner,
        &[
            "add",
            "--worker",
            &worker,
            "--title",
            "review",
            "--result-file",
            &result.to_string_lossy(),
        ],
    )
    .env("REXUME_TASK_META", meta)
    .output()
    .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let id = serde_json::from_slice::<Value>(&added.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let bound = query::<String>(
        &state,
        &format!("SELECT worker_runtime || ' ' || worker_native_id FROM task WHERE id = '{id}'"),
    );
    assert_eq!(bound.as_deref(), Some("claude run"));
    let exported = Command::new(REXUME)
        .args(["export", "--state-dir"])
        .arg(&state)
        .output()
        .unwrap();
    let task = String::from_utf8(exported.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["id"] == id.as_str())
        .unwrap();
    assert_eq!(task["data"]["task"]["meta"], meta);
}

pub(super) fn owner_and_run(
    server: &Server,
    state: &Path,
    dir: &Path,
    script: &str,
) -> (String, String, String) {
    let run = json!({ "block": { "flavor": "com.superlogical.terminal.shell", "label": "worker",
        "options": { "cwd": dir, "command": ["/bin/sh", "-c", script],
                     "exit": { "on_completion": false } } } });
    let created = server.call(
        "session.create",
        &json!({ "label": "orchestrate", "initial_windows": [{ "layout": { "split": { "direction": "horizontal",
            "ratio": 0.5, "before": shell_block("owner", dir), "after": run } } }] }),
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
    let owner_pid = shell_pids(server, state, session, &[owner])[0];
    db(state)
        .execute_batch(&format!(
            "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
               VALUES ('claude', 'owner', 1, 0);
             INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at)
               VALUES ('{owner}', 'claude', 'owner', {owner_pid}, 1, 0);"
        ))
        .unwrap();
    wait_until("the run to be the worker's foreground", WAIT, || {
        query::<i64>(
            state,
            &format!("SELECT fg_pid FROM block WHERE lineage = '{worker}' AND fg_pid IS NOT NULL"),
        )
    });
    (session.to_owned(), owner.to_owned(), worker.to_owned())
}
