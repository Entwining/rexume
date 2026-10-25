//! Claude Code.

use std::io::Write;
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Conversation, Open, Sealed, Sent, Span, Target, Turn, content_texts, text_at};
use crate::rex::time_at;

/// Claude Code takes the model and effort of a resumed session from the command line; its session
/// start leaves them out on resume, so the reported launch settings are what reopen it.
pub fn resume_argv(c: &Conversation) -> Vec<String> {
    let mut argv = vec![
        "claude".to_owned(),
        "--resume".to_owned(),
        c.native_id.clone(),
    ];
    for (key, flag) in [("model", "--model"), ("effort", "--effort")] {
        if let Some(value) = c.setting(key) {
            argv.extend([flag.to_owned(), value.to_owned()]);
        }
    }
    argv
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
/// `end_turn`; a turn cut off by the terminal's end leaves neither. It writes each content block
/// of a message as its own record, each carrying the message's final `stop_reason`, so an answer
/// has ended only once a record that is not part of its message follows; normally that is the
/// turn's duration, written right after.
#[derive(Default)]
pub struct Parser {
    turn: Option<Turn>,
    /// The message ID and texts of the answer being written.
    answer: Option<(String, Vec<String>)>,
    /// Where the turn not yet yielded starts, and when its first record was written.
    open: Option<(u64, Option<i64>)>,
    /// The offset after the answer's last record so far, once one carried `end_turn`.
    ending: Option<u64>,
}

impl Parser {
    pub fn feed(&mut self, record: &Value, span: Span, sealed: &mut Vec<Sealed>) {
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let kind = record.get("type").and_then(Value::as_str);
        if let Some(end) = self.ending {
            let message_id = text_at(record, "/message/id");
            let same = kind == Some("assistant")
                && self
                    .answer
                    .as_ref()
                    .is_some_and(|(id, _)| *id == message_id);
            if !same {
                self.ending = None;
                if let (Some((start, _)), Some(turn)) = (self.open.take(), self.turn.clone()) {
                    sealed.push(Sealed {
                        turn,
                        span: Span { start, end },
                    });
                }
            }
        }
        match kind {
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
                    if let Some((start, _)) = self.open.take() {
                        sealed.push(Sealed {
                            turn: Turn::Cancelled,
                            span: Span {
                                start,
                                end: span.end,
                            },
                        });
                    }
                } else if starts_turn {
                    let since = time_at(record, "/timestamp");
                    self.turn = Some(Turn::Open { since });
                    self.answer = None;
                    self.open = Some((span.start, since));
                }
            }
            Some("assistant") => self.assistant(record, span, sealed),
            _ => {}
        }
    }

    fn assistant(&mut self, record: &Value, span: Span, sealed: &mut Vec<Sealed>) {
        let message_id = text_at(record, "/message/id");
        let texts = content_texts(record.pointer("/message/content"));
        let start = self.open.map_or(span.start, |(start, _)| start);
        if record.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
            let failed = Turn::Failed {
                text: texts.join("\n"),
            };
            self.turn = Some(failed.clone());
            self.answer = None;
            self.open = None;
            sealed.push(Sealed {
                turn: failed,
                span: Span {
                    start,
                    end: span.end,
                },
            });
            return;
        }
        self.open
            .get_or_insert((start, time_at(record, "/timestamp")));
        match &mut self.answer {
            Some((id, collected)) if *id == message_id => collected.extend(texts),
            _ => self.answer = Some((message_id, texts)),
        }
        // A record reaching here while the answer is ending is another part of its message.
        if self.ending.is_some()
            || record
                .pointer("/message/stop_reason")
                .and_then(Value::as_str)
                == Some("end_turn")
        {
            let (reference, text) = self.answer.clone().unwrap_or_default();
            self.turn = Some(Turn::Completed {
                reference,
                text: text.join("\n"),
            });
            self.ending = Some(span.end);
        } else if matches!(self.turn, Some(Turn::Completed { .. })) {
            self.turn = Some(Turn::Open { since: None });
        }
    }

    pub fn turn(&self) -> Turn {
        self.turn.clone().unwrap_or(Turn::None)
    }

    pub fn open(&self) -> Option<Open> {
        let (start, since) = self.open?;
        Some(Open {
            start,
            since,
            answer: self
                .ending
                .zip(self.turn.clone())
                .map(|(end, turn)| (turn, end)),
        })
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
mod tests;
