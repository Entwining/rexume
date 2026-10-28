use super::super::tests::{ANSWER, DURATION, PROMPT, transcript};
use super::super::{Lines, Runtime, last_turn};
use super::*;

const THINKING: &str = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":null,"content":[{"type":"thinking","thinking":""}]}}"#;
const TOOL_USE: &str = r#"{"type":"assistant","message":{"id":"m0","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}"#;
const TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
const CANCEL: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
const API_REQUEST: &str = r#"{"type":"api-request"}"#;
const PEER: &str = r#"{"type":"user","isMeta":true,"turnOrigin":"peer","message":{"role":"user","content":"Another Claude session sent a message:\n[rexume d1] done"}}"#;
const CONTEXT: &str = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<system-reminder>context</system-reminder>"}}"#;

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

#[test]
fn an_answer_ends_with_the_last_record_of_its_message() -> Result<()> {
    let more = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"More."}]}}"#;
    let answered = [PROMPT, THINKING, ANSWER, more];
    // Until another record follows, another part of the message may still come.
    assert!(sealed(&answered)?.is_empty());
    let finished = [&answered[..], &[DURATION]].concat();
    assert_eq!(
        sealed(&finished)?,
        [Sealed {
            turn: Turn::Completed {
                reference: "m1".into(),
                text: "Done.\nMore.".into()
            },
            span: Span {
                start: 0,
                end: offset(&answered)
            }
        }]
    );
    assert_eq!(
        sealed(&[PROMPT, THINKING, CANCEL])?,
        [Sealed {
            turn: Turn::Cancelled,
            span: Span {
                start: 0,
                end: offset(&[PROMPT, THINKING, CANCEL])
            }
        }]
    );
    Ok(())
}

/// A turn the server's end cut off leaves no end in the record; the turn that continues it is
/// the next one to finish.
#[test]
fn a_turn_another_started_before_it_ended_is_dropped() -> Result<()> {
    let cut_off = [PROMPT, TOOL_USE];
    let found = sealed(&[&cut_off[..], &[PEER, ANSWER, DURATION]].concat())?;
    assert_eq!(
        found.iter().map(|s| s.span.start).collect::<Vec<_>>(),
        [offset(&cut_off)]
    );
    Ok(())
}

/// An answer is a finished turn also without a record that started it, and stays one when a
/// later part of its message carries no stop reason.
#[test]
fn an_answer_without_a_recorded_start_is_a_turn() -> Result<()> {
    let late_part = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":null,"content":[{"type":"text","text":"More."}]}}"#;
    let found = sealed(&[CONTEXT, ANSWER, late_part, DURATION])?;
    assert_eq!(
        found,
        [Sealed {
            turn: Turn::Completed {
                reference: "m1".into(),
                text: "Done.\nMore.".into()
            },
            span: Span {
                start: offset(&[CONTEXT]),
                end: offset(&[CONTEXT, ANSWER, late_part])
            }
        }]
    );
    Ok(())
}

#[test]
fn turn_states_follow_the_records() -> Result<()> {
    let open = Some(Turn::Open { since: None });
    assert_eq!(turn(&[PROMPT, TOOL_USE, TOOL_RESULT, API_REQUEST])?, open);
    assert_eq!(
        turn(&[PROMPT, THINKING, ANSWER])?,
        Some(Turn::Completed {
            reference: "m1".into(),
            text: "Done.".into()
        })
    );
    assert_eq!(turn(&[PROMPT, THINKING, CANCEL])?, Some(Turn::Cancelled));
    assert_eq!(turn(&[PROMPT, ANSWER, PROMPT])?, open);
    assert_eq!(turn(&[PROMPT, ANSWER, PEER])?, open);
    assert!(matches!(
        turn(&[PROMPT, ANSWER, CONTEXT])?,
        Some(Turn::Completed { .. })
    ));
    Ok(())
}

#[test]
fn an_open_prompt_carries_when_it_was_recorded() -> Result<()> {
    let prompt = r#"{"type":"user","timestamp":"2026-10-24T15:29:45.738Z","turnOrigin":"human","message":{"role":"user","content":"Count"}}"#;
    assert_eq!(
        turn(&[prompt])?,
        Some(Turn::Open {
            since: Some(1_792_855_785_738)
        })
    );
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
