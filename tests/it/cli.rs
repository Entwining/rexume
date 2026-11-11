use std::process::Output;

use crate::common::rexume;

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_usage_error_exits_2() {
    for (args, says) in [
        (
            &["report", "foo"][..],
            "[possible values: claude, codex, pi, omp]",
        ),
        (
            &[
                "task",
                "add",
                "--worker",
                "b",
                "--title",
                "t",
                "--result-file",
                "relative/x",
            ][..],
            "not an absolute path",
        ),
    ] {
        let output = rexume(args, &[], "");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains(says), "{}", stderr(&output));
    }
}

#[test]
fn a_task_command_outside_a_rex_pane_fails_with_one_line() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_string_lossy();
    let output = rexume(&["task", "list", "--state-dir", &state], &[], "");
    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(
        text.starts_with("rexume: ") && text.contains("REX_BLOCK") && text.lines().count() == 1,
        "{text}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn a_failed_report_exits_0_so_the_session_it_reports_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_string_lossy();
    let output = rexume(
        &["report", "claude", "--state-dir", &state],
        &[("REX_BLOCK", "block:1")],
        "not json",
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stderr(&output).starts_with("rexume: report claude: "),
        "{}",
        stderr(&output)
    );
}

/// A hook's agent writes the whole input whether or not the report needs it; input larger than a
/// pipe holds fails the write if the report exits without reading it.
#[test]
fn a_report_outside_a_rex_pane_reads_its_input_before_it_exits() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_string_lossy();
    let input = format!(
        r#"{{"session_id": "s", "padding": "{}"}}"#,
        "x".repeat(1 << 20)
    );
    let output = rexume(&["report", "pi", "--state-dir", &state], &[], &input);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn a_claude_turn_hook_appends_to_the_turn_record() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_string_lossy();
    for input in [
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","prompt_id":"p","prompt":"[rexume d1] Review the diff"}"#,
        r#"{"hook_event_name":"Stop","session_id":"s","prompt_id":"p","last_assistant_message":"Looks good."}"#,
    ] {
        let output = rexume(
            &["report", "claude", "--state-dir", &state],
            &[("REX_BLOCK", "block:1")],
            input,
        );
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(stderr(&output), "");
    }
    let record = std::fs::read_to_string(dir.path().join("claude/s.jsonl")).unwrap();
    let lines: Vec<serde_json::Value> = record
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        lines[0],
        serde_json::json!({ "event": "prompt", "prompt_id": "p", "token": "[rexume d1]" })
    );
    assert_eq!(
        (lines[1]["event"].as_str(), lines[1]["answer"].as_str()),
        (Some("stop"), Some("Looks good."))
    );
}

#[test]
fn export_prints_a_header_and_an_end_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let output = rexume(
        &["export", "--state-dir", &missing.to_string_lossy()],
        &[],
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).starts_with("rexume: ") && stderr(&output).lines().count() == 1);
    assert!(output.stdout.is_empty() && !missing.exists());

    let state = dir.path().join("state");
    let state = state.to_string_lossy();
    // `task list` creates the database before it finds that it runs outside a pane.
    rexume(&["task", "list", "--state-dir", &state], &[], "");
    let output = rexume(&["export", "--state-dir", &state], &[], "");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["record"], "header");
    assert_eq!(
        lines[1],
        serde_json::json!({ "record": "end", "entities": 0 })
    );
}

#[test]
fn task_add_refuses_a_task_meta_that_is_not_a_json_object() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let output = rexume(
        &[
            "task",
            "add",
            "--state-dir",
            &state.to_string_lossy(),
            "--worker",
            "block:w",
            "--title",
            "x",
        ],
        &[("REXUME_TASK_META", "eval=e1"), ("REX_BLOCK", "block:o")],
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    let said = stderr(&output);
    assert!(
        said.starts_with("rexume: REXUME_TASK_META is not a JSON object")
            && said.lines().count() == 1,
        "{said}"
    );
    assert!(!state.exists());
}
