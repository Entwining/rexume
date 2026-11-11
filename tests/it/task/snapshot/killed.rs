//! A command killed while it takes a snapshot leaves nothing a later one does not delete: a signal
//! that stops it waits until it has ended the snapshot's processes and deleted what they wrote,
//! and after `SIGKILL` the next capture deletes what is left once the snapshot's processes exit.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use super::git;
use crate::common::{Daemon, Server, alive, query, wait_until};
use crate::task::run::owner_and_run;
use crate::task::{WAIT, run_task, task_command};

/// A repository whose `big.bin` changed, which a program Git takes for Git LFS's filter cleans:
/// it records its process ID in `pid` and waits while `hold` exists, then fails the file.
fn held_repository(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (repo, tools) = (dir.join("repo"), dir.join("tools"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    let (pid, hold) = (tools.join("pid"), tools.join("hold"));
    let lfs = tools.join("git-lfs");
    std::fs::write(
        &lfs,
        format!(
            "#!/bin/sh\nprintf '0016git-filter-server\\n000eversion=2\\n00000015capability=clean\\n0000'\n\
             echo $$ > '{}'\nwhile [ -e '{}' ]; do sleep 0.1; done\n\
             printf '0011status=error\\n0000'\ncat >/dev/null\n",
            pid.display(),
            hold.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&lfs, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join(".gitattributes"), "*.bin filter=lfs -text\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "attributes"]);
    let process = format!("{} filter-process", lfs.display());
    git(&repo, &["config", "filter.lfs.process", &process]);
    git(&repo, &["config", "filter.lfs.required", "true"]);
    git(&repo, &["config", "lfs.repositoryformatversion", "0"]);
    std::fs::write(repo.join("big.bin"), "large\n").unwrap();
    std::fs::write(&hold, "").unwrap();
    (repo, pid, hold)
}

/// Starts `task add` for `worker`, with `tmp` as its temporary directory, and returns it once
/// the filter it runs has recorded `pid`.
fn add_until_filtering(
    server: &Server,
    state: &Path,
    (owner, worker): (&str, &str),
    tmp: &Path,
    pid: &Path,
) -> (Child, u32) {
    let mut add = task_command(
        server,
        state,
        owner,
        &["add", "--worker", worker, "--title", "t"],
    );
    let child = add
        .env("TMPDIR", tmp)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let filter = wait_until("the filter to start", WAIT, || {
        std::fs::read_to_string(pid).ok()?.trim().parse().ok()
    });
    (child, filter)
}

/// The entries of each task's snapshot directory but its objects: the captures' lock files and
/// objects directories.
fn scratch(state: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for task in std::fs::read_dir(state.join("snapshots")).unwrap() {
        for entry in std::fs::read_dir(task.unwrap().path()).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|name| name != "objects") {
                found.push(path);
            }
        }
    }
    found
}

/// The captures' temporary directories left in `tmp`, which hold Git LFS's storage.
fn temporary(tmp: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(tmp)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().contains(".rexume-"))
        .collect()
}

#[test]
#[ignore = "needs a Rex server"]
fn a_stopped_snapshot_ends_its_processes_and_deletes_what_they_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let (state, tmp) = (dir.path().join("state"), dir.path().join("tmp"));
    std::fs::create_dir(&tmp).unwrap();
    let (repo, pid, _hold) = held_repository(dir.path());
    let server = Server::start(dir.path(), None);
    let _daemon = Daemon::start(&server, &state);
    let (_, owner, worker) = owner_and_run(&server, &state, &repo, "sleep 600");
    let (mut add, filter) = add_until_filtering(&server, &state, (&owner, &worker), &tmp, &pid);
    kill(Pid::from_raw(add.id().cast_signed()), Signal::SIGTERM).unwrap();
    assert_eq!(add.wait().unwrap().signal(), Some(Signal::SIGTERM as i32));
    wait_until("the filter to end", WAIT, || (!alive(filter)).then_some(()));
    assert_eq!(scratch(&state), Vec::<PathBuf>::new());
    assert_eq!(temporary(&tmp), Vec::<PathBuf>::new());
    let rows = query::<i64>(&state, "SELECT COUNT(*) FROM snapshot_space");
    assert_eq!(rows, Some(0));
}

#[test]
#[ignore = "needs a Rex server"]
fn what_a_killed_snapshot_left_is_deleted_once_its_processes_exit() {
    let dir = tempfile::tempdir().unwrap();
    let (state, tmp) = (dir.path().join("state"), dir.path().join("tmp"));
    std::fs::create_dir(&tmp).unwrap();
    let (repo, pid, hold) = held_repository(dir.path());
    let server = Server::start(dir.path(), None);
    let _daemon = Daemon::start(&server, &state);
    let (_, owner, worker) = owner_and_run(&server, &state, &repo, "sleep 600");
    let (mut add, filter) = add_until_filtering(&server, &state, (&owner, &worker), &tmp, &pid);
    add.kill().unwrap();
    add.wait().unwrap();
    // Another capture, which ends at its deadline in the same filter, leaves what the filter still
    // running holds: the lock file, the objects directory, the temporary directory, and the space
    // reserved. Registering the owner's own pane takes that capture and is refused after it.
    let own = ["add", "--worker", &owner, "--title", "own"];
    assert!(!run_task(&server, &state, &owner, &own).status.success());
    assert_eq!(scratch(&state).len(), 2);
    assert_eq!(temporary(&tmp).len(), 1);
    let reserved = "SELECT COUNT(*) FROM snapshot_space WHERE state = 'reserved'";
    assert_eq!(query::<i64>(&state, reserved), Some(1));
    std::fs::remove_file(&hold).unwrap();
    wait_until("the filter to end", WAIT, || (!alive(filter)).then_some(()));
    let mut add = task_command(
        &server,
        &state,
        &owner,
        &["add", "--worker", &worker, "--title", "t"],
    );
    let added = add.env("TMPDIR", &tmp).output().unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert_eq!(scratch(&state), Vec::<PathBuf>::new());
    assert_eq!(temporary(&tmp), Vec::<PathBuf>::new());
    assert_eq!(
        query::<i64>(&state, "SELECT COUNT(*) FROM snapshot_space"),
        Some(0)
    );
}
