//! What this tool knows about each coding-agent runtime: how to reopen a conversation, how to read
//! its turns from the conversation's own record, how to pass it a notice, and which record shows
//! it took the notice in. A new runtime is a variant here and a file beside this one.

pub mod claude;
mod codex;
mod lines;
pub mod pi;
mod receipt;
#[cfg(test)]
mod tests;

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Result, bail};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use serde_json::{Map, Value};

pub use lines::{Lines, Taken, Tracker, last_turn};
use lines::{Open, Sealed, Span};
pub use receipt::Receipt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Runtime {
    /// Claude Code
    Claude,
    /// Codex
    Codex,
    /// Pi
    Pi,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }

    /// The command that reopens a conversation. Its identity, settings, and an allowlist of launch
    /// flags and provider variables are carried over. Original arguments are never replayed
    /// wholesale: those can hold a launch prompt that would run a finished task again.
    pub fn resume_argv(self, conversation: &Conversation) -> Vec<String> {
        match self {
            Self::Claude => claude::resume_argv(conversation),
            Self::Codex => codex::resume_argv(conversation),
            Self::Pi => pi::resume_argv(conversation),
        }
    }

    /// Whether the conversation can be reopened. Claude Code and Codex write a conversation's
    /// record only with its first prompt and refuse to resume one without it; Pi opens a session
    /// file that does not exist yet as a new session.
    pub fn can_reopen(self, conversation: &Conversation) -> bool {
        match self {
            Self::Claude | Self::Codex => conversation
                .transcript
                .as_deref()
                .is_none_or(|transcript| Path::new(transcript).exists()),
            Self::Pi => true,
        }
    }

    /// The settings a reopened conversation gets, from the runtime's own record where it keeps
    /// them, over the ones its session start reported.
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
        }
    }

    /// Hands `text`, a notice or a request with the ID `id`, to the conversation. An error means
    /// nothing was sent and it can be sent later.
    pub fn send(self, target: &Target, state_dir: &Path, id: &str, text: &str) -> Result<Sent> {
        match self {
            Self::Claude => claude::send(target, text),
            Self::Codex => codex::send(target, text),
            Self::Pi => pi::send(target, state_dir, id, text),
        }
    }

    /// Whether a record is the conversation receiving a notice or request as input, not merely
    /// text that mentions one; the token in the record tells which it was.
    fn received(self, record: &Value) -> bool {
        match self {
            Self::Claude => claude::received(record),
            Self::Codex => codex::received(record),
            Self::Pi => pi::received(record),
        }
    }

    fn parser(self) -> Parser {
        match self {
            Self::Claude => Parser::Claude(claude::Parser::default()),
            Self::Codex => Parser::Codex(codex::Parser::default()),
            Self::Pi => Parser::Pi(pi::Parser::default()),
        }
    }
}

impl FromStr for Runtime {
    type Err = anyhow::Error;

    fn from_str(name: &str) -> Result<Self> {
        Ok(match name {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "pi" => Self::Pi,
            _ => bail!("unknown runtime {name}"),
        })
    }
}

impl FromSql for Runtime {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|e: anyhow::Error| FromSqlError::Other(e.into()))
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A conversation and the settings that reopen it.
#[derive(Clone, Debug, Default)]
pub struct Conversation {
    pub native_id: String,
    pub transcript: Option<String>,
    /// The runtime module's settings (`conversation.settings`): model, effort, and whatever else
    /// its resume command takes.
    pub settings: Map<String, Value>,
}

impl Conversation {
    fn setting(&self, key: &str) -> Option<&str> {
        self.settings.get(key).and_then(Value::as_str)
    }

    fn setting_array(&self, key: &str) -> Option<&Vec<Value>> {
        self.settings.get(key).and_then(Value::as_array)
    }

    fn setting_object(&self, key: &str) -> Option<&Map<String, Value>> {
        self.settings.get(key).and_then(Value::as_object)
    }
}

/// Where a notice or request goes.
pub struct Target<'a> {
    pub native_id: &'a str,
    /// The occupant's `transport` object.
    pub transport: &'a Map<String, Value>,
}

