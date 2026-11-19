use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Conversation, Sealed, Span, Turn, inbox, text_at};

pub const PLUGIN: &str = include_str!("opencode/plugin.ts");

/// OpenCode reopens a session with the agent and model it last used.
pub fn resume_argv(c: &Conversation) -> Vec<String> {
    vec![
        "opencode".to_owned(),
        "--session".to_owned(),
        c.native_id.clone(),
    ]
}

pub fn records(state_dir: &Path) -> PathBuf {
    state_dir.join("opencode")
}

pub fn inbox(state_dir: &Path, native_id: &str) -> PathBuf {
    inbox::dir(&records(state_dir).join("inbox"), native_id)
}

/// OpenCode keeps its sessions in its service's database, so the daemon follows the messages the
/// plugin copies here, and the signals that ended a TUI showing the session.
pub fn record(state_dir: &Path, native_id: &str) -> PathBuf {
    records(state_dir).join(format!("{native_id}.jsonl"))
}

pub fn received(record: &Value) -> bool {
    record.get("received").is_some()
}

/// OpenCode ends each turn with an `idle` message, after every answer and prompt the turn took in,
/// including prompts queued while it ran.
#[derive(Default)]
pub struct Parser {
    turn: Option<Turn>,
    open: Option<u64>,
    answer: Option<Answer>,
}

struct Answer {
    id: String,
    finished: bool,
    text: String,
    error: Option<String>,
    end: u64,
}

impl Parser {
    pub fn feed(&mut self, record: &Value, span: Span, sealed: &mut Vec<Sealed>) {
        let text = |key: &str| record.get(key).and_then(Value::as_str);
        match text("type") {
            // A prompt queued while the turn ran is taken in after its last answer, without an
            // `idle` between, and that answer is the turn's.
            Some("user" | "synthetic") => {
                if let Some(answer) = self.answer.take_if(|a| a.finished) {
                    let end = answer.end;
                    self.seal(answer.completed(), end, sealed);
                }
                self.open.get_or_insert(span.start);
                self.turn = Some(Turn::Open);
            }
            Some("assistant") => {
                self.answer = Some(Answer {
                    id: text_at(record, "/id"),
                    // `tool-calls` goes on with the tools' results, `length` with a prompt OpenCode
                    // adds to continue the answer, and `error` with a retry, or with the prompt
                    // that continues it after the service restarted; a turn that failed ends with
                    // its `idle`. One without a finish was cut off.
                    finished: !matches!(
                        text("finish"),
                        None | Some("tool-calls" | "length" | "error")
                    ),
                    text: text_at(record, "/text"),
                    error: text("error").map(str::to_owned),
                    end: span.end,
                });
                self.open.get_or_insert(span.start);
                self.turn = Some(Turn::Open);
            }
            // A prompt OpenCode admitted before wakes the session again, which then goes idle
            // without a message of its own.
            Some("idle") if self.open.is_some() => {
                let answer = self.answer.take();
                let ended = match text("outcome") {
                    Some("succeeded") => answer.map_or(
                        Turn::Completed {
                            reference: text_at(record, "/id"),
                            text: String::new(),
                        },
                        Answer::completed,
                    ),
                    Some("interrupted") => Turn::Cancelled,
                    outcome => Turn::Failed {
                        text: text("error")
                            .map(str::to_owned)
                            .or_else(|| answer.and_then(|a| a.error))
                            .unwrap_or_else(|| {
                                format!("the turn ended with outcome `{}`", outcome.unwrap_or(""))
                            }),
                    },
                };
                self.seal(ended, span.end, sealed);
            }
            _ => {}
        }
    }

    fn seal(&mut self, turn: Turn, end: u64, sealed: &mut Vec<Sealed>) {
        if let Some(start) = self.open.take() {
            sealed.push(Sealed {
                turn: turn.clone(),
                span: Span { start, end },
                took: Vec::new(),
            });
        }
        self.turn = Some(turn);
    }

    pub fn turn(&self) -> Turn {
        self.turn.clone().unwrap_or(Turn::None)
    }
}

impl Answer {
    fn completed(self) -> Turn {
        Turn::Completed {
            reference: self.id,
            text: self.text,
        }
    }
}

#[cfg(test)]
mod tests;
