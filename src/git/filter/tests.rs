use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::git::tests::{
    age, capture, fake_lfs, git, lfs_repository, listing, repository, script, staged, untouched,
    write,
};
use crate::git::worktree;

mod lfs;

/// A repository whose `*.secret` files the clean filter `up` turns to upper case, after doing
/// `first`, recording each run in `tools`, and that holds `s.secret` cleaned.
fn filtered(root: &Path, tools: &Path, first: &str) -> Result<PathBuf> {
    let body = format!(
        "{first}echo run >> '{}' && tr a-z A-Z",
        tools.join("runs").display()
    );
    let up = script(tools, "up", &body)?;
    let repo = repository(root)?;
    git(&repo, &["config", "filter.up.clean", &up.to_string_lossy()])?;
    git(&repo, &["config", "filter.up.required", "true"])?;
    write(&repo.join(".gitattributes"), "*.secret filter=up\n")?;
    write(&repo.join("s.secret"), "plain\n")?;
    git(&repo, &["add", "-A"])?;
    git(&repo, &["commit", "-qm", "secret"])?;
    Ok(repo)
}

/// How many times the filter `filtered` configured has run.
fn runs(tools: &Path) -> Result<usize> {
    match std::fs::read_to_string(tools.join("runs")) {
        Ok(runs) => Ok(runs.lines().count()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}

/// Whether this process can deny a command writes, which on macOS a sandboxed one cannot.
fn can_deny_writes() -> bool {
    cfg!(target_os = "macos")
        && Command::new("/usr/bin/sandbox-exec")
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

/// A filter that no changed file needs does not run, and does not keep the snapshot from being
/// taken: a filtered file that is unchanged, or deleted, is not read.
#[test]
fn a_filter_runs_for_no_file_unchanged_or_deleted() -> Result<()> {
    let (root, tools, state) = (
        tempfile::tempdir()?,
        tempfile::tempdir()?,
        tempfile::tempdir()?,
    );
    let repo = filtered(root.path(), tools.path(), "")?;
    write(&repo.join("gone.secret"), "gone\n")?;
    git(&repo, &["add", "-A"])?;
    git(&repo, &["commit", "-qm", "gone"])?;
    std::fs::remove_file(repo.join("gone.secret"))?;
    write(&repo.join("a.txt"), "changed\n")?;
    age(root.path())?;
    // The index records the aged files, so that only what changed is read.
    git(&repo, &["update-index", "-q", "--refresh"])?;
    let (before, ran) = (listing(root.path())?, runs(tools.path())?);
    let tree = capture(&worktree(&repo)?, state.path())?;
    assert!(listing(root.path())? == before, "the repository changed");
    assert_eq!(runs(tools.path())?, ran, "the filter ran");
    git(&repo, &["add", "-A"])?;
    assert_eq!(tree, git(&repo, &["write-tree"])?);
    Ok(())
}

/// A filter other than Git LFS's runs for a changed file only denied writes to the repository: a
/// filter that writes there fails, and where writes cannot be denied, the filter does not run and
/// no snapshot is taken. Either way the repository stays as it was. The change keeps the file's
/// size, so that finding it changed compares its content, which a required filter that is off
/// would fail.
#[test]
fn a_changed_file_runs_its_filter_only_denied_writes_to_the_repository() -> Result<()> {
    let deny = can_deny_writes();
    for (first, writes) in [("", false), ("chmod 600 a.txt && touch .git/up && ", true)] {
        let (root, tools, state) = (
            tempfile::tempdir()?,
            tempfile::tempdir()?,
            tempfile::tempdir()?,
        );
        let repo = filtered(root.path(), tools.path(), first)?;
        write(&repo.join("s.secret"), "plaix\n")?;
        let ran = runs(tools.path())?;
        let (top, taken) = untouched(root.path(), &repo, state.path())?;
        assert_eq!(runs(tools.path())? > ran, deny && !writes, "{first}");
        match staged(&top, taken)? {
            Ok(_) if deny && !writes => {}
            Err(note) if deny && writes => assert!(
                note.starts_with("1 changed file needs the filter `up`, and `git add` exited"),
                "{note}"
            ),
            Err(note) if !deny => assert!(
                note.starts_with("1 changed file needs the filter `up`"),
                "{note}"
            ),
            taken => panic!("{first}: {taken:?}"),
        }
    }
    Ok(())
}

/// A filter the changed files were not found to need does not run even if `git add` needs it, so
/// it never runs where it could write in the repository. Finding the changed files compares the
/// content unfiltered, which here equals what is staged, so `git add` alone filters the file.
#[test]
fn a_filter_not_found_needed_does_not_run() -> Result<()> {
    let (root, tools, state) = (
        tempfile::tempdir()?,
        tempfile::tempdir()?,
        tempfile::tempdir()?,
    );
    let repo = filtered(root.path(), tools.path(), "")?;
    write(&repo.join("s.secret"), "PLAIN\n")?;
    let ran = runs(tools.path())?;
    let (_, taken) = untouched(root.path(), &repo, state.path())?;
    let said = taken.unwrap_err();
    assert!(said.contains("clean filter 'up' failed"), "{said}");
    assert_eq!(runs(tools.path())?, ran, "the filter ran");
    Ok(())
}

/// A filter that fails stores nothing: Git keeps a file unfiltered when a filter that is not
/// required fails, so every filter runs required. Git LFS's runs wherever Rexume runs.
#[test]
fn a_filter_that_fails_leaves_nothing_unfiltered() -> Result<()> {
    let (root, tools, state) = (
        tempfile::tempdir()?,
        tempfile::tempdir()?,
        tempfile::tempdir()?,
    );
    // An error for the one file to clean. Every Git version that has the protocol keeps that file
    // unfiltered unless the filter is required.
    let refuse = "printf '0011status=error\\n0000'\ncat >/dev/null";
    let repo = lfs_repository(root.path(), &fake_lfs(tools.path(), refuse)?)?;
    git(&repo, &["config", "filter.lfs.required", "false"])?;
    write(&repo.join("big.bin"), "unfiltered\n")?;
    let said = capture(&worktree(&repo)?, state.path())
        .unwrap_err()
        .to_string();
    assert!(
        said.contains("needs the filter `lfs`, and `git add` exited"),
        "{said}"
    );
    let unfiltered = git(&repo, &["hash-object", "--no-filters", "big.bin"])?;
    let (dir, file) = unfiltered.split_at(2);
    assert!(!state.path().join("objects").join(dir).join(file).exists());
    Ok(())
}
