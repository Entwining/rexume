use std::path::Path;
use std::time::Duration;

use rusqlite::params;
use serde_json::json;

use crate::common::{
    Daemon, SHELL, Server, claude, db, query, shell_block, shell_pids, wait_until,
};

const WAIT: Duration = Duration::from_secs(20);
const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"Write a story"}}"#;

#[test]
#[ignore = "needs a Rex server"]
fn a_turn_the_crash_cut_off_continues_once_and_a_second_pane_gets_a_shell() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    // A stand-in for Claude Code that records how it was started, and the provider it was given,
    // and keeps running.
    let (bin, runs) = claude::stand_in(
        dir.path(),
        r#"open(my $f, '>', "$run.provider") or die $!; print $f $ENV{CLAUDE_CODE_USE_VERTEX}; close $f; sleep 600;"#,
    );
    let transcript = dir.path().join("story.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n")).unwrap();
    // The user's shell wraps `claude` in a function, which the reopened conversation goes through.
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(format!(".{SHELL}rc")),
        "claude() { command claude --wrapped \"$@\"; }\n",
    )
    .unwrap();

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

    report(&state, &transcript, &blocks, &fg);

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
            (blocks[1].clone(), "shell".into())
        ]
    );
    let started = claude::runs(&runs, 1);
    assert_eq!(
        started.len(),
        1,
        "more than one process reopened the conversation"
    );
    let argv: Vec<&str> = started[0].1.lines().collect();
    assert_eq!(argv[..4], ["--wrapped", "--rc", "--resume", "story"]);
    assert!(
        argv.last().unwrap().starts_with("[rexume "),
        "no continuation prompt: {argv:?}"
    );
    let provider = runs.join(format!("{}.provider", started[0].0));
    assert_eq!(
        wait_until("the provider to be recorded", WAIT, || {
            std::fs::read_to_string(&provider)
                .ok()
                .filter(|v| !v.is_empty())
        }),
        "1"
    );
    // The viewer pane starts nothing of the conversation.
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        claude::runs(&runs, 1).len(),
        1,
        "the viewer pane reopened it"
    );
    drop(server);
}

/// What the hooks would have reported: the writer pane runs the conversation mid-turn, started
/// with a launch flag and a provider; the viewer pane shows the same conversation, as after
/// resuming it in a second pane.
fn report(state: &Path, transcript: &Path, blocks: &[String], fg: &[i64]) {
    let conn = db(state);
    conn.execute(
        "INSERT INTO conversation (runtime, native_id, transcript, resumable, settings, reported_at)
         VALUES ('claude', 'story', ?1, 1, ?2, 0)",
        params![
            transcript.to_string_lossy(),
            json!({"launch_flags": ["--rc"], "launch_env": ["CLAUDE_CODE_USE_VERTEX=1"]}).to_string()
        ],
    )
    .unwrap();
    for (block, pid) in blocks.iter().zip(fg) {
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
}
