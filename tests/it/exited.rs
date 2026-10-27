use std::path::Path;
use std::time::Duration;

use rusqlite::params;
use serde_json::{Value, json};

use crate::common::{Daemon, REXUME, Server, claude, db, query, wait_until};

const WAIT: Duration = Duration::from_secs(20);
const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"Write a story"}}"#;
const ANSWER: &str = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Done."}]}}"#;

/// A pane created to run `command` that stays open after it exits.
fn kept_block(label: &str, cwd: &Path, command: &[&str]) -> Value {
    json!({ "block": { "flavor": "com.superlogical.terminal.shell", "label": label,
        "options": { "cwd": cwd, "command": command,
                     "exit": { "on_completion": false } } } })
}

/// The exit each pane's record shows on `instance`, once every pane has one.
fn exits(state: &Path, instance: &str, blocks: &[String]) -> Vec<i64> {
    wait_until("each pane to show its exit", WAIT, || {
        blocks
            .iter()
            .map(|block| {
                query(
                    state,
                    &format!(
                        "SELECT last_exit FROM block WHERE lineage = '{block}'
                         AND instance_id = '{instance}' AND last_exit IS NOT NULL"
                    ),
                )
            })
            .collect()
    })
}

/// Whether the hook of run `pid` recorded it on `instance` as the pane's own program. The hook
/// finishes before the run exits.
fn own_program(state: &Path, block: &str, instance: &str, pid: i64) -> Option<i64> {
    query(
        state,
        &format!(
            "SELECT own_program FROM occupant WHERE block_lineage = '{block}'
             AND instance_id = '{instance}' AND pid = {pid} AND own_program IS NOT NULL"
        ),
    )
}

/// Restarts the server after a crash and returns how each slot of the new restore started.
fn restart(server: &mut Server, state: &Path, bin: &Path, restores: i64) -> Vec<String> {
    server.kill();
    wait_until("the epoch to end", WAIT, || {
        query::<i64>(
            state,
            "SELECT COUNT(*) FROM epoch WHERE end_reason = 'server_gone'",
        )
        .filter(|n| *n == restores)
    });
    let dir = server.dir.clone();
    *server = Server::start(&dir, Some(bin));
    let instance = server.instance();
    wait_until("every gate to claim its slot", WAIT, || {
        let started: Vec<String> = db(state)
            .prepare(
                "SELECT started FROM restore_slot WHERE instance_id = ?1 AND started IS NOT NULL
                 ORDER BY slot",
            )
            .unwrap()
            .query_map(params![instance], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        (started.len() == 3).then_some(started)
    })
}

/// A pane whose own program exited comes back exited with the same status, and one whose own
/// program was a conversation that exited reopens it without a prompt, in a pane that again stays
/// open after it exits: a second restart finds each pane as the first left it.
#[test]
#[ignore = "needs a Rex server"]
fn an_exited_pane_comes_back_the_same_after_each_restart() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    // A stand-in for Claude Code that reports itself through the hook, as Claude Code does on
    // its start, and exits soon after.
    let transcript = dir.path().join("story.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n{ANSWER}\n")).unwrap();
    let hook = json!({ "session_id": "story", "transcript_path": transcript }).to_string();
    let (bin, ran) = claude::stand_in(
        dir.path(),
        &format!(
            "open(my $hook, '|-', '{REXUME}', 'report', 'claude', '--state-dir', '{}') or die $!;\n\
             print $hook '{hook}';\nclose $hook;\nsleep 1;",
            state.display()
        ),
    );

    let mut server = Server::start(dir.path(), Some(&bin));
    let _daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let claude = bin.join("claude");
    let claude = claude.to_str().unwrap();
    let created = server.call(
        "session.create",
        &json!({ "label": "exits", "initial_windows": [{ "layout": { "split": { "direction": "horizontal",
            "ratio": 0.5, "before": kept_block("failed", dir.path(), &["sh", "-c", "exit 3"]),
            "after": { "split": { "direction": "vertical", "ratio": 0.5,
                "before": kept_block("killed", dir.path(), &["sh", "-c", "kill -TERM $$"]),
                "after": kept_block("agent", dir.path(), &[claude, "Write a story"]) } } } } }] }),
    );
    let blocks: Vec<String> = created["initial_windows"][0]["block_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_str().unwrap().to_owned())
        .collect();
    let first = server.instance();
    assert_eq!(exits(&state, &first, &blocks), [3, -1, 0]);
    assert_eq!(
        own_program(&state, &blocks[2], &first, claude::runs(&ran, 1)[0].0),
        Some(1)
    );
    // The hook found the record as long as the stand-in left it before it started.
    let length = std::fs::metadata(&transcript).unwrap().len();
    assert_eq!(
        query::<i64>(
            &state,
            &format!(
                "SELECT start_position FROM occupant WHERE block_lineage = '{}'",
                blocks[2]
            )
        ),
        Some(i64::try_from(length).unwrap())
    );
    wait_until("the layout to be saved", WAIT, || {
        query::<String>(&state, "SELECT doc FROM session")
            .filter(|doc| blocks.iter().all(|b| doc.contains(b.as_str())))
    });

    for restores in 1..=2 {
        let started = restart(&mut server, &state, &bin, restores);
        assert_eq!(started, ["exited", "exited", "agent"], "restart {restores}");
        let count = usize::try_from(restores).unwrap() + 1;
        let reopened = claude::runs(&ran, count);
        assert_eq!(reopened.len(), count, "restart {restores}");
        let (pid, argv) = reopened.last().unwrap();
        assert_eq!(argv.lines().collect::<Vec<_>>(), ["--resume", "story"]);
        let instance = server.instance();
        assert_eq!(exits(&state, &instance, &blocks), [3, -1, 0]);
        assert_eq!(own_program(&state, &blocks[2], &instance, *pid), Some(1));
    }
    drop(server);
}
