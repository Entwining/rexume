use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::file::physical;
use super::{Dirs, Outcome, refresh_file, remove_file, write};
use crate::skill::STUB;

const WRITTEN_BY: &str = "<!-- Written by `rexume install`";

struct Target {
    key: &'static str,
    skills: fn(&Dirs) -> PathBuf,
    /// The runtimes that read these skills; the stub is written only when one is set up.
    runtimes: fn(&Dirs) -> Vec<&Path>,
}

const TARGETS: [Target; 2] = [
    Target {
        key: "agents_skill",
        skills: |dirs| dirs.home.join(".agents/skills"),
        runtimes: |dirs| vec![&dirs.codex, &dirs.pi],
    },
    Target {
        key: "claude_skill",
        skills: |dirs| dirs.claude.join("skills"),
        runtimes: |dirs| vec![&dirs.claude],
    },
];

fn is_ours(text: &str) -> bool {
    text.lines().any(|line| line.starts_with(WRITTEN_BY))
}

fn each<T: Clone>(
    dirs: &Dirs,
    mut apply: impl FnMut(&[&Target], &Path) -> Result<T>,
) -> Result<[(&'static str, T); 2]> {
    let [agents, claude] = &TARGETS;
    let (agents_skills, claude_skills) = ((agents.skills)(dirs), (claude.skills)(dirs));
    let agents_path = agents_skills.join("rexume/SKILL.md");
    if physical(&claude_skills)? == physical(&agents_skills)? {
        let result = apply(&[agents, claude], &agents_path)?;
        return Ok([(agents.key, result.clone()), (claude.key, result)]);
    }
    let claude_path = claude_skills.join("rexume/SKILL.md");
    Ok([
        (agents.key, apply(&[agents], &agents_path)?),
        (claude.key, apply(&[claude], &claude_path)?),
    ])
}

pub(super) fn install(dirs: &Dirs) -> Result<[(&'static str, Outcome); 2]> {
    each(dirs, |targets, path| {
        let runtimes: Vec<&Path> = targets.iter().flat_map(|t| (t.runtimes)(dirs)).collect();
        if !runtimes.iter().any(|r| r.is_dir()) {
            let shown: Vec<String> = runtimes
                .iter()
                .map(|r| format!("`{}`", dirs.show(r)))
                .collect();
            return Ok(("skipped", Some(format!("no {}", shown.join(" or ")))));
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

pub(super) fn uninstall(dirs: &Dirs) -> Result<[(&'static str, Outcome); 2]> {
    each(dirs, |_, path| {
        let outcome = remove_file(path, is_ours)?;
        if outcome.0 == "removed"
            && let Some(dir) = path.parent()
        {
            match std::fs::remove_dir(dir) {
                Ok(()) => {}
                // A `rexume` that links to a directory fails with `NotADirectory`; the link is the
                // user's and stays.
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

pub fn refresh_skills(dirs: &Dirs) -> Result<[(&'static str, &'static str); 2]> {
    each(dirs, |_, path| refresh_file(path, STUB.as_bytes(), is_ours))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub(home: &Path, skills: &str) -> PathBuf {
        home.join(skills).join("rexume/SKILL.md")
    }

    #[test]
    fn linked_skills_directories_report_one_file_twice() -> Result<()> {
        let home = tempfile::tempdir()?;
        let home = home.path();
        std::fs::create_dir_all(home.join(".claude"))?;
        std::os::unix::fs::symlink("../.agents/skills", home.join(".claude/skills"))?;
        let values =
            |outcomes: [(&'static str, Outcome); 2]| outcomes.map(|(key, (v, _))| (key, v));
        assert_eq!(
            values(install(&Dirs::new(home))?),
            [("agents_skill", "created"), ("claude_skill", "created")]
        );
        assert_eq!(std::fs::read_to_string(stub(home, ".claude/skills"))?, STUB);
        assert_eq!(
            values(install(&Dirs::new(home))?),
            [("agents_skill", "unchanged"), ("claude_skill", "unchanged")]
        );
        assert_eq!(
            values(uninstall(&Dirs::new(home))?),
            [("agents_skill", "removed"), ("claude_skill", "removed")]
        );
        assert!(!home.join(".agents/skills/rexume").exists());
        assert_eq!(
            refresh_skills(&Dirs::new(home))?,
            [("agents_skill", "absent"), ("claude_skill", "absent")]
        );
        Ok(())
    }

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
            values(install(&Dirs::new(home))?),
            [("agents_skill", "updated"), ("claude_skill", "created")]
        );
        assert_eq!(
            values(uninstall(&Dirs::new(home))?),
            [("agents_skill", "removed"), ("claude_skill", "removed")]
        );
        for (link, target) in links {
            assert!(!link.is_symlink() && !link.parent().unwrap().exists());
            assert_eq!(std::fs::read_to_string(target)?, STUB);
        }
        Ok(())
    }

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
        assert_eq!(install(&Dirs::new(home))?[0].1.0, "updated");
        assert!(link.is_symlink());
        assert_eq!(std::fs::read_to_string(&file)?, STUB);
        assert_eq!(uninstall(&Dirs::new(home))?[0].1.0, "removed");
        assert!(link.is_symlink() && target.is_dir() && !file.exists());
        Ok(())
    }

    #[test]
    fn a_skill_install_did_not_write_stays() -> Result<()> {
        let home = tempfile::tempdir()?;
        let home = home.path();
        let (agents, claude) = (stub(home, ".agents/skills"), stub(home, ".claude/skills"));
        let (outcome, note) = install(&Dirs::new(home))?[0].1.clone();
        assert_eq!(
            (outcome, note.as_deref()),
            ("skipped", Some("no `~/.codex` or `~/.pi/agent`"))
        );
        assert!(!agents.exists());

        std::fs::create_dir_all(home.join(".pi/agent"))?;
        std::fs::create_dir_all(claude.parent().unwrap())?;
        std::fs::write(&claude, "the user's own skill\n")?;
        let [(_, agents_outcome), (_, claude_outcome)] = install(&Dirs::new(home))?;
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
            refresh_skills(&Dirs::new(home))?,
            [("agents_skill", "updated"), ("claude_skill", "kept")]
        );
        assert_eq!(std::fs::read_to_string(&agents)?, STUB);
        let [(_, agents_outcome), (_, claude_outcome)] = uninstall(&Dirs::new(home))?;
        assert_eq!((agents_outcome.0, claude_outcome.0), ("removed", "kept"));
        assert_eq!(std::fs::read_to_string(&claude)?, "the user's own skill\n");
        Ok(())
    }
}
