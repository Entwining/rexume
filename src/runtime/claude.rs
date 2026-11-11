pub mod launch;

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::lines::last_record;
use super::{Conversation, Sealed, Sent, Span, Target, Turn, text_at};
use crate::store::new_id;

pub const PLUGIN: &str = include_str!("claude/plugin.ts");

/// Claude Code's resume brings back neither the effort nor, on a provider with its own deployment
/// IDs, the model, and starts a session that was bypassing permissions in a new session's mode and
/// a Manual one in the settings' `defaultMode`, so all three are passed. Launch flags go before
/// `--resume` so that a continuation prompt the gate appends stays positional after a flag whose
/// value is optional or variadic.
pub fn resume_argv(c: &Conversation) -> Vec<String> {
    let mut argv: Vec<&str> = c.setting_strings("launch_command").collect();
    if argv.is_empty() {
        argv.push("claude");
    }
    argv.extend(c.setting_strings("launch_flags"));
    argv.extend(["--resume", &c.native_id]);
    for (key, flag) in [
        ("model", "--model"),
        ("effort", "--effort"),
        ("permission_mode", "--permission-mode"),
    ] {
        if let Some(value) = c.setting(key) {
            argv.extend([flag, value]);
        }
    }
    argv.into_iter().map(str::to_owned).collect()
}

pub fn resume_env(c: &Conversation) -> Vec<(String, String)> {
    c.setting_strings("launch_env")
        .filter_map(|entry| entry.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// `requestedModel` keeps a context-window suffix such as `[1m]` that the response's model drops.
/// A turn that invoked a skill (`attributionSkill`) may run at the model and effort the skill sets
/// for that turn alone. Claude Code writes `permissionMode` on each prompt and in a
/// `permission-mode` entry when a turn ends and when the session exits.
pub fn settings(transcript: &Path, conversation: &mut Conversation) -> Result<()> {
    let main_chain =
        |record: &Value| record.get("isSidechain").and_then(Value::as_bool) != Some(true);
    let used = last_record(transcript, "\"type\":\"assistant\"", |record| {
        if record.get("type").and_then(Value::as_str) != Some("assistant")
            || !main_chain(record)
            || record.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>")
            || record.get("attributionSkill").is_some()
        {
            return None;
        }
        let text = |field: &str| record.get(field).and_then(Value::as_str).map(str::to_owned);
        Some([
            ("model", text("requestedModel")),
            ("effort", text("effort")),
        ])
    })?;
    for (key, value) in used.into_iter().flatten() {
        if let Some(value) = value {
            conversation.settings.insert(key.to_owned(), value.into());
        }
    }
    let mode = last_record(transcript, "\"permissionMode\"", |record| {
        if !main_chain(record) {
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

/// Claude Code starts a turn with a line on its session socket when idle, and answers nothing.
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

/// Turns are read from this record the turn hooks write, since Claude Code's transcript shows no
/// turn's end and may not hold its final answer yet when the turn ends.
pub fn record(state_dir: &Path, native_id: &str) -> PathBuf {
    records(state_dir).join(format!("{native_id}.jsonl"))
}

pub fn records(state_dir: &Path) -> PathBuf {
    state_dir.join("claude")
}

pub fn turn_event(input: &Value) -> Option<Value> {
    if input.get("agent_id").is_some_and(|v| !v.is_null()) {
        return None;
    }
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    let prompt_id = text("prompt_id");
    Some(match text("hook_event_name")? {
        "UserPromptSubmit" => json!({
            "event": "prompt",
            "prompt_id": prompt_id,
            "token": text("prompt").and_then(leading_token),
        }),
        "Stop" => json!({
            "event": "stop",
            "prompt_id": prompt_id,
            "id": new_id(),
            "answer": text("last_assistant_message").unwrap_or(""),
        }),
        "StopFailure" => json!({
            "event": "failure",
            "prompt_id": prompt_id,
            "text": text("last_assistant_message").or_else(|| text("error")).unwrap_or(""),
        }),
        _ => return None,
    })
}

fn leading_token(prompt: &str) -> Option<&str> {
    let id = prompt.strip_prefix("[rexume ")?;
    let end = id
        .find(']')
        .filter(|&end| end > 0 && !id[..end].contains(char::is_whitespace))?;
    Some(&prompt[.."[rexume ".len() + end + 1])
}

/// The line goes in one write to a file opened for appending, so that a reader sees it whole or
/// not yet.
pub fn append(state_dir: &Path, native_id: &str, event: &Value) -> Result<()> {
    if native_id.is_empty() || native_id.starts_with('.') || native_id.contains('/') {
        bail!("`{native_id}` cannot name a record");
    }
    crate::store::create_private_dir(&records(state_dir))?;
    let path = record(state_dir, native_id);
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(&line))
        .with_context(|| format!("failed to append to `{}`", path.display()))
}

/// Input Claude Code takes while a turn runs, typed or from another session, carries that turn's
/// prompt ID. A Stop hook that makes Claude Code go on after it finished responding records another
/// `stop` once it finishes again. An interrupted prompt and one another `UserPromptSubmit` hook
/// rejected both record no end, so a prompt nothing ended when the next one starts is cancelled. A
/// turn the server's end cut off records no end either; the continuation prompt a restore sends
/// carries it on under the new prompt's ID.
#[derive(Default)]
pub struct Parser {
    turn: Option<Turn>,
    open: Option<Open>,
}

struct Open {
    prompt_id: Option<String>,
    start: u64,
    took: Vec<String>,
}

impl Parser {
    pub fn feed(&mut self, record: &Value, span: Span, sealed: &mut Vec<Sealed>) {
        let ended = match record.get("event").and_then(Value::as_str) {
            Some("prompt") => {
                let id = record.get("prompt_id").and_then(Value::as_str);
                let token = record.get("token").and_then(Value::as_str);
                match &mut self.open {
                    Some(open) if open.prompt_id.is_some() && open.prompt_id.as_deref() == id => {
                        open.took.extend(token.map(str::to_owned));
                    }
                    Some(open) if token.is_some_and(super::is_continuation) => {
                        open.prompt_id = id.map(str::to_owned);
                    }
                    _ => {
                        self.seal(Turn::Cancelled, span.start, sealed);
                        self.open = Some(Open {
                            prompt_id: id.map(str::to_owned),
                            start: span.start,
                            took: token.map(str::to_owned).into_iter().collect(),
                        });
                    }
                }
                self.turn = Some(Turn::Open);
                return;
            }
            Some("stop") => Turn::Completed {
                reference: text_at(record, "/id"),
                text: text_at(record, "/answer"),
            },
            Some("failure") => Turn::Failed {
                text: text_at(record, "/text"),
            },
            _ => return,
        };
        if self.open.is_none() {
            self.open = Some(Open {
                prompt_id: None,
                start: span.start,
                took: Vec::new(),
            });
        }
        self.seal(ended.clone(), span.end, sealed);
        self.turn = Some(ended);
    }

    fn seal(&mut self, turn: Turn, end: u64, sealed: &mut Vec<Sealed>) {
        if let Some(open) = self.open.take() {
            sealed.push(Sealed {
                turn,
                span: Span {
                    start: open.start,
                    end,
                },
                took: open.took,
            });
        }
    }

    pub fn turn(&self) -> Turn {
        self.turn.clone().unwrap_or(Turn::None)
    }
}

#[cfg(test)]
mod tests;
