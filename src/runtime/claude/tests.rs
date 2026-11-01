use serde_json::Value;

use super::super::tests::transcript;
use super::super::{Lines, Runtime, last_turn};
use super::*;

const PROMPT: &str = r#"{"event":"prompt","prompt_id":"p1","token":null}"#;
const ANSWER: &str = r#"{"event":"stop","prompt_id":"p1","id":"s1","answer":"Done."}"#;
const PROMPT_2: &str = r#"{"event":"prompt","prompt_id":"p2","token":null}"#;
const ANSWER_2: &str = r#"{"event":"stop","prompt_id":"p2","id":"s2","answer":"Again."}"#;

fn turn(lines: &[&str]) -> Result<Option<Turn>> {
    last_turn(Runtime::Claude, transcript(lines).path(), 0)
}

fn sealed(lines: &[&str]) -> Result<Vec<Sealed>> {
    let file = transcript(lines);
    let mut records = Lines::new(file.path(), 0);
    let mut batch = records.read()?;
    let mut parser = Parser::default();
    let mut sealed = Vec::new();
    while let Some(record) = batch.next_spanned() {
        let (record, span) = record?;
        parser.feed(&record, span, &mut sealed);
    }
    Ok(sealed)
}

fn offset(lines: &[&str]) -> u64 {
    lines.iter().map(|line| line.len() as u64 + 1).sum()
}

fn completed(reference: &str, text: &str) -> Turn {
    Turn::Completed {
        reference: reference.into(),
        text: text.into(),
    }
}

/// Input the running turn takes in, typed or from another session, carries that turn's prompt ID
/// and does not start another; the turn's end shows its token received.
#[test]
fn input_a_running_turn_takes_in_belongs_to_it() -> Result<()> {
    let peer = r#"{"event":"prompt","prompt_id":"p1","token":"[rexume d1]"}"#;
    let lines = [PROMPT, peer, ANSWER];
    let found = sealed(&lines)?;
    assert_eq!(
        found,
        [Sealed {
            turn: completed("s1", "Done."),
            span: Span {
                start: 0,
                end: offset(&lines)
            },
            took: vec!["[rexume d1]".into()],
        }]
    );
    assert!(found[0].received("[rexume d1]"));
    Ok(())
}

/// A prompt nothing ended when the next one starts was interrupted or rejected by another hook:
/// it ends as cancelled where the next starts, and its token is not shown received, while the
/// next turn's is.
#[test]
fn a_prompt_without_an_end_is_cancelled_when_another_starts() -> Result<()> {
    let rejected = r#"{"event":"prompt","prompt_id":"p1","token":"[rexume r1]"}"#;
    let next = r#"{"event":"prompt","prompt_id":"p2","token":"[rexume r2]"}"#;
    let found = sealed(&[rejected, next, ANSWER_2])?;
    assert_eq!(
        found,
        [
            Sealed {
                turn: Turn::Cancelled,
                span: Span {
                    start: 0,
                    end: offset(&[rejected])
                },
                took: vec!["[rexume r1]".into()],
            },
            Sealed {
                turn: completed("s2", "Again."),
                span: Span {
                    start: offset(&[rejected]),
                    end: offset(&[rejected, next, ANSWER_2])
                },
                took: vec!["[rexume r2]".into()],
            }
        ]
    );
    assert!(!found[0].received("[rexume r1]"));
    assert!(found[1].received("[rexume r2]"));
    Ok(())
}

/// A Stop hook that makes Claude Code go on records a second stop under the same prompt ID, which
/// ends a turn of its own.
#[test]
fn each_stop_ends_a_turn() -> Result<()> {
    let again = r#"{"event":"stop","prompt_id":"p1","id":"s2","answer":"More."}"#;
    let found = sealed(&[PROMPT, ANSWER, again])?;
    assert_eq!(
        found,
        [
            Sealed {
                turn: completed("s1", "Done."),
                span: Span {
                    start: 0,
                    end: offset(&[PROMPT, ANSWER])
                },
                took: Vec::new(),
            },
            Sealed {
                turn: completed("s2", "More."),
                span: Span {
                    start: offset(&[PROMPT, ANSWER]),
                    end: offset(&[PROMPT, ANSWER, again])
                },
                took: Vec::new(),
            }
        ]
    );
    Ok(())
}

