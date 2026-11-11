use anyhow::Result;

use super::*;
use crate::task::end_left_to_server;
use crate::task::tests::{PROMPT, record, task_world};

/// Abandoning a blocked request round ends it acknowledged, drops what was not sent yet, and leaves
/// the task taking rounds; a finished round cannot be abandoned.
#[test]
fn abandoning_a_round_ends_and_acknowledges_it_and_drops_what_was_not_sent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute_batch(
        "UPDATE task_round SET state = 'blocked';
         INSERT INTO blocked_episode (id, task_id, round, started_at) VALUES ('e', 't', 1, 5);
         INSERT INTO request (id, task_id, round, text) VALUES ('q', 't', 1, 'go');
         INSERT INTO notice (id, task_id, round, kind, blocked_episode_id, state, created_at)
           VALUES ('nb', 't', 1, 'blocked', 'e', 'pending', 5);",
    )?;
    let tx = store.write()?;
    assert_eq!(abandon_round(&tx, "t", None)?, 1);
    tx.commit()?;
    // The episode ends with the round, at the round's end.
    let round: (String, Option<String>, bool, bool, Option<String>) = store.conn().query_row(
        "SELECT r.state, r.note, r.finished_at IS NOT NULL, r.acked_at IS NOT NULL, e.end_reason
         FROM task_round r JOIN blocked_episode e ON e.task_id = r.task_id AND e.round = r.round
           AND e.ended_at = r.finished_at
         WHERE r.task_id = 't' AND r.round = 1",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    assert_eq!(
        round,
        (
            "abandoned".into(),
            Some("the owner abandoned the round".into()),
            true,
            true,
            Some("round_finished".into())
        )
    );
    let unsent: Vec<(String, String, Option<String>)> = store
        .conn()
        .prepare(
            "SELECT kind, state, note FROM notice UNION ALL
             SELECT 'request', state, note FROM request ORDER BY 1",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let superseded = |kind: &str, note: &str| {
        (
            kind.to_owned(),
            "superseded".to_owned(),
            Some(note.to_owned()),
        )
    };
    assert_eq!(
        unsent,
        [
            superseded("blocked", "the block ended before it was sent"),
            superseded("finished", "taken by task abandon"),
            superseded("request", "the round ended before it was sent"),
        ]
    );
    let tx = store.write()?;
    let again = abandon_round(&tx, "t", None).unwrap_err().to_string();
    assert!(again.contains("already finished as abandoned"), "{again}");
    assert_eq!(start_round(&tx, "t", &Kind::Next { unread: Some(1) })?, 2);
    Ok(())
}

/// Abandoning a queued round drops it, its request never sent, and leaves the open round as it
/// is; abandoning the open round starts the next queued one. A dropped round never ran, so with no
/// round open the commands take the latest round that ran, and the next round comes after it.
#[test]
fn an_abandoned_queued_round_is_dropped_and_never_taken_for_the_latest() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let tx = store.write()?;
    start_round(&tx, "t", &Kind::Request("first"))?;
    start_round(&tx, "t", &Kind::Request("second"))?;
    assert_eq!(abandon_round(&tx, "t", Some(3))?, 3);
    tx.commit()?;
    // Each round's state and whether it is acknowledged, with its request's state and note.
    type Row = (String, bool, Option<String>, Option<String>);
    let rounds = |store: &Store| -> Result<Vec<Row>> {
        Ok(store
            .conn()
            .prepare(
                "SELECT r.state, r.acked_at IS NOT NULL, q.state, q.note FROM task_round r
                 LEFT JOIN request q ON q.task_id = r.task_id AND q.round = r.round
                 WHERE r.task_id = 't' ORDER BY r.round",
            )?
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    };
    let dropped: Row = (
        "dropped".into(),
        true,
        Some("superseded".into()),
        Some("the round ended before it was sent".into()),
    );
    assert_eq!(
        rounds(&store)?,
        [
            ("running".into(), false, None, None),
            (
                "queued".into(),
                false,
                Some("pending".into()),
                Some("waiting for the rounds before it to finish".into())
            ),
            dropped.clone(),
        ]
    );
    let tx = store.write()?;
    assert_eq!(abandon_round(&tx, "t", None)?, 1);
    tx.commit()?;
    assert_eq!(
        rounds(&store)?,
        [
            ("abandoned".into(), true, None, None),
            ("running".into(), false, Some("pending".into()), None),
            dropped,
        ]
    );
    let tx = store.write()?;
    assert_eq!(abandon_round(&tx, "t", None)?, 2);
    assert_eq!(round_state(&tx, "t", None)?, (2, "abandoned".into()));
    assert_eq!(start_round(&tx, "t", &Kind::Next { unread: Some(1) })?, 4);
    Ok(())
}

