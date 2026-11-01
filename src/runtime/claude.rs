//! Claude Code.

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

/// Claude Code takes the model and effort of a resumed session from the command line; its session
/// start leaves them out on resume, so the reported launch settings are what reopen it. Its resume
/// starts a session that was bypassing permissions in a new session's mode, and a Manual one in a
/// settings `defaultMode`, so the mode the record shows is passed too. Launch flags go before
/// `--resume`, so a continuation prompt the gate appends stays positional after an optional or
/// variadic value.
pub fn resume_argv(c: &Conversation) -> Vec<String> {
    let mut argv = vec!["claude"];
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

/// The provider variables Claude Code was started with, saved as `NAME=value`.
pub fn resume_env(c: &Conversation) -> Vec<(String, String)> {
    c.setting_strings("launch_env")
        .filter_map(|entry| entry.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
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

/// The file this tool keeps a Claude Code conversation's turns in, written by its turn hooks:
/// Claude Code's transcript names no turn, so it does not show which records end one, while the
/// hooks name the prompt whose turn they report and give its final answer, which the transcript
/// may not hold yet when the turn ends.
pub fn record(state_dir: &Path, native_id: &str) -> PathBuf {
    records(state_dir).join(format!("{native_id}.jsonl"))
}

/// The directory of the turn records.
pub fn records(state_dir: &Path) -> PathBuf {
    state_dir.join("claude")
}

/// The line a turn hook adds to the conversation's record: `prompt` when Claude Code takes input,
/// typed or from another session, which while a turn runs carries that turn's prompt ID; `stop`
/// when it has finished responding, with its final answer; `failure` when an API error ended the
/// turn. `None` for any other event and for a subagent's. A prompt that starts with a notice's or
/// request's token keeps the token, and no other text.
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

/// The token a notice or request starts with (`runtime::token`).
fn leading_token(prompt: &str) -> Option<&str> {
    let id = prompt.strip_prefix("[rexume ")?;
    let end = id
        .find(']')
        .filter(|&end| end > 0 && !id[..end].contains(char::is_whitespace))?;
    Some(&prompt[.."[rexume ".len() + end + 1])
}

/// Appends a turn hook's line to the conversation's record. Each line goes in one write to a file
/// opened for appending, so a reader sees it whole or not yet, as with a runtime's own record.
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

/// Claude Code's turns as its hooks record them. A prompt with a new ID starts a turn, and one
/// with the running turn's ID is input that turn takes in. Each `stop` ends a turn with its
/// answer: a Stop hook that makes Claude Code go on after it finished responding records another
/// `stop` once it finishes again, which ends a turn of its own. A `failure` ends a turn with the
/// error. A prompt that nothing ended when the next one starts was interrupted, or rejected by
/// another `UserPromptSubmit` hook, which the hooks do not tell apart: it ends as cancelled, and
/// the input it carried counts as not received. A turn the server's end cut off records no end;
/// the continuation prompt a restore sends it (`continuation_prompt`) carries on that turn, with
/// what it took in, under the new prompt's ID.
#[derive(Default)]
pub struct Parser {
    turn: Option<Turn>,
    open: Option<Open>,
}

/// The turn not yet yielded.
struct Open {
    prompt_id: Option<String>,
    start: u64,
    /// The notice and request tokens of the prompts it took in.
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

    /// Yields the open turn, if any, as `turn`, ending at `end`.
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
