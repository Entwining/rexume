use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};

use super::block::{set_foreground, touch_block};
use super::view::{View, next_read};
use super::{Daemon, FIRST_FILE, Options};
use crate::event::Queue;
use crate::layout::bind_restored;
use crate::rex::{Cli, Pipe, Rex};
use crate::store::Store;

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
    set_foreground(&tx, &lineage, Some(&zsh), Some(1))?;
    set_foreground(&tx, &lineage, Some(&claude), Some(2))?;
    set_foreground(&tx, &lineage, Some(&zsh), Some(1))?;
    let row: (Option<i64>, Option<i64>, Option<i64>) = tx.query_row(
        "SELECT fg_pid, fg_generation, last_exit FROM block WHERE lineage = 'b'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(row, (Some(300), Some(2), None));
    Ok(())
}

/// The record a restored pane's events leave under its lineage, either bound to it before the
/// events or recorded under its own ID and moved by the binding after them.
fn pane_record(bound_first: bool, events: &[(&str, Value)]) -> Result<Vec<SqlValue>> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    store.conn().execute_batch(
        "INSERT INTO block (lineage, instance_id, cwd, columns, rows, fg_pid, fg_generation, status,
           status_pid, status_name, status_at, last_exit)
           VALUES ('b', 'i1', '/old', 80, 24, 100, 5, 'working', 100, 'claude', 50, 1);",
    )?;
    if bound_first {
        store.conn().execute(
            "INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'block:new', 'b')",
            [],
        )?;
    }
    let view = View::new("i2");
    for (name, payload) in events {
        let event = json!({ "event": name, "block_id": "block:new", "payload": payload });
        view.block_event(&mut store, name, &event, payload)?;
    }
    if !bound_first {
        let tx = store.write()?;
        bind_restored(&tx, "i2", "block:new", "b")?;
        tx.commit()?;
    }
    Ok(store
        .conn()
        .query_row("SELECT * FROM block WHERE lineage = 'b'", [], |row| {
            (0..row.as_ref().column_count())
                .map(|i| row.get(i))
                .collect()
        })?)
}

#[test]
fn a_pane_moved_after_its_events_ends_as_if_it_had_been_bound_first() -> Result<()> {
    let working = json!({ "state": "working", "owner": { "pid": 300, "name": "claude" },
                          "updated_at": "2026-10-25T00:00:01Z" });
    let scenarios: [&[(&str, Value)]; 5] = [
        &[],
        &[(
            "process_changed",
            json!({ "foreground": { "pid": 300, "cwd": "/new" }, "generation": 1 }),
        )],
        &[
            ("size_changed", json!({ "columns": 100, "rows": 30 })),
            ("program_status_changed", json!({ "record": working })),
        ],
        // A status the new instance cleared is not the earlier instance's status.
        &[
            ("program_status_changed", json!({ "record": working })),
            (
                "program_status_removed",
                json!({ "ids": [""], "reason": "exit" }),
            ),
        ],
        &[("child_exited", json!({ "exit_code": 2 }))],
    ];
    for events in scenarios {
        assert_eq!(
            pane_record(false, events)?,
            pane_record(true, events)?,
            "{events:?}"
        );
    }
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
    let event = json!({ "event": "size_changed", "block_id": "block:1",
                        "payload": { "columns": 80, "rows": 24 } });
    view.block_event(&mut daemon, "size_changed", &event, &event["payload"])?;
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
                    json!({ "foreground": { "pid": 7, "name": "bash", "cwd": "/work" } })
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
    view.handle(&mut rex, &mut store, &created)?;
    view.flush(&mut rex, &mut store)?;
    drop(rex);
    let asked = server.join().expect("the fake server ran")?;
    assert!(
        !asked.is_empty() && asked.iter().all(|a| a.starts_with(r#""block:new""#)),
        "{asked:?}"
    );
    let cwd: Option<String> = store.conn().query_row(
        "SELECT cwd FROM block WHERE lineage = 'block:new'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(cwd.as_deref(), Some("/work"));
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
        theme: None,
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
