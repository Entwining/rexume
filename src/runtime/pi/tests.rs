use std::os::unix::fs::PermissionsExt;

use anyhow::Result;

use super::super::tests::{node, settings, transcript};
use super::super::{Runtime, exit_of, last_turn};
use super::*;

#[test]
fn turn_states_follow_the_records() -> Result<()> {
    let user = r#"{"type":"message","id":"u1","message":{"role":"user","content":[{"type":"text","text":"go"}]}}"#;
    let tool = r#"{"type":"message","id":"a0","message":{"role":"assistant","stopReason":"toolUse","content":[]}}"#;
    let stop = r#"{"type":"message","id":"a1","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"ok"}]}}"#;
    let aborted = r#"{"type":"message","id":"a2","message":{"role":"assistant","stopReason":"aborted","errorMessage":"Operation aborted","content":[{"type":"thinking"}]}}"#;
    let notice = r#"{"type":"custom_message","id":"n1","customType":"rexume","content":"[rexume d1] done","display":true}"#;
    let turn = |lines: &[&str]| last_turn(Runtime::Pi, transcript(lines).path(), 0);
    let open = Some(Turn::Open);
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
fn a_session_with_a_file_reopens_without_model_settings() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let session = dir.path().join("session.jsonl");
    let path = session.to_string_lossy().into_owned();
    let meta = r#"{"eval":"e1"}"#;
    let reported = Conversation {
        native_id: "s".into(),
        transcript: Some(path.clone()),
        settings: settings(&[
            ("model", "fast"),
            ("effort", "low"),
            ("provider", "gw"),
            ("task_meta", meta),
        ]),
    };
    let reopened = || -> Result<Conversation> {
        let mut c = reported.clone();
        Runtime::Pi.settings(&mut c)?;
        Ok(c)
    };
    let argv = || -> Result<Vec<String>> { Ok(Runtime::Pi.resume_argv(&reopened()?)) };
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
    std::fs::write(&session, "")?;
    assert_eq!(argv()?, ["pi", "--session", &path]);
    assert_eq!(
        Runtime::Pi.resume_env(&reopened()?),
        [("REXUME_TASK_META".to_owned(), meta.to_owned())]
    );
    Ok(())
}

/// The fake `getSessionFile`, which the extension calls between starting the report and writing
/// its input, waits for `ready`, so a report that closes its input before creating `ready`
/// makes the write fail every time.
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

/// Starts a session in the given mode, passes the given signal, if any, to the process's
/// listeners as Node does, and ends the session as Pi does on either a signal or `/quit`; prints
/// the process's ID. The extension gets no way to write to the session.
const SHUTDOWN: &str = r#"
    const [extension, mode, signal, id] = process.argv.slice(1);
    const handlers = {};
    (await import(extension)).default({ on: (event, handler) => (handlers[event] = handler) });
    const ctx = {
      mode,
      hasUI: false,
      sessionManager: { getSessionId: () => id, getSessionFile: () => undefined },
    };
    handlers.session_start({}, ctx);
    if (signal) process.emit(signal, signal);
    handlers.session_shutdown({ reason: "quit" }, ctx);
    console.log(process.pid);
"#;

#[test]
fn the_extension_records_a_signals_end_of_an_interactive_session_in_a_rex_pane() -> Result<()> {
    let node = node()?;
    for (mode, signal, in_rex, id, recorded) in [
        ("tui", "SIGTERM", true, "s", true),
        ("tui", "SIGHUP", true, "s", true),
        // Pi takes an opened file's session ID from the file.
        ("tui", "SIGTERM", true, "../s", true),
        ("tui", "", true, "s", false),
        ("print", "SIGTERM", true, "s", false),
        ("tui", "SIGTERM", false, "s", false),
    ] {
        let home = tempfile::tempdir()?;
        let state_dir = home.path().join(".local/state/rexume");
        std::fs::create_dir_all(records(&state_dir))?;
        let extension = home.path().join("rexume.ts");
        std::fs::write(&extension, EXTENSION)?;
        let mut command = std::process::Command::new(&node);
        command
            .args(["--input-type=module", "-e", SHUTDOWN])
            .args([
                extension.as_os_str(),
                mode.as_ref(),
                signal.as_ref(),
                id.as_ref(),
            ])
            .env("HOME", home.path())
            .env("PATH", home.path())
            .env_remove("REX_BLOCK");
        if in_rex {
            command.env("REX_BLOCK", "block:b");
        }
        let before = crate::store::now_ms();
        let out = command.output()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::ensure!(out.status.success(), "Pi ended: {stderr}");
        let pid: i64 = String::from_utf8(out.stdout)?.trim().parse()?;
        let exits = exits(&state_dir, id);
        assert_eq!(exits.parent(), Some(records(&state_dir).as_path()));
        let at = exit_of(&exits, pid)?;
        assert_eq!(
            at.is_some_and(|at| (before..=crate::store::now_ms()).contains(&at)),
            recorded,
            "{mode} {signal} in Rex: {in_rex}: {at:?}"
        );
    }
    Ok(())
}

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
