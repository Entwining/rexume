//! A populated submodule is recorded by the commit its gitlink names, as `git add -A` records a
//! submodule whose working tree it does not read, and nothing runs in the submodule.

use anyhow::Result;

use super::{git, repository, script, staged, untouched, write};

/// Every way a submodule can stand gets the tree real Git stages, and a submodule whose own
/// configuration runs a filter on a changed file neither runs it nor has its Git directory
/// changed, which `untouched` sees as part of the repository; `git add -A` runs both after.
#[test]
fn a_submodule_is_recorded_without_running_anything_in_it() -> Result<()> {
    for scenario in [
        "changed",
        "a later commit checked out",
        "a later commit checked out behind --assume-unchanged",
        "a later commit checked out behind --skip-worktree",
        "a later commit checked out, ignored by a deleted .gitmodules",
        "a later commit checked out, ignored by a .gitmodules only HEAD has",
        "deleted",
        "emptied",
        "a split index",
        "a filter",
    ] {
        let (root, tools, state) = (
            tempfile::tempdir()?,
            tempfile::tempdir()?,
            tempfile::tempdir()?,
        );
        let repo = repository(root.path())?;
        let sub = repository(&root.path().join("sub"))?;
        git(
            &repo,
            &["submodule", "add", "-q", &sub.to_string_lossy(), "sm"],
        )?;
        git(&repo, &["commit", "-qm", "submodule"])?;
        let sm = repo.join("sm");
        let marker = tools.path().join("ran");
        match scenario {
            "changed" => write(&sm.join("a.txt"), "changed\n")?,
            "a later commit checked out" => {
                write(&sm.join("a.txt"), "changed\n")?;
                git(&sm, &["commit", "-qam", "later"])?;
            }
            flagged if flagged.contains(" behind ") => {
                let flag = flagged.rsplit(' ').next().unwrap_or_default();
                git(&repo, &["update-index", flag, "sm"])?;
                write(&sm.join("a.txt"), "changed\n")?;
                git(&sm, &["commit", "-qam", "later"])?;
            }
            ignored if ignored.contains(", ignored by ") => {
                let ignore = ["config", "-f", ".gitmodules", "submodule.sm.ignore", "all"];
                git(&repo, &ignore)?;
                git(&repo, &["commit", "-qam", "ignore"])?;
                if ignored.ends_with("only HEAD has") {
                    git(&repo, &["rm", "-q", "--cached", ".gitmodules"])?;
                }
                std::fs::remove_file(repo.join(".gitmodules"))?;
                write(&sm.join("a.txt"), "changed\n")?;
                git(&sm, &["commit", "-qam", "later"])?;
            }
            "deleted" => std::fs::remove_dir_all(&sm)?,
            "emptied" => {
                std::fs::remove_dir_all(&sm)?;
                std::fs::create_dir(&sm)?;
            }
            "a split index" => {
                git(&sm, &["update-index", "--split-index"])?;
                write(&sm.join("a.txt"), "changed\n")?;
            }
            _ => {
                let up = script(
                    tools.path(),
                    "up",
                    &format!("touch '{}'; cat", marker.display()),
                )?;
                git(&sm, &["config", "filter.up.clean", &up.to_string_lossy()])?;
                write(&sm.join(".gitattributes"), "*.txt filter=up\n")?;
                write(&sm.join("a.txt"), "changed\n")?;
            }
        }
        write(&repo.join("a.txt"), "changed\n")?;
        let (top, taken) = untouched(root.path(), &repo, state.path())?;
        assert!(!marker.exists(), "{scenario}: the submodule's filter ran");
        let taken = staged(&top, taken)?;
        assert!(taken.is_ok(), "{scenario}: {taken:?}");
    }
    Ok(())
}
