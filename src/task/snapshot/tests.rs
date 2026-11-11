use std::fs::File;
use std::io::Write as _;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::json;

use super::*;
use crate::task::outcome;
use crate::task::queue::Queued;
use crate::task::register::{Kind, Started, start_round};
use crate::task::tests::{PROMPT, record, task_world};

pub(super) fn running(state_dir: &Path, task: &str, capture: &str) -> Result<File> {
    let lock = idle(state_dir, task, capture)?;
    lock.try_lock()?;
    Ok(lock)
}

pub(super) fn idle(state_dir: &Path, task: &str, capture: &str) -> Result<File> {
    let dir = task_dir(state_dir, task);
    std::fs::create_dir_all(&dir)?;
    Ok(File::create(dir.join(format!("{capture}.lock")))?)
}

/// Polls: a process another test starts keeps this test's files open until it runs its program,
/// so a lock this test dropped stays held a while, and a capture's processes hold its lock until
/// they exit.
pub(super) fn finished(
    store: &mut Store,
    mut finish: impl FnMut(&mut Store) -> Result<()>,
    mut done: impl FnMut(&Store) -> Result<bool>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        finish(store)?;
        if done(store)? {
            return Ok(());
        }
        assert!(Instant::now() < deadline, "the captures were not finished");
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(super) fn killed(
    state_dir: &Path,
    temp: &Path,
    (task, capture): (&str, &str),
    objects: &[&str],
) -> Result<PathBuf> {
    let mut lock = idle(state_dir, task, capture)?;
    let temp = temp.join(format!(".rexume-{capture}"));
    std::fs::create_dir(&temp)?;
    std::fs::write(temp.join("index"), "index")?;
    lock.write_all(temp.as_os_str().as_encoded_bytes())?;
    for object in objects {
        let path = task_dir(state_dir, task).join(capture).join(object);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, "object")?;
    }
    Ok(temp)
}

pub(super) fn names(dir: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_>>()?;
    names.sort();
    Ok(names)
}

fn taken(tree: &str, created_at: i64) -> Taken {
    Taken {
        tree: Some(tree.to_owned()),
        note: None,
        created_at,
    }
}

fn rows(conn: &Connection) -> Result<Vec<(i64, String, Option<String>)>> {
    Ok(conn
        .prepare("SELECT round, side, tree FROM snapshot WHERE task_id = 't' ORDER BY round, side")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn finished_at(conn: &Connection, round: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT finished_at FROM task_round WHERE task_id = 't' AND round = ?1",
        params![round],
        |row| row.get(0),
    )?)
}

#[test]
fn an_after_is_recorded_once_from_a_snapshot_begun_after_the_round_finished() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let tx = store.write()?;
    record_after(&tx, "t", &taken("early", now_ms()))?;
    outcome::finish(&tx, "t", 1, "failed", None, Some("done"), Queued::Start)?;
    let finished = finished_at(&tx, 1)?;
    record_after(&tx, "t", &taken("during", finished - 1))?;
    assert_eq!(rows(&tx)?, []);
    record_after(&tx, "t", &taken("first", finished))?;
    record_after(&tx, "t", &taken("second", finished + 1))?;
    assert_eq!(rows(&tx)?, [(1, "after".into(), Some("first".into()))]);
    Ok(())
}

#[test]
fn a_round_a_queued_round_followed_gets_no_after() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let tx = store.write()?;
    assert_eq!(
        start_round(&tx, "t", &Kind::Request("next"), None)?,
        Started::Round(2)
    );
    outcome::finish(&tx, "t", 1, "failed", None, Some("done"), Queued::Start)?;
    record_after(&tx, "t", &taken("later", finished_at(&tx, 1)? + 1))?;
    assert_eq!(rows(&tx)?, []);
    Ok(())
}

#[test]
fn a_round_started_at_once_gets_a_before_and_a_queued_one_none() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let tx = store.write()?;
    tx.execute("UPDATE task SET worktree = '/repo'", [])?;
    outcome::finish(&tx, "t", 1, "failed", None, Some("done"), Queued::Start)?;
    let shared = taken("shared", finished_at(&tx, 1)?);
    assert_eq!(
        start_round(&tx, "t", &Kind::Request("a"), Some(&shared))?,
        Started::Round(2)
    );
    let queued = taken("queued", now_ms());
    assert_eq!(
        start_round(&tx, "t", &Kind::Request("b"), Some(&queued))?,
        Started::Round(3)
    );
    assert_eq!(
        rows(&tx)?,
        [
            (1, "after".into(), Some("shared".into())),
            (2, "before".into(), Some("shared".into())),
        ]
    );
    Ok(())
}

#[test]
fn a_round_starts_at_once_only_with_a_snapshot_begun_after_the_latest_finished() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let tx = store.write()?;
    tx.execute("UPDATE task SET worktree = '/repo'", [])?;
    outcome::finish(&tx, "t", 1, "failed", None, Some("done"), Queued::Start)?;
    let finished = finished_at(&tx, 1)?;
    let during = taken("during", finished - 1);
    for taken in [None, Some(&during)] {
        assert_eq!(
            start_round(&tx, "t", &Kind::Request("a"), taken)?,
            Started::SnapshotDue("/repo".into())
        );
    }
    let rounds: i64 = tx.query_row("SELECT COUNT(*) FROM task_round", [], |row| row.get(0))?;
    assert_eq!((rounds, rows(&tx)?), (1, vec![]));
    let fresh = taken("fresh", finished);
    assert_eq!(
        start_round(&tx, "t", &Kind::Request("a"), Some(&fresh))?,
        Started::Round(2)
    );
    assert_eq!(
        rows(&tx)?,
        [
            (1, "after".into(), Some("fresh".into())),
            (2, "before".into(), Some("fresh".into())),
        ]
    );
    Ok(())
}

#[test]
fn task_shows_the_working_tree_and_each_side_taken() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let tx = store.write()?;
    tx.execute("UPDATE task SET worktree_note = 'not in a repository'", [])?;
    assert_eq!(
        serde_json::to_value(shown(&tx, dir.path(), "t", 1)?)?,
        json!({"worktree": null, "note": "not in a repository", "object_directory": null,
               "before": null, "after": null})
    );
    tx.execute(
        "UPDATE task SET worktree = '/repo', worktree_note = NULL",
        [],
    )?;
    let failed = Taken {
        tree: None,
        note: Some("the repository has a split index".into()),
        created_at: 7,
    };
    record_before(&tx, "t", 1, &failed)?;
    assert_eq!(
        serde_json::to_value(shown(&tx, dir.path(), "t", 1)?)?,
        json!({"worktree": "/repo", "note": null,
               "object_directory": dir.path().join("snapshots/t/objects"),
               "before": {"tree": null, "note": "the repository has a split index",
                          "created_at": 7},
               "after": null})
    );
    Ok(())
}

mod recovery;
