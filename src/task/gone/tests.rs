use anyhow::Result;
use rusqlite::params;

use super::{block_gone, end_left_to_server};
use crate::task::tests::{PROMPT, record, round, task_world};

/// A pane the server restored itself runs a shell, so a round the earlier instance left open there
/// ends, with a notice to its owner. A round started since the daemon observed the instance, one of
/// a task registered on the instance before that, and one in a pane the server did not restore, go
/// on.
#[test]
fn a_round_an_earlier_instance_left_open_in_a_pane_the_server_restored_ends() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute_batch(
        "INSERT INTO epoch (instance_id, started_at) VALUES ('i2', 10);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           worker_instance_id, created_at)
           VALUES ('later', 'x', 'claude', 'owner', 'b', 0, 'i1', 0),
                  ('new', 'x', 'claude', 'owner', 'b', 0, 'i2', 5),
                  ('other', 'x', 'claude', 'owner', 'c', 0, 'i1', 0);
         INSERT INTO task_round (task_id, round, state, created_at)
           VALUES ('later', 1, 'running', 20), ('new', 1, 'running', 5), ('other', 1, 'running', 0);",
    )?;
    let tx = store.write()?;
    end_left_to_server(&tx, "i2", "b")?;
    tx.commit()?;
    let rounds: Vec<(String, String, Option<String>)> = store
        .conn()
        .prepare("SELECT task_id, state, note FROM task_round ORDER BY task_id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let (task, state, note) = &rounds[3];
    assert_eq!((task.as_str(), state.as_str()), ("t", "terminated"));
    assert!(
        note.as_deref()
            .unwrap()
            .contains("its conversation was not reopened there")
    );
    assert_eq!(
        rounds[..3]
            .iter()
            .map(|(t, s, _)| (t.as_str(), s.as_str()))
            .collect::<Vec<_>>(),
        [
            ("later", "running"),
            ("new", "running"),
            ("other", "running")
        ]
    );
    let notices: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM notice WHERE task_id = 't' AND state = 'pending'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(notices, 1);
    Ok(())
}

/// A worker's conversation someone reopened in its own pane on the new instance, before the daemon
/// planned the pane, takes its round up from its record there.
#[test]
fn a_round_whose_conversation_reopened_in_its_pane_goes_on() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute_batch(
        "INSERT INTO epoch (instance_id, started_at) VALUES ('i2', 10);
         UPDATE occupant SET instance_id = 'i2', pid = 300 WHERE block_lineage = 'b';",
    )?;
    let tx = store.write()?;
    end_left_to_server(&tx, "i2", "b")?;
    tx.commit()?;
    let state: String = store.conn().query_row(
        "SELECT state FROM task_round WHERE task_id = 't'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(state, "running");
    Ok(())
}

/// A closed worker pane ends the rounds no record will finish: a run's, also one bound to a
/// conversation whose record shows turns, and an interactive worker's whose conversation has no
/// record. A Claude Code or omp worker's round is left to the record Rexume keeps for it, which
/// the next pass reads, also for an omp session without a session file.
#[test]
fn a_closed_worker_pane_ends_only_the_rounds_no_record_finishes() -> Result<()> {
    for (runtime, run, ended) in [
        ("claude", true, true),
        ("codex", false, true),
        ("omp", false, false),
        ("claude", false, false),
    ] {
        let dir = tempfile::tempdir()?;
        let transcript = record(&[PROMPT])?;
        let result_file = run.then_some("/result.md");
        let mut store = task_world(dir.path(), transcript.path(), result_file)?;
        if runtime != "claude" {
            store.conn().execute(
                "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
                 VALUES (?1, 'worker', 0, 50)",
                params![runtime],
            )?;
        }
        store.conn().execute(
            "UPDATE task SET worker_runtime = ?1, worker_native_id = 'worker'",
            params![runtime],
        )?;
        let tx = store.write()?;
        block_gone(&tx, "b")?;
        tx.commit()?;
        let expected = if ended { "worker_closed" } else { "running" };
        assert_eq!(round(&store)?.0, expected, "{runtime} run: {run}");
    }
    Ok(())
}
