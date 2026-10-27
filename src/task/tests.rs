use std::io::Write as _;
use std::path::Path;

use anyhow::Result;
use rusqlite::params;
use serde_json::Value;

use super::follow::{Follow, OPEN_ROUNDS, TAKEN};
use super::gone::{WORKER_ROUNDS, block_gone};
use super::outcome;
use super::register::{Kind, refuse_stopped, start_round};
use super::wait::register_waiter;
use super::{cleanup, notice, request, shown, unread};
use crate::store::Store;

pub(super) const PROMPT: &str = r#"{"type":"user","timestamp":"2026-10-24T15:29:45.738Z","message":{"role":"user","content":"Count"}}"#;
pub(super) const ANSWER: &str = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Done."}]}}"#;
/// What Claude Code writes after a turn's last record, which seals its answer.
pub(super) const DURATION: &str = r#"{"type":"system","subtype":"turn_duration"}"#;
pub(super) const PROMPT_2: &str =
    r#"{"type":"user","message":{"role":"user","content":"Count again"}}"#;
pub(super) const ANSWER_2: &str = r#"{"type":"assistant","message":{"id":"m2","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Again."}]}}"#;

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
    let (runtime, native_id) = match result_file {
        None => (Some("claude"), Some("worker")),
        Some(_) => (None, None),
    };
    tx.execute(
        "INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, worker_pid,
           worker_instance_id, result_file, keep_pane, worker_runtime, worker_native_id, created_at)
         VALUES ('t', 'count', 'claude', 'owner', 'b', 100, 'i1', ?1, 0, ?2, ?3, 0)",
        params![result_file, runtime, native_id],
    )?;
    tx.execute_batch(
        "INSERT INTO task_round (task_id, round, state, created_at) VALUES ('t', 1, 'running', 0)",
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
fn a_new_round_needs_a_finished_one_on_an_unreleased_interactive_pane() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let next = Kind::Next { unread: None };
    let mut attempt = |change: &str, kind: &Kind| -> Result<i64> {
        store.conn().execute_batch(change)?;
        let tx = store.write()?;
        let round = start_round(&tx, "t", kind)?;
        tx.commit()?;
        Ok(round)
    };
    let error = |result: Result<i64>| result.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(error(attempt("", &next)).contains("is running"));
    let finished = "UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = 'm1',
                      finished_at = 1;";
    // Another task on the same pane holds it for a close.
    assert!(
        error(attempt(
            &format!(
                "{finished} INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage,
                   keep_pane, released_at, pane_state, created_at)
                 VALUES ('u', 'x', 'claude', 'owner', 'b', 0, 1, 'closing', 0)"
            ),
            &next
        ))
        .contains("is closing")
    );
    let closed = "DELETE FROM task WHERE id = 'u';
                  UPDATE task SET pane_state = 'closed', pane_closed_at = 2";
    assert!(error(attempt(closed, &next)).contains("no turn is unread"));
    assert!(error(attempt("", &Kind::Request("go"))).contains("is closed"));
    // A closed pane's record still holds turns no round took.
    assert_eq!(attempt("", &Kind::Next { unread: Some(1) })?, 2);
    assert!(
        error(attempt(
            "UPDATE task SET pane_state = 'open', pane_closed_at = NULL, released_at = 3",
            &next
        ))
        .contains("is released")
    );
    assert!(
        error(attempt(
            "UPDATE task SET released_at = NULL, worker_runtime = NULL, worker_native_id = NULL,
               result_file = '/r'",
            &next
        ))
        .contains("non_interactive")
    );
    assert_eq!(
        attempt(
            &format!(
                "{finished} UPDATE task SET result_file = NULL, worker_runtime = 'claude',
                   worker_native_id = 'worker', pane_state = 'kept', pane_note = 'in use'"
            ),
            &Kind::Request("go")
        )?,
        3
    );
    let (acked, request): (bool, String) = store.conn().query_row(
        "SELECT r.acked_at IS NOT NULL, q.text FROM task_round r
         JOIN request q ON q.task_id = r.task_id AND q.round = 3 WHERE r.round = 2",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert!(acked);
    assert_eq!(request, "go");
    Ok(())
}

/// Turns the worker finishes after its task was released are no round's, so none is unread.
#[test]
fn a_released_task_has_no_unread_turns() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT, ANSWER, DURATION])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    Follow::default().pass(&mut store, None)?;
    writeln!(transcript, "{PROMPT_2}\n{ANSWER_2}\n{DURATION}")?;
    transcript.flush()?;
    assert_eq!(unread(store.conn(), "t")?, Some(1));
    store
        .conn()
        .execute_batch("UPDATE task SET released_at = 2")?;
    assert_eq!(unread(store.conn(), "t")?, None);
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

/// A task registers on an instance unless the daemon started to observe another one after the
/// instance answered: servers run one after another, so that one came later.
#[test]
fn a_task_on_an_instance_a_later_one_followed_is_refused() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    store.conn().execute_batch(
        "INSERT INTO epoch (instance_id, started_at) VALUES ('i1', 0), ('i1', 2), ('i2', 5)",
    )?;
    assert!(refuse_stopped(store.conn(), "i1", 3).is_err());
    // Never observed before the later one, as when no daemon ran.
    assert!(refuse_stopped(store.conn(), "i0", 3).is_err());
    refuse_stopped(store.conn(), "i2", 3)?;
    refuse_stopped(store.conn(), "i3", 6)?;
    Ok(())
}

/// A round no record can finish, here one whose worker never reported, ends with its pane.
#[test]
fn a_closed_worker_pane_ends_its_unbound_round_and_every_task_on_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute_batch(
        "UPDATE task SET worker_runtime = NULL, worker_native_id = NULL;
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
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
    let statements: [(&str, &str, bool); 15] = [
        ("OPEN_ROUNDS", &OPEN_ROUNDS, true),
        ("outcome::ROUND", &outcome::ROUND, false),
        ("WORKER_ROUNDS", &WORKER_ROUNDS, true),
        ("cleanup::CLAIM", &cleanup::CLAIM, true),
        ("cleanup::DUE", &cleanup::DUE, true),
        ("cleanup::RESTORING", &cleanup::RESTORING, false),
        ("notice::PENDING", notice::PENDING, false),
        ("notice::SENT", notice::SENT, false),
        ("notice::READY_OWNER", notice::READY_OWNER, false),
        ("notice::CLAIM", notice::CLAIM, false),
        ("TAKEN", TAKEN, false),
        ("request::PENDING", &request::PENDING, true),
        ("request::WORKER", request::WORKER, false),
        ("request::CLAIM", &request::CLAIM, false),
        ("request::SENT_TO_PI", &request::SENT_TO_PI, true),
    ];
    for (name, sql, partial) in statements {
        let mut plan = store.conn().prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let mut rows = plan.raw_query();
        let mut details = Vec::new();
        while let Some(row) = rows.next()? {
            details.push(row.get::<_, String>(3)?);
        }
        for detail in &details {
            // A `SELECT` without `FROM`, such as `SELECT EXISTS (...)`, scans no table.
            let Some(scanned) = detail
                .strip_prefix("SCAN ")
                .filter(|scanned| *scanned != "CONSTANT ROW")
            else {
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
