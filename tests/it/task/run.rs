use std::path::Path;
use std::time::Duration;

use serde_json::json;

use super::{WAIT, run_task, task};
use crate::common::{Daemon, Server, db, poll, query, shell_block, shell_pids, wait_until};

/// A non-interactive worker is the terminal's own program, kept open after it exits. It prints
/// before it exits, as `codex exec` does, after which the server reports an empty foreground once
/// more, when the round already has its result.
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
    let id = task(
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
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    std::fs::write(&go, "").unwrap();
    let waited = task(&server, &state, owner, &["wait", &id, "--timeout", "20"]);
    assert_eq!(
        (waited["state"].as_str(), waited["result"].as_str()),
        (Some("completed"), Some("Reviewed."))
    );
    // Rex has so far reported the pane once more within a second of the result, moving its
    // generation past the round's; the pane closes either way.
    poll(Duration::from_secs(5), || {
        query::<i64>(
            &state,
            &format!(
                "SELECT 1 FROM task t JOIN task_round r ON r.task_id = t.id
                 JOIN block b ON b.lineage = t.worker_lineage
                 WHERE t.id = '{id}' AND b.fg_generation > r.finished_generation"
            ),
        )
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

/// A session with an owner whose conversation the state records, beside a pane running `script`
/// as its own program, kept open after it exits. Returns the session, owner, and worker IDs once
/// the daemon has recorded the run as the worker's foreground.
fn owner_and_run(
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
