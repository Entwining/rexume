use std::path::Path;

use serde_json::json;

use super::{WAIT, task};
use crate::common::{Daemon, Server, claude, db, query, wait_until};
use crate::restore::{agents, report, restart, restores, screen};

fn pane_state(server: &Server, state: &Path, owner: &str, id: &str) -> String {
    task(server, state, owner, &["show", id])["pane"]["state"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn placements(server: &Server, session: &str) -> Vec<(String, String)> {
    let list = server.call("session.list_blocks", &json!({ "session_id": session }));
    let mut blocks: Vec<(String, String)> = list["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            let field = |name: &str| b[name].as_str().unwrap().to_owned();
            (field("block_id"), field("placement"))
        })
        .collect();
    blocks.sort();
    blocks
}

/// Creating a pane in no window changes no view, so the daemon records it from the pane's own
/// events.
fn start_detached(server: &Server, state: &Path, session: &str, cwd: &Path) -> String {
    let worker = server.call(
        "session.new_block",
        &json!({ "session_id": session, "flavor": "com.superlogical.terminal.shell",
                 "label": "worker", "options": { "cwd": cwd } }),
    )["block_id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_until("the worker's shell to be recorded", WAIT, || {
        query::<i64>(
            state,
            &format!("SELECT fg_pid FROM block WHERE lineage = '{worker}' AND fg_pid IS NOT NULL"),
        )
    });
    server.call(
        "block.event",
        &json!({ "session_id": session, "block_id": worker,
                 "event": { "type": "paste", "payload": { "text": "echo detached-$((40 + 2))" } } }),
    );
    assert!(
        server
            .cli()
            .args(["send-key", "-s", session, "-b", &worker, "enter"])
            .status()
            .unwrap()
            .success()
    );
    wait_until("the worker to run the command", WAIT, || {
        screen(server, session, &worker)
            .contains("detached-42")
            .then_some(())
    });
    worker
}

#[test]
#[ignore = "needs a Rex server"]
fn a_worker_in_no_window_stays_a_worker_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (bin, runs) = claude::stand_in(dir.path(), "sleep 600;");
    let transcript = dir.path().join("story.jsonl");
    std::fs::write(&transcript, "").unwrap();
    claude::turn(&state, "story", "p1", "Write a story", None);
    let mut server = Server::start(dir.path(), Some(&bin));
    let mut daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let (session, blocks, fg) = agents(&server, &state, dir.path());
    let (owner, viewer) = (blocks[0].as_str(), blocks[1].as_str());
    db(&state)
        .execute_batch(&format!(
            "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
               VALUES ('claude', 'owner', 1, 0);
             INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at)
               VALUES ('{owner}', 'claude', 'owner', {}, 1, 0);",
            fg[0]
        ))
        .unwrap();
    let worker = start_detached(&server, &state, &session, dir.path());
    let id = task(
        &server,
        &state,
        owner,
        &["add", "--worker", &worker, "--title", "background"],
    )["id"]
        .as_str()
        .map(str::to_owned)
        .unwrap();
    server.call(
        "session.detach_block",
        &json!({ "session_id": session, "block_id": viewer }),
    );

    daemon.stop();
    daemon = Daemon::start(&server, &state);
    wait_until("the second daemon's epoch", WAIT, || {
        query::<i64>(&state, "SELECT COUNT(*) FROM epoch").filter(|n| *n == 2)
    });
    assert_eq!(pane_state(&server, &state, owner, &id), "open");

    report(&state, &transcript, &[viewer.to_owned()], &fg[1..]);
    restart(&mut server, &state, &bin, 1, true);
    let mut expected = [
        (owner.to_owned(), "placed".to_owned()),
        (viewer.to_owned(), "detached".to_owned()),
        (worker.clone(), "detached".to_owned()),
    ];
    expected.sort();
    assert_eq!(placements(&server, &session), expected);
    let mut claimed = wait_until("the gate to claim the restores", WAIT, || {
        Some(restores(&server, &state))
            .filter(|r| r.len() == 2 && r.iter().all(|(_, s, _)| s == "claimed"))
    });
    claimed.sort();
    let mut expected = [
        (
            owner.to_owned(),
            "claimed".to_owned(),
            Some("agent".to_owned()),
        ),
        (
            viewer.to_owned(),
            "claimed".to_owned(),
            Some("continue".to_owned()),
        ),
    ];
    expected.sort();
    assert_eq!(claimed, expected);
    assert_eq!(claude::runs(&runs, 2).len(), 2);
    assert_eq!(pane_state(&server, &state, owner, &id), "open");

    server.call(
        "block.close",
        &json!({ "session_id": session, "block_id": worker }),
    );
    wait_until("the task to record the close", WAIT, || {
        Some(pane_state(&server, &state, owner, &id)).filter(|s| s == "closed")
    });
    drop(daemon);
}
