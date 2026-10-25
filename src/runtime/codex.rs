//! Codex.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{Conversation, Lines, Open, Sealed, Sent, Span, Target, Turn, text_at};

/// `codex resume` takes the configured defaults rather than the conversation's own settings, so
/// they are passed on the command line.
pub fn resume_argv(c: &Conversation) -> Vec<String> {
    let mut argv = vec!["codex".to_owned(), "resume".to_owned(), c.native_id.clone()];
    if let Some(model) = c.setting("model") {
        argv.extend(["-m".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = c.setting("effort") {
        argv.extend([
            "-c".to_owned(),
            format!("model_reasoning_effort=\"{effort}\""),
        ]);
    }
    argv
}

/// The model and reasoning effort a Codex conversation last applied, from its own record. Codex
/// records them whenever a session starts, resumes, or changes them; the session-start hook
/// carries no effort.
pub fn settings(path: &Path, conversation: &mut Conversation) -> Result<()> {
    for record in Lines::new(path, 0).read()? {
        let record = record?;
        if record.pointer("/payload/type").and_then(Value::as_str)
            != Some("thread_settings_applied")
        {
            continue;
        }
        for (field, key) in [("model", "model"), ("reasoning_effort", "effort")] {
            if let Some(value) = record.pointer(&format!("/payload/thread_settings/{field}")) {
                conversation.settings.insert(key.to_owned(), value.clone());
            }
        }
    }
    Ok(())
}

/// `codex queue` stores the message for the thread, and an idle session runs it.
pub fn send(target: &Target, text: &str) -> Result<Sent> {
    let child = Command::new("codex")
        .args(["queue", "--thread", target.native_id, "--message", text])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start `codex`")?;
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(e) => {
            return Ok(Sent::Unknown(format!(
                "`codex queue` ran but its output could not be read: {e}"
            )));
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(if !output.status.success() {
        Sent::Unknown(format!(
            "`codex queue` exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    } else if stdout.starts_with("Queued message") {
        Sent::Accepted(stdout)
    } else {
        Sent::Unknown(format!(
            "`codex queue` did not confirm the message: {stdout}"
        ))
    })
}

/// A queued message reaches the thread as a user response item.
pub fn received(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("response_item")
        && record.pointer("/payload/role").and_then(Value::as_str) == Some("user")
}

/// Codex opens a turn with `task_started` and ends it with `task_complete`, or with
/// `turn_aborted` when the user interrupts; a turn cut off by the terminal's end, including a
/// graceful server stop, leaves neither.
#[derive(Default)]
pub struct Parser {
    turn: Option<Turn>,
    /// Where the turn not yet yielded starts.
    open: Option<u64>,
}

impl Parser {
    pub fn feed(&mut self, record: &Value, span: Span, sealed: &mut Vec<Sealed>) {
        if record.get("type").and_then(Value::as_str) != Some("event_msg") {
            return;
        }
        let Some(payload) = record.get("payload") else {
            return;
        };
        let ended = match payload.get("type").and_then(Value::as_str) {
            Some("task_started") => {
                self.turn = Some(Turn::Open { since: None });
                self.open = Some(span.start);
                return;
            }
            Some("task_complete") => Turn::Completed {
                reference: text_at(payload, "/turn_id"),
                text: text_at(payload, "/last_agent_message"),
            },
            Some("turn_aborted") => {
                if payload.get("reason").and_then(Value::as_str) == Some("interrupted") {
                    Turn::Cancelled
                } else {
                    Turn::Failed {
                        text: text_at(payload, "/reason"),
                    }
                }
            }
            _ => return,
        };
        self.turn = Some(ended.clone());
        sealed.push(Sealed {
            turn: ended,
            span: Span {
                start: self.open.take().unwrap_or(span.start),
                end: span.end,
            },
        });
    }

    pub fn turn(&self) -> Turn {
        self.turn.clone().unwrap_or(Turn::None)
    }

    pub fn open(&self) -> Option<Open> {
        self.open.map(|start| Open {
            start,
            since: None,
            answer: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::super::tests::{settings, transcript};
    use super::super::{Runtime, last_turn};
    use super::*;

    #[test]
    fn turn_states_follow_the_records() -> Result<()> {
        let started = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#;
        let reasoning = r#"{"type":"response_item","payload":{"type":"reasoning"}}"#;
        let complete = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"ok"}}"#;
        let aborted = r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t1","reason":"interrupted"}}"#;
        let turn = |lines: &[&str]| last_turn(Runtime::Codex, transcript(lines).path(), 0);
        assert_eq!(
            turn(&[started, reasoning])?,
            Some(Turn::Open { since: None })
        );
        assert_eq!(
            turn(&[started, complete])?,
            Some(Turn::Completed {
                reference: "t1".into(),
                text: "ok".into()
            })
        );
        assert_eq!(turn(&[started, aborted])?, Some(Turn::Cancelled));
        Ok(())
    }

    fn applied(effort: &str) -> String {
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"thread_settings_applied","thread_settings":{{"model":"gpt-6.1-sol","reasoning_effort":"{effort}"}}}}}}"#
        )
    }

    #[test]
    fn the_last_applied_settings_win_over_the_reported_ones() -> Result<()> {
        let mut rollout = transcript(&[&applied("low")]);
        // The session-start hook reports the model but no effort.
        let reported = Conversation {
            native_id: "thread".into(),
            transcript: Some(rollout.path().to_string_lossy().into_owned()),
            settings: settings(&[("model", "gpt-6.1-sol")]),
        };
        let mut c = reported.clone();
        Runtime::Codex.settings(&mut c)?;
        assert_eq!(
            Runtime::Codex.resume_argv(&c),
            [
                "codex",
                "resume",
                "thread",
                "-m",
                "gpt-6.1-sol",
                "-c",
                r#"model_reasoning_effort="low""#
            ]
        );
        writeln!(rollout, "{}", applied("medium"))?;
        rollout.flush()?;
        let mut c = reported;
        Runtime::Codex.settings(&mut c)?;
        assert_eq!(c.setting("effort"), Some("medium"));
        Ok(())
    }
}
