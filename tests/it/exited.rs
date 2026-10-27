use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use rusqlite::params;
use serde_json::{Value, json};

use crate::common::{Daemon, Server, db, query, wait_until};

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

/// The stand-in's runs, by the order they started, each with its pid and arguments.
fn runs(dir: &Path, count: usize) -> Vec<(i64, String)> {
    wait_until("the agent to run", WAIT, || {
        let mut runs: Vec<(std::time::SystemTime, i64, String)> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let path = e.unwrap().path();
                let pid = path.file_name().unwrap().to_str().unwrap().parse().unwrap();
                let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
                (modified, pid, std::fs::read_to_string(path).unwrap())
            })
            .collect();
        runs.sort();
        (runs.len() == count).then(|| runs.into_iter().map(|(_, p, a)| (p, a)).collect())
    })
}

/// Records the stand-in run `pid` on `instance` as the pane's own program, as its hook would.
fn report(state: &Path, block: &str, instance: &str, pid: i64) {
    db(state)
        .execute(
            "INSERT INTO occupant (block_lineage, runtime, native_id, instance_id, pid, pid_name,
               own_program, keeps_session_env, reported_at)
             VALUES (?1, 'claude', 'story', ?2, ?3, 'claude', 1, 1, 0)
             ON CONFLICT (block_lineage) DO UPDATE SET instance_id = excluded.instance_id,
               pid = excluded.pid",
            params![block, instance, pid],
        )
        .unwrap();
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
    // A stand-in for Claude Code that records how it was started and exits soon after.
    let (bin, ran) = (dir.path().join("bin"), dir.path().join("runs"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&ran).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'/$$\nsleep 1\n",
            ran.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let transcript = dir.path().join("story.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n{ANSWER}\n")).unwrap();

    let mut server = Server::start(dir.path(), Some(&bin));
    let _daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    db(&state)
        .execute(
            "INSERT INTO conversation (runtime, native_id, transcript, resumable, reported_at)
             VALUES ('claude', 'story', ?1, 1, 0)",
            params![transcript.to_string_lossy()],
        )
        .unwrap();
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
    report(&state, &blocks[2], &first, runs(&ran, 1)[0].0);
    assert_eq!(exits(&state, &first, &blocks), [3, -1, 0]);
    wait_until("the layout to be saved", WAIT, || {
        query::<String>(&state, "SELECT doc FROM session")
            .filter(|doc| blocks.iter().all(|b| doc.contains(b.as_str())))
    });

    for restores in 1..=2 {
        let started = restart(&mut server, &state, &bin, restores);
        assert_eq!(started, ["exited", "exited", "agent"], "restart {restores}");
        let reopened = runs(&ran, usize::try_from(restores).unwrap() + 1);
        let (pid, argv) = reopened.last().unwrap();
        assert_eq!(argv.lines().collect::<Vec<_>>(), ["--resume", "story"]);
        let instance = server.instance();
        report(&state, &blocks[2], &instance, *pid);
        assert_eq!(exits(&state, &instance, &blocks), [3, -1, 0]);
    }
    drop(server);
}
