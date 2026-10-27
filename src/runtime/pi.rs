//! Pi, through the extension `rexume install` writes and the daemon keeps current.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Conversation, Open, Sealed, Sent, Span, Target, Turn, content_texts, text_at};

/// The extension `rexume install` writes into Pi's extensions directory.
pub const EXTENSION: &str = include_str!("pi/extension.ts");

/// How the extension starts in every version, so that a file install wrote is recognized after
/// the rest of its first line changes.
pub const WRITTEN_BY: &str = "// Written by `rexume install`";

/// The `customType` of the notices and requests the extension passes to Pi.
const NOTICE: &str = "rexume";

/// Pi reopens a session from its file; before its first message it has written none, and then the
/// reported settings are all there is.
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

/// Pi restores the model and thinking level of the session's current branch by itself once the
/// session has messages, and writes its file only then; a setting given on the command line would
/// override that, so the reported ones serve only a session that has no file yet.
pub fn settings(transcript: &Path, conversation: &mut Conversation) {
    if transcript.exists() {
        conversation.settings.clear();
    }
}

pub fn inbox(state_dir: &Path, native_id: &str) -> PathBuf {
    state_dir.join("inbox").join(native_id.replace('/', "_"))
}

/// The extension takes each message from the conversation's inbox, renames it to `.accepted`, and
/// passes it to Pi, which records it as a `custom_message`.
pub fn send(target: &Target, state_dir: &Path, id: &str, text: &str) -> Result<Sent> {
    let dir = inbox(state_dir, target.native_id);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create `{}`", dir.display()))?;
    // Written whole under a name the extension ignores, then renamed into view.
    let mut file = tempfile::NamedTempFile::with_prefix_in(".", &dir)?;
    file.write_all(json!({ "text": text }).to_string().as_bytes())?;
    file.persist(dir.join(format!("{id}.json")))?;
    Ok(Sent::Handed)
}

/// Whether the extension took the message from the inbox: Pi holds it, but the conversation may
/// not have taken it in yet.
pub fn accepted(state_dir: &Path, native_id: &str, id: &str) -> bool {
    inbox(state_dir, native_id)
        .join(format!("{id}.accepted"))
        .exists()
}

pub fn received(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("custom_message")
        && record.get("customType").and_then(Value::as_str) == Some(NOTICE)
}

/// Pi ends an answered turn with an assistant message whose `stopReason` is `stop`, a cancelled
/// one with `aborted`, and a failed one with `error`; a user message that nothing answered is a
/// turn cut off.
#[derive(Default)]
pub struct Parser {
    turn: Option<Turn>,
    /// Where the turn not yet yielded starts.
    open: Option<u64>,
}

