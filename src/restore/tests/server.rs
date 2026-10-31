//! A fake server that answers the calls a restore makes from what a test sets.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{Value, json};

use crate::rex::{Pipe, Rex};

/// What the fake server's panes show, which a test changes between passes, and what was done to
/// them.
#[derive(Default)]
pub(super) struct Server {
    /// The `process` reply, by pane.
    pub(super) process: HashMap<String, Value>,
    /// The screen as text and as escape sequences, if not `IDLE`.
    pub(super) screen: Option<(&'static str, &'static str)>,
    /// Whether the shell still reads its startup files, with the terminal in canonical mode.
    pub(super) startup: bool,
    /// The error a write gets, if any.
    pub(super) refuse_write: Option<&'static str>,
    pub(super) written: Vec<(String, String)>,
    pub(super) closed: Vec<String>,
    /// The session's focused pane, if any.
    pub(super) focused: Option<&'static str>,
    /// A pane method the server refuses, if any.
    pub(super) refuse: Option<&'static str>,
}

pub(super) const SHELL: &str =
    r#"{"foreground":{"pid":2,"argv0":"-zsh","name":"zsh"},"ancestors":["login"]}"#;
pub(super) const STARTUP: &str =
    r#"{"foreground":{"pid":3,"argv0":"sleep","name":"sleep"},"ancestors":["zsh","login"]}"#;
const IDLE: (&str, &str) = ("p% ", "p% \u{1b}[1;4H");
pub(super) const TYPED: (&str, &str) = ("p% ls", "p% ls\u{1b}[1;6H");
/// What `TYPED` shows once the cursor is moved back to the start of the text.
pub(super) const MOVED_BACK: (&str, &str) = ("p% ls", "p% ls\u{1b}[1;4H");

fn answer(server: &Mutex<Server>, method: &str, params: &Value) -> Result<Value, String> {
    let mut server = server.lock().unwrap();
    let block = params["block_id"].as_str().unwrap_or_default().to_owned();
    let shown = |block: &str, server: &Server| {
        server.process.contains_key(block) && !server.closed.iter().any(|c| c == block)
    };
    Ok(match method {
        "session.list" => json!({ "sessions": [{ "session_id": "session:1", "label": "work" }] }),
        "session.view" => {
            let blocks: Vec<Value> = server
                .process
                .keys()
                .filter(|b| shown(b, &server))
                .map(|b| json!({ "block_id": b }))
                .collect();
            json!({ "active_window_id": "window:1", "windows": [{ "window_id": "window:1",
                "focused_block_id": server.focused, "layers": [{ "blocks": blocks }] }] })
        }
        "target.resolve" => match params["queries"][0].as_str() {
            Some(b) if shown(b, &server) => {
                json!({ "results": [{ "status": "ok", "target": { "session_id": "session:1" } }] })
            }
            _ => json!({ "results": [{ "status": "not_found" }] }),
        },
        "block.close" => {
            server.closed.push(block);
            json!({})
        }
        "block.method" if params["method"].as_str() == server.refuse => {
            return Err("refused (status=500 code=internal)".to_owned());
        }
        "block.method" => match params["method"].as_str() {
            Some("process") => server.process[&block].clone(),
            Some("input_mode") => json!({ "echo": server.startup, "canonical": server.startup }),
            Some("size") => json!({ "columns": 80 }),
            Some("format") => {
                let (text, vt) = server.screen.unwrap_or(IDLE);
                let vt_wanted = params["args"]["format"] == "vt";
                json!({ "content": if vt_wanted { vt } else { text } })
            }
            Some("write") => {
                if let Some(error) = server.refuse_write {
                    return Err(error.to_owned());
                }
                let data = params["args"]["data"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                server.written.push((block, data));
                json!({ "bytes_written": 1 })
            }
            other => {
                return Err(format!(
                    "unexpected method {other:?} (status=400 code=bad_request)"
                ));
            }
        },
        other => {
            return Err(format!(
                "unexpected call {other} (status=400 code=bad_request)"
            ));
        }
    })
}

/// A connection to a fake server that answers from `server` until the connection ends.
pub(super) fn connect(server: &Arc<Mutex<Server>>) -> Result<Rex> {
    let (ours, theirs) = UnixStream::pair()?;
    let server = Arc::clone(server);
    std::thread::spawn(move || -> Result<()> {
        let mut out = theirs.try_clone()?;
        for line in BufReader::new(theirs).lines() {
            let request: Value = serde_json::from_str(&line?)?;
            let reply = match answer(
                &server,
                request["method"].as_str().unwrap_or_default(),
                &request["params"],
            ) {
                Ok(result) => json!({ "id": request["id"], "result": result }),
                Err(error) => json!({ "id": request["id"], "error": error }),
            };
            writeln!(out, "{reply}")?;
        }
        Ok(())
    });
    Ok(Rex::Pipe(Pipe::new(ours)?))
}
