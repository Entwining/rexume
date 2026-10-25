use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use rusqlite::params;
use serde_json::json;

use crate::common::{Daemon, Server, db, query, shell_block, shell_pids, wait_until};

const WAIT: Duration = Duration::from_secs(20);
const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"Write a story"}}"#;

#[test]
#[ignore = "needs a Rex server"]
#[expect(
    clippy::too_many_lines,
    reason = "a scenario reads as one fixture, its calls, and its assertions"
)]
fn a_turn_the_crash_cut_off_continues_once_and_a_second_pane_only_types_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    // A stand-in for Claude Code that records how it was started, one file per process.
    let bin = dir.path().join("bin");
    let runs = dir.path().join("runs");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&runs).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'/$$\nexec sleep 600\n",
            runs.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let transcript = dir.path().join("story.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n")).unwrap();

    let mut server = Server::start(dir.path(), Some(&bin));
    let _daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let created = server.call(
        "session.create",
        &json!({ "label": "agents", "initial_windows": [{ "layout": { "split": { "direction": "horizontal",
            "ratio": 0.5, "before": shell_block("writer", dir.path()), "after": shell_block("viewer", dir.path()) } } }] }),
    );
    let blocks: Vec<String> = created["initial_windows"][0]["block_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_str().unwrap().to_owned())
        .collect();
    let fg = shell_pids(
        &server,
        &state,
        created["session_id"].as_str().unwrap(),
        &[&blocks[0], &blocks[1]],
    );
    wait_until("the layout to be saved", WAIT, || {
        query::<String>(&state, "SELECT doc FROM session")
            .filter(|doc| blocks.iter().all(|b| doc.contains(b.as_str())))
    });

    // The writer pane runs the conversation mid-turn; the viewer pane shows the same conversation,
    // as after resuming it in a second pane. Their hooks would have reported this.
    let conn = db(&state);
    conn.execute(
        "INSERT INTO conversation (runtime, native_id, transcript, resumable, reported_at)
         VALUES ('claude', 'story', ?1, 1, 0)",
        params![transcript.to_string_lossy()],
    )
    .unwrap();
    for (block, pid) in blocks.iter().zip(&fg) {
        conn.execute(
            "INSERT INTO occupant (block_lineage, runtime, native_id, pid, pid_name, keeps_session_env, reported_at)
             VALUES (?1, 'claude', 'story', ?2, 'claude', 1, 0)",
            params![block, pid],
        )
        .unwrap();
    }
    conn.execute(
        "UPDATE block SET status = 'working', status_pid = fg_pid, status_name = 'claude' WHERE lineage = ?1",
        params![blocks[0]],
    )
    .unwrap();

    server.kill();
    wait_until("the epoch to end", WAIT, || {
        query::<String>(
            &state,
            "SELECT end_reason FROM epoch WHERE end_reason IS NOT NULL",
        )
    });
    server = Server::start(dir.path(), Some(&bin));
    let started: Vec<(String, String)> = wait_until(
        "both gates to claim their slots",
        WAIT,
        || {
            let rows: Vec<(String, String)> = db(&state)
            .prepare("SELECT block_lineage, started FROM restore_slot WHERE started IS NOT NULL ORDER BY slot")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
            (rows.len() == 2).then_some(rows)
        },
    );
    assert_eq!(
        started,
        [
            (blocks[0].clone(), "continue".into()),
            (blocks[1].clone(), "typed".into())
        ]
    );
    let argv = wait_until("the conversation to start", WAIT, || {
        let mut files = std::fs::read_dir(&runs)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        files
            .pop()
            .map(|file| (files.len(), std::fs::read_to_string(file).unwrap()))
    });
    assert_eq!(argv.0, 0, "more than one process reopened the conversation");
    let argv: Vec<&str> = argv.1.lines().collect();
    assert_eq!(argv[..2], ["--resume", "story"]);
    assert!(
        argv.last().unwrap().starts_with("[rexume "),
        "no continuation prompt: {argv:?}"
    );
    // The viewer pane only has the command typed at its prompt.
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(std::fs::read_dir(&runs).unwrap().count(), 1);
    drop(server);
}
