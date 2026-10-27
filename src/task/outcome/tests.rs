use std::io::Write as _;

use anyhow::Result;

use crate::runtime;
use crate::store::Store;
use crate::task::follow::Follow;
use crate::task::gone::block_gone;
use crate::task::register::{Kind, start_round};
use crate::task::tests::{ANSWER, ANSWER_2, DURATION, PROMPT, PROMPT_2, record, task_world};
use crate::task::unread;

const ANSWER_3: &str = r#"{"type":"assistant","message":{"id":"m3","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Sent."}]}}"#;

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
    let transcript = record(&[PROMPT, ANSWER, DURATION, PROMPT_2, ANSWER_2, DURATION])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    assert_eq!(unread(store.conn(), "t")?, Some(1));
    start(&mut store, &NEXT)?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?, [completed("Done."), completed("Again.")]);
    assert_eq!(unread(store.conn(), "t")?, Some(0));
    Ok(())
}

#[test]
fn a_finished_turn_is_taken_though_another_has_started_after_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, DURATION, PROMPT_2])?;
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
    let mut transcript = record(&[PROMPT, ANSWER, DURATION])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    // The worker's final report finishes while no round is open.
    writeln!(transcript, "{PROMPT_2}\n{ANSWER_2}\n{DURATION}")?;
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
    let mut transcript = record(&[PROMPT, ANSWER, DURATION])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    start(&mut store, &Kind::Request("go"))?;
    // The user asks the worker something before the request arrives.
    writeln!(transcript, "{PROMPT_2}\n{ANSWER_2}\n{DURATION}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?[1].0, "running");
    let id: String = store.conn().query_row(
        "UPDATE request SET state = 'sent' RETURNING id",
        [],
        |row| row.get(0),
    )?;
    let received = serde_json::json!({ "type": "user", "isMeta": true, "turnOrigin": "peer",
        "origin": { "kind": "peer" }, "message": { "role": "user",
        "content": format!("Another Claude session sent a message:\n{} go", runtime::token(&id)) } });
    writeln!(transcript, "{received}\n{ANSWER_3}\n{DURATION}")?;
    transcript.flush()?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?[1], completed("Sent."));
    let request: String = store
        .conn()
        .query_row("SELECT state FROM request", [], |row| row.get(0))?;
    assert_eq!(request, "delivered");
    // The user's turn is left for `task next`, after which the position passes every turn.
    assert_eq!(unread(store.conn(), "t")?, Some(1));
    start(&mut store, &NEXT)?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?[2], completed("Again."));
    let position: i64 = store
        .conn()
        .query_row("SELECT position FROM task", [], |row| row.get(0))?;
    let len = std::fs::metadata(transcript.path())?.len();
    assert_eq!(u64::try_from(position)?, len - DURATION.len() as u64 - 1);
    Ok(())
}

/// A turn the worker was still in when the request was sent is not its answer, also when the
/// pane closes with that turn answered but not yet sealed.
#[test]
fn a_request_round_does_not_take_an_open_turn_that_started_before_its_request() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT, ANSWER, DURATION])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    start(&mut store, &Kind::Request("go"))?;
    writeln!(transcript, "{PROMPT_2}\n{ANSWER_2}")?;
    transcript.flush()?;
    let tx = store.write()?;
    block_gone(&tx, "b")?;
    tx.commit()?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?[1], ("worker_closed".into(), None));
    Ok(())
}

#[test]
fn a_replaced_record_fails_the_round_instead_of_reading_it_again() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    let replacement = record(&[PROMPT, ANSWER, DURATION])?;
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

/// A pane that closes ends its round with the answer the record shows, which no later record can
/// seal, or as `worker_closed` without one; its record still gives turns no round took, the last
/// answer included.
#[test]
fn a_closed_pane_ends_its_round_from_its_record() -> Result<()> {
    for (lines, ended) in [
        (&[PROMPT, ANSWER][..], completed("Done.")),
        (&[PROMPT][..], ("worker_closed".into(), None)),
    ] {
        let dir = tempfile::tempdir()?;
        let transcript = record(lines)?;
        let mut store = task_world(dir.path(), transcript.path(), None)?;
        let mut follow = Follow::default();
        follow.pass(&mut store, None)?;
        assert_eq!(rounds(&store)?[0].0, "running");
        let tx = store.write()?;
        block_gone(&tx, "b")?;
        tx.commit()?;
        follow.pass(&mut store, None)?;
        assert_eq!(rounds(&store)?, [ended]);
    }
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER, DURATION, PROMPT_2, ANSWER_2])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    let tx = store.write()?;
    block_gone(&tx, "b")?;
    tx.commit()?;
    let unread = unread(store.conn(), "t")?;
    assert_eq!(unread, Some(1));
    start(&mut store, &Kind::Next { unread })?;
    follow.pass(&mut store, None)?;
    assert_eq!(rounds(&store)?, [completed("Done."), completed("Again.")]);
    Ok(())
}

/// A round upgraded from version 1 left the position after the first part of its answer and
/// recorded no start; the rest of that answer is not another turn.
#[test]
fn the_rest_of_an_answer_a_round_took_is_not_another_turn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let rest = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"More."}]}}"#;
    let transcript = record(&[PROMPT, ANSWER, rest, DURATION, PROMPT_2, ANSWER_2, DURATION])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute(
        "UPDATE task SET position = ?1",
        [i64::try_from(PROMPT.len() + ANSWER.len() + 2)?],
    )?;
    store.conn().execute_batch(
        "UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = 'm1', finished_at = 1",
    )?;
    assert_eq!(unread(store.conn(), "t")?, Some(1));
    start(&mut store, &NEXT)?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(rounds(&store)?[1], completed("Again."));
    Ok(())
}
