use std::path::Path;
use std::sync::{Arc, Mutex};

use super::*;
use crate::task::tests::task_world;
use server::{MOVED_BACK, SHELL, STARTUP, Server, TYPED, connect};

mod continuation;
mod opened;
mod server;

fn restarted(dir: &Path, a: &str, b: &str) -> Result<(Store, Arc<Mutex<Server>>, Rex)> {
    let store = task_world(dir, &dir.join("worker.jsonl"), None)?;
    store.conn().execute_batch(
        "UPDATE epoch SET ended_at = 1, end_reason = 'disconnected';
         INSERT INTO epoch (id, instance_id, started_at) VALUES (2, 'i2', 2);",
    )?;
    let server = Arc::new(Mutex::new(Server {
        process: HashMap::from([
            ("a".to_owned(), serde_json::from_str(a)?),
            ("b".to_owned(), serde_json::from_str(b)?),
        ]),
        ..Server::default()
    }));
    let rex = connect(&server)?;
    Ok((store, server, rex))
}

fn new_restorer() -> Restorer {
    Restorer::new(vec!["rexume".into(), "gate".into()])
}

fn restore_of(store: &Store, block: &str) -> Result<(String, String, Option<String>)> {
    Ok(store.conn().query_row(
        "SELECT id, state, note FROM restore WHERE block_lineage = ?1",
        params![block],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

fn foreground_moved(store: &Store) -> Result<()> {
    store
        .conn()
        .execute_batch("UPDATE block SET fg_generation = fg_generation + 1 WHERE lineage = 'a'")?;
    Ok(())
}

#[test]
fn the_gate_is_typed_once_when_the_shell_is_ready() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, server, mut rex) = restarted(dir.path(), STARTUP, SHELL.as_str())?;
    let mut restorer = new_restorer();
    restorer.start(&mut rex, &mut store, "i2")?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    let (id, state, note) = restore_of(&store, "a")?;
    assert_eq!(
        (state.as_str(), note.as_deref()),
        ("planned", Some("sleep runs in the pane"))
    );
    assert!(restorer.deadline().is_some());
    assert!(restore_of(&store, "b").is_err());

    server
        .lock()
        .unwrap()
        .process
        .insert("a".into(), serde_json::from_str(SHELL.as_str())?);
    restorer.reconcile(&mut rex, &mut store, "i2");
    assert!(server.lock().unwrap().written.iter().all(|(b, _)| b != "a"));
    foreground_moved(&store)?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    assert_eq!(restore_of(&store, "a")?.1, "typed");
    restorer.reconcile(&mut rex, &mut store, "i2");
    let mut again = new_restorer();
    again.start(&mut rex, &mut store, "i2")?;
    again.reconcile(&mut rex, &mut store, "i2");
    let written: Vec<(String, String)> = server.lock().unwrap().written.clone();
    let into_a: Vec<&String> = written
        .iter()
        .filter(|(b, _)| b == "a")
        .map(|(_, l)| l)
        .collect();
    assert_eq!(into_a, [&format!("\u{5}\u{15} rexume gate {id}\r")]);
    Ok(())
}

#[test]
fn a_detached_pane_is_restored() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, server, mut rex) = restarted(dir.path(), SHELL.as_str(), SHELL.as_str())?;
    server.lock().unwrap().detached.push("a");
    let mut restorer = new_restorer();
    restorer.start(&mut rex, &mut store, "i2")?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    assert_eq!(restore_of(&store, "a")?.1, "typed");
    Ok(())
}

#[test]
fn a_pane_with_text_at_its_prompt_is_left_to_the_user() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, server, mut rex) = restarted(dir.path(), SHELL.as_str(), SHELL.as_str())?;
    server.lock().unwrap().screen = Some(TYPED);
    let mut restorer = new_restorer();
    restorer.start(&mut rex, &mut store, "i2")?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    assert_eq!(restore_of(&store, "a")?.1, "planned");
    store.conn().execute(
        "UPDATE restore SET created_at = created_at - ?1",
        params![millis(PANE_WAIT)],
    )?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    let (_, state, note) = restore_of(&store, "a")?;
    assert_eq!(
        (state.as_str(), note.as_deref()),
        (
            "skipped",
            Some("text is typed in the pane, or its prompt does not end in a space")
        )
    );
    assert!(server.lock().unwrap().written.is_empty());
    Ok(())
}

#[test]
fn a_shell_reading_its_startup_files_takes_the_line() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, server, mut rex) = restarted(dir.path(), SHELL.as_str(), SHELL.as_str())?;
    {
        let mut server = server.lock().unwrap();
        server.screen = Some(TYPED);
        server.startup = true;
    }
    let mut restorer = new_restorer();
    restorer.start(&mut rex, &mut store, "i2")?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    assert_eq!(restore_of(&store, "a")?.1, "typed");
    Ok(())
}

