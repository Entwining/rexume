use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::json;

use super::super::tests::{node, settings};
use super::super::{Runtime, inbox};
use super::*;

#[test]
fn a_session_reopens_from_its_file_only_once_it_exists() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let session = dir.path().join("session.jsonl");
    let path = session.to_string_lossy().into_owned();
    let mut c = Conversation {
        native_id: "s".into(),
        transcript: Some(path.clone()),
        settings: settings(&[("model", "fast")]),
    };
    assert!(!Runtime::Omp.can_reopen(&c));
    std::fs::write(&session, "")?;
    assert!(Runtime::Omp.can_reopen(&c));
    Runtime::Omp.settings(&mut c)?;
    assert_eq!(Runtime::Omp.resume_argv(&c), ["omp", "--resume", &path]);
    Ok(())
}

const HARNESS: &str = r#"
    import { join } from "node:path";
    const [extension, home] = process.argv.slice(1);
    let minted = 0;
    globalThis.Bun = { randomUUIDv7: () => `id${++minted}` };
    const { default: factory } = await import(extension);
    const sent = [];
    const files = {};
    const session = (kind, mode) => {
      const handlers = {};
      factory({ on: (event, handler) => (handlers[event] = handler), sendMessage: (m) => sent.push(m.content) });
      return (event, id, fields = {}) =>
        handlers[event]?.({ type: event, ...fields }, {
          mode, hasUI: false, agent: { kind },
          sessionManager: { getSessionId: () => id, getSessionFile: () => files[id] ?? join(home, `${id}.jsonl`) },
        });
    };
    const [main, sub] = [session("main", "tui"), session("sub", "print")];
    const text = (t) => [{ type: "text", text: t }];
    const failed = { role: "assistant", stopReason: "error", errorMessage: "overloaded", content: [] };
    const answer = { role: "assistant", stopReason: "stop", content: [{ type: "thinking" }, ...text("Here."), ...text("Done.")] };
    sub("session_start", "sub");
    main("session_start", "main");
    sub("agent_start", "sub");
    sub("agent_end", "sub", { messages: [answer] });
    main("agent_start", "main");
    for (const message of [
      { role: "custom", customType: "rexume", content: "[rexume d] to main" },
      { role: "custom", customType: "other", content: "[rexume e] quoted" },
      { role: "user", content: "[rexume i/b] Go on." },
      { role: "user", content: text("plain") },
      { role: "user", synthetic: true, content: text("[rexume x] Continue.") },
    ]) main("message_end", "main", { message });
    main("agent_end", "main", { messages: [failed], willContinue: true });
    const reading = { role: "assistant", stopReason: "toolUse", content: text("Reading.") };
    main("agent_end", "main", { messages: [reading, { role: "toolResult" }, answer], willContinue: true, awaitingAsyncWork: true });
    main("agent_end", "main", { messages: [] });
    // `/move` renames the session's file between runs.
    files.main = join(home, "moved/main.jsonl");
    main("agent_start", "main");
    // omp moves the session off a file another process holds, to another ID.
    main("agent_end", "off", { messages: [] });
    // Another process of the same session.
    main("session_start", "main");
    main("session_switch", "next", { reason: "new" });
    main("session_shutdown", "next");
    process.once("beforeExit", () => console.log(JSON.stringify(sent)));
"#;