#[test]
fn turn_states_follow_the_records() -> Result<()> {
    let failure = r#"{"event":"failure","prompt_id":"p1","text":"API Error"}"#;
    assert_eq!(turn(&[])?, Some(Turn::None));
    assert_eq!(turn(&[PROMPT])?, Some(Turn::Open));
    assert_eq!(turn(&[PROMPT, ANSWER])?, Some(completed("s1", "Done.")));
    assert_eq!(
        turn(&[PROMPT, failure])?,
        Some(Turn::Failed {
            text: "API Error".into()
        })
    );
    assert_eq!(turn(&[PROMPT, ANSWER, PROMPT_2])?, Some(Turn::Open));
    Ok(())
}

/// The hooks' input becomes a record line with the prompt's token and nothing else of its text; a
/// subagent's events and other events write nothing.
#[test]
fn a_hook_input_becomes_its_turn_event() {
    let event = |input: Value| turn_event(&input);
    let prompt = |text: &str| {
        event(json!({ "hook_event_name": "UserPromptSubmit", "prompt_id": "p", "prompt": text }))
    };
    assert_eq!(
        prompt("[rexume d1] Worker task t finished."),
        Some(json!({ "event": "prompt", "prompt_id": "p", "token": "[rexume d1]" }))
    );
    for text in ["count", "see [rexume d1]", "[rexume ] x", "[rexume a b] x"] {
        assert_eq!(prompt(text).unwrap()["token"], Value::Null, "{text}");
    }
    // A Rex server instance ID is a UUID and a block ID `block:` and a UUID.
    let resumed = prompt(&super::super::continuation_prompt(
        "0f8e7d6c-5b4a-4392-8170-6f5e4d3c2b1a",
        "block:01a00000-0000-7000-8000-000000000001",
        &["t".into()],
    ));
    assert!(
        resumed.unwrap()["token"]
            .as_str()
            .is_some_and(super::super::is_continuation)
    );
    let stop = event(json!({ "hook_event_name": "Stop", "prompt_id": "p",
        "last_assistant_message": "Done." }))
    .unwrap();
    assert_eq!(
        (&stop["event"], &stop["answer"]),
        (&json!("stop"), &json!("Done."))
    );
    assert!(stop["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(
        event(
            json!({ "hook_event_name": "StopFailure", "prompt_id": "p", "error": "rate_limit",
            "last_assistant_message": "API Error: rate limited" })
        ),
        Some(json!({ "event": "failure", "prompt_id": "p", "text": "API Error: rate limited" }))
    );
    assert_eq!(
        event(json!({ "hook_event_name": "Stop", "agent_id": "a", "prompt_id": "p" })),
        None
    );
    assert_eq!(event(json!({ "hook_event_name": "SessionStart" })), None);
}

#[test]
fn a_record_is_appended_only_under_a_plain_session_id() -> Result<()> {
    let dir = tempfile::tempdir()?;
    append(dir.path(), "s", &json!({ "event": "prompt" }))?;
    append(dir.path(), "s", &json!({ "event": "stop" }))?;
    assert_eq!(
        std::fs::read_to_string(record(dir.path(), "s"))?,
        "{\"event\":\"prompt\"}\n{\"event\":\"stop\"}\n"
    );
    for id in ["", "../s", ".hidden"] {
        assert!(append(dir.path(), id, &json!({})).is_err(), "{id}");
    }
    Ok(())
}

#[test]
fn a_resume_sets_the_launch_environment_and_puts_launch_flags_before_resume() {
    let c = Conversation {
        native_id: "id".into(),
        settings: serde_json::from_value(json!({
            "model": "big",
            "launch_flags": ["--rc", "--add-dir", "/x"],
            "launch_env": ["CLAUDE_CODE_USE_VERTEX=1", "CLOUD_ML_REGION=global"]
        }))
        .unwrap(),
        ..Conversation::default()
    };
    assert_eq!(
        resume_argv(&c),
        [
            "claude",
            "--rc",
            "--add-dir",
            "/x",
            "--resume",
            "id",
            "--model",
            "big"
        ]
    );
    assert_eq!(
        resume_env(&c),
        [
            ("CLAUDE_CODE_USE_VERTEX".into(), "1".into()),
            ("CLOUD_ML_REGION".into(), "global".into())
        ]
    );
}