#[test]
fn a_typed_line_ends_by_its_answer_or_its_gate() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, server, mut rex) = restarted(dir.path(), SHELL.as_str(), SHELL.as_str())?;
    server.lock().unwrap().refuse_write = Some("no (status=409 code=conflict)");
    let mut restorer = new_restorer();
    restorer.start(&mut rex, &mut store, "i2")?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    assert_eq!(restore_of(&store, "a")?.1, "failed");

    let dir = tempfile::tempdir()?;
    let (mut store, server, mut rex) = restarted(dir.path(), SHELL.as_str(), SHELL.as_str())?;
    server.lock().unwrap().refuse_write = Some("the connection ended");
    let mut restorer = new_restorer();
    restorer.start(&mut rex, &mut store, "i2")?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    assert_eq!(restore_of(&store, "a")?.1, "typed");
    store.conn().execute(
        "UPDATE restore SET typed_at = typed_at - ?1",
        params![millis(CLAIM_WAIT)],
    )?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    assert_eq!(restore_of(&store, "a")?.1, "unknown");
    assert!(restorer.deadline().is_none());
    Ok(())
}

#[test]
fn a_retired_workers_pane_is_closed_unless_it_is_in_use() -> Result<()> {
    for (shown, screen, focused, refuse, reported, kept) in [
        (SHELL.as_str(), None, None, None, false, None),
        (
            STARTUP,
            None,
            None,
            None,
            false,
            Some("sleep runs in the pane"),
        ),
        (
            SHELL.as_str(),
            Some(MOVED_BACK),
            None,
            None,
            false,
            Some("typed after the cursor"),
        ),
        (
            SHELL.as_str(),
            None,
            Some("b"),
            None,
            false,
            Some("the worker's pane has focus"),
        ),
        (
            SHELL.as_str(),
            None,
            None,
            Some("input_mode"),
            false,
            Some("refused"),
        ),
        (
            SHELL.as_str(),
            None,
            None,
            None,
            true,
            Some("a conversation was opened in the pane"),
        ),
    ] {
        let dir = tempfile::tempdir()?;
        let (mut store, server, mut rex) = restarted(dir.path(), SHELL.as_str(), shown)?;
        server.lock().unwrap().focused = focused;
        server.lock().unwrap().screen = screen;
        store.conn().execute_batch(
            "UPDATE block SET fg_pid = NULL, last_exit = 0 WHERE lineage = 'b';
             UPDATE task SET result_file = '/r', worker_runtime = NULL, worker_native_id = NULL,
               released_at = 120;
             UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = '/r',
               finished_at = 100, finished_instance_id = 'i1', finished_fg_changes = 0, acked_at = 120;",
        )?;
        let mut restorer = new_restorer();
        restorer.start(&mut rex, &mut store, "i2")?;
        server.lock().unwrap().refuse = refuse;
        if reported {
            let opened = "UPDATE occupant SET instance_id = 'i2' WHERE block_lineage = 'b'";
            store.conn().execute_batch(opened)?;
        }
        restorer.reconcile(&mut rex, &mut store, "i2");
        let pane = |store: &Store| -> Result<(String, Option<String>)> {
            Ok(store
                .conn()
                .query_row("SELECT pane_state, pane_note FROM task", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?)
        };
        let Some(kept) = kept else {
            assert_eq!(pane(&store)?, ("closed".into(), None));
            assert_eq!(server.lock().unwrap().closed, ["b"]);
            continue;
        };
        assert_eq!(pane(&store)?.0, "closing");
        restorer.retired[0].until -= millis(PANE_WAIT);
        restorer.reconcile(&mut rex, &mut store, "i2");
        let (state, note) = pane(&store)?;
        assert_eq!(state, "kept");
        assert!(note.is_some_and(|n| n.contains(kept)), "{kept}");
        assert!(server.lock().unwrap().closed.is_empty());
        assert!(restorer.retired.is_empty());
    }
    Ok(())
}

#[test]
fn a_pane_held_by_an_earlier_epoch_is_let_go() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, _server, mut rex) = restarted(dir.path(), SHELL.as_str(), STARTUP)?;
    store.conn().execute_batch(
        "UPDATE block SET fg_pid = NULL, last_exit = 0 WHERE lineage = 'b';
         UPDATE task SET result_file = '/r', worker_runtime = NULL, worker_native_id = NULL,
           released_at = 120;
         UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = '/r',
           finished_at = 100, finished_instance_id = 'i1', finished_fg_changes = 0, acked_at = 120;",
    )?;
    new_restorer().start(&mut rex, &mut store, "i2")?;
    let pane = |store: &Store| -> Result<String> {
        Ok(store
            .conn()
            .query_row("SELECT pane_state FROM task", [], |row| row.get(0))?)
    };
    assert_eq!(pane(&store)?, "closing");
    store
        .conn()
        .execute("UPDATE block SET instance_id = 'i2'", [])?;
    let mut next = new_restorer();
    next.start(&mut rex, &mut store, "i2")?;
    assert_eq!(pane(&store)?, "open");
    assert!(next.retired.is_empty());
    Ok(())
}
