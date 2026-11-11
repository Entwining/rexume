use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixListener;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use super::block::{set_foreground, touch_block};
use super::view::View;
use super::{Daemon, FIRST_FILE, Options};
use crate::event::Queue;
use crate::rex::Cli;
use crate::store::Store;

mod view;

/// A pane restored on a new server counts its foreground changes and its reports from its own
/// start there. Only another process counts: a later report or a read without a generation about
/// the recorded process does not, and a report older than the one recorded is ignored.
#[test]
fn a_restored_pane_counts_new_foreground_processes_and_ignores_older_reports() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let tx = store.write()?;
    tx.execute_batch(
        "INSERT INTO block (lineage, instance_id, fg_pid, fg_generation, fg_changes, last_exit)
           VALUES ('b', 'i1', 100, 5, 4, 1);
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'nb', 'b');",
    )?;
    let lineage = touch_block(&tx, "i2", "nb")?;
    let row: (Option<i64>, Option<i64>, i64, Option<i64>) = tx.query_row(
        "SELECT fg_pid, fg_generation, fg_changes, last_exit FROM block WHERE lineage = 'b'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    assert_eq!(row, (None, None, 0, None));
    let zsh = json!({ "pid": 200, "name": "zsh" });
    let claude = json!({ "pid": 300, "name": "claude" });
    for (foreground, generation, recorded) in [
        (&zsh, Some(1), (Some(200), Some(1), 1)),
        (&claude, Some(2), (Some(300), Some(2), 2)),
        (&zsh, Some(1), (Some(300), Some(2), 2)),
        (&claude, Some(3), (Some(300), Some(3), 2)),
        (&claude, None, (Some(300), Some(3), 2)),
        (&zsh, None, (Some(200), Some(3), 3)),
    ] {
        set_foreground(&tx, &lineage, Some(foreground), generation, None)?;
        let row: (Option<i64>, Option<i64>, i64) = tx.query_row(
            "SELECT fg_pid, fg_generation, fg_changes FROM block WHERE lineage = 'b'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(row, recorded, "{foreground} at {generation:?}");
    }
    Ok(())
}

/// A conversation the gate started through a shell function runs as a child of the process the
/// gate claimed its restore with, maybe after another command the function runs first, and the
/// pane's occupant becomes each such child in turn; one the shell runs in place, after a command
/// its startup files ran, is that process again. An occupant that reported itself, or one
/// recorded on an earlier instance, stays as it is.
#[test]
fn a_gate_recorded_occupant_becomes_the_foreground_its_process_started() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let tx = store.write()?;
    tx.execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('codex', 'c', 1, 0), ('codex', 'n', 1, 0), ('codex', 'o', 1, 0),
                  ('codex', 'p', 1, 0);
         INSERT INTO block (lineage, instance_id)
           VALUES ('gate', 'i2'), ('named', 'i2'), ('old', 'i2'), ('in-place', 'i2');
         INSERT INTO restore (id, instance_id, block_lineage, start, runtime, native_id, plan, state,
           created_at, typed_at, claimed_at, started, pid)
           VALUES ('r1', 'i2', 'gate', 'agent', 'codex', 'c', '{}', 'claimed', 0, 0, 0, 'agent', 200),
                  ('r2', 'i2', 'named', 'agent', 'codex', 'n', '{}', 'claimed', 0, 0, 0, 'agent', 200),
                  ('r3', 'i1', 'old', 'agent', 'codex', 'o', '{}', 'claimed', 0, 0, 0, 'agent', 200),
                  ('r4', 'i2', 'in-place', 'agent', 'codex', 'p', '{}', 'claimed', 0, 0, 0, 'agent', 600);
         INSERT INTO occupant (block_lineage, runtime, native_id, instance_id, pid, pid_name,
           keeps_session_env, reported_at)
           VALUES ('gate', 'codex', 'c', 'i2', 200, NULL, 0, 0),
                  ('named', 'codex', 'n', 'i2', 200, 'codex', 0, 0),
                  ('old', 'codex', 'o', 'i1', 200, NULL, 0, 0),
                  ('in-place', 'codex', 'p', 'i2', 600, NULL, 0, 0);",
    )?;
    let first = json!({ "pid": 300, "name": "sleep" });
    for lineage in ["gate", "named", "old"] {
        set_foreground(&tx, lineage, Some(&first), Some(1), Some(200))?;
    }
    set_foreground(
        &tx,
        "gate",
        Some(&json!({ "pid": 400, "name": "codex" })),
        Some(2),
        Some(200),
    )?;
    set_foreground(
        &tx,
        "in-place",
        Some(&json!({ "pid": 700 })),
        Some(1),
        Some(600),
    )?;
    set_foreground(
        &tx,
        "in-place",
        Some(&json!({ "pid": 600 })),
        Some(2),
        Some(1),
    )?;
    // A foreground some other process started leaves the occupant alone.
    set_foreground(&tx, "gate", Some(&json!({ "pid": 500 })), Some(3), Some(1))?;
    let pids: Vec<(String, i64)> = tx
        .prepare("SELECT block_lineage, pid FROM occupant ORDER BY block_lineage")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        pids,
        [
            ("gate".to_owned(), 400),
            ("in-place".to_owned(), 600),
            ("named".to_owned(), 200),
            ("old".to_owned(), 200)
        ]
    );
    Ok(())
}

#[test]
fn an_event_the_daemon_does_not_record_writes_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut daemon = Store::open(dir.path())?;
    let observer = Store::open(dir.path())?;
    let view = View::new("i1");
    let event = json!({ "event": "title_changed", "block_id": "block:1", "payload": {} });
    let before = observer.data_version()?;
    assert!(!view.block_event(&mut daemon, "title_changed", &event, &event["payload"])?);
    assert_eq!(observer.data_version()?, before);
    let event = json!({ "event": "child_exited", "block_id": "block:1",
                        "payload": { "exit_code": 2 } });
    view.block_event(&mut daemon, "child_exited", &event, &event["payload"])?;
    assert_ne!(
        observer.data_version()?,
        before,
        "a recorded event is a commit"
    );
    Ok(())
}

/// The daemon also waits between epochs, while it backs off or waits for the server.
#[test]
fn any_wait_drops_the_watch_of_a_replaced_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let options = Options {
        state_dir: dir.path().to_owned(),
        cli: Cli {
            bin: "rex".into(),
            endpoint: String::new(),
        },
        exe: dir.path().join("rexume"),
        home: dir.path().to_owned(),
    };
    let socket = dir.path().join("daemon.sock");
    let mut daemon = Daemon {
        options: &options,
        store: Store::open(dir.path())?,
        queue: Queue::new()?,
        listener: UnixListener::bind(&socket)?,
        socket,
        files: HashMap::new(),
        via_dir: HashSet::new(),
        next_file: FIRST_FILE + 1,
        pids: HashSet::new(),
    };
    let path = dir.path().join("t.jsonl");
    std::fs::write(&path, "a\n")?;
    assert!(daemon.queue.file(&path, FIRST_FILE)?);
    daemon.files.insert(path.clone(), FIRST_FILE);
    std::fs::write(dir.path().join("new"), "b\n")?;
    std::fs::rename(dir.path().join("new"), &path)?;
    daemon.wait(Some(Duration::from_secs(1)))?;
    assert!(daemon.files.is_empty());
    Ok(())
}