/// Starting a round with none open acknowledges every earlier round, also those a queued round
/// started behind without acknowledging, and drops their notices not yet sent.
#[test]
fn starting_a_round_acknowledges_every_earlier_round() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let tx = store.write()?;
    tx.execute_batch(
        "UPDATE task_round SET state = 'cancelled', finished_at = 2;
         INSERT INTO task_round (task_id, round, state, finished_at, created_at)
           VALUES ('t', 2, 'failed', 3, 1);
         INSERT INTO notice (id, task_id, round, kind, state, created_at)
           VALUES ('n1', 't', 1, 'finished', 'pending', 2), ('n2', 't', 2, 'finished', 'pending', 3);",
    )?;
    assert_eq!(start_round(&tx, "t", &Kind::Request("again"))?, 3);
    let left: Vec<(i64, bool, String)> = tx
        .prepare(
            "SELECT r.round, r.acked_at IS NOT NULL, n.state FROM task_round r
             JOIN notice n ON n.task_id = r.task_id AND n.round = r.round ORDER BY r.round",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        left,
        [
            (1, true, "superseded".into()),
            (2, true, "superseded".into())
        ]
    );
    Ok(())
}

/// A queued round belongs to the server instance its request was sent on, also once it starts on a
/// later one: when that instance restores the worker's pane with a shell, the round ends with the
/// rest of the earlier instance's rounds.
#[test]
fn a_queued_round_started_on_a_later_instance_ends_with_its_pane() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let tx = store.write()?;
    start_round(&tx, "t", &Kind::Request("first"))?;
    tx.execute_batch(
        "UPDATE task_round SET created_at = 0 WHERE round = 2;
         INSERT INTO epoch (id, instance_id, started_at) VALUES (2, 'i2', 1);",
    )?;
    abandon_round(&tx, "t", None)?;
    assert_eq!(round_state(&tx, "t", None)?, (2, "running".into()));
    end_left_to_server(&tx, "i2", "b")?;
    assert_eq!(round_state(&tx, "t", None)?, (2, "terminated".into()));
    Ok(())
}

/// A pane takes a new task only once its task is released and the daemon is not closing it.
#[test]
fn a_pane_takes_no_second_task_until_its_task_is_released() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let store = task_world(dir.path(), transcript.path(), None)?;
    let refusal = |store: &Store| {
        refuse_taken(store.conn(), "b", "block:b")
            .err()
            .map(|e| e.to_string())
    };
    assert_eq!(
        refusal(&store),
        Some(
            "pane block:b is the worker of task t, which is not released; send that task another \
             round, or release it first"
                .into()
        )
    );
    store
        .conn()
        .execute_batch("UPDATE task SET released_at = 1, pane_state = 'closing'")?;
    assert_eq!(
        refusal(&store),
        Some("rexume is closing pane block:b".into())
    );
    store
        .conn()
        .execute_batch("UPDATE task SET pane_state = 'open'")?;
    assert_eq!(refusal(&store), None);
    Ok(())
}