fn run_harness(home: &Path, block: Option<&str>) -> Result<Vec<String>> {
    let state = home.join(".local/state/rexume");
    for id in ["main", "sub", "off", "next"] {
        let inbox = Runtime::Omp.inbox(&state, id).context("no inbox")?;
        inbox::send(&inbox, "d", &format!("[rexume d] to {id}"))?;
    }
    let pi = Runtime::Pi.inbox(&state, "main").context("no inbox")?;
    inbox::send(&pi, "p", "[rexume p] to Pi")?;
    let extension = home.join("rexume.ts");
    std::fs::write(&extension, EXTENSION)?;
    let (bin, reports) = (home.join("bin"), home.join("reports"));
    std::fs::create_dir(&bin)?;
    std::fs::create_dir(&reports)?;
    let fake = bin.join("rexume");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\n[ \"$*\" = 'report omp' ] || exit 2\n/bin/cat > \"$(/usr/bin/mktemp '{}/r.XXXXXX')\"\n",
            reports.display()
        ),
    )?;
    std::fs::set_permissions(&fake, PermissionsExt::from_mode(0o755))?;
    let mut command = std::process::Command::new(node()?);
    command
        .args(["--input-type=module", "-e", HARNESS])
        .arg(&extension)
        .arg(home)
        .env_clear()
        .env("PATH", &bin)
        .env("HOME", home);
    if let Some(block) = block {
        command.env("REX_BLOCK", block);
    }
    let out = command.output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    anyhow::ensure!(out.status.success(), "omp ended: {stderr}");
    Ok(serde_json::from_slice(&out.stdout)?)
}

