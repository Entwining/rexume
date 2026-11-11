use std::io::{Seek, SeekFrom, Write as _};

use anyhow::Result;

use crate::git::filter::lfs_command;
use crate::git::tests::{capture, fake_lfs, git, holds, lfs_repository, script, write};
use crate::git::{CONTENT_LIMIT, worktree};

#[test]
fn only_the_commands_git_lfs_installs_are_trusted() {
    for (kind, command) in [
        ("process", "git-lfs filter-process"),
        ("process", "git-lfs filter-process --skip"),
        ("process", "/opt/tools/git-lfs filter-process"),
        ("clean", "git-lfs clean -- %f"),
    ] {
        assert!(lfs_command(kind, command), "{command}");
    }
    for (kind, command) in [
        ("process", "git-lfs filter-process; touch .git/m"),
        ("process", "git-lfs  filter-process"),
        ("process", "git-lfs$(touch .git/m) filter-process"),
        ("process", "env git-lfs filter-process"),
        ("process", "tools/git-lfs filter-process"),
        ("process", "/opt/my tools/git-lfs filter-process"),
        ("process", "/opt/$HOME/git-lfs filter-process"),
        ("process", "git-lfs clean -- %f"),
        ("clean", "git-lfs clean -- %f; printf x > .git/m"),
        ("smudge", "git-lfs smudge -- %f"),
    ] {
        assert!(!lfs_command(kind, command), "{command}");
    }
}

#[test]
fn git_lfs_runs_only_where_it_has_run_before() -> Result<()> {
    let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let repo = lfs_repository(root.path(), "git-lfs filter-process")?;
    git(&repo, &["config", "--unset", "lfs.repositoryformatversion"])?;
    git(&repo, &["config", "extensions.worktreeConfig", "true"])?;
    git(
        &repo,
        &["config", "--worktree", "lfs.repositoryformatversion", "0"],
    )?;
    write(&repo.join("new.bin"), "large\n")?;
    let key = "lfs.repositoryformatversion";
    for values in [&[][..], &[""], &["0", ""]] {
        for (at, value) in values.iter().enumerate() {
            let how = if at == 0 { "--replace-all" } else { "--add" };
            git(&repo, &["config", how, key, value])?;
        }
        let note = holds(root.path(), &repo, state.path())?.expect_err("no tree");
        assert!(
            note.contains("writes `lfs.repositoryformatversion` to the repository's configuration"),
            "{values:?}: {note}"
        );
    }
    git(&repo, &["config", "--replace-all", key, "0"])?;
    let linked = root.path().join("linked");
    git(&repo, &["worktree", "add", "-q", &linked.to_string_lossy()])?;
    write(&linked.join("new.bin"), "large, linked\n")?;
    holds(root.path(), &linked, state.path())?.expect("a tree");
    Ok(())
}

#[test]
fn git_lfs_in_another_form_runs_as_any_other_filter() -> Result<()> {
    let tools = tempfile::tempdir()?;
    let extension = script(tools.path(), "ext", "touch .git/extension-ran; cat")?;
    for (key, value) in [
        (
            "filter.lfs.process",
            "git-lfs filter-process; touch .git/lfs-tail".to_owned(),
        ),
        (
            "filter.lfs.clean",
            "git-lfs clean -- %f; touch .git/lfs-tail".to_owned(),
        ),
        (
            "lfs.extension.x.clean",
            format!("{} %f", extension.display()),
        ),
    ] {
        let (root, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let repo = lfs_repository(root.path(), "git-lfs filter-process")?;
        write(&repo.join("first.bin"), "first\n")?;
        git(&repo, &["add", "-A"])?;
        git(&repo, &["commit", "-qm", "first"])?;
        git(&repo, &["config", key, &value])?;
        if key.starts_with("lfs.extension.") {
            git(&repo, &["config", "lfs.extension.x.smudge", "cat"])?;
            git(&repo, &["config", "lfs.extension.x.priority", "0"])?;
        }
        write(&repo.join("new.bin"), "large\n")?;
        let _ = holds(root.path(), &repo, state.path())?;
    }
    Ok(())
}

#[test]
fn listing_the_changed_files_runs_no_filter() -> Result<()> {
    let (root, tools, state) = (
        tempfile::tempdir()?,
        tempfile::tempdir()?,
        tempfile::tempdir()?,
    );
    let calls = tools.path().join("calls");
    let wrapper = script(
        tools.path(),
        "git-lfs",
        &format!("echo \"$@\" >> '{}'\nexec git-lfs \"$@\"", calls.display()),
    )?;
    let repo = lfs_repository(
        root.path(),
        &format!("{} filter-process", wrapper.display()),
    )?;
    std::fs::File::create(repo.join("big.bin"))?.set_len(CONTENT_LIMIT + (1 << 20))?;
    git(&repo, &["add", "-A"])?;
    git(&repo, &["commit", "-qm", "big"])?;
    let mut big = std::fs::OpenOptions::new()
        .write(true)
        .open(repo.join("big.bin"))?;
    // The same size, so that the listing would clean it to compare its content were filters on.
    big.seek(SeekFrom::Start(0))?;
    big.write_all(b"x")?;
    std::fs::remove_file(&calls)?;
    let note = holds(root.path(), &repo, state.path())?.expect_err("no tree");
    assert!(
        note.starts_with("the changed and untracked files total more than"),
        "{note}"
    );
    assert!(
        !calls.exists(),
        "Git LFS ran: {}",
        std::fs::read_to_string(&calls)?
    );
    Ok(())
}

#[test]
fn a_note_keeps_nothing_a_filter_printed() -> Result<()> {
    let (root, tools, state) = (
        tempfile::tempdir()?,
        tempfile::tempdir()?,
        tempfile::tempdir()?,
    );
    let body = "cat big.bin >&2\nprintf '0011status=error\\n0000'\ncat >/dev/null";
    let repo = lfs_repository(root.path(), &fake_lfs(tools.path(), body)?)?;
    write(&repo.join("big.bin"), "unfiltered secret\n")?;
    let note = format!(
        "{:#}",
        capture(&worktree(&repo)?, state.path()).unwrap_err()
    );
    assert!(note.contains("its message is not kept"), "{note}");
    assert!(!note.contains("unfiltered secret"), "{note}");
    Ok(())
}
