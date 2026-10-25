//! Claude Code.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::lines::last_record;
use super::{Conversation, Sent, Target, Turn, content_texts, text_at};
use crate::rex::time_at;

/// Claude Code takes the model and effort of a resumed session from the command line; its session
/// start leaves them out on resume, so the reported launch settings are what reopen it. Its resume
/// starts a session that was bypassing permissions in a new session's mode, and a Manual one in a
/// settings `defaultMode`, so the mode the record shows is passed too.
pub fn resume_argv(c: &Conversation) -> Vec<String> {
    let mut argv = vec![
        "claude".to_owned(),
        "--resume".to_owned(),
        c.native_id.clone(),
    ];
    for (key, flag) in [
        ("model", "--model"),
        ("effort", "--effort"),
        ("permission_mode", "--permission-mode"),
    ] {
        if let Some(value) = c.setting(key) {
            argv.extend([flag.to_owned(), value.to_owned()]);
        }
    }
    argv
}

/// The permission mode the conversation was last in. Claude Code writes it on each prompt and in
/// a `permission-mode` entry when a turn ends and when the session exits, so a mode changed after
/// the last prompt is there too.
pub fn settings(transcript: &Path, conversation: &mut Conversation) -> Result<()> {
    let mode = last_record(transcript, "\"permissionMode\"", |record| {
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        record.get("permissionMode")?.as_str().map(str::to_owned)
    })?;
    if let Some(mode) = mode {
        conversation
            .settings
            .insert("permission_mode".to_owned(), mode.into());
    }
    Ok(())
}

/// Claude Code delivers a line on its session socket as a message from another session, and
/// starts a turn with it when idle. The socket answers nothing.
pub fn send(target: &Target, text: &str) -> Result<Sent> {
    let socket = target
        .transport
        .get("messaging_socket")
        .and_then(Value::as_str)
        .context("the conversation reported no messaging socket")?;
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("failed to connect to `{socket}`"))?;
    let line = json!({ "type": "user", "message": { "role": "user", "content": text } })
        .to_string()
        + "\n";
    match stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.flush())
    {
        Ok(()) => Ok(Sent::Handed),
        Err(e) => Ok(Sent::Unknown(format!(
            "the write to the messaging socket failed midway: {e}"
        ))),
    }
}

/// A message from another session is a user record whose origin is a peer.
pub fn received(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("user")
        && record.pointer("/origin/kind").and_then(Value::as_str) == Some("peer")
}

const INTERRUPT: &str = "[Request interrupted by user";

/// Claude Code writes the user's cancel as a user text `[Request interrupted by user]` (or `... for
/// tool use]`) and ends an answered turn with an assistant message whose `stop_reason` is
/// `end_turn`; a turn cut off by the terminal's end leaves neither.
#[derive(Default)]
pub struct Parser {
    turn: Option<Turn>,
    /// The message ID and texts of the answer being written.
    answer: Option<(String, Vec<String>)>,
}

impl Parser {
    pub fn feed(&mut self, record: &Value) {
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        match record.get("type").and_then(Value::as_str) {
            Some("user") => {
                let content = record.pointer("/message/content");
                // Every turn start, typed or delivered from another session, carries `turnOrigin`;
                // a delivered message is also marked `isMeta` like injected context.
                let starts_turn = record.get("turnOrigin").is_some()
                    || (record.get("isMeta").and_then(Value::as_bool) != Some(true)
                        && is_prompt(content));
                if content_texts(content)
                    .iter()
                    .any(|t| t.starts_with(INTERRUPT))
                {
                    self.turn = Some(Turn::Cancelled);
                    self.answer = None;
                } else if starts_turn {
                    self.turn = Some(Turn::Open {
                        since: time_at(record, "/timestamp"),
                    });
                    self.answer = None;
                }
            }
            Some("assistant") => {
                let message_id = text_at(record, "/message/id");
                let texts = content_texts(record.pointer("/message/content"));
                if record.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
                    self.turn = Some(Turn::Failed {
                        text: texts.join("\n"),
                    });
                    self.answer = None;
                    return;
                }
                match &mut self.answer {
                    Some((id, collected)) if *id == message_id => collected.extend(texts),
                    _ => self.answer = Some((message_id, texts)),
                }
                if record
                    .pointer("/message/stop_reason")
                    .and_then(Value::as_str)
                    == Some("end_turn")
                {
                    let (reference, text) = self.answer.clone().unwrap_or_default();
                    self.turn = Some(Turn::Completed {
                        reference,
                        text: text.join("\n"),
                    });
                } else if matches!(self.turn, Some(Turn::Completed { .. })) {
                    self.turn = Some(Turn::Open { since: None });
                }
            }
            _ => {}
        }
    }

    pub fn turn(&self) -> Turn {
        self.turn.clone().unwrap_or(Turn::None)
    }
}

/// A user record that starts a turn, as opposed to a tool result fed back into a running one.
fn is_prompt(content: Option<&Value>) -> bool {
    match content {
        Some(Value::String(_)) => true,
        Some(Value::Array(items)) => {
            !items.is_empty()
                && items
                    .iter()
                    .all(|item| item.get("type").and_then(Value::as_str) != Some("tool_result"))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ANSWER, PROMPT, transcript};
    use super::super::{Runtime, last_turn};
    use super::*;

    const THINKING: &str = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":null,"content":[{"type":"thinking","thinking":""}]}}"#;
    const TOOL_USE: &str = r#"{"type":"assistant","message":{"id":"m0","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}"#;
    const TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
    const CANCEL: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
    const API_REQUEST: &str = r#"{"type":"api-request"}"#;
    const PEER: &str = r#"{"type":"user","isMeta":true,"turnOrigin":"peer","message":{"role":"user","content":"Another Claude session sent a message:\n[rexume d1] done"}}"#;
    const CONTEXT: &str = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<system-reminder>context</system-reminder>"}}"#;

    fn turn(lines: &[&str]) -> Result<Option<Turn>> {
        last_turn(Runtime::Claude, transcript(lines).path(), 0)
    }

    #[test]
    fn turn_states_follow_the_records() -> Result<()> {
        let open = Some(Turn::Open { since: None });
        assert_eq!(turn(&[PROMPT, TOOL_USE, TOOL_RESULT, API_REQUEST])?, open);
        assert_eq!(
            turn(&[PROMPT, THINKING, ANSWER])?,
            Some(Turn::Completed {
                reference: "m1".into(),
                text: "Done.".into()
            })
        );
        assert_eq!(turn(&[PROMPT, THINKING, CANCEL])?, Some(Turn::Cancelled));
        assert_eq!(turn(&[PROMPT, ANSWER, PROMPT])?, open);
        assert_eq!(turn(&[PROMPT, ANSWER, PEER])?, open);
        assert!(matches!(
            turn(&[PROMPT, ANSWER, CONTEXT])?,
            Some(Turn::Completed { .. })
        ));
        Ok(())
    }

    #[test]
    fn an_open_prompt_carries_when_it_was_recorded() -> Result<()> {
        let prompt = r#"{"type":"user","timestamp":"2026-10-24T15:29:45.738Z","turnOrigin":"human","message":{"role":"user","content":"Count"}}"#;
        assert_eq!(
            turn(&[prompt])?,
            Some(Turn::Open {
                since: Some(1_792_855_785_738)
            })
        );
        Ok(())
    }
}
