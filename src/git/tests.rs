use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Result, ensure};

use super::*;

/// Runs a setup command in `dir`, as a user would, with nothing of this machine's own that could
/// sign or run a hook.
pub(super) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command::scrub(&mut command);
    let output = command
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "protocol.file.allow=always",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "`git {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
}

/// Takes a snapshot of `worktree` into `store`, a task's snapshot directory, as the task layer does
/// once it has reserved the space.
pub(super) fn capture(worktree: &Path, store: &Path) -> Result<String> {
    let mut capture = Capture::list(worktree, store, BUDGET)?;
    let (tree, _) = capture.stage()?;
    capture.keep()?;
    Ok(tree)
}

pub(super) fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(std::fs::write(path, text)?)
}

/// Writes an executable shell script `name` in `dir`, running `body`, and returns its path.
pub(super) fn script(dir: &Path, name: &str, body: &str) -> Result<PathBuf> {
    let path = dir.join(name);
    write(&path, &format!("#!/bin/sh\n{body}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// Writes `len` random bytes to `path`, which Git cannot compress.
pub(super) fn random(path: &Path, len: u64) -> Result<()> {
    let mut source = std::io::Read::take(std::fs::File::open("/dev/urandom")?, len);
    std::io::copy(&mut source, &mut std::fs::File::create(path)?)?;
    Ok(())
}

/// A repository at `root/repo` with two commits, whose own rules alone decide line endings and
/// what is ignored: the machine's global ones would otherwise decide it in their place.
pub(crate) fn repository(root: &Path) -> Result<PathBuf> {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo)?;
    git(&repo, &["init", "-q"])?;
    for (key, value) in [
        ("core.autocrlf", "false"),
        ("core.excludesFile", "/dev/null"),
        ("core.attributesFile", "/dev/null"),
    ] {
        git(&repo, &["config", key, value])?;
    }
    write(&repo.join(".gitignore"), "*.log\n")?;
    write(&repo.join("a.txt"), "one\n")?;
    write(&repo.join("dir/b.txt"), "b\n")?;
    git(&repo, &["add", "-A"])?;
    git(&repo, &["commit", "-qm", "first"])?;
    write(&repo.join("a.txt"), "two\n")?;
    git(&repo, &["commit", "-qam", "second"])?;
    Ok(repo)
}

/// Every entry under `root`: its kind, mode, size, modification time, and a file's content.
pub(super) type Listing = BTreeMap<PathBuf, (u32, u64, SystemTime, Option<Vec<u8>>)>;

pub(super) fn listing(root: &Path) -> Result<Listing> {
    let mut found = Listing::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                pending.push(path.clone());
            }
            let content = meta.is_file().then(|| std::fs::read(&path)).transpose()?;
            let at = (
                meta.permissions().mode(),
                meta.len(),
                meta.modified()?,
                content,
            );
            found.insert(path, at);
        }
    }
    Ok(found)
}

/// Sets every entry under `root` to have been modified long ago, so that a refresh shows.
pub(super) fn age(root: &Path) -> Result<()> {
    let long_ago = SystemTime::now() - Duration::from_secs(400 * 24 * 60 * 60);
    for path in listing(root)?.keys().rev() {
        if !std::fs::symlink_metadata(path)?.is_symlink() {
            std::fs::File::open(path)?.set_modified(long_ago)?;
        }
    }
    Ok(())
}

/// The objects `store`, a task's snapshot directory, keeps. Git refreshes the modification time
/// of one it would write again, so only their paths, which name their content, are compared.
pub(super) fn objects(store: &Path) -> Result<Vec<PathBuf>> {
    let dir = store.join("objects");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    Ok(listing(&dir)?.into_keys().collect())
}

/// Everything in `store` but its objects, which is what a capture left behind.
pub(super) fn left(store: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut left = Vec::new();
    if store.exists() {
        for entry in std::fs::read_dir(store)? {
            let name = entry?.file_name();
            if name != "objects" {
                left.push(name);
            }
        }
    }
    Ok(left)
}

/// Snapshots `dir`'s working tree into `store` and checks what every snapshot holds to: the
/// repository under `root` as it was down to modification times, nothing left in `store` but
/// objects, and either the tree `git add -A` then records, or no tree and no objects kept.
/// Returns the tree or the note.
pub(super) fn holds(root: &Path, dir: &Path, store: &Path) -> Result<Result<String, String>> {
    let (top, taken) = untouched(root, dir, store)?;
    staged(&top, taken)
}

