use std::path::Path;
use std::sync::{Arc, Mutex};

use super::*;
use crate::task::tests::{PROMPT, task_world};
use server::{SHELL, STARTUP, Server, TYPED, connect};

mod server;

/// Instance `i1`'s panes `a`, the orchestrator's conversation `owner` in its foreground, and `b`,
/// a worker; the server restarted as `i2` and shows both with the shell given.
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

/// Moves pane `a`'s recorded foreground, as the server's event about it does.
fn foreground_moved(store: &Store) -> Result<()> {
    store
        .conn()
        .execute_batch("UPDATE block SET fg_generation = fg_generation + 1 WHERE lineage = 'a'")?;
    Ok(())
}

/// The line goes in once the shell is the pane's foreground, and once only: neither another pass
/// nor a daemon that starts again types it again.
#[test]
fn the_gate_is_typed_once_when_the_shell_is_ready() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, server, mut rex) = restarted(dir.path(), STARTUP, SHELL)?;
    let mut restorer = new_restorer();
    restorer.start(&mut rex, &mut store, "i2")?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    let (id, state, note) = restore_of(&store, "a")?;
    assert_eq!(
        (state.as_str(), note.as_deref()),
        ("planned", Some("sleep runs in the pane"))
    );
    assert!(restorer.deadline().is_some());
    // The worker's conversation wrote no record yet, so its pane keeps the shell.
    assert!(restore_of(&store, "b").is_err());

    server
        .lock()
        .unwrap()
        .process
        .insert("a".into(), serde_json::from_str(SHELL)?);
    // Nothing recorded the change yet, so the pane is not read again.
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

/// Text at the prompt is the user's: nothing is typed over it, and past the wait the pane keeps it.
#[test]
fn a_pane_with_text_at_its_prompt_is_left_to_the_user() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, server, mut rex) = restarted(dir.path(), SHELL, SHELL)?;
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

/// A refused line ran nothing; one whose answer was lost waits for its gate and is then left
/// unknown, which the gate still claims, and is never typed again.
#[test]
fn a_typed_line_ends_by_its_answer_or_its_gate() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, server, mut rex) = restarted(dir.path(), SHELL, SHELL)?;
    server.lock().unwrap().refuse_write = Some("no (status=409 code=conflict)");
    let mut restorer = new_restorer();
    restorer.start(&mut rex, &mut store, "i2")?;
    restorer.reconcile(&mut rex, &mut store, "i2");
    assert_eq!(restore_of(&store, "a")?.1, "failed");

    let dir = tempfile::tempdir()?;
    let (mut store, server, mut rex) = restarted(dir.path(), SHELL, SHELL)?;
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

/// A conversation that reported from another pane since the plan, or from another pane on this
/// instance before it, as when someone reopened it while no daemon ran, runs there: its restore
/// types nothing, and a round its worker left open goes on from the conversation's record.
#[test]
fn a_conversation_opened_elsewhere_is_not_reopened() -> Result<()> {
    for before_the_plan in [false, true] {
        let dir = tempfile::tempdir()?;
        let (mut store, server, mut rex) = restarted(dir.path(), STARTUP, STARTUP)?;
        std::fs::write(dir.path().join("worker.jsonl"), format!("{PROMPT}\n"))?;
        let opened = |store: &Store, reported_at: i64| {
            store.conn().execute(
                "INSERT INTO occupant (block_lineage, runtime, native_id, instance_id, pid,
                   keeps_session_env, reported_at)
                 VALUES ('c', 'claude', 'owner', 'i2', 9, 0, ?1), ('d', 'claude', 'worker', 'i2', 8, 0, ?1)",
                params![reported_at],
            )
        };
        if before_the_plan {
            opened(&store, 1)?;
        }
        let mut restorer = new_restorer();
        restorer.start(&mut rex, &mut store, "i2")?;
        restorer.reconcile(&mut rex, &mut store, "i2");
        if !before_the_plan {
            opened(&store, 9_000_000_000_000)?;
        }
        for block in ["a", "b"] {
            server
                .lock()
                .unwrap()
                .process
                .insert(block.into(), serde_json::from_str(SHELL)?);
        }
        foreground_moved(&store)?;
        store.conn().execute_batch(
            "UPDATE block SET fg_generation = fg_generation + 1 WHERE lineage = 'b'",
        )?;
        restorer.reconcile(&mut rex, &mut store, "i2");
        for block in ["a", "b"] {
            let (_, state, note) = restore_of(&store, block)?;
            assert_eq!(
                (state.as_str(), note.as_deref()),
                ("skipped", Some(OPENED_IN_ANOTHER_PANE)),
                "{block}"
            );
        }
        assert!(server.lock().unwrap().written.is_empty());
        let round: String = store
            .conn()
            .query_row("SELECT state FROM task_round", [], |row| row.get(0))?;
        assert_eq!(round, "running");
    }
    Ok(())
}

/// The pane of a released worker that nothing used after its result comes back with the rest; it
/// is closed while it shows only the server's shell, and kept when something else runs there, the
/// user looks at it, or the server will not describe it, past the wait.
#[test]
fn a_retired_workers_pane_is_closed_unless_it_is_in_use() -> Result<()> {
    // A pane the server will not describe is kept at the end of its wait too, and nothing reads it
    // again.
    for (shown, focused, refuse, kept) in [
        (SHELL, None, None, None),
        (STARTUP, None, None, Some("sleep runs in the pane")),
        (SHELL, Some("b"), None, Some("the worker's pane has focus")),
        (SHELL, None, Some("modes"), Some("refused")),
    ] {
        let dir = tempfile::tempdir()?;
        let (mut store, server, mut rex) = restarted(dir.path(), SHELL, shown)?;
        server.lock().unwrap().focused = focused;
        store.conn().execute_batch(
            "UPDATE block SET fg_pid = NULL, last_exit = 0 WHERE lineage = 'b';
             UPDATE task SET result_file = '/r', worker_runtime = NULL, worker_native_id = NULL,
               released_at = 120;
             UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = '/r',
               finished_at = 100, finished_instance_id = 'i1', finished_generation = 5, acked_at = 120;",
        )?;
        let mut restorer = new_restorer();
        restorer.start(&mut rex, &mut store, "i2")?;
        server.lock().unwrap().refuse = refuse;
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

/// A hold lasts one epoch: the next one lets go of it first, and on the same instance, where the
/// pane is no longer one the server restored, leaves it to the cleanup pass.
#[test]
fn a_pane_held_by_an_earlier_epoch_is_let_go() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut store, _server, mut rex) = restarted(dir.path(), SHELL, STARTUP)?;
    store.conn().execute_batch(
        "UPDATE block SET fg_pid = NULL, last_exit = 0 WHERE lineage = 'b';
         UPDATE task SET result_file = '/r', worker_runtime = NULL, worker_native_id = NULL,
           released_at = 120;
         UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = '/r',
           finished_at = 100, finished_instance_id = 'i1', finished_generation = 5, acked_at = 120;",
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
