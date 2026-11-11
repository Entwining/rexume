use std::io::Write as _;

use anyhow::Result;
use serde_json::json;

use crate::runtime::Runtime;
use crate::store::Store;
use crate::task::follow::Follow;
use crate::task::gone::block_gone;
use crate::task::register::{Kind, Started, start_round};
use crate::task::tests::{ANSWER, PROMPT, record, task_world};
use crate::task::{shown, unacknowledged};

/// A round's state, note, and whether it is acknowledged, with its request's state and note.
type Row = (String, Option<String>, bool, Option<String>, Option<String>);

/// Each round of task `t` in order.
fn rounds(store: &Store) -> Result<Vec<Row>> {
    Ok(store
        .conn()
        .prepare(
            "SELECT r.state, r.note, r.acked_at IS NOT NULL, q.state, q.note FROM task_round r
             LEFT JOIN request q ON q.task_id = r.task_id AND q.round = r.round
             WHERE r.task_id = 't' ORDER BY r.round",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn send(store: &mut Store, message: &str) -> Result<i64> {
    let tx = store.write()?;
    let Started::Round(round) = start_round(&tx, "t", &Kind::Request(message), None)? else {
        anyhow::bail!("a snapshot is due");
    };
    tx.commit()?;
    Ok(round)
}

fn queued() -> Row {
    let note = "waiting for the rounds before it to finish".into();
    (
        "queued".into(),
        None,
        false,
        Some("pending".into()),
        Some(note),
    )
}

/// Requests sent while round 1 runs wait behind it in the order sent and acknowledge nothing, and
/// none is sent while round 1 is open, even to a worker between turns; the pass that finishes round
/// 1 starts the first of them and tries to send it, and the prompt that continues the owner lists
/// every round it has not acknowledged.
#[test]
fn requests_sent_during_a_round_wait_in_order_and_start_once_it_finishes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    assert_eq!(send(&mut store, "first")?, 2);
    assert_eq!(send(&mut store, "second")?, 3);
    let running: Row = ("running".into(), None, false, None, None);
    let waiting_behind = [running, queued(), queued()];
    assert_eq!(rounds(&store)?, waiting_behind);
    let task = shown(store.conn(), store.dir(), "i1", "t", None)?;
    assert_eq!(
        (&task["round"], &task["queued"]),
        (&json!(1), &json!([2, 3]))
    );
    let status = |store: &Store, status: &str| {
        store
            .conn()
            .execute("UPDATE block SET status = ?1 WHERE lineage = 'b'", [status])
    };
    status(&store, "idle")?;
    Follow::default().pass(&mut store, None)?;
    assert_eq!(rounds(&store)?, waiting_behind);
    status(&store, "working")?;

    writeln!(transcript, "{ANSWER}")?;
    transcript.flush()?;
    Follow::default().pass(&mut store, None)?;
    let rows = rounds(&store)?;
    assert_eq!((rows[0].0.as_str(), rows[0].2), ("completed", false));
    // The worker still reports working, so the request waits for it to end its turn.
    let waiting = Some("waiting for the worker to end its turn".into());
    assert_eq!(
        rows[1..],
        [
            (
                "running".into(),
                None,
                false,
                Some("pending".into()),
                waiting
            ),
            queued()
        ]
    );
    assert_eq!(
        unacknowledged(store.conn(), Runtime::Claude, "owner")?,
        [
            "t \"count\": round 1 is completed",
            "t \"count\": round 2 is running",
            "t \"count\": round 3 is queued"
        ]
    );
    Ok(())
}

/// How the worker stops taking requests while round 1 is open.
enum Gone {
    /// Its pane closes, after the record shows round 1's turn or before.
    Closed { answered: bool },
    /// Its record is replaced, so its turns can no longer be told apart.
    Replaced,
}

/// When the worker can take no further request, the rounds queued behind the open one are dropped
/// unsent, each with its own finished notice, also when the record already showed the open round's
/// turn before the pane closed.
#[test]
fn queued_rounds_end_when_the_worker_can_take_no_further_request() -> Result<()> {
    for (gone, first, why) in [
        (
            Gone::Closed { answered: false },
            "worker_closed",
            "round 1 ended as worker_closed before it started",
        ),
        (
            Gone::Closed { answered: true },
            "completed",
            "the worker's pane closed before the round started",
        ),
        (
            Gone::Replaced,
            "failed",
            "round 1 ended as failed before it started",
        ),
    ] {
        let dir = tempfile::tempdir()?;
        let lines: &[&str] = match gone {
            Gone::Closed { answered: true } => &[PROMPT, ANSWER],
            _ => &[PROMPT],
        };
        let transcript = record(lines)?;
        let mut store = task_world(dir.path(), transcript.path(), None)?;
        send(&mut store, "first")?;
        send(&mut store, "second")?;
        let mut follow = Follow::default();
        match gone {
            Gone::Closed { .. } => {
                let tx = store.write()?;
                block_gone(&tx, "b")?;
                tx.commit()?;
            }
            // The daemon must have read the record to see it replaced.
            Gone::Replaced => {
                follow.pass(&mut store, None)?;
                let replacement = record(&[PROMPT, ANSWER])?;
                std::fs::rename(replacement.path(), transcript.path())?;
            }
        }
        follow.pass(&mut store, None)?;
        let rows = rounds(&store)?;
        assert_eq!(rows[0].0, first);
        let ended: Row = (
            "dropped".into(),
            Some(why.into()),
            false,
            Some("superseded".into()),
            Some("the round ended before it was sent".into()),
        );
        assert_eq!(rows[1..], [ended.clone(), ended]);
        let notices: Vec<i64> = store
            .conn()
            .prepare("SELECT round FROM notice WHERE kind = 'finished' ORDER BY created_at, id")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(notices, [1, 2, 3]);
    }
    Ok(())
}
