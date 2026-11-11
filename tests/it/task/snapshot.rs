use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use super::run::owner_and_run;
use super::{WAIT, failure, run_task, task, task_command};
use crate::common::{Daemon, Server, query, rex_bin, wait_until};

mod killed;

/// Runs `git` in `dir` with nothing of this machine's own that could sign or run a hook.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

/// Every entry under `root` with its size, modification time, and a file's content.
fn listing(root: &Path) -> BTreeMap<PathBuf, (u64, SystemTime, Option<Vec<u8>>)> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                pending.push(path.clone());
            }
            let content = meta.is_file().then(|| std::fs::read(&path).unwrap());
            found.insert(path, (meta.len(), meta.modified().unwrap(), content));
        }
    }
    found
}

/// A worker in a repository: `task add` snapshots the clean tree before the worker writes, the
/// owner's `task wait` that takes the result snapshots the tree the worker left, which is what
/// `git add -A` stages, and neither writes in the repository's Git directory.
#[test]
#[ignore = "needs a Rex server"]
fn a_round_is_snapshotted_at_registration_and_at_the_owners_next_command() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "first"]);
    let (go, result) = (dir.path().join("go"), dir.path().join("result.md"));
    let script = format!(
        "while [ ! -e '{}' ]; do sleep 0.1; done; echo changed > a.txt; echo new > b.txt; \
         printf Done. > '{}'",
        go.display(),
        result.display()
    );
    let server = Server::start(dir.path(), None);
    let _daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let (_, owner, worker) = owner_and_run(&server, &state, &repo, &script);
    let git_dir = listing(&repo.join(".git"));
    let added = task(
        &server,
        &state,
        &owner,
        &[
            "add",
            "--worker",
            &worker,
            "--title",
            "change",
            "--result-file",
            &result.to_string_lossy(),
        ],
    );
    let snapshot = &added["snapshot"];
    assert_eq!(
        snapshot["worktree"].as_str(),
        Some(git(&repo, &["rev-parse", "--show-toplevel"]).as_str())
    );
    assert_eq!(
        snapshot["before"]["tree"].as_str(),
        Some(git(&repo, &["rev-parse", "HEAD^{tree}"]).as_str())
    );
    std::fs::write(&go, "").unwrap();
    let id = added["id"].as_str().unwrap();
    let waited = task(&server, &state, &owner, &["wait", id, "--timeout", "20"]);
    assert_eq!(waited["state"].as_str(), Some("completed"));
    let after = waited["snapshot"]["after"]["tree"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        listing(&repo.join(".git")) == git_dir,
        "the Git directory changed"
    );
    // Read as `docs/cli.md` says, with the snapshots' objects as an alternate.
    let diff = Command::new("git")
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            waited["snapshot"]["object_directory"].as_str().unwrap(),
        )
        .arg("-C")
        .arg(&repo)
        .args(["diff-tree", "-r", "-p"])
        .args([snapshot["before"]["tree"].as_str().unwrap(), &after])
        .output()
        .unwrap();
    assert!(diff.status.success());
    let diff = String::from_utf8(diff.stdout).unwrap();
    assert!(
        diff.contains("\n+changed\n") && diff.contains("\n+new\n"),
        "{diff}"
    );
    git(&repo, &["add", "-A"]);
    assert_eq!(after, git(&repo, &["write-tree"]));
}

/// A registration refused after its snapshot deletes what the snapshot kept, which would
/// otherwise count against the snapshots' space until the sweep found no task for it.
#[test]
#[ignore = "needs a Rex server"]
fn a_refused_registration_deletes_its_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("new.txt"), "untracked\n").unwrap();
    let server = Server::start(dir.path(), None);
    let _daemon = Daemon::start(&server, &state);
    let (_, owner, _) = owner_and_run(&server, &state, &repo, "sleep 600");
    let args = ["add", "--worker", &owner, "--title", "own"];
    let (code, said) = failure(&run_task(&server, &state, &owner, &args));
    assert_eq!(code, Some(1));
    assert!(said.contains("is the orchestrator's own pane"), "{said}");
    let left: Vec<_> = std::fs::read_dir(state.join("snapshots"))
        .unwrap()
        .collect();
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(
        query::<i64>(&state, "SELECT COUNT(*) FROM snapshot_space"),
        Some(0)
    );
}

/// A relative state directory and `TMPDIR` name the directories under the one the command runs
/// in, though Git runs in the working tree: the snapshot is taken, writes nothing in the
/// repository, and names its object directory by an absolute path.
#[test]
#[ignore = "needs a Rex server"]
fn relative_directories_are_taken_from_where_the_command_runs() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("new.txt"), "untracked\n").unwrap();
    std::fs::create_dir(dir.path().join("tmp")).unwrap();
    let server = Server::start(dir.path(), None);
    let _daemon = Daemon::start(&server, &state);
    let (_, owner, worker) = owner_and_run(&server, &state, &repo, "sleep 600");
    let unchanged = listing(&repo);
    let result = dir.path().join("result.md").to_string_lossy().into_owned();
    let args = [
        "add",
        "--worker",
        &worker,
        "--title",
        "relative",
        "--result-file",
        &result,
    ];
    let output = task_command(&server, Path::new("state"), &owner, &args)
        .current_dir(dir.path())
        .env("TMPDIR", "tmp")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", failure(&output));
    let added: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let snapshot = &added["snapshot"];
    assert!(snapshot["before"]["tree"].is_string(), "{snapshot}");
    let objects = snapshot["object_directory"].as_str().unwrap();
    assert!(Path::new(objects).is_absolute(), "{objects}");
    assert!(listing(&repo) == unchanged, "the repository changed");
    let temporary: Vec<_> = std::fs::read_dir(dir.path().join("tmp")).unwrap().collect();
    assert!(temporary.is_empty(), "{temporary:?}");
}

/// Git LFS runs as the environment configures it, while nothing else of Git's in the environment
/// reaches it: a filter configured only there cleans the file, and the progress file Git LFS
/// would write is not written.
#[test]
#[ignore = "needs a Rex server and Git LFS"]
fn git_lfs_runs_with_the_configuration_the_environment_gives() {
    let lfs = which::which("git-lfs").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (state, repo) = (dir.path().join("state"), dir.path().join("repo"));
    let progress = dir.path().join("progress");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join(".gitattributes"), "*.bin filter=lfs -text\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "attributes"]);
    git(&repo, &["config", "lfs.repositoryformatversion", "0"]);
    std::fs::write(repo.join("big.bin"), "large\n").unwrap();
    let server = Server::start(dir.path(), None);
    let _daemon = Daemon::start(&server, &state);
    let (_, owner, worker) = owner_and_run(&server, &state, &repo, "sleep 600");
    let args = ["add", "--worker", &worker, "--title", "t"];
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        lfs.parent().unwrap().display(),
        rex_bin().parent().unwrap().display()
    );
    let config = [
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "filter.lfs.process"),
        ("GIT_CONFIG_VALUE_0", "git-lfs filter-process"),
        ("GIT_CONFIG_KEY_1", "filter.lfs.required"),
        ("GIT_CONFIG_VALUE_1", "true"),
    ];
    let added = task_command(&server, &state, &owner, &args)
        .envs(config)
        .env("GIT_LFS_PROGRESS", &progress)
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let added: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    let tree = added["snapshot"]["before"]["tree"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!progress.exists(), "Git LFS wrote its progress");
    let staged = |args: &[&str]| {
        let output = Command::new("git")
            .envs(config)
            .env("PATH", &path)
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    };
    staged(&["add", "-A"]);
    assert_eq!(tree, staged(&["write-tree"]));
}
