use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::lines::last_record;
use super::{Conversation, Sealed, Span, Turn, content_texts, inbox, text_at};

pub const EXTENSION: &str = include_str!("pi/extension.ts");

const NOTICE: &str = "rexume";

pub fn records(state_dir: &Path) -> PathBuf {
    state_dir.join("pi")
}

/// The extension appends a line for each signal that ended an interactive Pi in a Rex pane, since
/// Pi reports that end to extensions as a quit.
pub fn exits(state_dir: &Path, native_id: &str) -> PathBuf {
    records(state_dir).join(format!("{}.jsonl", native_id.replace('/', "_")))
}

/// Every released extension watches this path, and a running Pi keeps the extension it loaded.
pub fn inbox(state_dir: &Path, native_id: &str) -> PathBuf {
    inbox::dir(&state_dir.join("inbox"), native_id)
}

pub fn resume_argv(c: &Conversation) -> Vec<String> {
    let mut argv = vec![
        "pi".to_owned(),
        "--session".to_owned(),
        c.transcript.clone().unwrap_or_else(|| c.native_id.clone()),
    ];
    if let (Some(provider), Some(model)) = (c.setting("provider"), c.setting("model")) {
        argv.extend([
            "--provider".to_owned(),
            provider.to_owned(),
            "--model".to_owned(),
            model.to_owned(),
        ]);
    }
    if let Some(effort) = c.setting("effort") {
        argv.extend(["--thinking".to_owned(), effort.to_owned()]);
    }
    argv
}

/// Pi restores the current branch's model and thinking level itself once the session has messages,
/// which is when it writes its file, and a setting given on the command line would override them.
pub fn settings(transcript: &Path, conversation: &mut Conversation) {
    if transcript.exists() {
        for key in ["provider", "model", "effort"] {
            conversation.settings.remove(key);
        }
    }
}

/// Another Pi may have the same session open, so only the end of the process the pane ran counts.
pub fn killed_at(exits: &Path, pid: i64) -> Result<Option<i64>> {
    last_record(exits, "\"pid\"", |exit| {
        (exit.get("pid").and_then(Value::as_i64) == Some(pid))
            .then(|| exit.get("at").and_then(Value::as_i64))
            .flatten()
    })
}

pub fn received(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("custom_message")
        && record.get("customType").and_then(Value::as_str) == Some(NOTICE)
}

#[derive(Default)]
pub struct Parser {
    turn: Option<Turn>,
    open: Option<u64>,
}

impl Parser {
    pub fn feed(&mut self, record: &Value, span: Span, sealed: &mut Vec<Sealed>) {
        let message = match record.get("type").and_then(Value::as_str) {
            Some("message") => record.get("message"),
            Some("custom_message") if received(record) => return self.start(span),
            _ => return,
        };
        let Some(message) = message else {
            return;
        };
        let ended = match message.get("role").and_then(Value::as_str) {
            Some("user") => return self.start(span),
            Some("assistant") => match message.get("stopReason").and_then(Value::as_str) {
                Some("stop") => Turn::Completed {
                    reference: text_at(record, "/id"),
                    text: content_texts(message.get("content")).join("\n"),
                },
                Some("aborted") => Turn::Cancelled,
                Some("error") => Turn::Failed {
                    text: text_at(message, "/errorMessage"),
                },
                _ => {
                    self.turn = Some(Turn::Open);
                    self.open.get_or_insert(span.start);
                    return;
                }
            },
            _ => return,
        };
        self.turn = Some(ended.clone());
        sealed.push(Sealed {
            turn: ended,
            span: Span {
                start: self.open.take().unwrap_or(span.start),
                end: span.end,
            },
            took: Vec::new(),
        });
    }

    fn start(&mut self, span: Span) {
        self.turn = Some(Turn::Open);
        self.open = Some(span.start);
    }

    pub fn turn(&self) -> Turn {
        self.turn.clone().unwrap_or(Turn::None)
    }
}

#[cfg(test)]
mod tests;
