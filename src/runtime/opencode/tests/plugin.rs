use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::runtime::opencode::{PLUGIN, record};
use crate::runtime::tests::node;
use crate::runtime::{Conversation, Runtime};

const HARNESS: &str = include_str!("harness.mjs");

struct Run {
    pid: i64,
    prompted: Vec<Value>,
    warnings: Vec<String>,
    reports: Vec<Value>,
    at_home: Vec<String>,
    shown_again: Vec<String>,
}

fn run_harness(home: &Path, block: Option<&str>, record: &[Value]) -> Result<Run> {
    let state = home.join(".local/state/rexume");
    let path = super::record(&state, "ses_a");
    std::fs::create_dir_all(path.parent().context("no directory")?)?;
    let lines: Vec<String> = record.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, lines.concat())?;
    let plugin = home.join("tui.ts");
    std::fs::write(&plugin, PLUGIN)?;
    let (bin, reports) = (home.join("bin"), home.join("reports.jsonl"));
    std::fs::create_dir(&bin)?;
    let fake = bin.join("rexume");
    // The first session's report outlasts the TUI's switch to the next one.
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\n[ \"$*\" = 'report opencode' ] || exit 2\ninput=$(/bin/cat)\n\
             case $input in *ses_a*) /bin/sleep 1;; esac\nprintf '%s\\n' \"$input\" >> '{}'\n",
            reports.display()
        ),
    )?;
    std::fs::set_permissions(&fake, PermissionsExt::from_mode(0o755))?;
    let mut command = std::process::Command::new(node()?);
    command
        .args(["--input-type=module", "-e", HARNESS])
        .arg(&plugin)
        .arg(home)
        .env_clear()
        .env("PATH", &bin)
        .env("HOME", home);
    if let Some(block) = block {
        command.env("REX_BLOCK", block);
    }
    let out = command.output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    anyhow::ensure!(out.status.success(), "the TUI ended: {stderr}");
    let run: Value = serde_json::from_slice(&out.stdout)?;
    let reports = match std::fs::read_to_string(&reports) {
        Ok(text) => text
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    Ok(Run {
        pid: run["pid"].as_i64().context("no pid")?,
        prompted: serde_json::from_value(run["prompted"].clone())?,
        warnings: serde_json::from_value(run["warnings"].clone())?,
        reports,
        at_home: serde_json::from_value(run["atHome"].clone())?,
        shown_again: serde_json::from_value(run["shownAgain"].clone())?,
    })
}

