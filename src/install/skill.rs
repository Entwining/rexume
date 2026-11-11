//! The stub of Rexume's skill (`skill::STUB`), which install writes where Claude Code reads skills,
//! `~/.claude/skills`, and where Codex and Pi read them, `~/.agents/skills`. The stub tells the
//! agent to run `rexume skill show`, so the guide it follows always comes from the installed
//! binary, and a file install wrote is recognized by its `WRITTEN_BY` line in every version.

use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};

use super::file::physical;
use super::{Outcome, refresh_file, remove_file, write};
use crate::skill::STUB;

const WRITTEN_BY: &str = "<!-- Written by `rexume install`";

/// One skills directory under home, and the runtimes whose directories decide whether install
/// writes there.
struct Target {
    key: &'static str,
    skills: &'static str,
    runtimes: &'static [&'static str],
}

const TARGETS: [Target; 2] = [
    Target {
        key: "agents_skill",
        skills: ".agents/skills",
        runtimes: &[".codex", ".pi/agent"],
    },
    Target {
        key: "claude_skill",
        skills: ".claude/skills",
        runtimes: &[".claude"],
    },
];

fn is_ours(text: &str) -> bool {
    text.lines().any(|line| line.starts_with(WRITTEN_BY))
}

/// Runs `apply` once per stub file with the targets that name it: when `~/.claude/skills` links to
/// `~/.agents/skills`, both keys report what was done to their one file.
fn each<T: Clone>(
    home: &Path,
    mut apply: impl FnMut(&[&Target], &Path) -> Result<T>,
) -> Result<[(&'static str, T); 2]> {
    let [agents, claude] = &TARGETS;
    let agents_path = home.join(agents.skills).join("rexume/SKILL.md");
    // Resolved only to compare: `apply` gets the path the runtime reads, so a write follows its
    // links and a removal takes the entry there, never what a link points to.
    if physical(&home.join(claude.skills))? == physical(&home.join(agents.skills))? {
        let result = apply(&[agents, claude], &agents_path)?;
        return Ok([(agents.key, result.clone()), (claude.key, result)]);
    }
    let claude_path = home.join(claude.skills).join("rexume/SKILL.md");
    Ok([
        (agents.key, apply(&[agents], &agents_path)?),
        (claude.key, apply(&[claude], &claude_path)?),
    ])
}

/// Writes the stub into each skills directory one of whose runtimes is set up, leaving a
/// `rexume` skill install did not write as it is.
pub(super) fn install(home: &Path) -> Result<[(&'static str, Outcome); 2]> {
    each(home, |targets, path| {
        let runtimes = targets.iter().flat_map(|t| t.runtimes);
        if !runtimes.clone().any(|r| home.join(r).is_dir()) {
            let dirs: Vec<String> = runtimes.map(|r| format!("`~/{r}`")).collect();
            return Ok(("skipped", Some(format!("no {}", dirs.join(" or ")))));
        }
        match std::fs::read_to_string(path) {
            Ok(text) if !is_ours(&text) => {
                return Ok(("kept", Some("not written by install".to_owned())));
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read `{}`", path.display()));
            }
        }
        Ok((write(path, STUB.as_bytes())?, None))
    })
}

/// Removes each stub install wrote, with the `rexume` directory it leaves empty.
pub(super) fn uninstall(home: &Path) -> Result<[(&'static str, Outcome); 2]> {
    each(home, |_, path| {
        let outcome = remove_file(path, is_ours)?;
        if outcome.0 == "removed"
            && let Some(dir) = path.parent()
        {
            match std::fs::remove_dir(dir) {
                Ok(()) => {}
                // A `rexume` that links to a directory is the user's arrangement and stays.
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::DirectoryNotEmpty | ErrorKind::NotADirectory
                    ) => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("failed to remove `{}`", dir.display()));
                }
            }
        }
        Ok(outcome)
    })
}

