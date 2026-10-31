use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::block::{set_foreground, touch_block};
use super::view::{View, next_read};
use super::{Daemon, FIRST_FILE, Options};
use crate::event::Queue;
use crate::rex::{Cli, Pipe, Rex};
use crate::store::Store;
use crate::task::tests::{PROMPT, record, task_world};

#[test]
fn a_restored_pane_takes_new_foreground_reports_and_ignores_older_ones() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let tx = store.write()?;
    tx.execute_batch(
        "INSERT INTO block (lineage, instance_id, fg_pid, fg_generation, last_exit)
           VALUES ('b', 'i1', 100, 5, 1);
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'nb', 'b');",
    )?;
    // The restored pane's generations count from its own start on the new server.
    let lineage = touch_block(&tx, "i2", "nb")?;
    let zsh = json!({ "pid": 200, "name": "zsh" });
    let claude = json!({ "pid": 300, "name": "claude" });
    set_foreground(&tx, &lineage, Some(&zsh), Some(1), None)?;
    set_foreground(&tx, &lineage, Some(&claude), Some(2), None)?;
    set_foreground(&tx, &lineage, Some(&zsh), Some(1), None)?;
    let row: (Option<i64>, Option<i64>, Option<i64>) = tx.query_row(
        "SELECT fg_pid, fg_generation, last_exit FROM block WHERE lineage = 'b'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(row, (Some(300), Some(2), None));
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

#[test]
fn a_change_is_read_at_once_unless_a_read_was_just_made() {
    let now = Instant::now();
    assert_eq!(next_read(None, now), now);
    assert_eq!(next_read(Some(now - Duration::from_secs(1)), now), now);
    let read = now - Duration::from_millis(100);
    assert_eq!(
        next_read(Some(read), now),
        read + Duration::from_millis(300)
    );
}

/// A pane whose first report reached the stream before its session did has nothing recorded, so
/// reading its session's view reads it; a pane already recorded is left to its events.
#[test]
fn reading_a_view_reads_only_its_unrecorded_panes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let tx = store.write()?;
    touch_block(&tx, "i1", "block:known")?;
    tx.commit()?;
    let (ours, theirs) = UnixStream::pair()?;
    let server = std::thread::spawn(move || -> Result<Vec<String>> {
        let mut requests = BufReader::new(theirs.try_clone()?).lines();
        let mut out = &theirs;
        let mut asked = Vec::new();
        while let Some(line) = requests.next().transpose()? {
            let request: Value = serde_json::from_str(&line)?;
            let params = &request["params"];
            let result = match request["method"].as_str() {
                Some("session.view") => json!({ "label": "s", "windows": [{
                    "window_id": "window:1", "layers": [{ "kind": "tiled", "bounds": [0, 0, 1, 1],
                    "layout": { "split": { "direction": "horizontal", "ratio": 0.5,
                        "before": { "block_id": "block:new" }, "after": { "block_id": "block:known" } } },
                    "blocks": [{ "block_id": "block:new" }, { "block_id": "block:known" }] }] }] }),
                _ => {
                    asked.push(format!("{} {}", params["block_id"], params["method"]));
                    json!({ "foreground": { "pid": 7, "name": "bash" } })
                }
            };
            writeln!(out, "{}", json!({ "id": request["id"], "result": result }))?;
        }
        Ok(asked)
    });
    let mut rex = Rex::Pipe(Pipe::new(ours)?);
    let mut view = View::new("i1");
    let created = json!({ "event": "session_created",
                          "payload": { "session": { "session_id": "session:1" } } });
    view.handle(&mut store, &created)?;
    view.flush(&mut rex, &mut store)?;
    drop(rex);
    let asked = server.join().expect("the fake server ran")?;
    assert!(
        !asked.is_empty() && asked.iter().all(|a| a.starts_with(r#""block:new""#)),
        "{asked:?}"
    );
    let fg_pid: Option<i64> = store.conn().query_row(
        "SELECT fg_pid FROM block WHERE lineage = 'block:new'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(fg_pid, Some(7));
    Ok(())
}

/// The server restores every pane it saved, so a worker's pane a new instance does not show was
/// closed before the earlier instance ended: the task records the close, as its close event would
/// have, also for a worker registered while no daemon ran, which has no row in `block`. A worker
/// registered after the server listed its sessions is not judged by that list.
#[test]
fn a_worker_pane_a_new_instance_does_not_show_is_closed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           worker_instance_id, created_at)
           VALUES ('unrecorded', 'x', 'claude', 'owner', 'w', 0, 'i1', 0),
                  ('later', 'x', 'claude', 'owner', 'x', 0, 'i2', ?1)",
        [crate::store::now_ms() + 60_000],
    )?;
    let (ours, theirs) = UnixStream::pair()?;
    let server = std::thread::spawn(move || -> Result<()> {
        let mut out = &theirs;
        for line in BufReader::new(theirs.try_clone()?).lines() {
            let request: Value = serde_json::from_str(&line?)?;
            let result = match request["method"].as_str() {
                Some("session.list") => json!({ "sessions": [{ "session_id": "session:1" }] }),
                Some("session.view") => json!({ "windows": [{ "window_id": "window:1",
                    "layers": [{ "blocks": [{ "block_id": "a" }] }] }] }),
                _ => json!({}),
            };
            writeln!(out, "{}", json!({ "id": request["id"], "result": result }))?;
        }
        Ok(())
    });
    let mut rex = Rex::Pipe(Pipe::new(ours)?);
    View::new("i2").baseline(&mut rex, &mut store)?;
    drop(rex);
    server.join().expect("the fake server ran")?;
    let panes: Vec<(String, String)> = store
        .conn()
        .prepare("SELECT id, pane_state FROM task ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        panes,
        [
            ("later".to_owned(), "open".to_owned()),
            ("t".to_owned(), "closed".to_owned()),
            ("unrecorded".to_owned(), "closed".to_owned())
        ]
    );
    let blocks: Vec<(String, String)> = store
        .conn()
        .prepare("SELECT lineage, instance_id FROM block")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(blocks, [("a".to_owned(), "i2".to_owned())]);
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
