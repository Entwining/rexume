use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::super::block::touch_block;
use super::super::view::{View, next_read};
use crate::rex::{Pipe, Rex};
use crate::store::Store;
use crate::task::tests::{PROMPT, record, task_world};

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
/// listing its session's panes reads it, in a window or detached; a pane already recorded is left
/// to its events.
#[test]
fn listing_a_session_reads_only_its_unrecorded_panes() -> Result<()> {
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
                Some("session.list_blocks") => json!({ "blocks": [
                    { "block_id": "block:known", "placement": "placed" },
                    { "block_id": "block:new", "placement": "placed" },
                    { "block_id": "block:quiet", "placement": "detached" }] }),
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
    let read: HashSet<&str> = asked.iter().map(|a| a.split(' ').next().unwrap()).collect();
    assert_eq!(read, HashSet::from([r#""block:new""#, r#""block:quiet""#]));
    let fg_pids: Vec<Option<i64>> = store
        .conn()
        .prepare("SELECT fg_pid FROM block WHERE lineage IN ('block:new', 'block:quiet')")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(fg_pids, [Some(7), Some(7)]);
    Ok(())
}

/// The server restores every pane it saved, so a worker's pane a new instance does not list was
/// closed before the earlier instance ended: the task records the close, as its close event would
/// have, also for a worker registered while no daemon ran, which has no row in `block`. A worker
/// registered after the server listed its sessions is not judged by that list, and one in a pane
/// in no window is listed like the rest.
#[test]
fn a_worker_pane_a_new_instance_does_not_list_is_closed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           worker_instance_id, created_at)
           VALUES ('unrecorded', 'x', 'claude', 'owner', 'w', 0, 'i1', 0),
                  ('detached', 'x', 'claude', 'owner', 'd', 0, 'i1', 0),
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
                Some("session.list_blocks") => json!({ "blocks": [
                    { "block_id": "a", "placement": "placed" },
                    { "block_id": "d", "placement": "detached" }] }),
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
            ("detached".to_owned(), "open".to_owned()),
            ("later".to_owned(), "open".to_owned()),
            ("t".to_owned(), "closed".to_owned()),
            ("unrecorded".to_owned(), "closed".to_owned())
        ]
    );
    let blocks: Vec<(String, String)> = store
        .conn()
        .prepare("SELECT lineage, instance_id FROM block ORDER BY lineage")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        blocks,
        [
            ("a".to_owned(), "i2".to_owned()),
            ("d".to_owned(), "i2".to_owned())
        ]
    );
    Ok(())
}

/// A list of panes carries no revision, so the revision a view change reports stands for the list
/// read after it: a change at that revision or an earlier one, which can arrive late, lists
/// nothing more, and a later one lists again.
#[test]
fn a_view_change_at_a_revision_already_reported_lists_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let (ours, theirs) = UnixStream::pair()?;
    let server = std::thread::spawn(move || -> Result<()> {
        let mut out = &theirs;
        for line in BufReader::new(theirs.try_clone()?).lines() {
            let request: Value = serde_json::from_str(&line?)?;
            writeln!(
                out,
                "{}",
                json!({ "id": request["id"], "result": { "blocks": [] } })
            )?;
        }
        Ok(())
    });
    let mut rex = Rex::Pipe(Pipe::new(ours)?);
    let mut view = View::new("i1");
    let changed = |revision: i64| {
        json!({ "event": "session_view_changed", "session_id": "session:1",
                "payload": { "revision": revision } })
    };
    view.handle(&mut store, &changed(2))?;
    view.flush(&mut rex, &mut store)?;
    assert_eq!(view.deadline(), None);
    view.handle(&mut store, &changed(2))?;
    view.handle(&mut store, &changed(1))?;
    assert_eq!(view.deadline(), None);
    view.handle(&mut store, &changed(3))?;
    assert!(view.deadline().is_some());
    drop(rex);
    server.join().expect("the fake server ran")?;
    Ok(())
}