#[test]
fn the_extension_records_and_reports_only_the_panes_session() -> Result<()> {
    let home = tempfile::tempdir()?;
    let sent = run_harness(home.path(), Some("block:b"))?;
    assert_eq!(
        sent,
        [
            "[rexume d] to main",
            "[rexume d] to off",
            "[rexume d] to next"
        ]
    );

    let state = home.path().join(".local/state/rexume");
    let path = record(&state, "main");
    assert!(!record(&state, "sub").exists());
    let text = std::fs::read_to_string(&path)?;
    let lines = text
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<Value>, _>>()?;
    let end = |continues: bool, awaits: bool, id: &str| json!({"event": "end", "will_continue": continues, "awaiting_async_work": awaits, "id": id});
    let mut retried = end(true, false, "id1");
    retried["stop_reason"] = "error".into();
    retried["answer"] = "".into();
    retried["error"] = "overloaded".into();
    let mut answered = end(true, true, "id2");
    answered["stop_reason"] = "stop".into();
    answered["answer"] = "Here.\nDone.".into();
    assert_eq!(
        lines,
        [
            json!({"event": "session"}),
            json!({"event": "start"}),
            json!({"event": "received", "token": "[rexume d]"}),
            json!({"event": "prompt", "token": "[rexume i/b]"}),
            json!({"event": "prompt"}),
            retried,
            answered,
            end(false, false, "id3"),
            json!({"event": "start"}),
            json!({"event": "session"}),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(record(&state, "next"))?,
        "{\"event\":\"session\"}\n"
    );
    let off: Value = serde_json::from_str(&std::fs::read_to_string(record(&state, "off"))?)?;
    assert_eq!(off, end(false, false, "id4"));

    let mut reported = std::fs::read_dir(home.path().join("reports"))?
        .map(|entry| -> Result<Value> {
            Ok(serde_json::from_slice(&std::fs::read(entry?.path())?)?)
        })
        .map(|report| {
            report.map(|r| {
                let file = r["session_file"].as_str().unwrap_or_default().to_owned();
                (r["session_id"].clone(), file, r["record_length"].clone())
            })
        })
        .collect::<Result<Vec<_>>>()?;
    reported.sort_by_key(|r| (r.0.to_string(), r.2.as_u64()));
    let file = |name: &str| home.path().join(name).to_string_lossy().into_owned();
    let before = |line: usize| {
        text.split_inclusive('\n')
            .take(line)
            .map(str::len)
            .sum::<usize>()
    };
    assert_eq!(
        reported,
        [
            ("main".into(), file("main.jsonl"), 0.into()),
            ("main".into(), file("moved/main.jsonl"), before(8).into()),
            ("main".into(), file("moved/main.jsonl"), before(9).into()),
            ("next".into(), file("next.jsonl"), 0.into()),
            ("off".into(), file("off.jsonl"), 0.into()),
        ]
    );
    Ok(())
}

#[test]
fn the_extension_does_nothing_outside_a_rex_pane() -> Result<()> {
    let home = tempfile::tempdir()?;
    assert!(run_harness(home.path(), None)?.is_empty());
    assert!(!record(&home.path().join(".local/state/rexume"), "main").exists());
    assert_eq!(std::fs::read_dir(home.path().join("reports"))?.count(), 0);
    Ok(())
}

fn turns(lines: &[Value]) -> (Vec<(Turn, u64)>, Turn) {
    let mut parser = Parser::default();
    let mut sealed = Vec::new();
    for (i, line) in (0..).zip(lines) {
        parser.feed(
            line,
            Span {
                start: i,
                end: i + 1,
            },
            &mut sealed,
        );
    }
    let sealed = sealed.into_iter().map(|s| (s.turn, s.span.start)).collect();
    (sealed, parser.turn())
}

#[test]
fn a_turn_ends_as_its_last_run_stopped() {
    let completed = Turn::Completed {
        reference: "r".into(),
        text: "Here.".into(),
    };
    let failed = |text: &str| Turn::Failed { text: text.into() };
    for (stop, ended) in [
        (json!("stop"), completed.clone()),
        (json!("length"), completed.clone()),
        (json!("toolUse"), completed.clone()),
        (json!("aborted"), Turn::Cancelled),
        (json!("error"), failed("quota")),
        (json!(null), failed("the run ended without an answer")),
        (json!("new"), failed("the run ended with stop reason `new`")),
    ] {
        let end = json!({"event": "end", "id": "r", "stop_reason": stop, "answer": "Here.",
                         "error": "quota"});
        let start = json!({"event": "start"});
        assert_eq!(
            turns(&[start, end]),
            (vec![(ended.clone(), 0)], ended),
            "{stop}"
        );
    }
    assert_eq!(turns(&[json!({"event": "start"})]), (vec![], Turn::Open));
}

#[test]
fn a_turn_goes_on_only_through_a_continuation() {
    let session = json!({"event": "session"});
    let start = json!({"event": "start"});
    let prompt = json!({"event": "prompt"});
    let continuation = json!({"event": "prompt", "token": "[rexume i/b]"});
    let notice = json!({"event": "received", "token": "[rexume d]"});
    let retry = json!({"event": "end", "will_continue": true, "stop_reason": "error"});
    let waits = json!({"event": "end", "will_continue": true, "awaiting_async_work": true,
                       "id": "r", "stop_reason": "stop", "answer": "Here."});
    let end = json!({"event": "end", "id": "r", "stop_reason": "stop", "answer": "Here."});
    let completed = Turn::Completed {
        reference: "r".into(),
        text: "Here.".into(),
    };
    let one = vec![(completed.clone(), 0)];
    let after = |start| vec![(Turn::Cancelled, 0), (completed.clone(), start)];
    for (lines, sealed) in [
        (
            vec![&start, &notice, &retry, &start, &prompt, &end],
            one.clone(),
        ),
        // omp passes an end to its extensions without waiting, so the next start can come first.
        (
            vec![&start, &prompt, &start, &retry, &prompt, &end],
            one.clone(),
        ),
        (
            vec![&start, &waits, &start, &end],
            vec![(completed.clone(), 0), (completed.clone(), 2)],
        ),
        (
            vec![&start, &prompt, &session, &start, &continuation, &end],
            one.clone(),
        ),
        (
            vec![&start, &session, &start, &retry, &start, &prompt, &end],
            one.clone(),
        ),
        (
            vec![&start, &prompt, &session, &start, &prompt, &end],
            after(3),
        ),
        (vec![&start, &session, &start, &notice, &end], after(2)),
        (
            vec![&start, &retry, &session, &start, &prompt, &end],
            after(3),
        ),
        (vec![&end], one.clone()),
    ] {
        let lines: Vec<Value> = lines.into_iter().cloned().collect();
        assert_eq!(turns(&lines).0, sealed, "{lines:?}");
    }
}