#[derive(Debug)]
pub enum Sent {
    /// Handed over; nothing confirms the conversation has it yet.
    Handed,
    /// The runtime confirmed it holds the message for the conversation, with what it said.
    Accepted(String),
    /// The send may or may not have happened; it must not be repeated.
    Unknown(String),
}

impl Sent {
    /// The outbox state and note a send leaves. An error means nothing left, so the message goes
    /// back to `pending` to be sent again.
    pub fn recorded(sent: Result<Self>) -> (&'static str, Option<String>) {
        match sent {
            Ok(Self::Handed) => ("sent", None),
            Ok(Self::Accepted(receipt)) => ("accepted", Some(receipt)),
            Ok(Self::Unknown(why)) => ("unknown", Some(why)),
            Err(e) => ("pending", Some(format!("{e:#}"))),
        }
    }
}

/// The text that identifies a notice or request in the receiving conversation's record.
pub fn token(id: &str) -> String {
    format!("[rexume {id}]")
}

/// The one prompt this tool sends to a conversation whose turn the server's end cut off. The
/// reference lets the conversation's own record show that this prompt, and no other, was sent.
pub fn continuation_prompt(reference: &str, tasks: &[String]) -> String {
    let mut prompt = format!(
        "[rexume {reference}] The Rex terminal server stopped while your previous turn was still \
         running, so that turn was cut off. First check whether the last actions you started took \
         effect, then continue the unfinished work. Do not repeat steps that already completed."
    );
    if !tasks.is_empty() {
        prompt.push_str(" Worker tasks you registered, as recorded now:");
        for task in tasks {
            prompt.push_str("\n- ");
            prompt.push_str(task);
        }
    }
    prompt
}

/// How the last turn in a conversation's record stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn {
    /// No turn after the offset read from.
    None,
    /// A prompt was accepted and nothing ended its turn: cut off, or still running. `since` is when
    /// the prompt was recorded, in Unix milliseconds, for a runtime that records nothing when its
    /// turn is stopped before any output (Claude Code); its program going idle after that time is
    /// what ends the turn.
    Open { since: Option<i64> },
    /// The turn ended with an answer.
    Completed { reference: String, text: String },
    /// The user stopped the turn.
    Cancelled,
    /// The runtime ended the turn with an error.
    Failed { text: String },
}

impl Turn {
    /// How a turn that is not open stands, for a person reading why it was not continued.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::None => "the record shows no turn",
            Self::Open { .. } => "the record shows a turn nothing ended",
            Self::Completed { .. } => "the record shows the turn finished",
            Self::Cancelled => "the record shows the turn was cancelled",
            Self::Failed { .. } => "the record shows the turn failed",
        }
    }
}

enum Parser {
    Claude(claude::Parser),
    Codex(codex::Parser),
    Pi(pi::Parser),
}

/// Each runtime's parser keeps how the last turn stands and yields each turn as it finishes. A
/// turn that a new one starts before anything ended it was cut off, and is never yielded.
impl Parser {
    fn feed(&mut self, record: &Value, span: Span, sealed: &mut Vec<Sealed>) {
        match self {
            Self::Claude(p) => p.feed(record, span, sealed),
            Self::Codex(p) => p.feed(record, span, sealed),
            Self::Pi(p) => p.feed(record, span, sealed),
        }
    }

    fn open(&self) -> Option<Open> {
        match self {
            Self::Claude(p) => p.open(),
            Self::Codex(p) => p.open(),
            Self::Pi(p) => p.open(),
        }
    }

    fn turn(&self) -> Turn {
        match self {
            Self::Claude(p) => p.turn(),
            Self::Codex(p) => p.turn(),
            Self::Pi(p) => p.turn(),
        }
    }
}

fn mentions(record: &Value, needle: &str) -> bool {
    match record {
        Value::String(text) => text.contains(needle),
        Value::Array(items) => items.iter().any(|v| mentions(v, needle)),
        Value::Object(map) => map.values().any(|v| mentions(v, needle)),
        _ => false,
    }
}

/// The texts of a message's content, whether a plain string or a list of typed parts.
fn content_texts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|item| item.get("text").and_then(Value::as_str).map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn text_at(value: &Value, pointer: &str) -> String {
    crate::rex::str_at(value, pointer).unwrap_or("").to_owned()
}
