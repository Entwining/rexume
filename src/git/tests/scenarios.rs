use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::{git, holds, repository, script, write};

/// Each way a repository can stand that makes Git write in it while it reads or updates an index,
/// set up on top of `repository`. Returns the working tree to snapshot.
fn arrange(scenario: &str, root: &Path, repo: &Path) -> Result<PathBuf> {
    match scenario {
        "every kind of change" => {
            write(&repo.join("a.txt"), "changed\n")?;
            std::fs::remove_file(repo.join("dir/b.txt"))?;
            write(&repo.join("new.txt"), "untracked\n")?;
            write(&repo.join("skip.log"), "ignored\n")?;
            write(&repo.join("kept.log"), "forced\n")?;
            git(repo, &["add", "-f", "kept.log"])?;
            write(&repo.join("kept.log"), "forced, then changed\n")?;
            std::os::unix::fs::symlink("a.txt", repo.join("link"))?;
            std::fs::set_permissions(repo.join("a.txt"), std::fs::Permissions::from_mode(0o755))?;
        }
        // Git refreshes an object it is asked to write again; this content is packed already.
        "content the pack holds" => {
            git(repo, &["gc", "-q"])?;
            write(&repo.join("a.txt"), "one\n")?;
        }
        "an index hook" => {
            script(
                &repo.join("hooks"),
                "post-index-change",
                "touch .git/hook-ran",
            )?;
            git(repo, &["config", "core.hooksPath", "hooks"])?;
            write(&repo.join("a.txt"), "changed\n")?;
        }
        "a file system monitor hook" => {
            script(repo, "monitor", "touch .git/monitor-ran")?;
            git(repo, &["config", "core.fsmonitor", "./monitor"])?;
        }
        "split index configured" => {
            git(repo, &["config", "core.splitIndex", "true"])?;
            write(&repo.join("a.txt"), "changed\n")?;
        }
        "many files" => {
            git(repo, &["config", "feature.manyFiles", "true"])?;
            git(
                repo,
                &["update-index", "--index-version", "4", "--untracked-cache"],
            )?;
            write(&repo.join("new.txt"), "untracked\n")?;
        }
        // Git LFS stores what it cleans under the Git directory, unless told otherwise.
        "a file Git LFS stores" => {
            git(
                repo,
                &["config", "filter.lfs.process", "git-lfs filter-process"],
            )?;
            git(repo, &["config", "filter.lfs.required", "true"])?;
            write(&repo.join(".gitattributes"), "*.bin filter=lfs -text\n")?;
            write(&repo.join("big.bin"), "large\n")?;
            git(repo, &["add", "-A"]).context("this scenario runs Git LFS")?;
            git(repo, &["commit", "-qm", "lfs"])?;
            write(&repo.join("big.bin"), "large, changed\n")?;
            write(&repo.join("new.bin"), "large, new\n")?;
        }
        "a linked worktree" => {
            let linked = root.join("linked");
            git(repo, &["worktree", "add", "-q", &linked.to_string_lossy()])?;
            write(&linked.join("a.txt"), "changed in the linked tree\n")?;
            return Ok(linked);
        }
        _ => unreachable!("{scenario}"),
    }
    Ok(repo.to_path_buf())
}

/// A snapshot changes nothing in the repository, its Git directory included, down to modification
/// times, and its tree is the one `git add -A` then `git write-tree` record. All it leaves in its
/// directory is objects, and no file Git LFS cleaned.
#[test]
fn a_snapshot_is_what_git_add_stages_and_leaves_the_repository_as_it_was() -> Result<()> {
    for scenario in [
        "every kind of change",
        "content the pack holds",
        "an index hook",
        "a file system monitor hook",
        "split index configured",
        "many files",
        "a file Git LFS stores",
        "a linked worktree",
    ] {
        let root = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let repo = repository(root.path())?;
        let dir = arrange(scenario, root.path(), &repo)?;
        holds(root.path(), &dir, state.path())?.map_err(|note| anyhow!("{scenario}: {note}"))?;
    }
    Ok(())
}
