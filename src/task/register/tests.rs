use anyhow::Result;

use super::*;
use crate::task::tests::{PROMPT, record, task_world};

/// Abandoning a blocked request round ends it acknowledged, drops what was not sent yet, and leaves
/// the task taking rounds; a finished round cannot be abandoned.
#[test]
fn abandoning_a_round_ends_and_acknowledges_it_and_drops_what_was_not_sent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    store.conn().execute_batch(
        "UPDATE task_round SET state = 'blocked', blocked_at = 5;
         INSERT INTO request (id, task_id, round, text) VALUES ('q', 't', 1, 'go');
         INSERT INTO notice (id, task_id, round, kind, state, created_at)
           VALUES ('nb', 't', 1, 'blocked', 'pending', 5);",
    )?;
    let tx = store.write()?;
    assert_eq!(abandon_round(&tx, "t")?, 1);
    tx.commit()?;
    let round: (String, Option<String>, bool, bool, Option<i64>) = store.conn().query_row(
        "SELECT state, note, finished_at IS NOT NULL, acked_at IS NOT NULL, blocked_at
         FROM task_round WHERE task_id = 't' AND round = 1",
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
            None
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
    let again = abandon_round(&tx, "t").unwrap_err().to_string();
    assert!(again.contains("already finished as abandoned"), "{again}");
    assert_eq!(start_round(&tx, "t", &Kind::Next { unread: Some(1) })?, 2);
    Ok(())
}