impl Parser {
    pub fn feed(&mut self, record: &Value, span: Span, sealed: &mut Vec<Sealed>) {
        let message = match record.get("type").and_then(Value::as_str) {
            Some("message") => record.get("message"),
            // A notice or request the extension passes to Pi starts a turn of its own.
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
                    self.turn = Some(Turn::Open { since: None });
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
        });
    }

    fn start(&mut self, span: Span) {
        self.turn = Some(Turn::Open { since: None });
        self.open = Some(span.start);
    }

    pub fn turn(&self) -> Turn {
        self.turn.clone().unwrap_or(Turn::None)
    }

    pub fn open(&self) -> Option<Open> {
        self.open.map(|start| Open {
            start,
            since: None,
            answer: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::super::tests::{settings, transcript};
    use super::super::{Runtime, last_turn};
    use super::*;

    #[test]
    fn turn_states_follow_the_records() -> Result<()> {
        let user = r#"{"type":"message","id":"u1","message":{"role":"user","content":[{"type":"text","text":"go"}]}}"#;
        let tool = r#"{"type":"message","id":"a0","message":{"role":"assistant","stopReason":"toolUse","content":[]}}"#;
        let stop = r#"{"type":"message","id":"a1","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"ok"}]}}"#;
        let aborted = r#"{"type":"message","id":"a2","message":{"role":"assistant","stopReason":"aborted","errorMessage":"Operation aborted","content":[{"type":"thinking"}]}}"#;
        let notice = r#"{"type":"custom_message","id":"n1","customType":"rexume","content":"[rexume d1] done","display":true}"#;
        let turn = |lines: &[&str]| last_turn(Runtime::Pi, transcript(lines).path(), 0);
        let open = Some(Turn::Open { since: None });
        assert_eq!(turn(&[user])?, open);
        assert_eq!(turn(&[user, tool])?, open);
        assert_eq!(
            turn(&[user, stop])?,
            Some(Turn::Completed {
                reference: "a1".into(),
                text: "ok".into()
            })
        );
        assert_eq!(turn(&[user, aborted])?, Some(Turn::Cancelled));
        assert_eq!(turn(&[user, stop, notice])?, open);
        Ok(())
    }

    #[test]
    fn a_session_with_a_file_reopens_without_settings() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let session = dir.path().join("session.jsonl");
        let path = session.to_string_lossy().into_owned();
        let reported = Conversation {
            native_id: "s".into(),
            transcript: Some(path.clone()),
            settings: settings(&[("model", "fast"), ("effort", "low"), ("provider", "gw")]),
        };
        let argv = || -> Result<Vec<String>> {
            let mut c = reported.clone();
            Runtime::Pi.settings(&mut c)?;
            Ok(Runtime::Pi.resume_argv(&c))
        };
        // Before its first message Pi has written no session to restore settings from.
        assert_eq!(
            argv()?,
            [
                "pi",
                "--session",
                &path,
                "--provider",
                "gw",
                "--model",
                "fast",
                "--thinking",
                "low"
            ]
        );
        // Once the file exists Pi applies its current branch's settings itself; any setting
        // passed here would override them.
        std::fs::write(&session, "")?;
        assert_eq!(argv()?, ["pi", "--session", &path]);
        Ok(())
    }

    /// Loads the extension in Node, as Pi does, and starts a session with a fake Pi whose session
    /// file is `session`, if given. The fake's `getSessionFile`, which the extension calls
    /// after starting the report and before writing its input, waits until `ready` exists, so a
    /// report that closes its input first makes the write fail every time rather than only when it
    /// wins the race.
    const HARNESS: &str = r#"
        import { existsSync } from "node:fs";
        const [extension, ready, session] = process.argv.slice(1);
        const handlers = {};
        (await import(extension)).default({ on: (event, handler) => (handlers[event] = handler) });
        const warnings = [];
        process.on("exit", () => console.log(JSON.stringify(warnings)));
        handlers.session_start({}, {
          mode: "print",
          hasUI: true,
          ui: { notify: (text) => warnings.push(text) },
          sessionManager: {
            getSessionId: () => "s",
            getSessionFile: () => {
              while (!existsSync(ready)) Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 5);
              return session;
            },
          },
        });
    "#;

    /// Nothing the report does may end Pi: its exit status alone decides whether Pi warns, also
    /// when it exits without reading its input, as it did outside a Rex pane, or cannot start.
    #[test]
    fn the_extension_survives_any_report_outcome() -> Result<()> {
        let node = node()?;
        let dir = tempfile::tempdir()?;
        let extension = dir.path().join("rexume.ts");
        std::fs::write(&extension, EXTENSION)?;
        let ready = dir.path().join("ready");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin)?;
        let run = |report: Option<i32>| -> Result<Vec<String>> {
            let _ = std::fs::remove_file(&ready);
            let fake = bin.join("rexume");
            match report {
                Some(status) => {
                    let script = format!(
                        "#!/bin/sh\nexec 0<&-\n: > '{}'\nexit {status}\n",
                        ready.display()
                    );
                    std::fs::write(&fake, script)?;
                    std::fs::set_permissions(&fake, PermissionsExt::from_mode(0o755))?;
                }
                None => {
                    let _ = std::fs::remove_file(&fake);
                    std::fs::write(&ready, "")?;
                }
            }
            let out = std::process::Command::new(&node)
                .args(["--input-type=module", "-e", HARNESS])
                .arg(&extension)
                .arg(&ready)
                .env("PATH", &bin)
                .output()?;
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::ensure!(out.status.success(), "Pi ended: {stderr}");
            Ok(serde_json::from_slice(&out.stdout)?)
        };
        assert_eq!(run(Some(0))?, Vec::<String>::new());
        for report in [Some(1), None] {
            let warnings = run(report)?;
            assert!(
                matches!(&warnings[..], [w] if w.starts_with("rexume could not record this session")),
                "{report:?}: {warnings:?}"
            );
        }
        Ok(())
    }

    /// A version manager's `node` may be a shim that needs the `PATH` the harness replaces.
    fn node() -> Result<String> {
        let node = std::process::Command::new("node")
            .args(["-p", "process.execPath"])
            .output()
            .context("this test runs the Pi extension in Node")?;
        Ok(String::from_utf8(node.stdout)?.trim().to_owned())
    }

    /// The report gets the session file's length as the extension found it, and none when the
    /// extension could not measure it, which must not end Pi either.
    #[test]
    fn the_extension_sends_the_session_files_length_when_it_can_measure_it() -> Result<()> {
        let node = node()?;
        let dir = tempfile::tempdir()?;
        let extension = dir.path().join("rexume.ts");
        std::fs::write(&extension, EXTENSION)?;
        let (ready, input, bin) = (
            dir.path().join("ready"),
            dir.path().join("input"),
            dir.path().join("bin"),
        );
        std::fs::write(&ready, "")?;
        std::fs::create_dir(&bin)?;
        let fake = bin.join("rexume");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\n/bin/cat > '{}'\n", input.display()),
        )?;
        std::fs::set_permissions(&fake, PermissionsExt::from_mode(0o755))?;
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked)?;
        std::fs::write(locked.join("session.jsonl"), "x".repeat(42))?;
        for (mode, length) in [(0o755, Some(42)), (0o000, None)] {
            std::fs::set_permissions(&locked, PermissionsExt::from_mode(mode))?;
            let out = std::process::Command::new(&node)
                .args(["--input-type=module", "-e", HARNESS])
                .arg(&extension)
                .arg(&ready)
                .arg(locked.join("session.jsonl"))
                .env("PATH", &bin)
                .output();
            std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o755))?;
            let out = out?;
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::ensure!(out.status.success(), "Pi ended: {stderr}");
            let sent: Value = serde_json::from_slice(&std::fs::read(&input)?)?;
            assert_eq!(sent["session_file_size"].as_u64(), length, "{mode:o}");
        }
        Ok(())
    }
}
