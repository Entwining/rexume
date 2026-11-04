//! oh-my-pi (omp), a fork of Pi, through the extension `rexume install` writes and the daemon keeps
//! current. omp replaces its session file when it rewrites it, which a record followed by offset
//! cannot follow, so the extension writes the conversation's turns to a record of this tool's.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Conversation, Sealed, Span, Turn, inbox, text_at};

/// The extension `rexume install` writes into omp's extensions directory.
pub const EXTENSION: &str = include_str!("omp/extension.ts");

/// omp reopens a session from its file with the model and thinking level the session last used; a
/// setting given on the command line would override those.
pub fn resume_argv(c: &Conversation) -> Vec<String> {
    vec![
        "omp".to_owned(),
        "--resume".to_owned(),
        c.transcript.clone().unwrap_or_else(|| c.native_id.clone()),
    ]
}

/// The directory of the turn records the extension writes, which also holds the inboxes.
pub fn records(state_dir: &Path) -> PathBuf {
    state_dir.join("omp")
}

pub fn inbox(state_dir: &Path, native_id: &str) -> PathBuf {
    inbox::dir(&records(state_dir).join("inbox"), native_id)
}

pub fn record(state_dir: &Path, native_id: &str) -> PathBuf {
    records(state_dir).join(format!("{native_id}.jsonl"))
}

pub fn received(record: &Value) -> bool {
    record.get("event").and_then(Value::as_str) == Some("received")
}

/// omp's turns as the extension records them: `session` when a process takes the session up,
/// `start` when omp starts a run, `prompt` for input from the user or the command line, `received`
/// for a notice or request in the conversation, and `end` when a run ends. A turn ends at an `end`
/// after which omp waits for input, or only for background work whose result starts another; within
/// one process, every run that starts before then carries the turn on, as omp's retries,
/// compactions, and recoveries do. A process that ended without that `end`, as the server's end
/// cuts one off, leaves the turn open: the next process's first run carries it on with the
/// continuation prompt or no input, and with any other input starts a new turn, after which the
/// cut-off one ends as cancelled, as Claude Code's does.
#[derive(Default)]
pub struct Parser {
    turn: Option<Turn>,
    /// Where the turn not yet yielded starts.
    open: Option<u64>,
    /// A process took the session up while the turn was open, and has started no run since.
    resumed: bool,
    /// Where that process's first run started, until its input or its end tells whose it is.
    restart: Option<u64>,
}

impl Parser {
    pub fn feed(&mut self, record: &Value, span: Span, sealed: &mut Vec<Sealed>) {
        let text = |key: &str| record.get(key).and_then(Value::as_str);
        let flag = |key: &str| record.get(key).and_then(Value::as_bool) == Some(true);
        let ended = match text("event") {
            Some("session") => {
                self.resumed = self.open.is_some();
                return;
            }
            Some("start") => {
                if self.open.is_none() {
                    self.open = Some(span.start);
                } else if self.resumed {
                    self.restart = Some(span.start);
                }
                self.resumed = false;
                self.turn = Some(Turn::Open);
                return;
            }
            Some("prompt" | "received") => {
                if let Some(start) = self.restart.take()
                    && !text("token").is_some_and(super::is_continuation)
                {
                    self.seal(Turn::Cancelled, start, sealed);
                    self.open = Some(start);
                }
                return;
            }
            Some("end") if flag("will_continue") && !flag("awaiting_async_work") => {
                self.restart = None;
                return;
            }
            Some("end") => match text("stop_reason") {
                // `length` cut the answer short; `toolUse`, a tool such as `yield` ended the run.
                Some("stop" | "length" | "toolUse") => Turn::Completed {
                    reference: text_at(record, "/id"),
                    text: text_at(record, "/answer"),
                },
                Some("aborted") => Turn::Cancelled,
                Some("error") => Turn::Failed {
                    text: text_at(record, "/error"),
                },
                Some(reason) => Turn::Failed {
                    text: format!("the run ended with stop reason `{reason}`"),
                },
                None => Turn::Failed {
                    text: "the run ended without an answer".to_owned(),
                },
            },
            _ => return,
        };
        self.open.get_or_insert(span.start);
        self.seal(ended.clone(), span.end, sealed);
        self.turn = Some(ended);
    }

    /// Yields the open turn as `turn`, ending at `end`.
    fn seal(&mut self, turn: Turn, end: u64, sealed: &mut Vec<Sealed>) {
        if let Some(start) = self.open.take() {
            sealed.push(Sealed {
                turn,
                span: Span { start, end },
                took: Vec::new(),
            });
        }
        self.resumed = false;
        self.restart = None;
    }

    pub fn turn(&self) -> Turn {
        self.turn.clone().unwrap_or(Turn::None)
    }
}

#[cfg(test)]
mod tests;
