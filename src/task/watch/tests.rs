use std::io::Write as _;

use anyhow::Result;
use rusqlite::params;

use super::Watch;
use crate::store::{Store, now_ms};
use crate::task::Interest;
use crate::task::acknowledge::{abandon_round, acknowledge, release_task};
use crate::task::follow::Follow;
use crate::task::register::{Kind, Started, start_round};
use crate::task::tests::{ANSWER, ANSWER_2, PROMPT, PROMPT_2, record, task_world};

const PROMPT_3: &str = r#"{"event":"prompt","prompt_id":"p3","token":null}"#;
const ANSWER_3: &str = r#"{"event":"stop","prompt_id":"p3","id":"m3","answer":"More."}"#;
const PROMPT_4: &str = r#"{"event":"prompt","prompt_id":"p4","token":null}"#;
const ANSWER_4: &str = r#"{"event":"stop","prompt_id":"p4","id":"m4","answer":"Last."}"#;

type Notice = (String, Option<i64>, String, Option<String>);

fn notices(store: &Store) -> Result<Vec<Notice>> {
    Ok(store
        .conn()
        .prepare("SELECT kind, round, state, note FROM notice ORDER BY rowid")?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn notice(kind: &str, round: Option<i64>, state: &str, note: Option<&str>) -> Notice {
    (kind.into(), round, state.into(), note.map(Into::into))
}

fn acked(store: &mut Store, round: i64) -> Result<()> {
    let tx = store.write()?;
    acknowledge(&tx, "t", round, "taken by task ack")?;
    tx.commit()?;
    Ok(())
}

fn append(file: &mut tempfile::NamedTempFile, lines: &[&str]) -> Result<()> {
    for line in lines {
        writeln!(file, "{line}")?;
    }
    file.flush()?;
    Ok(())
}

fn record_end(file: &tempfile::NamedTempFile) -> Result<i64> {
    Ok(i64::try_from(file.as_file().metadata()?.len())?)
}

fn blocked_since(store: &Store, at: i64) -> Result<()> {
    store.conn().execute(
        "UPDATE block SET status = 'blocked', status_kind = 'question', status_at = ?1
         WHERE lineage = 'b'",
        params![at],
    )?;
    Ok(())
}

#[test]
fn a_turn_finished_between_rounds_is_told_once_until_a_notice_reports_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut transcript = record(&[PROMPT, ANSWER])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    append(&mut transcript, &[PROMPT_2, ANSWER_2])?;
    follow.pass(&mut store, None)?;
    let finished = notice("finished", Some(1), "pending", None);
    assert_eq!(
        notices(&store)?,
        [finished],
        "the pending notice reports it"
    );

    acked(&mut store, 1)?;
    follow.pass(&mut store, None)?;
    append(&mut transcript, &[PROMPT_3, ANSWER_3])?;
    follow.pass(&mut store, None)?;
    let acked_notice = notice("finished", Some(1), "superseded", Some("taken by task ack"));
    let unread = notice("unread", None, "pending", None);
    assert_eq!(notices(&store)?, [acked_notice.clone(), unread.clone()]);

    store.conn().execute(
        "UPDATE notice SET state = 'sent', sent_at = 1, unread_end = ?1 WHERE kind = 'unread'",
        params![record_end(&transcript)?],
    )?;
    follow.pass(&mut store, None)?;
    assert_eq!(notices(&store)?.len(), 2, "its turns were reported");
    append(&mut transcript, &[PROMPT_4, ANSWER_4])?;
    follow.pass(&mut store, None)?;
    let sent = notice("unread", None, "sent", None);
    assert_eq!(notices(&store)?, [acked_notice, sent, unread]);
    Ok(())
}

#[test]
fn a_block_between_rounds_is_the_task_s_until_it_is_released() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    acked(&mut store, 1)?;
    blocked_since(&store, now_ms() - 2000)?;
    follow.pass(&mut store, None)?;
    let blocked = notice("blocked", None, "pending", None);
    assert_eq!(notices(&store)?[1], blocked);

    let tx = store.write()?;
    let next = Kind::Next { unread: None };
    assert_eq!(start_round(&tx, "t", &next, None)?, Started::Round(2));
    tx.commit()?;
    follow.pass(&mut store, None)?;
    let (state, episodes): (String, i64) = store.conn().query_row(
        "SELECT state, (SELECT COUNT(*) FROM blocked_episode) FROM task_round WHERE round = 2",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!((state.as_str(), episodes), ("blocked", 1));
    assert_eq!(notices(&store)?.len(), 2, "one notice per episode");

    let tx = store.write()?;
    abandon_round(&tx, "t", None)?;
    release_task(&tx, "t")?;
    tx.commit()?;
    follow.pass(&mut store, None)?;
    let reasons: Vec<String> = store
        .conn()
        .prepare("SELECT end_reason FROM blocked_episode")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(reasons, ["task_released"]);
    let note = Some("the block ended before it was sent");
    assert_eq!(
        notices(&store)?[1],
        notice("blocked", None, "superseded", note)
    );
    Ok(())
}

#[test]
fn a_pending_notice_keeps_the_worker_s_record_watched() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    assert_eq!(
        notices(&store)?,
        [notice("finished", Some(1), "pending", None)]
    );
    let interest = follow.pass(&mut store, None)?;
    assert!(
        interest
            .files
            .contains(&crate::runtime::claude::record(dir.path(), "worker")),
        "{:?}",
        interest.files
    );
    Ok(())
}

#[test]
fn a_release_after_the_read_keeps_a_new_episode_off_the_task() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT, ANSWER])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    Follow::default().pass(&mut store, None)?;
    acked(&mut store, 1)?;
    blocked_since(&store, now_ms() - 2000)?;
    let mut watch = Watch::default();
    let mut interest = Interest::default();
    let observed = watch.observe(&store, &mut interest)?;
    let tx = store.write()?;
    release_task(&tx, "t")?;
    tx.commit()?;
    watch.apply(&mut store, &observed)?;
    let episodes: i64 =
        store
            .conn()
            .query_row("SELECT COUNT(*) FROM blocked_episode", [], |row| row.get(0))?;
    assert_eq!(episodes, 0);
    assert_eq!(
        notices(&store)?,
        [notice(
            "finished",
            Some(1),
            "superseded",
            Some("taken by task ack")
        )]
    );
    Ok(())
}
