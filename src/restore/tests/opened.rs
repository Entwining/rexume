use anyhow::Result;
use rusqlite::params;

use super::server::{SHELL, STARTUP};
use super::{foreground_moved, new_restorer, restarted, restore_of};
use crate::restore::opened;
use crate::store::Store;
use crate::task::tests::PROMPT;

#[test]
fn a_conversation_opened_elsewhere_or_in_the_pane_is_not_reopened() -> Result<()> {
    let elsewhere = [("skipped", Some(opened::OPENED_IN_ANOTHER_PANE), None); 2];
    let here = [("opened", None, None), ("opened", None, Some("pending"))];
    for (before_the_plan, in_the_pane, ends) in [
        (false, false, elsewhere),
        (true, false, elsewhere),
        (false, true, here),
        (true, true, [("opened", None, None); 2]),
    ] {
        let dir = tempfile::tempdir()?;
        let (mut store, server, mut rex) = restarted(dir.path(), STARTUP, STARTUP)?;
        std::fs::write(dir.path().join("worker.jsonl"), format!("{PROMPT}\n"))?;
        let opened = |store: &Store, reported_at: i64| {
            store.conn().execute(
                if in_the_pane {
                    "UPDATE occupant SET instance_id = 'i2', reported_at = ?1"
                } else {
                    "INSERT INTO occupant (block_lineage, runtime, native_id, instance_id, pid,
                       reported_at)
                     VALUES ('c', 'claude', 'owner', 'i2', 9, ?1), ('d', 'claude', 'worker', 'i2', 8, ?1)"
                },
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
        if in_the_pane && !before_the_plan {
            let continues = "UPDATE restore SET start = 'continue' WHERE block_lineage = 'b'";
            store.conn().execute_batch(continues)?;
        }
        for block in ["a", "b"] {
            server
                .lock()
                .unwrap()
                .process
                .insert(block.into(), serde_json::from_str(SHELL.as_str())?);
        }
        foreground_moved(&store)?;
        store.conn().execute_batch(
            "UPDATE block SET fg_generation = fg_generation + 1 WHERE lineage = 'b'",
        )?;
        restorer.reconcile(&mut rex, &mut store, "i2");
        for (block, (state, note, continuation)) in ["a", "b"].into_iter().zip(ends) {
            let (_, got, got_note) = restore_of(&store, block)?;
            let got_continuation: Option<String> = store.conn().query_row(
                "SELECT continuation_state FROM restore WHERE block_lineage = ?1",
                [block],
                |row| row.get(0),
            )?;
            assert_eq!(
                (got.as_str(), got_continuation.as_deref()),
                (state, continuation),
                "{block}"
            );
            // A pending continuation's note says why it waits.
            if continuation.is_none() {
                assert_eq!(got_note.as_deref(), note, "{block}");
            }
        }
        assert!(server.lock().unwrap().written.is_empty());
        let round: String = store
            .conn()
            .query_row("SELECT state FROM task_round", [], |row| row.get(0))?;
        assert_eq!(round, "running");
    }
    Ok(())
}

#[test]
fn another_conversation_in_the_pane_ends_the_rounds_of_one_no_pane_reopened() -> Result<()> {
    for (worker_elsewhere, round) in [(false, "terminated"), (true, "running")] {
        let dir = tempfile::tempdir()?;
        let (mut store, server, mut rex) = restarted(dir.path(), STARTUP, STARTUP)?;
        store.conn().execute_batch(
            "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
               VALUES ('claude', 'other', 1, 1);
             UPDATE occupant SET native_id = 'other', instance_id = 'i2', reported_at = 1
               WHERE block_lineage = 'b';",
        )?;
        if worker_elsewhere {
            store.conn().execute_batch(
                "INSERT INTO occupant (block_lineage, runtime, native_id, instance_id, pid,
                   reported_at) VALUES ('c', 'claude', 'worker', 'i2', 8, 1)",
            )?;
        }
        let mut restorer = new_restorer();
        restorer.start(&mut rex, &mut store, "i2")?;
        restorer.reconcile(&mut rex, &mut store, "i2");
        let (_, state, note) = restore_of(&store, "b")?;
        assert_eq!((state.as_str(), note.as_deref()), ("opened", None));
        assert!(server.lock().unwrap().written.is_empty());
        let state: String = store
            .conn()
            .query_row("SELECT state FROM task_round", [], |row| row.get(0))?;
        assert_eq!(state, round, "worker elsewhere: {worker_elsewhere}");
    }
    Ok(())
}
