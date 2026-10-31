//! Rexume reopens conversations in the panes the server restored itself.

use std::path::Path;
use std::time::Duration;

use rusqlite::params;
use serde_json::{Value, json};

use crate::common::{
    Daemon, REXUME, SHELL, Server, claude, db, query, shell_block, shell_pids, wait_until,
};

const WAIT: Duration = Duration::from_secs(20);
/// Rex saves a change to its sessions about a second later; a server killed sooner restores them
/// as they were before it.
const REX_SAVES: Duration = Duration::from_secs(3);
const PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"Write a story"}}"#;

/// A session `agents` of two panes, `writer` and `viewer`, each recorded with its shell as the
/// foreground; returns the session and the panes.
fn agents(server: &Server, state: &Path, cwd: &Path) -> (String, Vec<String>, Vec<i64>) {
    let created = server.call(
        "session.create",
        &json!({ "label": "agents", "initial_windows": [{ "layout": { "split": { "direction": "horizontal",
            "ratio": 0.5, "before": shell_block("writer", cwd), "after": shell_block("viewer", cwd) } } }] }),
    );
    let session = created["session_id"].as_str().unwrap().to_owned();
    let blocks: Vec<String> = created["initial_windows"][0]["block_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_str().unwrap().to_owned())
        .collect();
    let fg = shell_pids(server, state, &session, &[&blocks[0], &blocks[1]]);
    (session, blocks, fg)
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

/// The restores the daemon planned on the server's current instance, by pane, with what each
/// started.
fn restores(server: &Server, state: &Path) -> Vec<(String, String, Option<String>)> {
    db(state)
        .prepare("SELECT block_lineage, state, started FROM restore WHERE instance_id = ?1")
        .unwrap()
        .query_map([server.instance()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn screen(server: &Server, session: &str, block: &str) -> String {
    server.call(
        "block.method",
        &json!({ "session_id": session, "block_id": block, "method": "format" }),
    )["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn restart(server: &mut Server, state: &Path, bin: &Path, ended: i64, crash: bool) {
    std::thread::sleep(REX_SAVES);
    if crash {
        server.kill();
    } else {
        server.stop();
    }
    wait_until("the epoch to end", WAIT, || {
        query::<i64>(
            state,
            "SELECT COUNT(*) FROM epoch WHERE end_reason = 'server_gone'",
        )
        .filter(|n| *n == ended)
    });
    let dir = server.dir.clone();
    *server = Server::start(&dir, Some(bin));
}

/// A turn a crash cut off continues once, in the pane it ran in, which the server restored under
/// the same ID; the other pane that showed the conversation keeps its shell, and a daemon that
/// starts again types nothing more. After a stop, the conversation reopens there again without
/// the prompt.
#[test]
#[ignore = "needs a Rex server"]
fn a_turn_the_crash_cut_off_continues_once_in_its_own_pane() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let transcript = dir.path().join("story.jsonl");
    // A stand-in for Claude Code that records how it was started, and the provider it was given,
    // reports itself as its session-start hook would, and keeps running.
    let hook = json!({ "session_id": "story", "transcript_path": transcript });
    let (bin, runs) = claude::stand_in(
        dir.path(),
        &(r#"open(my $f, '>', "$run.provider") or die $!; print $f $ENV{CLAUDE_CODE_USE_VERTEX}; close $f; "#.to_owned()
            + &format!(
                "open(my $r, '|-', '{REXUME}', 'report', 'claude', '--state-dir', '{}') or die $!; print $r '{hook}'; close $r; sleep 600;",
                state.display()
            )),
    );
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
    let mut daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let (session, blocks, fg) = agents(&server, &state, dir.path());
    report(&state, &transcript, &blocks, &fg);

    restart(&mut server, &state, &bin, 1, true);
    let claimed = wait_until("the gate to claim the restore", WAIT, || {
        Some(restores(&server, &state))
            .filter(|r| !r.is_empty() && r.iter().all(|(_, s, _)| s == "claimed"))
    });
    assert_eq!(
        claimed,
        [(blocks[0].clone(), "claimed".into(), Some("continue".into()))]
    );
    let started = claude::runs(&runs, 1);
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

    // A daemon that starts again on the same instance types nothing more, here or in the viewer.
    daemon.stop();
    daemon = Daemon::start(&server, &state);
    wait_until("the second daemon's epoch", WAIT, || {
        query::<i64>(&state, "SELECT COUNT(*) FROM epoch").filter(|n| *n == 3)
    });
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        claude::runs(&runs, 1).len(),
        1,
        "the conversation reopened twice"
    );
    assert!(!screen(&server, &session, &blocks[1]).contains("gate"));

    restart(&mut server, &state, &bin, 2, false);
    wait_until("the gate to claim the restore", WAIT, || {
        Some(restores(&server, &state))
            .filter(|r| r.len() == 1 && r[0].1 == "claimed")
            .map(|_| ())
    });
    assert_eq!(restores(&server, &state)[0].2.as_deref(), Some("agent"));
    let reopened = claude::runs(&runs, 2);
    assert!(
        !reopened[1].1.contains("[rexume "),
        "a stopped server's turn continued: {}",
        reopened[1].1
    );
    drop(daemon);
}

/// Text typed in a restored pane before the daemon reads it is the user's: no line is typed over
/// it while the pane waits.
#[test]
#[ignore = "needs a Rex server"]
fn text_typed_in_a_restored_pane_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (bin, runs) = claude::stand_in(dir.path(), "sleep 600;");
    let transcript = dir.path().join("story.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n")).unwrap();
    let mut server = Server::start(dir.path(), Some(&bin));
    let mut daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let (session, blocks, fg) = agents(&server, &state, dir.path());
    report(&state, &transcript, &blocks[..1], &fg[..1]);
    daemon.stop();
    std::thread::sleep(REX_SAVES);
    server.kill();
    server = Server::start(dir.path(), Some(&bin));
    let pane = |method: &str, args: Value| {
        server.call(
            "block.method",
            &json!({ "session_id": session, "block_id": blocks[0], "method": method, "args": args }),
        )
    };
    wait_until("the restored shell's prompt", WAIT, || {
        (pane("input_mode", json!({}))["canonical"] == false).then_some(())
    });
    pane("write", json!({ "data": "echo kept" }));
    wait_until("the typed text to show", WAIT, || {
        screen(&server, &session, &blocks[0])
            .contains("echo kept")
            .then_some(())
    });
    daemon = Daemon::start(&server, &state);
    let note = wait_until("the restore to wait", WAIT, || {
        query::<String>(
            &state,
            "SELECT note FROM restore WHERE state = 'planned' AND note IS NOT NULL",
        )
    });
    assert_eq!(
        note,
        "text is typed in the pane, or its prompt does not end in a space"
    );
    std::thread::sleep(Duration::from_secs(2));
    assert!(!screen(&server, &session, &blocks[0]).contains("gate"));
    assert!(std::fs::read_dir(&runs).unwrap().next().is_none());
    drop(daemon);
}

/// An epoch the daemon ends on an error shows in `status` until the daemon observes the server
/// again without one. A trigger refuses every new binding, which the restore after the restart
/// needs.
#[test]
#[ignore = "needs a Rex server"]
fn status_shows_the_error_that_ended_the_latest_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mut server = Server::start(dir.path(), Some(&bin));
    let _daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    server.call(
        "session.create",
        &json!({ "label": "work", "initial_windows": [{ "layout": shell_block("main", dir.path()) }] }),
    );
    let recorded = |server: &Server| {
        let instance = server.instance();
        wait_until("the pane to be recorded", WAIT, || {
            query::<i64>(
                &state,
                &format!("SELECT 1 FROM block WHERE instance_id = '{instance}'"),
            )
        });
    };
    recorded(&server);
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
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["epoch_error"].clone()
    };
    restart(&mut server, &state, &bin, 1, false);
    let shown = wait_until("the error to show", WAIT, || {
        Some(epoch_error()).filter(|e| !e.is_null())
    });
    assert!(
        shown["error"].as_str().unwrap().contains("refused"),
        "{shown}"
    );

    db(&state).execute_batch("DROP TRIGGER refuse").unwrap();
    recorded(&server);
    assert_eq!(epoch_error(), json!(null));
}
