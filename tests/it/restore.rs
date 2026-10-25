use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{Daemon, Server, alive, query, shell_block, wait_until};

const WAIT: Duration = Duration::from_secs(20);

/// Saved layouts by session lineage, and the working directory of each pane.
fn saved(state: &Path) -> (BTreeMap<String, Value>, BTreeMap<String, Option<String>>) {
    let conn = crate::common::db(state);
    let docs = conn
        .prepare("SELECT lineage, doc FROM session")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .map(|row| {
            let (lineage, doc) = row.unwrap();
            (lineage, serde_json::from_str(&doc).unwrap())
        })
        .collect();
    let cwds = conn
        .prepare("SELECT lineage, cwd FROM block")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    (docs, cwds)
}

fn blocks_of(doc: &Value) -> usize {
    doc.to_string().matches("\"lineage\":\"block:").count()
}

#[test]
#[ignore = "needs a Rex server"]
#[expect(
    clippy::too_many_lines,
    reason = "a scenario reads as one fixture, its calls, and its assertions"
)]
fn a_crashed_or_stopped_server_comes_back_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let mut server = Server::start(dir.path(), None);
    let mut daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });

    let work = server.call(
        "session.create",
        &json!({ "label": "work", "initial_windows": [
            { "layout": { "split": { "direction": "horizontal", "ratio": 0.3,
                "before": shell_block("left", &a), "after": shell_block("right", &b) } } },
            { "window_label": "logs", "layout": { "split": { "direction": "vertical", "ratio": 0.5,
                "before": shell_block("logs", &a), "after": shell_block("doomed", &b) } } },
        ] }),
    );
    let session = work["session_id"].as_str().unwrap();
    let first_window = work["initial_windows"][0]["window_id"].as_str().unwrap();
    server.call(
        "session.new_layer",
        &json!({ "session_id": session, "window_id": first_window, "bounds": [0.2, 0.2, 0.5, 0.5],
                 "layout": shell_block("float", &b) }),
    );
    server.call(
        "session.create",
        &json!({ "label": "other", "initial_windows": [{ "layout": shell_block("solo", &b) }] }),
    );
    let doomed = work["initial_windows"][1]["block_ids"][1].as_str().unwrap();
    server.call(
        "block.close",
        &json!({ "session_id": session, "block_id": doomed }),
    );

    // Four panes of `work` and one of `other` saved, each with its directory.
    let (docs, cwds) = wait_until("the layouts to be saved", WAIT, || {
        let (docs, cwds) = saved(&state);
        let panes: usize = docs.values().map(blocks_of).sum();
        (docs.len() == 2 && panes == 5 && cwds.len() == 5 && cwds.values().all(Option::is_some))
            .then_some((docs, cwds))
    });

    let adapter = daemon.adapter().expect("the daemon started rex do");
    server.kill();
    wait_until(
        "rex do to exit with its server",
        Duration::from_secs(5),
        || (!alive(adapter)).then_some(()),
    );
    let reason: String = wait_until("the epoch to end", WAIT, || {
        query(
            &state,
            "SELECT end_reason FROM epoch WHERE end_reason IS NOT NULL ORDER BY id DESC LIMIT 1",
        )
    });
    assert_eq!(reason, "server_gone");

    let started = Instant::now();
    server = Server::start(dir.path(), None);
    wait_until("both restores to complete", WAIT, || {
        (query::<i64>(
            &state,
            "SELECT COUNT(*) FROM restore_op WHERE state = 'completed'",
        ) == Some(2))
        .then_some(())
    });
    eprintln!("restore took {:?} from server start", started.elapsed());
    let instance = server.instance();
    let (restored, restored_cwds) = wait_until("the restored layouts to be saved", WAIT, || {
        let (docs, cwds) = saved(&state);
        let current: Option<i64> = query(
            &state,
            &format!("SELECT COUNT(*) FROM session WHERE instance_id = '{instance}'"),
        );
        let shown: Option<i64> = query(
            &state,
            &format!(
                "SELECT COUNT(*) FROM block WHERE instance_id = '{instance}' AND cwd IS NOT NULL"
            ),
        );
        (current == Some(2) && shown == Some(5)).then_some((docs, cwds))
    });
    assert_eq!(restored, docs, "the layouts differ after the restore");
    assert_eq!(
        restored_cwds, cwds,
        "the working directories differ after the restore"
    );
    assert_eq!(server.sessions().len(), 2);

    // A daemon restarted on the same instance restores nothing again.
    let adapter = daemon.adapter().expect("the daemon started rex do");
    daemon.stop();
    wait_until(
        "rex do to end with its daemon's group",
        Duration::from_secs(5),
        || (!alive(adapter)).then_some(()),
    );
    daemon = Daemon::start(&server, &state);
    wait_until("the second daemon's epoch", WAIT, || {
        query::<i64>(&state, "SELECT COUNT(*) FROM epoch WHERE ended_at IS NULL")
            .filter(|n| *n == 1)
            .and_then(|_| query::<i64>(&state, "SELECT COUNT(*) FROM epoch").filter(|n| *n == 3))
    });
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(server.sessions().len(), 2);
    assert_eq!(
        query::<i64>(&state, "SELECT COUNT(*) FROM restore_op"),
        Some(2)
    );

    // A normal stop brings the same layout back too.
    server.stop();
    wait_until("the epoch to end", WAIT, || {
        query::<i64>(
            &state,
            "SELECT COUNT(*) FROM epoch WHERE end_reason = 'server_gone'",
        )
        .filter(|n| *n == 2)
    });
    server = Server::start(dir.path(), None);
    wait_until("the second restore", WAIT, || {
        (query::<i64>(
            &state,
            "SELECT COUNT(*) FROM restore_op WHERE state = 'completed'",
        ) == Some(4))
        .then_some(())
    });
    assert_eq!(server.sessions().len(), 2);
    drop(daemon);
}

