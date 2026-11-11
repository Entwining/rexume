use anyhow::Result;

use super::*;
use crate::task::acknowledge::abandon_round;
use crate::task::end_left_to_server;
use crate::task::tests::{PROMPT, record, task_world};

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
    assert_eq!(
        start_round(&tx, "t", &Kind::Request("again"), None)?,
        Started::Round(3)
    );
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

#[test]
fn a_queued_round_started_on_a_later_instance_ends_with_its_pane() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let tx = store.write()?;
    start_round(&tx, "t", &Kind::Request("first"), None)?;
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
