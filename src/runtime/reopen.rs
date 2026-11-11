use std::path::Path;

use anyhow::Result;
use serde_json::{Map, Value};

use super::{Conversation, Runtime, claude, codex, omp, pi};
use crate::proc;

const TASK_META: &str = "REXUME_TASK_META";

impl Runtime {
    pub fn resume_argv(self, conversation: &Conversation) -> Vec<String> {
        match self {
            Self::Claude => claude::resume_argv(conversation),
            Self::Codex => codex::resume_argv(conversation),
            Self::Pi => pi::resume_argv(conversation),
            Self::Omp => omp::resume_argv(conversation),
        }
    }

    pub fn resume_env(self, conversation: &Conversation) -> Vec<(String, String)> {
        let mut env = match self {
            Self::Claude => claude::resume_env(conversation),
            Self::Codex | Self::Pi | Self::Omp => Vec::new(),
        };
        let meta = conversation.setting("task_meta").unwrap_or_default();
        env.push((TASK_META.to_owned(), meta.to_owned()));
        env
    }

    /// Read from the process, not from the hook: Codex runs hooks through a login shell, and a
    /// Claude Code hook's environment also holds the `env` of Claude Code's settings, which Claude
    /// Code applies itself on resume.
    pub fn launch_settings(self, pid: u32) -> Map<String, Value> {
        proc::launch(pid).map_or_else(
            || {
                log::warn!("process {pid}: cannot read what it was started with");
                Map::new()
            },
            |launch| self.launch_patch(&launch, pid),
        )
    }

    fn launch_patch(self, launch: &proc::Launch, pid: u32) -> Map<String, Value> {
        let mut settings = match self {
            Self::Claude => claude::launch::patch(launch, pid),
            Self::Codex | Self::Pi | Self::Omp => Map::new(),
        };
        let meta = launch.var(TASK_META).filter(|meta| !meta.is_empty());
        settings.insert(
            "task_meta".to_owned(),
            meta.map_or(Value::Null, Value::from),
        );
        settings
    }

    pub fn launch_error(self, conversation: &Conversation) -> Option<&str> {
        match self {
            Self::Claude => conversation.setting("launch_error"),
            Self::Codex | Self::Pi | Self::Omp => None,
        }
    }

    /// Claude Code, Codex, and omp refuse to resume a conversation whose file does not exist yet,
    /// which Claude Code and Codex write only with its first prompt; Pi opens a missing session
    /// file as a new session.
    pub fn can_reopen(self, conversation: &Conversation) -> bool {
        match self {
            Self::Claude | Self::Codex | Self::Omp => conversation
                .transcript
                .as_deref()
                .is_none_or(|transcript| Path::new(transcript).exists()),
            Self::Pi => true,
        }
    }

    pub fn settings(self, conversation: &mut Conversation) -> Result<()> {
        let Some(transcript) = conversation.transcript.clone() else {
            return Ok(());
        };
        match self {
            Self::Claude => claude::settings(Path::new(&transcript), conversation),
            Self::Codex => codex::settings(Path::new(&transcript), conversation),
            Self::Pi => {
                pi::settings(Path::new(&transcript), conversation);
                Ok(())
            }
            // omp restores them from the session file it reopens.
            Self::Omp => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(env: &[String]) -> proc::Launch {
        proc::Launch {
            argv: vec!["agent".to_owned()],
            env: env.to_vec(),
        }
    }

    #[test]
    fn a_task_meta_reopens_with_the_conversation_it_started() {
        let meta = r#"{"eval": "it's 1"}"#;
        let given = format!("REXUME_TASK_META={meta}");
        let declared = [
            given.clone(),
            "REXUME_LAUNCH_PID=7".to_owned(),
            r#"REXUME_LAUNCH=["claude"]"#.to_owned(),
            "AWS_REGION=x".to_owned(),
        ];
        for runtime in [Runtime::Claude, Runtime::Codex, Runtime::Pi, Runtime::Omp] {
            for env in [&[given.clone()][..], &declared] {
                let settings = runtime.launch_patch(&launch(env), 7);
                assert_eq!(settings["task_meta"], meta, "{runtime}");
                let reopened = Conversation {
                    settings,
                    ..Conversation::default()
                };
                let env = runtime.resume_env(&reopened);
                assert_eq!(env, [(TASK_META.to_owned(), meta.to_owned())], "{runtime}");
            }
            for env in [&[][..], &["REXUME_TASK_META=".to_owned()]] {
                let settings = runtime.launch_patch(&launch(env), 7);
                assert_eq!(settings["task_meta"], Value::Null, "{runtime}");
                let reopened = Conversation {
                    settings,
                    ..Conversation::default()
                };
                let env = runtime.resume_env(&reopened);
                assert_eq!(env, [(TASK_META.to_owned(), String::new())], "{runtime}");
            }
        }
    }
}
