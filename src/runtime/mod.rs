pub mod claude;
mod codex;
pub mod inbox;
mod lines;
pub mod omp;
pub mod pi;
mod receipt;
mod reopen;
#[cfg(test)]
pub(crate) mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Result, bail};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use serde_json::{Map, Value};

pub use lines::{Lines, Taken, Tracker, last_turn};
use lines::{Sealed, Span};
pub use receipt::Receipt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Runtime {
    /// Claude Code
    Claude,
    /// Codex
    Codex,
    /// Pi
    Pi,
    /// oh-my-pi
    Omp,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Omp => "omp",
        }
    }

    pub fn record(
        self,
        state_dir: &Path,
        native_id: &str,
        transcript: Option<&str>,
    ) -> Option<PathBuf> {
        match self {
            Self::Claude => Some(claude::record(state_dir, native_id)),
            Self::Omp => Some(omp::record(state_dir, native_id)),
            Self::Codex | Self::Pi => transcript.map(PathBuf::from),
        }
    }

    pub fn records(self, state_dir: &Path) -> Option<PathBuf> {
        match self {
            Self::Claude => Some(claude::records(state_dir)),
            Self::Omp => Some(omp::records(state_dir)),
            Self::Codex | Self::Pi => None,
        }
    }

    pub fn inbox(self, state_dir: &Path, native_id: &str) -> Option<PathBuf> {
        match self {
            Self::Pi => Some(pi::inbox(state_dir, native_id)),
            Self::Omp => Some(omp::inbox(state_dir, native_id)),
            Self::Claude | Self::Codex => None,
        }
    }

    /// An error means nothing was sent, so it can be sent again.
    pub fn send(self, target: &Target, state_dir: &Path, id: &str, text: &str) -> Result<Sent> {
        match self {
            Self::Claude => claude::send(target, text),
            Self::Codex => codex::send(target, text),
            Self::Pi => inbox::send(&pi::inbox(state_dir, target.native_id), id, text),
            Self::Omp => inbox::send(&omp::inbox(state_dir, target.native_id), id, text),
        }
    }

    /// Claude Code's hooks see input before another hook can reject it, so its record shows input
    /// received only with the turn that took it in (`Sealed::received`).
    fn received(self, record: &Value) -> bool {
        match self {
            Self::Claude => false,
            Self::Codex => codex::received(record),
            Self::Pi => pi::received(record),
            Self::Omp => omp::received(record),
        }
    }

    /// Whether a turn the record shows ended may still be running. A Stop hook that blocks makes
    /// Claude Code go on after its `Stop`, which the record shows only once it stops again.
    pub fn works_after_end(self) -> bool {
        self == Self::Claude
    }

    fn parser(self) -> Parser {
        match self {
            Self::Claude => Parser::Claude(claude::Parser::default()),
            Self::Codex => Parser::Codex(codex::Parser::default()),
            Self::Pi => Parser::Pi(pi::Parser::default()),
            Self::Omp => Parser::Omp(omp::Parser::default()),
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
            "omp" => Self::Omp,
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

#[derive(Clone, Debug, Default)]
pub struct Conversation {
    pub native_id: String,
    pub transcript: Option<String>,
    pub settings: Map<String, Value>,
}

impl Conversation {
    fn setting(&self, key: &str) -> Option<&str> {
        self.settings.get(key).and_then(Value::as_str)
    }

    fn setting_strings(&self, key: &str) -> impl Iterator<Item = &str> {
        self.settings
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
    }
}

pub struct Target<'a> {
    pub native_id: &'a str,
    pub transport: &'a Map<String, Value>,
}

#[derive(Debug)]
pub enum Sent {
    Handed,
    Accepted(String),
    Unknown(String),
}

impl Sent {
    pub fn recorded(sent: Result<Self>) -> (&'static str, Option<String>) {
        match sent {
            Ok(Self::Handed) => ("sent", None),
            Ok(Self::Accepted(receipt)) => ("accepted", Some(receipt)),
            Ok(Self::Unknown(why)) => ("unknown", Some(why)),
            Err(e) => ("pending", Some(format!("{e:#}"))),
        }
    }
}

pub fn token(id: &str) -> String {
    format!("[rexume {id}]")
}

pub fn continuation_prompt(instance: &str, block: &str, tasks: &[String]) -> String {
    let mut prompt = format!(
        "[rexume {instance}/{block}] The Rex terminal server stopped while your previous turn was still \
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

/// A continuation prompt's token holds a `/`, which no notice or request ID, a UUID, does.
fn is_continuation(token: &str) -> bool {
    token.contains('/')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn {
    None,
    Open,
    Completed { reference: String, text: String },
    Cancelled,
    Failed { text: String },
}

impl Turn {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::None => "the record shows no turn",
            Self::Open => "the record shows a turn nothing ended",
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
    Omp(omp::Parser),
}

impl Parser {
    fn feed(&mut self, record: &Value, span: Span, sealed: &mut Vec<Sealed>) {
        match self {
            Self::Claude(p) => p.feed(record, span, sealed),
            Self::Codex(p) => p.feed(record, span, sealed),
            Self::Pi(p) => p.feed(record, span, sealed),
            Self::Omp(p) => p.feed(record, span, sealed),
        }
    }

    fn turn(&self) -> Turn {
        match self {
            Self::Claude(p) => p.turn(),
            Self::Codex(p) => p.turn(),
            Self::Pi(p) => p.turn(),
            Self::Omp(p) => p.turn(),
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
