use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde_json::json;

use crate::common::{Daemon, REXUME, Server, db, query, shell_block, wait_until};

const WAIT: Duration = Duration::from_secs(20);

fn lineages(state: &Path, table: &str) -> BTreeSet<String> {
    db(state)
        .prepare(&format!("SELECT lineage FROM {table}"))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Waits until the daemon has saved `sessions` sessions and recorded `blocks` panes on the server's
/// current instance.
fn saved_on(server: &Server, state: &Path, sessions: i64, blocks: i64) {
    let instance = server.instance();
    wait_until("the layouts to be saved", WAIT, || {
        let count = |sql: &str| query::<i64>(state, &sql.replace("INSTANCE", &instance));
        (count("SELECT COUNT(*) FROM session WHERE instance_id = 'INSTANCE'") == Some(sessions)
            && count("SELECT COUNT(*) FROM block WHERE instance_id = 'INSTANCE'") == Some(blocks))
        .then_some(())
    });
}

fn restart(server: &mut Server, state: &Path, ended: i64, start: fn(&Path) -> Server) {
    server.stop();
    wait_until("the epoch to end", WAIT, || {
        query::<i64>(
            state,
            "SELECT COUNT(*) FROM epoch WHERE end_reason = 'server_gone'",
        )
        .filter(|n| *n == ended)
    });
    let dir = server.dir.clone();
    *server = start(&dir);
}

fn restores_on(server: &Server, state: &Path) -> Option<i64> {
    query(
        state,
        &format!(
            "SELECT COUNT(*) FROM restore_op WHERE instance_id = '{}'",
            server.instance()
        ),
    )
}

/// Sessions a server without its own restore never saved, as an older Rex's, come back through
/// Rexume under new IDs, and a server that restores its own brings them back under those IDs from
/// then on. The daemon takes them as the sessions it saved instead of restoring them a second time,
/// across every restart, and goes on recording them; a pane whose exit Rexume would have brought
/// back is named in its log.
#[test]
#[ignore = "needs a Rex server that restores its own sessions"]
fn sessions_the_server_restored_itself_are_not_restored_again() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut server = Server::start(dir.path(), None);
    let daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let failed = json!({ "block": { "flavor": "com.superlogical.terminal.shell", "label": "failed",
        "options": { "cwd": dir.path(), "command": ["sh", "-c", "exit 3"],
                     "exit": { "on_completion": false } } } });
    server.call(
        "session.create",
        &json!({ "label": "work", "initial_windows": [{ "layout": { "split": {
            "direction": "horizontal", "ratio": 0.5,
            "before": shell_block("left", dir.path()), "after": failed } } }] }),
    );
    let other = server.call(
        "session.create",
        &json!({ "label": "other", "initial_windows": [{ "layout": shell_block("solo", dir.path()) }] }),
    );
    let other = other["session_id"].as_str().unwrap().to_owned();
    saved_on(&server, &state, 2, 3);
    wait_until("the exit to be recorded", WAIT, || {
        query::<i64>(
            &state,
            "SELECT last_exit FROM block WHERE last_exit IS NOT NULL",
        )
    });
    let sessions = lineages(&state, "session");
    let blocks = lineages(&state, "block");

    restart(&mut server, &state, 1, Server::start_restoring);
    saved_on(&server, &state, 2, 3);
    assert!(server.sessions().iter().all(|id| !sessions.contains(id)));
    assert_eq!(restores_on(&server, &state), Some(2));
    restart(&mut server, &state, 2, Server::start_restoring);
    saved_on(&server, &state, 2, 3);
    assert_eq!(server.sessions().len(), 2);
    assert_eq!(lineages(&state, "session"), sessions);
    assert_eq!(lineages(&state, "block"), blocks);
    assert_eq!(restores_on(&server, &state), Some(0));
    let log = daemon.log();
    assert!(
        log.contains("the server opened a shell instead of exited"),
        "{log}"
    );

    // The daemon still records what changes, and a session closed is gone after the next restart.
    let other = query::<String>(
        &state,
        &format!(
            "SELECT live_id FROM live WHERE instance_id = '{}' AND lineage = '{other}'",
            server.instance()
        ),
    )
    .unwrap();
    let renamed = server
        .cli()
        .args(["session", "rename", &other, "renamed"])
        .status()
        .unwrap();
    assert!(renamed.success());
    wait_until("the rename to be saved", WAIT, || {
        query::<i64>(
            &state,
            "SELECT 1 FROM session WHERE json_extract(doc, '$.label') = 'renamed'",
        )
    });
    server.call("session.destroy", &json!({ "session_id": other }));
    restart(&mut server, &state, 3, Server::start_restoring);
    saved_on(&server, &state, 1, 2);
    assert_eq!(server.sessions().len(), 1);
    assert_eq!(lineages(&state, "session").len(), 1);
    assert_eq!(restores_on(&server, &state), Some(0));
    let log = daemon.log();
    assert!(!log.contains("epoch ended with an error"), "{log}");
    drop(daemon);
}

/// An epoch the daemon ends on an error shows in `status` until the daemon observes the server
/// again without one. A trigger refuses every new binding, which the restore after the restart
/// needs.
#[test]
#[ignore = "needs a Rex server that restores its own sessions"]
fn status_shows_the_error_that_ended_the_latest_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut server = Server::start_restoring(dir.path());
    let _daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    server.call(
        "session.create",
        &json!({ "label": "work", "initial_windows": [{ "layout": shell_block("main", dir.path()) }] }),
    );
    saved_on(&server, &state, 1, 1);
    db(&state)
        .execute_batch(
            "CREATE TRIGGER refuse BEFORE INSERT ON live BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let epoch_error = || {
        let output = std::process::Command::new(REXUME)
            .args(["status", "--state-dir"])
            .arg(&state)
            .output()
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["epoch_error"].clone()
    };
    restart(&mut server, &state, 1, Server::start_restoring);
    let shown = wait_until("the error to show", WAIT, || {
        Some(epoch_error()).filter(|e| !e.is_null())
    });
    assert!(
        shown["error"].as_str().unwrap().contains("refused"),
        "{shown}"
    );

    db(&state).execute_batch("DROP TRIGGER refuse").unwrap();
    saved_on(&server, &state, 1, 1);
    assert_eq!(epoch_error(), json!(null));
}
