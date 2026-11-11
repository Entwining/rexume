use std::time::{Duration, SystemTime};

use anyhow::Result;

use super::{fake_lfs, git, holds, left, lfs_repository, objects, random, repository, write};
use crate::git::{BUDGET, Capture, worktree};

#[test]
fn a_file_changed_as_the_index_was_written_is_found() -> Result<()> {
    let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let repo = repository(root.path())?;
    git(&repo, &["config", "core.trustctime", "false"])?;
    let at = SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::open(repo.join("a.txt"))?.set_modified(at)?;
    git(&repo, &["update-index", "--refresh"])?;
    std::fs::File::open(repo.join(".git/index"))?.set_modified(at)?;
    std::fs::write(repo.join("a.txt"), "six\n")?;
    std::fs::File::open(repo.join("a.txt"))?.set_modified(at)?;
    let top = worktree(&repo)?;
    let tree = super::capture(&top, state.path())?;
    git(&top, &["add", "-A"])?;
    assert_eq!(tree, git(&top, &["write-tree"])?);
    Ok(())
}

#[test]
fn a_capture_writes_only_its_lock_file_before_it_stages() -> Result<()> {
    let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let repo = repository(root.path())?;
    random(&repo.join("big.bin"), 1 << 20)?;
    git(&repo, &["add", "big.bin"])?;
    git(&repo, &["commit", "-qm", "big"])?;
    write(&repo.join("big.bin"), "x\n")?;
    let capture = Capture::list(&worktree(&repo)?, state.path(), BUDGET)?;
    assert_eq!(capture.estimate(), 2);
    let lock = std::ffi::OsString::from(format!("{}.lock", capture.id()));
    assert_eq!(left(state.path())?, [lock]);
    assert_eq!(objects(state.path())?, Vec::<std::path::PathBuf>::new());
    Ok(())
}

#[test]
fn a_capture_keeps_its_objects_only_whole_and_once() -> Result<()> {
    let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let repo = repository(root.path())?;
    write(&repo.join("new.txt"), "untracked\n")?;
    let top = worktree(&repo)?;
    holds(root.path(), &repo, state.path())?.expect("a tree");
    let mut again = Capture::list(&top, state.path(), BUDGET)?;
    assert_eq!(again.stage()?.1, 0, "the second capture wrote objects");
    drop(again);
    // A nested repository without a commit fails `git add` after it stored the files before it.
    write(&repo.join("a-new.txt"), "untracked\n")?;
    std::fs::create_dir(repo.join("nested"))?;
    git(&repo.join("nested"), &["init", "-q"])?;
    let note = holds(root.path(), &repo, state.path())?.expect_err("no tree");
    assert!(
        note.contains("does not have a commit checked out"),
        "{note}"
    );
    Ok(())
}

#[test]
fn a_file_grown_after_the_listing_gets_no_snapshot() -> Result<()> {
    let (root, tools, state) = (
        tempfile::tempdir()?,
        tempfile::tempdir()?,
        tempfile::tempdir()?,
    );
    let grow = format!(
        "head -c {} /dev/urandom >> grow.txt\nprintf '0013status=success\\n000000000000'\ncat >/dev/null",
        crate::git::CONTENT_LIMIT + (1 << 20)
    );
    let repo = lfs_repository(root.path(), &fake_lfs(tools.path(), &grow)?)?;
    write(&repo.join("a.bin"), "changed\n")?;
    write(&repo.join("grow.txt"), "small\n")?;
    let before = objects(state.path())?;
    let note = super::capture(&worktree(&repo)?, state.path()).unwrap_err();
    assert!(
        format!("{note:#}").contains("the snapshot's objects take"),
        "{note:#}"
    );
    assert!(objects(state.path())? == before);
    Ok(())
}