/// What `holds` checks before it runs `git add -A`, which may run what the snapshot must not,
/// with the working tree's top level.
pub(super) fn untouched(
    root: &Path,
    dir: &Path,
    store: &Path,
) -> Result<(PathBuf, Result<String, String>)> {
    let top = worktree(dir)?;
    age(root)?;
    let before = listing(root)?;
    let kept = objects(store)?;
    let taken = capture(&top, store);
    let after = listing(root)?;
    let changed: Vec<_> = before
        .iter()
        .filter(|(path, at)| after.get(*path) != Some(at))
        .map(|(path, _)| path)
        .chain(after.keys().filter(|path| !before.contains_key(*path)))
        .collect();
    assert!(changed.is_empty(), "the repository changed: {changed:?}");
    assert_eq!(left(store)?, Vec::<std::ffi::OsString>::new());
    let taken = taken.map_err(|e| format!("{e:#}"));
    if let Err(note) = &taken {
        assert!(objects(store)? == kept, "objects kept after {note}");
    }
    Ok((top, taken))
}

/// What `holds` checks after: a tree is the one `git add -A` records in `top`.
pub(super) fn staged(top: &Path, taken: Result<String, String>) -> Result<Result<String, String>> {
    if let Ok(tree) = &taken {
        git(top, &["add", "-A"])?;
        assert_eq!(
            tree,
            &git(top, &["write-tree"])?,
            "not the tree `git add -A` records"
        );
    }
    Ok(taken)
}

/// A repository made by `repository` whose `*.bin` files the filter `lfs` cleans with `process`,
/// configured as Git LFS leaves a repository it has run in.
pub(crate) fn lfs_repository(root: &Path, process: &str) -> Result<PathBuf> {
    let repo = repository(root)?;
    write(&repo.join(".gitattributes"), "*.bin filter=lfs -text\n")?;
    git(&repo, &["add", ".gitattributes"])?;
    git(&repo, &["commit", "-qm", "attributes"])?;
    git(&repo, &["config", "filter.lfs.process", process])?;
    git(&repo, &["config", "filter.lfs.required", "true"])?;
    git(&repo, &["config", "lfs.repositoryformatversion", "0"])?;
    Ok(repo)
}

/// A program named `git-lfs` in `tools` that Git takes for Git LFS's filter: it answers the
/// filter protocol's handshake, which it does not read, then runs `body`. Returns the command.
pub(crate) fn fake_lfs(tools: &Path, body: &str) -> Result<String> {
    // Git's long-running filter protocol in pkt-lines, each led by its length in hex.
    let handshake =
        r"printf '0016git-filter-server\n000eversion=2\n00000015capability=clean\n0000'";
    let lfs = script(tools, "git-lfs", &format!("{handshake}\n{body}"))?;
    Ok(format!("{} filter-process", lfs.display()))
}

/// A repository with a split index gets no snapshot, as reading it refreshes the shared file.
#[test]
fn a_split_index_gets_no_snapshot() -> Result<()> {
    let root = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let repo = repository(root.path())?;
    git(&repo, &["update-index", "--split-index"])?;
    let (_, taken) = untouched(root.path(), &repo, state.path())?;
    assert_eq!(taken.unwrap_err(), "the repository has a split index");
    Ok(())
}

/// Changed and untracked files larger than `CONTENT_LIMIT` together are not copied.
#[test]
fn a_snapshot_copies_no_more_than_the_limit() -> Result<()> {
    let root = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let repo = repository(root.path())?;
    std::fs::File::create(repo.join("big.bin"))?.set_len(CONTENT_LIMIT + 1)?;
    let said = capture(&worktree(&repo)?, state.path())
        .unwrap_err()
        .to_string();
    assert!(
        said.starts_with("the changed and untracked files total more than"),
        "{said}"
    );
    assert_eq!(listing(&state.path().join("objects"))?, Listing::new());
    Ok(())
}

/// A directory outside any repository has no working tree, and one below a working tree's top
/// level gets no snapshot, which would be of the repository above it.
#[test]
fn only_the_top_level_of_a_working_tree_is_snapshotted() -> Result<()> {
    let root = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let said = worktree(root.path()).unwrap_err().to_string();
    assert!(said.contains("not a git repository"), "{said}");
    let repo = repository(root.path())?;
    let said = capture(&repo.join("dir"), state.path())
        .unwrap_err()
        .to_string();
    assert!(
        said.ends_with("is no longer the top level of a working tree"),
        "{said}"
    );
    Ok(())
}

mod limits;
mod scenarios;
mod seed;
mod staging;
mod submodule;
