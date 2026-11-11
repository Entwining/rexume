mod pipe;
pub mod relay;

use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

pub use pipe::Pipe;

const ADAPTER: &str = include_str!("adapter.lua");

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("failed to start `rex`: {0}")]
    Unsent(std::io::Error),
    /// The server refused the request and changed nothing.
    #[error("the server refused it ({code}): {message}")]
    Rejected { code: String, message: String },
    /// No server answer: a mutating call may or may not have taken effect.
    #[error("outcome unknown: {0}")]
    Unknown(String),
}

impl CallError {
    pub fn not_found(&self) -> bool {
        matches!(self, Self::Rejected { code, .. } if code == "not_found" || code == "session_not_found")
    }

    pub fn reason(self) -> String {
        match self {
            Self::Rejected { message, .. } | Self::Unknown(message) => message,
            unsent @ Self::Unsent(_) => unsent.to_string(),
        }
    }
}

/// The `rex` CLI on stderr and the Lua API in its error string both report a refusal as
/// `... (status=404 code=not_found)`.
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
        self.block_method_with(session, block, method, &json!({}))
    }

    pub fn block_method_with(
        &mut self,
        session: &str,
        block: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, CallError> {
        self.call(
            "block.method",
            &json!({ "session_id": session, "block_id": block, "method": method, "args": args }),
        )
    }

    pub fn block_session(&mut self, block: &str) -> Result<Option<String>> {
        let reply = self.call(
            "target.resolve",
            &json!({ "kinds": ["block"], "queries": [block] }),
        )?;
        resolved_session(&reply, block)
    }
}

/// An ambiguous query, such as a label several panes share, carries no target; Rex's schema asks
/// clients to treat a status they don't know as `not_found`.
fn resolved_session(reply: &Value, block: &str) -> Result<Option<String>> {
    match require_str(reply, "/results/0/status")?.as_str() {
        "resolved" => require_str(reply, "/results/0/target/session_id").map(Some),
        "ambiguous" => {
            let reason =
                str_at(reply, "/results/0/reason").map_or(String::new(), |r| format!(": {r}"));
            bail!("Rex finds {block:?} ambiguous{reason}, not one pane's block ID")
        }
        _ => Ok(None),
    }
}

#[derive(Clone)]
pub struct Cli {
    pub bin: PathBuf,
    pub endpoint: String,
}

impl Cli {
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

pub fn time_at(value: &Value, pointer: &str) -> Option<i64> {
    let time = chrono::DateTime::parse_from_rfc3339(str_at(value, pointer)?).ok()?;
    Some(time.timestamp_millis())
}

pub fn require_str(value: &Value, pointer: &str) -> Result<String> {
    str_at(value, pointer)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("reply has no {pointer}"))
}

/// Lua encodes an empty list as an empty object and drops nulls, so anything that is not a list
/// holds no items.
pub fn items<'a>(value: &'a Value, pointer: &str) -> &'a [Value] {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// `session.list_blocks` lists detached blocks too, which `session.view` leaves out.
pub fn listed_blocks(list: &Value) -> Vec<String> {
    items(list, "/blocks")
        .iter()
        .filter_map(|block| str_at(block, "/block_id").map(str::to_owned))
        .collect()
}

pub fn root_status(status: &Value) -> Option<&Value> {
    items(status, "/records").iter().find(|r| is_root(r))
}

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
        assert!(rejected.not_found());
        let gone = classify(
            "Error: stream response \"control-1\" failed: session session:x not found (status=404 code=session_not_found)\n",
        );
        assert!(gone.not_found());
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
    fn an_ambiguous_pane_is_refused_with_its_reason_and_an_unknown_status_is_not_found() {
        let ambiguous = json!({ "results": [{
            "query": " ", "status": "ambiguous", "reason": "multiple sessions are available",
            "candidates": [{ "kind": "session", "session_id": "session:s" }],
        }] });
        let blank = resolved_session(&ambiguous, " ").unwrap_err().to_string();
        assert!(
            blank.contains("ambiguous: multiple sessions are available"),
            "{blank}"
        );
        let unknown =
            json!({ "results": [{ "status": "pending", "target": { "session_id": "s" } }] });
        assert_eq!(resolved_session(&unknown, "block:b").unwrap(), None);
    }

    #[test]
    fn reads_lua_lists_leniently() {
        let reply = json!({ "windows": {}, "sessions": [{ "session_id": "s" }] });
        assert!(items(&reply, "/windows").is_empty());
        assert!(items(&reply, "/absent").is_empty());
        assert_eq!(items(&reply, "/sessions").len(), 1);
        assert!(listed_blocks(&json!({ "blocks": {} })).is_empty());
        let list = json!({ "blocks": [{ "block_id": "block:a", "placement": "detached" }, {}] });
        assert_eq!(listed_blocks(&list), ["block:a"]);
    }
}
