use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;

use anyhow::Result;
use rusqlite::params;

use super::*;
use crate::task::snapshot::tests::{finished, killed, names, running};

const MIB: u64 = 1 << 20;

/// Each row of `snapshot_space`, as (task, capture, state, bytes).
fn rows(store: &Store) -> Result<Vec<(String, String, String, i64)>> {
    Ok(store
        .conn()
        .prepare("SELECT task_id, id, state, bytes FROM snapshot_space ORDER BY task_id, id")?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn row(task: &str, capture: &str, state: &str, bytes: u64) -> (String, String, String, i64) {
    let bytes = i64::try_from(bytes).unwrap();
    (task.to_owned(), capture.to_owned(), state.to_owned(), bytes)
}

/// Two captures reserving at once, from their own connections, cannot both take the space left
/// for one.
#[test]
fn a_capture_is_refused_the_space_another_reserved() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut first, mut second) = (Store::open(dir.path())?, Store::open(dir.path())?);
    reserve(&mut first, "t1", "c1", 600 * MIB)?;
    let said = reserve(&mut second, "t2", "c2", 600 * MIB)
        .unwrap_err()
        .to_string();
    assert_eq!(
        said,
        "snapshots take 600 MiB of the 1024 MiB they may in the state directory, too little for \
         this one's 600 MiB"
    );
    assert_eq!(rows(&first)?, [row("t1", "c1", "reserved", 600 * MIB)]);
    Ok(())
}

/// A capture settles on the size of what it wrote, or forgets its row when it wrote nothing; one
/// that wrote more than the space left, or whose reservation is gone, is refused and keeps what
/// it reserved until it releases it, once it has deleted what it wrote.
#[test]
fn a_capture_settles_on_what_it_wrote() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    reserve(&mut store, "t", "c1", 10 * MIB)?;
    settle(&mut store, "t", "c1", 3 * MIB)?;
    reserve(&mut store, "t", "c2", 10 * MIB)?;
    settle(&mut store, "t", "c2", 0)?;
    assert_eq!(rows(&store)?, [row("t", "c1", "kept", 3 * MIB)]);
    reserve(&mut store, "t", "c3", 10 * MIB)?;
    let said = settle(&mut store, "t", "c3", SPACE_LIMIT.cast_unsigned()).unwrap_err();
    assert!(
        said.to_string().starts_with("snapshots take 3 MiB"),
        "{said}"
    );
    assert_eq!(rows(&store)?[1], row("t", "c3", "reserved", 10 * MIB));
    release(&mut store, "t", "c3")?;
    release(&mut store, "t", "c1")?;
    assert_eq!(rows(&store)?, [row("t", "c1", "kept", 3 * MIB)]);
    store.conn().execute("DELETE FROM snapshot_space", [])?;
    let said = settle(&mut store, "t", "c1", MIB).unwrap_err().to_string();
    assert_eq!(
        said,
        "the space reserved for the snapshot was released before it settled"
    );
    Ok(())
}

/// What killed captures left is finished by their rows once their locks can be taken: one that
/// had reserved space, its lock file gone or not, or had not yet, leaves nothing and counts no
/// more, and the objects of one that kept them move to the task's and keep counting. A capture
/// still running keeps its reservation, and another task's reservation is finished too.
#[test]
fn recovery_finishes_what_killed_captures_left() -> Result<()> {
    let (dir, temp) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let mut store = Store::open(dir.path())?;
    let left = [
        killed(dir.path(), temp.path(), ("t", "staging"), &["ab/cdef"])?,
        killed(dir.path(), temp.path(), ("t", "listing"), &[])?,
        killed(dir.path(), temp.path(), ("t", "keeping"), &["12/3456"])?,
        killed(dir.path(), temp.path(), ("u", "other"), &["ab/0000"])?,
    ];
    let _held = running(dir.path(), "t", "held")?;
    for (task, capture, state, bytes) in [
        ("t", "staging", "reserved", MIB),
        ("t", "deleted", "reserved", MIB),
        ("t", "keeping", "kept", 6),
        ("t", "held", "reserved", MIB),
        ("u", "other", "reserved", MIB),
    ] {
        store.conn().execute(
            "INSERT INTO snapshot_space (task_id, id, state, bytes) VALUES (?1, ?2, ?3, ?4)",
            params![task, capture, state, bytes.cast_signed()],
        )?;
    }
    recover(&mut store, "t")?;
    assert_eq!(
        rows(&store)?,
        [
            row("t", "held", "reserved", MIB),
            row("t", "keeping", "kept", 6)
        ]
    );
    for temp in left {
        assert!(!temp.exists(), "{}", temp.display());
    }
    let t = task_dir(dir.path(), "t");
    assert_eq!(names(&t)?, ["held.lock", "objects"]);
    assert_eq!(
        std::fs::read_to_string(t.join("objects/12/3456"))?,
        "object"
    );
    assert_eq!(names(&task_dir(dir.path(), "u"))?, Vec::<String>::new());
    Ok(())
}

/// A killed capture whose files cannot be deleted, nor read to measure them, keeps its reservation
/// counting all the space snapshots may take, so that no capture is given space until they are
/// deleted.
#[test]
fn files_a_capture_left_that_cannot_be_read_take_all_the_space() -> Result<()> {
    let (dir, temp) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let mut store = Store::open(dir.path())?;
    killed(dir.path(), temp.path(), ("t", "c"), &["ab/cdef"])?;
    let left = task_dir(dir.path(), "t").join("c");
    store.conn().execute(
        "INSERT INTO snapshot_space (task_id, id, state, bytes) VALUES ('t', 'c', 'reserved', 1)",
        [],
    )?;
    let fanout = left.join("ab");
    std::fs::set_permissions(&fanout, Permissions::from_mode(0o000))?;
    recover(&mut store, "t")?;
    let refused = reserve(&mut store, "u", "d", 1);
    std::fs::set_permissions(&fanout, Permissions::from_mode(0o700))?;
    assert_eq!(
        rows(&store)?,
        [row("t", "c", "reserved", u64::try_from(SPACE_LIMIT)?)]
    );
    assert!(refused.is_err());
    finished(
        &mut store,
        |store| recover(store, "t"),
        |_| Ok(!left.exists()),
    )?;
    reserve(&mut store, "u", "d", 1)?;
    assert_eq!(rows(&store)?, [row("u", "d", "reserved", 1)]);
    Ok(())
}
