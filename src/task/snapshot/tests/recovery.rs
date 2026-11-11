use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::time::SystemTime;

use super::*;

#[test]
fn the_sweep_deletes_only_old_directories_of_tasks_gone() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    idle(dir.path(), "t", "killed")?;
    let _running = running(dir.path(), "t", "running")?;
    let orphaned = running(dir.path(), "orphaned", "c")?;
    let old = SystemTime::now() - RETENTION - Duration::from_secs(60);
    for (name, modified) in [
        ("t", old),
        ("archived", old),
        ("orphaned", old),
        ("registering", SystemTime::now()),
    ] {
        let path = super::task_dir(dir.path(), name);
        std::fs::create_dir_all(path.join("objects"))?;
        std::fs::File::open(&path)?.set_modified(modified)?;
    }
    for task in ["t", "archived", "orphaned", "registering", "vanished"] {
        store.conn().execute(
            "INSERT INTO snapshot_space (task_id, id, state, bytes) VALUES (?1, 'c', 'kept', 1)",
            params![task],
        )?;
    }
    sweep(&mut store)?;
    let counted = |store: &Store| -> Result<Vec<String>> {
        Ok(store
            .conn()
            .prepare("SELECT task_id FROM snapshot_space ORDER BY task_id")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    };
    assert_eq!(
        names(&dir.path().join("snapshots"))?,
        ["orphaned", "registering", "t"]
    );
    assert_eq!(
        names(&super::task_dir(dir.path(), "t"))?,
        ["objects", "running.lock"]
    );
    assert_eq!(counted(&store)?, ["orphaned", "registering", "t"]);
    drop(orphaned);
    let snapshots = dir.path().join("snapshots");
    finished(&mut store, sweep, |_| {
        Ok(names(&snapshots)? == ["registering", "t"])
    })?;
    assert_eq!(counted(&store)?, ["registering", "t"]);
    Ok(())
}

#[test]
fn objects_that_failed_to_move_are_moved_by_the_next_capture() -> Result<()> {
    let (dir, root) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let mut store = Store::open(dir.path())?;
    let repo = crate::git::tests::repository(root.path())?;
    std::fs::write(repo.join("new.txt"), "untracked\n")?;
    let objects = git::object_directory(&task_dir(dir.path(), "t"));
    std::fs::create_dir_all(&objects)?;
    std::fs::set_permissions(&objects, Permissions::from_mode(0o500))?;
    let top = git::worktree(&repo)?;
    let taken = take(&mut store, "t", &top.to_string_lossy());
    std::fs::set_permissions(&objects, Permissions::from_mode(0o700))?;
    assert!(taken.tree.is_none());
    let note = taken.note.unwrap_or_default();
    assert!(note.contains("failed to keep"), "{note}");
    let (capture, bytes): (String, i64) = store.conn().query_row(
        "SELECT id, bytes FROM snapshot_space WHERE task_id = 't' AND state = 'kept'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let left = task_dir(dir.path(), "t").join(&capture);
    let staged = size(&left)?;
    assert_eq!(i64::try_from(staged)?, bytes);
    finished(
        &mut store,
        |store| space::recover(store, "t"),
        |_| Ok(!left.exists()),
    )?;
    assert!(
        !task_dir(dir.path(), "t")
            .join(format!("{capture}.lock"))
            .exists()
    );
    assert_eq!(size(&objects)?, staged);
    let rows: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM snapshot_space", [], |row| row.get(0))?;
    assert_eq!(rows, 1);
    Ok(())
}

fn size(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        total += if path.is_dir() {
            size(&path)?
        } else {
            std::fs::metadata(&path)?.len()
        };
    }
    Ok(total)
}

#[test]
fn objects_a_failed_capture_cannot_delete_keep_their_reservation() -> Result<()> {
    let (dir, root, tools) = (
        tempfile::tempdir()?,
        tempfile::tempdir()?,
        tempfile::tempdir()?,
    );
    let mut store = Store::open(dir.path())?;
    // Git LFS's filter runs after Git stored the changed tracked file.
    let body =
        "chmod -R a-w \"$GIT_OBJECT_DIRECTORY\"\nprintf '0011status=error\\n0000'\ncat >/dev/null";
    let repo = git::tests::lfs_repository(root.path(), &git::tests::fake_lfs(tools.path(), body)?)?;
    std::fs::write(repo.join("a.txt"), "changed\n")?;
    std::fs::write(repo.join("b.bin"), "large\n")?;
    let top = git::worktree(&repo)?;
    let taken = take(&mut store, "t", &top.to_string_lossy());
    assert!(taken.tree.is_none());
    let (capture, state, bytes): (String, String, i64) = store.conn().query_row(
        "SELECT id, state, bytes FROM snapshot_space WHERE task_id = 't'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(state, "reserved");
    let left = task_dir(dir.path(), "t").join(&capture);
    assert!(size(&left)? > 0);
    assert_eq!(bytes, i64::try_from(size(&left)?)?);
    assert!(
        !std::env::temp_dir()
            .join(format!(".rexume-{capture}"))
            .exists()
    );
    let mut dirs = vec![left.clone()];
    while let Some(dir) = dirs.pop() {
        std::fs::set_permissions(&dir, Permissions::from_mode(0o700))?;
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }
    let t = task_dir(dir.path(), "t");
    finished(
        &mut store,
        |store| space::recover(store, "t"),
        |_| Ok(names(&t)? == ["objects"]),
    )?;
    let rows: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM snapshot_space", [], |row| row.get(0))?;
    assert_eq!(rows, 0);
    Ok(())
}

#[test]
fn a_refused_registration_deletes_what_its_captures_left() -> Result<()> {
    let (dir, temp) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let mut store = Store::open(dir.path())?;
    let left = killed(dir.path(), temp.path(), ("t", "c"), &["ab/cdef"])?;
    let _running = running(dir.path(), "u", "c")?;
    for task in ["t", "u"] {
        store.conn().execute(
            "INSERT INTO snapshot_space (task_id, id, state, bytes) VALUES (?1, 'c', 'reserved', 1)",
            params![task],
        )?;
    }
    for task in ["t", "u"] {
        discard(&mut store, task)?;
    }
    assert!(!left.exists());
    assert_eq!(names(&dir.path().join("snapshots"))?, ["u"]);
    let counted: Vec<String> = store
        .conn()
        .prepare("SELECT task_id FROM snapshot_space")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(counted, ["u"]);
    Ok(())
}
