use anyhow::Result;

use super::{git, holds, random, repository, write};
use crate::git::CONTENT_LIMIT;

#[test]
fn a_file_staged_with_crlf_keeps_it() -> Result<()> {
    for autocrlf in ["input", "true"] {
        let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let repo = repository(root.path())?;
        write(&repo.join("crlf.txt"), "one\r\ntwo\r\n")?;
        git(&repo, &["add", "-A"])?;
        git(&repo, &["commit", "-qm", "crlf"])?;
        git(&repo, &["config", "core.autocrlf", autocrlf])?;
        write(&repo.join("crlf.txt"), "one\r\nthree\r\n")?;
        let taken = holds(root.path(), &repo, state.path())?;
        assert!(taken.is_ok(), "{autocrlf}: {taken:?}");
    }
    Ok(())
}

#[test]
fn rules_left_out_of_a_sparse_checkout_apply() -> Result<()> {
    let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let repo = repository(root.path())?;
    let sub = repository(&root.path().join("sub"))?;
    git(
        &repo,
        &["submodule", "add", "-q", &sub.to_string_lossy(), "sm"],
    )?;
    git(
        &repo,
        &["config", "-f", ".gitmodules", "submodule.sm.ignore", "all"],
    )?;
    write(&repo.join(".gitattributes"), "*.dat text\n")?;
    write(&repo.join("x.dat"), "one\n")?;
    git(&repo, &["add", "-A"])?;
    git(&repo, &["commit", "-qm", "rules"])?;
    let set = ["sparse-checkout", "set", "--no-cone", "/*"];
    git(
        &repo,
        &[
            &set[..],
            &["!/.gitattributes", "!/.gitignore", "!/.gitmodules"],
        ]
        .concat(),
    )?;
    for rules in [".gitattributes", ".gitignore", ".gitmodules"] {
        assert!(!repo.join(rules).exists(), "{rules}");
    }
    write(&repo.join("x.dat"), "one\r\ntwo\r\n")?;
    write(&repo.join("skip.log"), "ignored\n")?;
    write(&repo.join("sm/a.txt"), "later\n")?;
    git(&repo.join("sm"), &["commit", "-qam", "later"])?;
    let tree = holds(root.path(), &repo, state.path())?.expect("a tree");
    let listed = git(&repo, &["ls-tree", "--name-only", &tree])?;
    assert!(!listed.contains("skip.log"), "{listed}");
    Ok(())
}

#[test]
fn rules_of_a_gitattributes_missing_from_the_working_tree_apply() -> Result<()> {
    for marked in [false, true] {
        let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let repo = repository(root.path())?;
        write(&repo.join("sub/.gitattributes"), "*.dat text\n")?;
        write(&repo.join("sub/x.dat"), "one\n")?;
        git(&repo, &["add", "-A"])?;
        git(&repo, &["commit", "-qm", "rules"])?;
        if marked {
            git(
                &repo,
                &["update-index", "--assume-unchanged", "sub/.gitattributes"],
            )?;
        }
        std::fs::remove_file(repo.join("sub/.gitattributes"))?;
        write(&repo.join("sub/x.dat"), "one\r\ntwo\r\n")?;
        let taken = holds(root.path(), &repo, state.path())?;
        assert!(taken.is_ok(), "marked {marked}: {taken:?}");
    }
    Ok(())
}

#[test]
fn a_sparse_index_gets_the_tree_git_stages() -> Result<()> {
    for scenario in [
        "an ignored file left out",
        "a left-out file changed",
        "settings that no longer allow it",
    ] {
        let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let repo = repository(root.path())?;
        write(&repo.join("out/c.txt"), "c\n")?;
        write(&repo.join("out/deep/d.txt"), "d\n")?;
        write(&repo.join("other/e.txt"), "e\n")?;
        git(&repo, &["add", "-A"])?;
        git(&repo, &["commit", "-qm", "out"])?;
        git(
            &repo,
            &["sparse-checkout", "set", "--cone", "--sparse-index", "dir"],
        )?;
        // Packed, so that Git refreshing its pack would show.
        git(&repo, &["gc", "-q"])?;
        assert!(!repo.join("out").exists());
        match scenario {
            "an ignored file left out" => write(&repo.join("out/x.log"), "ignored\n")?,
            "a left-out file changed" => write(&repo.join("out/c.txt"), "changed\n")?,
            _ => drop(git(
                &repo,
                &["config", "--worktree", "core.sparseCheckoutCone", "false"],
            )?),
        }
        let taken = holds(root.path(), &repo, state.path())?;
        match scenario {
            "settings that no longer allow it" => {
                let note = taken.expect_err("no tree");
                assert!(note.contains("error: Could not read"), "{note}");
            }
            _ => assert!(taken.is_ok(), "{scenario}: {taken:?}"),
        }
    }
    Ok(())
}

#[test]
fn staged_versions_over_the_limit_are_not_copied() -> Result<()> {
    let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let repo = repository(root.path())?;
    random(&repo.join("big.bin"), CONTENT_LIMIT + (1 << 20))?;
    git(&repo, &["add", "-A"])?;
    git(&repo, &["commit", "-qm", "big"])?;
    write(&repo.join("big.bin"), "small\n")?;
    let note = holds(root.path(), &repo, state.path())?.expect_err("no tree");
    assert!(note.contains("may total more than 64 MiB"), "{note}");
    Ok(())
}
