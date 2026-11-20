use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixStream;

use anyhow::Result;
use serde_json::{Value, json};

use super::super::block::touch_block;
use super::super::view::View;
use crate::rex::{Pipe, Rex};
use crate::store::Store;
use crate::task::tests::{PROMPT, record, task_world};

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

#[test]
fn a_view_change_wakes_the_task_pass_and_lists_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let (ours, theirs) = UnixStream::pair()?;
    let server = std::thread::spawn(move || -> Result<Vec<String>> {
        let mut out = &theirs;
        let mut asked = Vec::new();
        for line in BufReader::new(theirs.try_clone()?).lines() {
            let request: Value = serde_json::from_str(&line?)?;
            asked.push(request["method"].to_string());
            writeln!(
                out,
                "{}",
                json!({ "id": request["id"], "result": { "blocks": [] } })
            )?;
        }
        Ok(asked)
    });
    let mut rex = Rex::Pipe(Pipe::new(ours)?);
    let mut view = View::new("i1");
    let changed = json!({ "event": "session_view_changed", "session_id": "session:1",
                          "payload": { "revision": 1 } });
    assert!(view.handle(&mut store, &changed)?);
    view.flush(&mut rex, &mut store)?;
    drop(rex);
    assert_eq!(
        server.join().expect("the fake server ran")?,
        Vec::<String>::new()
    );
    Ok(())
}
