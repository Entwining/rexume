use std::io::Write as _;

use anyhow::Result;

use crate::runtime;
use crate::store::Store;
use crate::task::follow::Follow;
use crate::task::gone::block_gone;
use crate::task::register::{Kind, start_round};
use crate::task::tests::{ANSWER, ANSWER_2, PROMPT, PROMPT_2, record, task_world};
use crate::task::unread;

const ANSWER_3: &str = r#"{"event":"stop","prompt_id":"p3","id":"m3","answer":"Sent."}"#;

/// Each round of task `t` in order, with its state and result.
fn rounds(store: &Store) -> Result<Vec<(String, Option<String>)>> {
    Ok(store
        .conn()
        .prepare("SELECT state, result FROM task_round WHERE task_id = 't' ORDER BY round")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn completed(result: &str) -> (String, Option<String>) {
    ("completed".into(), Some(result.into()))
}

fn start(store: &mut Store, kind: &Kind) -> Result<i64> {
    let tx = store.write()?;
    let round = start_round(&tx, "t", kind)?;
    tx.commit()?;
    Ok(round)
}

const NEXT: Kind = Kind::Next { unread: None };

#[test]
fn turns_that_finished_before_a_pass_are_taken_one_per_round_in_order() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, PROMPT_2, ANSWER_2])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    assert_eq!(unread(store.conn(), store.dir(), "t")?, Some(1));
    start(&mut store, &NEXT)?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?, [completed("Done."), completed("Again.")]);
    assert_eq!(unread(store.conn(), store.dir(), "t")?, Some(0));
    Ok(())
}

#[test]
fn a_finished_turn_is_taken_though_another_has_started_after_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, PROMPT_2])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(rounds(&store)?, [completed("Done.")]);
    // The position stops where the open turn starts, so the next round can take it.
    let position: i64 = store
        .conn()
        .query_row("SELECT position FROM task", [], |row| row.get(0))?;
    assert_eq!(usize::try_from(position)?, PROMPT.len() + ANSWER.len() + 2);
    Ok(())
}

#[test]
fn a_turn_that_finished_between_rounds_is_the_next_rounds() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT, ANSWER])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    // The worker's final report finishes while no round is open.
    writeln!(transcript, "{PROMPT_2}\n{ANSWER_2}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    start(&mut store, &NEXT)?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?, [completed("Done."), completed("Again.")]);
    Ok(())
}

#[test]
fn a_request_round_takes_only_the_turn_that_took_its_request_in() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT, ANSWER])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    start(&mut store, &Kind::Request("go"))?;
    // The user asks the worker something before the request arrives.
    writeln!(transcript, "{PROMPT_2}\n{ANSWER_2}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?[1].0, "running");
    let id: String = store.conn().query_row(
        "UPDATE request SET state = 'sent' RETURNING id",
        [],
        |row| row.get(0),
    )?;
    let received =
        serde_json::json!({ "event": "prompt", "prompt_id": "p3", "token": runtime::token(&id) });
    writeln!(transcript, "{received}\n{ANSWER_3}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?[1], completed("Sent."));
    let request: String = store
        .conn()
        .query_row("SELECT state FROM request", [], |row| row.get(0))?;
    assert_eq!(request, "delivered");
    // The user's turn is left for `task next`, after which the position passes every turn.
    assert_eq!(unread(store.conn(), store.dir(), "t")?, Some(1));
    start(&mut store, &NEXT)?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?[2], completed("Again."));
    let position: i64 = store
        .conn()
        .query_row("SELECT position FROM task", [], |row| row.get(0))?;
    let len = std::fs::metadata(transcript.path())?.len();
    assert_eq!(u64::try_from(position)?, len);
    Ok(())
}

/// Claude Code's `UserPromptSubmit` hook runs before another hook can reject the prompt, which
/// then gets no `Stop`. A rejected request is not received, and an unrelated turn after it is
/// not its answer: the request's round ends as cancelled once the worker takes other input, and
/// that input's turn is unread.
#[test]
fn a_rejected_request_is_not_received_and_its_round_ends_cancelled() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT, ANSWER])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    start(&mut store, &Kind::Request("go"))?;
    let id: String = store.conn().query_row(
        "UPDATE request SET state = 'sent' RETURNING id",
        [],
        |row| row.get(0),
    )?;
    let request = |store: &Store| -> Result<String> {
        Ok(store
            .conn()
            .query_row("SELECT state FROM request", [], |row| row.get(0))?)
    };
    let rejected =
        serde_json::json!({ "event": "prompt", "prompt_id": "p3", "token": runtime::token(&id) });
    writeln!(transcript, "{rejected}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    assert_eq!(
        (rounds(&store)?[1].0.as_str(), request(&store)?.as_str()),
        ("running", "sent")
    );
    writeln!(transcript, "{PROMPT_2}\n{ANSWER_2}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    assert_eq!(
        (rounds(&store)?[1].0.as_str(), request(&store)?.as_str()),
        ("cancelled", "sent")
    );
    assert_eq!(unread(store.conn(), store.dir(), "t")?, Some(1));
    Ok(())
}

/// A Claude Code worker's turn record does not depend on the transcript its session start
/// reported, so a pane that closes after its turn finished leaves the round that turn.
#[test]
fn a_closed_pane_leaves_a_claude_round_its_turn_without_a_reported_transcript() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store
        .conn()
        .execute_batch("UPDATE conversation SET transcript = NULL")?;
    let tx = store.write()?;
    block_gone(&tx, "b")?;
    tx.commit()?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(rounds(&store)?, [completed("Done.")]);
    Ok(())
}

#[test]
fn a_replaced_record_fails_the_round_instead_of_reading_it_again() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    let replacement = record(&[PROMPT, ANSWER])?;
    std::fs::rename(replacement.path(), transcript.path())?;
    follow.pass(&mut store, None)?;
    let (state, note): (String, String) =
        store
            .conn()
            .query_row("SELECT state, note FROM task_round", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
    assert_eq!(state, "failed");
    assert!(note.contains("replaced or cut"), "{note}");
    Ok(())
}

/// A pane that closes before its round's turn ended ends the round as `worker_closed`; its record
/// still gives the turns no round took.
#[test]
fn a_closed_pane_ends_its_round_from_its_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?[0].0, "running");
    let tx = store.write()?;
    block_gone(&tx, "b")?;
    tx.commit()?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?, [("worker_closed".into(), None)]);
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, PROMPT_2, ANSWER_2])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    let tx = store.write()?;
    block_gone(&tx, "b")?;
    tx.commit()?;
    let unread = unread(store.conn(), store.dir(), "t")?;
    assert_eq!(unread, Some(1));
    start(&mut store, &Kind::Next { unread })?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?, [completed("Done."), completed("Again.")]);
    Ok(())
}

/// A round upgraded from version 1 left the position inside the turn it took and recorded no
/// start; its answer's ID still names that turn, which is not another.
#[test]
fn the_rest_of_an_answer_a_round_took_is_not_another_turn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, PROMPT_2, ANSWER_2])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute(
        "UPDATE task SET position = ?1",
        [i64::try_from(PROMPT.len() + 1)?],
    )?;
    store.conn().execute_batch(
        "UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = 'm1', finished_at = 1",
    )?;
    assert_eq!(unread(store.conn(), store.dir(), "t")?, Some(1));
    start(&mut store, &NEXT)?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(rounds(&store)?[1], completed("Again."));
    Ok(())
}
