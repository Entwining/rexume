//! Exit statuses and error shapes that callers branch on, from commands that need no server.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use crate::common::REXUME;

fn rexume(args: &[&str], vars: &[(&str, &str)], stdin: &str) -> Output {
    let mut child = Command::new(REXUME)
        .args(args)
        .env_clear()
        .envs(vars.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_usage_error_exits_2() {
    for (args, says) in [
        (
            &["report", "foo"][..],
            "[possible values: claude, codex, pi]",
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