#[test]
fn the_plugin_copies_the_shown_sessions_messages_once_each_in_order() -> Result<()> {
    let home = tempfile::tempdir()?;
    // A copy cut off after its first message.
    let before = [
        json!({"id": "m1", "type": "user"}),
        json!({"cursor": "c:m1", "anchor": "m1"}),
        json!({"id": "m2", "type": "assistant", "finish": "stop", "text": "ONE"}),
    ];
    let run = run_harness(home.path(), Some("block:b"), &before)?;
    let state = home.path().join(".local/state/rexume");
    let length = before
        .iter()
        .map(|line| format!("{line}\n").len())
        .sum::<usize>();
    assert_eq!(
        run.reports,
        [
            json!({"session_id": "ses_a", "record_length": length}),
            json!({"session_id": "ses_c", "record_length": 0}),
        ],
        "a subagent's session is its parent's, and reports keep the order of the switches"
    );
    // A cursor line's `at` is its anchor's creation time, on which the copy after the revert relies.
    let lines = |session: &str| -> Result<Vec<Value>> {
        let text = std::fs::read_to_string(record(&state, session))?;
        text.lines()
            .map(|line| {
                let mut line: Value = serde_json::from_str(line)?;
                if line.get("cursor").is_some() {
                    line.as_object_mut().context("not an object")?.remove("at");
                }
                Ok(line)
            })
            .collect()
    };
    let cursor = |id: &str| json!({"cursor": format!("c:{id}"), "anchor": id});
    let answer = |id: &str, text: &str| json!({"id": id, "type": "assistant", "finish": "stop", "text": text});
    let idle = |id: &str| json!({"id": id, "type": "idle", "outcome": "succeeded"});
    let shown_later = lines("ses_c")?;
    let (exit, copied) = shown_later.split_last().context("empty record")?;
    assert_eq!(
        copied,
        [
            json!({"id": "c3", "type": "user"}),
            idle("c4"),
            cursor("c4"),
            json!({"id": "c5", "type": "user"}),
            json!({"cursor": null, "anchor": "c5"}),
            json!({"id": "c7", "type": "user"}),
            idle("c8"),
            cursor("c8"),
        ],
        "a session's first copy leaves out what it held before the TUI started"
    );
    let expected: Vec<Value> = before
        .into_iter()
        .chain([
            idle("m3"),
            cursor("m3"),
            json!({"id": "m4", "type": "user", "received": "[rexume n]"}),
            json!({"cursor": null, "anchor": "m4s"}),
            answer("m5", "TWO"),
            idle("m6"),
            cursor("m6"),
            json!({"id": "m7", "type": "user"}),
            answer("m8", "THREE"),
            idle("m9"),
            cursor("m9"),
            idle("m10"),
            cursor("m10"),
            json!({"id": "p0", "type": "user", "received": "[rexume p]"}),
        ])
        .chain((0..200).map(|i| idle(&format!("p1.{i}"))))
        .chain([
            json!({"cursor": null, "anchor": "p1.199"}),
            answer("p2", "LONG"),
            cursor("p2"),
        ])
        .chain((0..200).map(|i| idle(&format!("p3.{i}"))))
        .chain([idle("p4"), cursor("p4")])
        .collect();
    assert_eq!(lines("ses_a")?, expected);
    assert_eq!(exit["signal"], "SIGHUP", "the first signal counts");
    let shown = Conversation {
        native_id: "ses_c".into(),
        ..Conversation::default()
    };
    let killed_at = |pid| Runtime::OpenCode.killed_at(&state, &shown, pid);
    assert!(killed_at(run.pid)?.is_some());
    assert!(killed_at(run.pid + 1)?.is_none());
    assert!(!record(&state, "ses_b").exists(), "a session never shown");
    assert!(!state.join("opencode/ses_child.jsonl").exists());

    assert_delivered(run, &state)
}

/// Each inbox message goes once, while the TUI shows its session.
fn assert_delivered(run: Run, state: &Path) -> Result<()> {
    let sorted = |mut ids: Vec<String>| {
        ids.sort();
        ids
    };
    assert_eq!(run.at_home, ["msg_d"], "the session is not shown");
    assert_eq!(sorted(run.shown_again), ["msg_d", "msg_h", "msg_r"]);
    assert!(run.prompted.iter().all(|p| p["delivery"] == "queue"));
    let delivered = run.prompted.iter().filter_map(|p| p["id"].as_str());
    assert_eq!(
        sorted(delivered.map(str::to_owned).collect()),
        ["msg_d", "msg_e", "msg_h", "msg_r"]
    );
    let inbox = Runtime::OpenCode
        .inbox(state, "ses_a")
        .context("no inbox")?;
    let mut names: Vec<String> = std::fs::read_dir(&inbox)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_>>()?;
    names.sort();
    assert_eq!(
        names,
        ["d.accepted", "e.accepted", "h.accepted", "r.accepted"]
    );
    assert!(
        run.warnings.iter().any(|w| w.contains("r.json")),
        "{:?}",
        run.warnings
    );
    Ok(())
}

#[test]
fn the_plugin_does_nothing_outside_a_rex_pane() -> Result<()> {
    let home = tempfile::tempdir()?;
    let run = run_harness(home.path(), None, &[])?;
    assert!(run.prompted.is_empty() && run.reports.is_empty());
    let state = home.path().join(".local/state/rexume");
    assert_eq!(std::fs::read_to_string(record(&state, "ses_a"))?, "");
    let inbox = Runtime::OpenCode
        .inbox(&state, "ses_a")
        .context("no inbox")?;
    assert!(inbox.join("d.json").exists());
    Ok(())
}
