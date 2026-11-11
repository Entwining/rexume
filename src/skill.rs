use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

pub const STUB: &str = include_str!("../skills/rexume/SKILL.md");
const GUIDE: &str = include_str!("../skills/rexume/guide.md");
const AGENTS: &str = include_str!("../skills/rexume/agents.md");
const RUNTIMES: &str = include_str!("../skills/rexume/runtimes.md");
const CLI: &str = include_str!("../docs/cli.md");
const REX_GUIDE_REVIEW: &str = include_str!("../skills/rexume/rex-guide.json");

/// A part of the guide that its main text tells the agent to read before a given action.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Topic {
    /// Starting, prompting, and collecting from other coding agents, and running a panel
    Agents,
    /// What differs between Claude Code, Codex, and Pi, as the host or as a participant
    Runtimes,
    /// The task commands' contract: the `task` section of Rexume's command-line reference, after
    /// the conventions every command shares
    Task,
}

pub fn text(topic: Option<Topic>) -> Result<String> {
    Ok(match topic {
        None => GUIDE.to_owned(),
        Some(Topic::Agents) => AGENTS.to_owned(),
        Some(Topic::Runtimes) => RUNTIMES.to_owned(),
        Some(Topic::Task) => task_contract()?,
    })
}

pub fn status() -> Result<Value> {
    #[derive(Deserialize)]
    struct Review {
        rex_version: String,
        rex_guide_sha256: String,
    }
    let review: Review =
        serde_json::from_str(REX_GUIDE_REVIEW).context("skills/rexume/rex-guide.json")?;
    Ok(json!({
        "version": 1,
        "rex_version": review.rex_version,
        "rex_guide_sha256": review.rex_guide_sha256,
    }))
}

fn task_contract() -> Result<String> {
    let mut starts: Vec<usize> = CLI.match_indices("\n## ").map(|(i, _)| i + 1).collect();
    starts.insert(0, 0);
    starts.push(CLI.len());
    let parts: Vec<&str> = starts.windows(2).map(|w| &CLI[w[0]..w[1]]).collect();
    let task = parts
        .iter()
        .find(|part| part.starts_with("## task\n"))
        .context("docs/cli.md has no `task` section")?;
    Ok(format!("{}{task}", parts[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_topic_is_the_conventions_and_the_whole_task_section() -> Result<()> {
        let task = text(Some(Topic::Task))?;
        assert!(task.starts_with("# Rexume command line\n"));
        let sections: Vec<&str> = task.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(sections, ["## task"], "only the task section follows");
        assert!(
            task.contains("`task wait` exits 3"),
            "the exit codes are kept"
        );
        assert!(task.contains("`TASK` has `id`"), "the section is whole");
        Ok(())
    }

    #[test]
    fn the_review_record_holds_a_version_and_a_sha256() -> Result<()> {
        let status = status()?;
        assert_ne!(status["rex_version"].as_str(), Some(""));
        let sha256 = status["rex_guide_sha256"].as_str().unwrap_or_default();
        assert!(
            sha256.len() == 64
                && sha256
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "a lowercase hex SHA-256, not {sha256:?}"
        );
        Ok(())
    }
}
