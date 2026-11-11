//! What bounds a snapshot: its deadline, the paths it takes, and what it reads of Git's output.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::{fake_lfs, left, lfs_repository, repository, write};
use crate::git::run::Git;
use crate::git::{BUDGET, Capture, PATH_LIMIT, worktree};

/// A snapshot still running at its deadline ends with every process it started, and leaves
/// nothing behind.
#[test]
fn a_snapshot_ends_with_its_processes_at_its_deadline() -> Result<()> {
    let (root, tools, state) = (
        tempfile::tempdir()?,
        tempfile::tempdir()?,
        tempfile::tempdir()?,
    );
    let pid = tools.path().join("pid");
    let body = format!("echo $$ > '{}'\nsleep 30", pid.display());
    let repo = lfs_repository(root.path(), &fake_lfs(tools.path(), &body)?)?;
    write(&repo.join("big.bin"), "slow\n")?;
    let budget = Duration::from_secs(2);
    let began = Instant::now();
    let mut capture = Capture::list(&worktree(&repo)?, state.path(), budget)?;
    let note = format!("{:#}", capture.stage().unwrap_err());
    drop(capture);
    let took = began.elapsed();
    assert_eq!(note, "the snapshot took longer than 2 seconds");
    assert!(took < budget + Duration::from_secs(1), "{took:?}");
    let pid: i32 = std::fs::read_to_string(&pid)?.trim().parse()?;
    let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
    assert!(!alive, "the filter outlived the deadline");
    assert!(left(state.path())?.is_empty());
    Ok(())
}

/// More changed and untracked paths than a snapshot takes get none.
#[test]
fn a_snapshot_takes_no_more_than_its_paths() -> Result<()> {
    let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let repo = repository(root.path())?;
    for i in 0..=PATH_LIMIT {
        std::fs::File::create(repo.join(format!("f{i}")))?;
    }
    let note = format!(
        "{:#}",
        super::capture(&worktree(&repo)?, state.path()).unwrap_err()
    );
    assert_eq!(
        note,
        "more than 10000 files are changed or untracked, the most a snapshot takes"
    );
    assert!(left(state.path())?.is_empty());
    Ok(())
}

/// A note quotes only the beginning of a command's standard error, however much it prints.
#[test]
fn a_note_quotes_the_beginning_of_what_a_command_printed() -> Result<()> {
    let mut git = Git::new(BUDGET)?;
    let print = "head -c 5000000 /dev/zero | tr '\\0' e >&2; exit 3";
    let ran = git.run(Command::new("sh"), &["-c", print], Stdio::null(), false)?;
    let quoted = ran.stderr.as_deref().unwrap_or_default();
    assert_eq!(quoted.len(), 4 << 10);
    assert!(ran.check().is_err());
    Ok(())
}
