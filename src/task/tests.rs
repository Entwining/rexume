use std::io::Write as _;
use std::path::Path;

use anyhow::Result;
use rusqlite::params;
use serde_json::Value;

use super::follow::OPEN_ROUNDS;
use super::outcome::{self, WORKER_ROUNDS, block_gone};
use super::register::start_round;
use super::wait::register_waiter;
use super::{cleanup, notice, shown};
use crate::store::Store;

pub(super) const PROMPT: &str = r#"{"type":"user","timestamp":"2026-10-24T15:29:45.738Z","message":{"role":"user","content":"Count"}}"#;
pub(super) const ANSWER: &str = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Done."}]}}"#;

pub(super) fn record(lines: &[&str]) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    for line in lines {
        writeln!(file, "{line}")?;
    }
    file.flush()?;
    Ok(file)
}

/// The owner `claude/owner` in pane `a` and task `t` on its first round, running in pane `b`,
/// whose occupant is `claude/worker` with record `transcript` and foreground pid 100. Without
/// `result_file` the round follows that conversation; with it the task is a run of pid 100.
pub(super) fn task_world(
    dir: &Path,
    transcript: &Path,
    result_file: Option<&str>,
) -> Result<Store> {
    let mut store = Store::open(dir)?;
    let tx = store.write()?;
    tx.execute_batch(
        "INSERT INTO epoch (id, instance_id, started_at) VALUES (1, 'i1', 0);
         INSERT INTO block (lineage, instance_id, fg_pid, fg_generation, status, status_pid, status_at)
           VALUES ('a', 'i1', 10, 1, 'idle', 10, 1), ('b', 'i1', 100, 5, 'working', 100, 1);
         INSERT INTO conversation (runtime, native_id, resumable, reported_at) VALUES ('claude', 'owner', 1, 0);
         INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at)
           VALUES ('a', 'claude', 'owner', 10, 0, 0);",
    )?;
    tx.execute(
        "INSERT INTO conversation (runtime, native_id, transcript, resumable, reported_at)
         VALUES ('claude', 'worker', ?1, 1, 50)",
        params![transcript.to_string_lossy()],
    )?;
    tx.execute_batch(
        "INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at)
           VALUES ('b', 'claude', 'worker', 100, 0, 50);",
    )?;
    tx.execute(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, worker_pid,
           result_file, keep_pane, created_at) VALUES ('t', 'count', 'claude', 'owner', 'b', 100, ?1, 0, 0)",
        params![result_file],
    )?;
    let (runtime, native_id) = match result_file {
        None => (Some("claude"), Some("worker")),
        Some(_) => (None, None),
    };
    tx.execute(
        "INSERT INTO task_round (task_id, round, worker_runtime, worker_native_id, cursor, state, created_at)
         VALUES ('t', 1, ?1, ?2, 0, 'running', 0)",
        params![runtime, native_id],
    )?;
    tx.commit()?;
    Ok(store)
}

pub(super) fn round(store: &Store) -> Result<(String, Option<String>, Option<String>)> {
    Ok(store.conn().query_row(
        "SELECT state, result, note FROM task_round WHERE task_id = 't' AND round = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

#[test]
fn a_new_round_needs_a_finished_one_and_an_interactive_pane_not_closing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut attempt = |change: &str| -> Result<i64> {
        store.conn().execute_batch(change)?;
        let tx = store.write()?;
        start_round(&tx, "t", None, 0)
    };
    let error = |result: Result<i64>| result.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error(attempt("")).contains("is running"));
    let finished = "UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = 'm1',
                      finished_at = 1;";
    // Another task on the same pane holds it for a close.
    assert!(
        error(attempt(&format!(
            "{finished} INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage,
               keep_pane, pane_state, created_at) VALUES ('u', 'x', 'claude', 'owner', 'b', 0, 'closing', 0)"
        )))
        .contains("is closing")
    );
    assert!(
        error(attempt(
            "UPDATE task SET pane_state = 'closed', pane_closed_at = 2"
        ))
        .contains("is closed")
    );
    assert!(
        error(attempt(
            "UPDATE task SET pane_state = 'open', pane_closed_at = NULL, result_file = '/r'"
        ))
        .contains("cannot be rearmed")
    );
    assert_eq!(
        attempt(
            "DELETE FROM task WHERE id = 'u';
             UPDATE task SET result_file = NULL, pane_state = 'kept', pane_note = 'in use'"
        )?,
        2
    );
    Ok(())
}

#[test]
fn a_waiter_replaces_only_the_dead_waiter_it_saw() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store
        .conn()
        .execute("UPDATE task_round SET waiter_pid = 111", [])?;
    let tx = store.write()?;
    assert!(!register_waiter(&tx, "t", 1, 222, None)?);
    assert!(!register_waiter(&tx, "t", 1, 222, Some(333))?);
    assert!(register_waiter(&tx, "t", 1, 222, Some(111))?);
    Ok(())
}

#[test]
fn a_closed_worker_pane_ends_its_open_round_and_every_task_on_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute_batch(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           pane_state, pane_note, created_at)
         VALUES ('k', 'earlier', 'claude', 'owner', 'b', 0, 'kept', 'in use', 0);
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i1', 'block:b', 'b');",
    )?;
    let tx = store.write()?;
    block_gone(&tx, "b")?;
    tx.commit()?;
    assert_eq!(round(&store)?.0, "worker_closed");
    assert_eq!(shown(store.conn(), "i1", "t", None)?["worker"], Value::Null);
    let panes: Vec<(String, bool, Option<String>)> = store
        .conn()
        .prepare("SELECT pane_state, pane_closed_at IS NOT NULL, pane_note FROM task ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        panes,
        [("closed".into(), true, None), ("closed".into(), true, None)]
    );
    let notice: String =
        store
            .conn()
            .query_row("SELECT state FROM notice WHERE task_id = 't'", [], |row| {
                row.get(0)
            })?;
    assert_eq!(notice, "pending");
    Ok(())
}

/// The statements every daemon pass runs reach their rows through an index, apart from scans of
/// `task` and `notice`, which retention bounds; the ones on unfinished rounds use the partial
/// index, which only a predicate matching its own text can use.
#[test]
fn every_pass_statement_uses_an_index() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    let statements: [(&str, &str, bool); 9] = [
        ("OPEN_ROUNDS", &OPEN_ROUNDS, true),
        ("outcome::ROUND", &outcome::ROUND, false),
        ("WORKER_ROUNDS", &WORKER_ROUNDS, true),
        ("cleanup::CLAIM", &cleanup::CLAIM, true),
        ("cleanup::DUE", &cleanup::DUE, true),
        ("notice::PENDING", notice::PENDING, false),
        ("notice::SENT", notice::SENT, false),
        ("notice::READY_OWNER", notice::READY_OWNER, false),
        ("notice::CLAIM", notice::CLAIM, false),
    ];
    for (name, sql, partial) in statements {
        let mut plan = store.conn().prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let mut rows = plan.raw_query();
        let mut details = Vec::new();
        while let Some(row) = rows.next()? {
            details.push(row.get::<_, String>(3)?);
        }
        for detail in &details {
            let Some(scanned) = detail.strip_prefix("SCAN ") else {
                continue;
            };
            let table = scanned.split(' ').next().unwrap_or_default();
            assert!(
                detail.contains("task_round_open") || ["t", "task", "n", "notice"].contains(&table),
                "{name}: {detail}"
            );
        }
        assert_eq!(
            details.iter().any(|d| d.contains("task_round_open")),
            partial,
            "{name}: {details:?}"
        );
    }
    Ok(())
}
