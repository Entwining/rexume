//! Talks to a Rex server. The daemon holds one long-running connection, a Lua script run by
//! `rex do` that passes calls and events through `rexume relay` (`Pipe`); short commands use one
//! `rex api call` per request (`Cli`). Neither can start a server.

mod pipe;
pub mod relay;

use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

pub use pipe::Pipe;

/// The script `rex do` runs for the daemon.
const ADAPTER: &str = include_str!("adapter.lua");

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// `rex` could not be started, so nothing was sent.
    #[error("failed to start `rex`: {0}")]
    Unsent(std::io::Error),
    /// The server refused the request, so it changed nothing.
    #[error("the server refused it ({code}): {message}")]
    Rejected { code: String, message: String },
    /// The call failed without a server answer; a mutating call may or may not have taken effect.
    #[error("outcome unknown: {0}")]
    Unknown(String),
}

impl CallError {
    pub fn not_found(&self) -> bool {
        matches!(self, Self::Rejected { code, .. } if code == "not_found")
    }

    /// What a note says about the failure: the server's own message, or why nothing was sent.
    pub fn reason(self) -> String {
        match self {
            Self::Rejected { message, .. } | Self::Unknown(message) => message,
            unsent @ Self::Unsent(_) => unsent.to_string(),
        }
    }
}

/// Both report a server's refusal as `... (status=404 code=not_found)`, the CLI on stderr and the
/// Lua API in its error string; anything else, such as a lost connection, leaves the outcome
/// unknown.
fn classify(error: &str) -> CallError {
    let message = error.trim().to_owned();
    match message.split("code=").nth(1) {
        Some(rest) => {
            let code = rest
                .split(|c: char| c == ')' || c.is_whitespace())
                .next()
                .unwrap_or("")
                .to_owned();
            CallError::Rejected { code, message }
        }
        None => CallError::Unknown(message),
    }
}

pub enum Rex {
    Pipe(Pipe),
    Cli(Cli),
}

impl Rex {
    pub fn call(&mut self, method: &str, params: &Value) -> Result<Value, CallError> {
        match self {
            Self::Pipe(pipe) => pipe.call(method, params),
            Self::Cli(cli) => cli.call(method, params),
        }
    }

    pub fn block_method(
        &mut self,
        session: &str,
        block: &str,
        method: &str,
    ) -> Result<Value, CallError> {
        self.call(
            "block.method",
            &json!({ "session_id": session, "block_id": block, "method": method }),
        )
    }

    /// The session holding a pane, or `None` when the server has no such pane.
    pub fn block_session(&mut self, block: &str) -> Result<Option<String>> {
        let reply = self.call(
            "target.resolve",
            &json!({ "kinds": ["block"], "queries": [block] }),
        )?;
        if str_at(&reply, "/results/0/status") == Some("not_found") {
            return Ok(None);
        }
        require_str(&reply, "/results/0/target/session_id").map(Some)
    }
}

/// The `rex` executable and the server it talks to.
#[derive(Clone)]
pub struct Cli {
    pub bin: PathBuf,
    pub endpoint: String,
}

impl Cli {
    /// The server a pane belongs to, as Rex sets it in the pane's environment.
    pub fn from_env() -> Result<Self> {
        let endpoint = std::env::var_os("REX_SERVER").context("REX_SERVER is not set")?;
        Ok(Self {
            bin: PathBuf::from("rex"),
            endpoint: endpoint.to_string_lossy().into_owned(),
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.bin);
        command
            .arg("--autostart=false")
            .arg("-S")
            .arg(&self.endpoint);
        command
    }

    /// The socket file of a `unix://` endpoint, to check for a server without connecting.
    pub fn socket_path(&self) -> Option<PathBuf> {
        let path = self.endpoint.strip_prefix("unix://")?;
        let path = percent_encoding::percent_decode_str(path).collect::<Vec<u8>>();
        Some(PathBuf::from(std::ffi::OsString::from_vec(path)))
    }

    pub fn call(&self, method: &str, params: &Value) -> Result<Value, CallError> {
        let mut child = self
            .command()
            .args(["api", "call", method, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(CallError::Unsent)?;
        let write = child.stdin.take().map_or(Ok(()), |mut stdin| {
            stdin.write_all(params.to_string().as_bytes())
        });
        let output = child
            .wait_with_output()
            .map_err(|e| CallError::Unknown(e.to_string()))?;
        write.map_err(|e| CallError::Unknown(e.to_string()))?;
        if output.status.success() {
            return serde_json::from_slice(&output.stdout)
                .map_err(|e| CallError::Unknown(format!("the reply is not JSON: {e}")));
        }
        Err(classify(&String::from_utf8_lossy(&output.stderr)))
    }

    /// Starts the script that connects the daemon: it runs `rexume relay` back to `socket` with
    /// `token` as its first line, and passes calls and events through it.
    pub fn adapter(&self, relay: &[String], token: &str) -> std::io::Result<Child> {
        let args = json!({ "relay": relay, "token": token });
        self.command()
            .args(["do", "--all", "--args", &args.to_string(), "-e", ADAPTER])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
    }
}

pub fn str_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

/// A time in a reply, which Rex writes in RFC 3339, as Unix milliseconds.
pub fn time_at(value: &Value, pointer: &str) -> Option<i64> {
    let time = chrono::DateTime::parse_from_rfc3339(str_at(value, pointer)?).ok()?;
    Some(time.timestamp_millis())
}

pub fn require_str(value: &Value, pointer: &str) -> Result<String> {
    str_at(value, pointer)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("reply has no {pointer}"))
}

/// The items of a list in a reply. Lua encodes an empty list as an empty object and drops nulls,
/// so anything that is not a list holds no items.
pub fn items<'a>(value: &'a Value, pointer: &str) -> &'a [Value] {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The record a `program_status` reply holds for the pane's program itself, which has no ID.
pub fn root_status(status: &Value) -> Option<&Value> {
    items(status, "/records").iter().find(|r| is_root(r))
}

/// Whether a program status record, or an event about one, is the pane's program itself.
pub fn is_root(record: &Value) -> bool {
    str_at(record, "/id").is_none_or(str::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_server_answers_apart_from_lost_calls() {
        let rejected = classify(
            "Error: stream response \"control-2\" failed: block not found: block:x (status=404 code=not_found)\n",
        );
        assert!(matches!(rejected, CallError::Rejected { ref code, .. } if code == "not_found"));
        let unknown = classify("Error: server went away: reading API control: EOF\n");
        assert!(matches!(unknown, CallError::Unknown(_)));
    }

    #[test]
    fn decodes_the_socket_path() {
        let cli = Cli {
            bin: "rex".into(),
            endpoint: "unix:///Users/me/Library/Application%20Support/rex/server.sock".into(),
        };
        assert_eq!(
            cli.socket_path().unwrap(),
            PathBuf::from("/Users/me/Library/Application Support/rex/server.sock")
        );
    }

    #[test]
    fn reads_lua_lists_leniently() {
        let reply = json!({ "windows": {}, "sessions": [{ "session_id": "s" }] });
        assert!(items(&reply, "/windows").is_empty());
        assert!(items(&reply, "/absent").is_empty());
        assert_eq!(items(&reply, "/sessions").len(), 1);
    }
}