/// Rewrites each stub install wrote when it differs from this version's, returning each key with
/// what `refresh_file` returned.
pub fn refresh_skills(home: &Path) -> Result<[(&'static str, &'static str); 2]> {
    each(home, |_, path| refresh_file(path, STUB.as_bytes(), is_ours))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn stub(home: &Path, skills: &str) -> PathBuf {
        home.join(skills).join("rexume/SKILL.md")
    }

    /// Where `~/.claude/skills` links to `~/.agents/skills`, even before that exists, the one file
    /// is written for Claude Code alone, both keys report what was done to it, and uninstall leaves
    /// no `rexume` directory behind.
    #[test]
    fn linked_skills_directories_report_one_file_twice() -> Result<()> {
        let home = tempfile::tempdir()?;
        let home = home.path();
        std::fs::create_dir_all(home.join(".claude"))?;
        std::os::unix::fs::symlink("../.agents/skills", home.join(".claude/skills"))?;
        let values =
            |outcomes: [(&'static str, Outcome); 2]| outcomes.map(|(key, (v, _))| (key, v));
        assert_eq!(
            values(install(home)?),
            [("agents_skill", "created"), ("claude_skill", "created")]
        );
        assert_eq!(std::fs::read_to_string(stub(home, ".claude/skills"))?, STUB);
        assert_eq!(
            values(install(home)?),
            [("agents_skill", "unchanged"), ("claude_skill", "unchanged")]
        );
        assert_eq!(
            values(uninstall(home)?),
            [("agents_skill", "removed"), ("claude_skill", "removed")]
        );
        assert!(!home.join(".agents/skills/rexume").exists());
        assert_eq!(
            refresh_skills(home)?,
            [("agents_skill", "absent"), ("claude_skill", "absent")]
        );
        Ok(())
    }

    /// A stub that links to a file elsewhere is written through the link, as every file install
    /// writes is, even when that file does not exist yet, and uninstall removes the link and its
    /// `rexume` directory, leaving the file it pointed to.
    #[test]
    fn a_linked_stub_is_written_through_and_its_link_removed() -> Result<()> {
        let home = tempfile::tempdir()?;
        let home = home.path();
        std::fs::create_dir_all(home.join(".pi/agent"))?;
        std::fs::create_dir_all(home.join(".claude"))?;
        let existing = home.join("dotfiles/agents/SKILL.md");
        std::fs::create_dir_all(existing.parent().unwrap())?;
        std::fs::write(
            &existing,
            format!("{WRITTEN_BY}, by an older version -->\n"),
        )?;
        let missing = home.join("dotfiles/claude/SKILL.md");
        let links = [
            (stub(home, ".agents/skills"), &existing),
            (stub(home, ".claude/skills"), &missing),
        ];
        for (link, target) in &links {
            std::fs::create_dir_all(link.parent().unwrap())?;
            std::os::unix::fs::symlink(target, link)?;
        }
        let values =
            |outcomes: [(&'static str, Outcome); 2]| outcomes.map(|(key, (v, _))| (key, v));
        assert_eq!(
            values(install(home)?),
            [("agents_skill", "updated"), ("claude_skill", "created")]
        );
        assert_eq!(
            values(uninstall(home)?),
            [("agents_skill", "removed"), ("claude_skill", "removed")]
        );
        for (link, target) in links {
            assert!(!link.is_symlink() && !link.parent().unwrap().exists());
            assert_eq!(std::fs::read_to_string(target)?, STUB);
        }
        Ok(())
    }

    /// A `rexume` directory that links elsewhere is the user's arrangement: install writes the stub
    /// into its target, and uninstall removes that file and leaves the link and its target.
    #[test]
    fn a_linked_rexume_directory_stays() -> Result<()> {
        let home = tempfile::tempdir()?;
        let home = home.path();
        std::fs::create_dir_all(home.join(".pi/agent"))?;
        let target = home.join("dotfiles/rexume");
        std::fs::create_dir_all(&target)?;
        let file = target.join("SKILL.md");
        std::fs::write(&file, format!("{WRITTEN_BY}, by an older version -->\n"))?;
        let link = home.join(".agents/skills/rexume");
        std::fs::create_dir_all(link.parent().unwrap())?;
        std::os::unix::fs::symlink(&target, &link)?;
        assert_eq!(install(home)?[0].1.0, "updated");
        assert!(link.is_symlink());
        assert_eq!(std::fs::read_to_string(&file)?, STUB);
        assert_eq!(uninstall(home)?[0].1.0, "removed");
        assert!(link.is_symlink() && target.is_dir() && !file.exists());
        Ok(())
    }

    /// A `rexume` skill install did not write is kept by install, refresh, and uninstall alike,
    /// while the other directory's stub is written, refreshed, and removed; a directory none of
    /// whose runtimes is set up gets nothing.
    #[test]
    fn a_skill_install_did_not_write_stays() -> Result<()> {
        let home = tempfile::tempdir()?;
        let home = home.path();
        let (agents, claude) = (stub(home, ".agents/skills"), stub(home, ".claude/skills"));
        let (outcome, note) = install(home)?[0].1.clone();
        assert_eq!(
            (outcome, note.as_deref()),
            ("skipped", Some("no `~/.codex` or `~/.pi/agent`"))
        );
        assert!(!agents.exists());

        std::fs::create_dir_all(home.join(".pi/agent"))?;
        std::fs::create_dir_all(claude.parent().unwrap())?;
        std::fs::write(&claude, "the user's own skill\n")?;
        let [(_, agents_outcome), (_, claude_outcome)] = install(home)?;
        assert_eq!(agents_outcome.0, "created");
        assert_eq!(
            claude_outcome,
            ("kept", Some("not written by install".to_owned()))
        );

        std::fs::write(
            &agents,
            format!("---\n---\n{WRITTEN_BY}, by an older version -->\n"),
        )?;
        assert_eq!(
            refresh_skills(home)?,
            [("agents_skill", "updated"), ("claude_skill", "kept")]
        );
        assert_eq!(std::fs::read_to_string(&agents)?, STUB);
        let [(_, agents_outcome), (_, claude_outcome)] = uninstall(home)?;
        assert_eq!((agents_outcome.0, claude_outcome.0), ("removed", "kept"));
        assert_eq!(std::fs::read_to_string(&claude)?, "the user's own skill\n");
        Ok(())
    }
}
