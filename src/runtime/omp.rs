use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Conversation, Sealed, Span, Turn, inbox, text_at};

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

pub fn records(state_dir: &Path) -> PathBuf {
    state_dir.join("omp")
}

/// Apart from Pi's inboxes: omp opens a Pi session file, and Pi an omp one, under the ID the file
/// records.
pub fn inbox(state_dir: &Path, native_id: &str) -> PathBuf {
    inbox::dir(&records(state_dir).join("inbox"), native_id)
}

/// omp replaces its session file when it rewrites it, so the daemon follows the turns the
/// extension appends here instead.
pub fn record(state_dir: &Path, native_id: &str) -> PathBuf {
    records(state_dir).join(format!("{native_id}.jsonl"))
}

pub fn received(record: &Value) -> bool {
    record.get("event").and_then(Value::as_str) == Some("received")
}

#[derive(Default)]
pub struct Parser {
    turn: Option<Turn>,
    open: Option<u64>,
    resumed: bool,
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
            // Another run carries the turn on, as omp's retries, compactions, and recoveries do; an
            // end awaiting background work ends the turn, since the work's result starts another.
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