fn layers(server: &Server) -> usize {
    let session = &server.sessions()[0];
    server.call("session.view", &json!({ "session_id": session }))["windows"][0]["layers"]
        .as_array()
        .unwrap()
        .len()
}

#[test]
#[ignore = "needs a Rex server"]
fn a_floating_layer_that_may_exist_is_not_requested_again_and_a_refused_one_leaves_the_restore_partial()
 {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut server = Server::start(dir.path(), None);
    let mut daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let work = server.call(
        "session.create",
        &json!({ "label": "work", "initial_windows": [{ "layout": shell_block("main", dir.path()) }] }),
    );
    server.call(
        "session.new_layer",
        &json!({ "session_id": work["session_id"], "window_id": work["initial_windows"][0]["window_id"],
                 "bounds": [0.2, 0.2, 0.5, 0.5], "layout": shell_block("float", dir.path()) }),
    );
    wait_until("both panes to be saved", WAIT, || {
        let (docs, _) = saved(&state);
        (docs.values().map(blocks_of).sum::<usize>() == 2).then_some(())
    });
    // Saved state changed between the crash and the next server is what that server restores.
    let crash = |server: &mut Server, ended: i64, edit: &str| {
        server.kill();
        wait_until("the epoch to end", WAIT, || {
            query::<i64>(
                &state,
                "SELECT COUNT(*) FROM epoch WHERE end_reason = 'server_gone'",
            )
            .filter(|n| *n == ended)
        });
        crate::common::db(&state).execute_batch(edit).unwrap();
        *server = Server::start(dir.path(), None);
    };
    crash(&mut server, 1, "");
    wait_until("the restore", WAIT, || {
        query::<String>(&state, "SELECT state FROM restore_op").filter(|s| s == "completed")
    });
    assert_eq!(layers(&server), 2);

    // The layer was requested and its reply lost: the restore has no block of it bound, and its
    // gate has long since run, so no claim will come.
    daemon.stop();
    crate::common::db(&state)
        .execute_batch(
            "UPDATE restore_op SET state = 'created';
             DELETE FROM live WHERE (instance_id, lineage) IN
               (SELECT instance_id, block_lineage FROM restore_slot WHERE layer_sent_at IS NOT NULL);",
        )
        .unwrap();
    daemon = Daemon::start(&server, &state);
    let note: String = wait_until("the restore to give up on the layer", WAIT, || {
        query(
            &state,
            "SELECT note FROM restore_op WHERE state = 'partial'",
        )
    });
    assert!(note.contains("may or may not exist"), "{note}");
    assert_eq!(layers(&server), 2, "the layer was requested again");

    // The next server refuses the saved layer: the session comes back without it, and the restore
    // says so instead of completing.
    crash(
        &mut server,
        2,
        "UPDATE session SET doc = json_set(doc, '$.windows[0].floating[0].bounds', json('[0.5, 0.5, 0.9, 0.9]'))",
    );
    wait_until("the second restore", WAIT, || {
        query(
            &state,
            "SELECT note FROM restore_op WHERE state = 'partial' ORDER BY created_at DESC LIMIT 1",
        )
        .filter(|note: &String| note.contains("refused"))
    });
    assert_eq!(layers(&server), 1);
    drop(daemon);
}
