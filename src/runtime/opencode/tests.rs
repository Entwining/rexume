use serde_json::json;

use super::super::Runtime;
use super::*;

fn turns(lines: &[Value]) -> (Vec<(Turn, Span)>, Turn) {
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
    let sealed = sealed.into_iter().map(|s| (s.turn, s.span)).collect();
    (sealed, parser.turn())
}

fn completed(reference: &str, text: &str) -> Turn {
    Turn::Completed {
        reference: reference.into(),
        text: text.into(),
    }
}

fn span(start: u64, end: u64) -> Span {
    Span { start, end }
}

#[test]
fn a_turn_ends_with_its_idle_or_the_prompt_queued_after_its_answer() {
    let user = json!({"id": "u", "type": "user"});
    let synthetic = json!({"id": "s", "type": "synthetic"});
    let tools = json!({"id": "t", "type": "assistant", "finish": "tool-calls", "text": "Reading."});
    let cut = json!({"id": "c", "type": "assistant", "text": "Rea"});
    let one = json!({"id": "a1", "type": "assistant", "finish": "stop", "text": "ONE"});
    let two = json!({"id": "a2", "type": "assistant", "finish": "stop", "text": "TWO"});
    let truncated = json!({"id": "l", "type": "assistant", "finish": "length", "text": "ON"});
    let failed = json!({"id": "f", "type": "assistant", "finish": "error", "text": "O"});
    let idle = json!({"id": "i", "type": "idle", "outcome": "succeeded"});
    let copied = json!({"cursor": "c", "anchor": "i"});
    for (lines, sealed) in [
        (
            vec![&user, &tools, &copied, &one, &idle],
            vec![(completed("a1", "ONE"), span(0, 5))],
        ),
        // OpenCode drains a prompt queued during a turn into the same busy period.
        (
            vec![&user, &one, &user, &two, &idle],
            vec![
                (completed("a1", "ONE"), span(0, 2)),
                (completed("a2", "TWO"), span(2, 5)),
            ],
        ),
        // A prompt steered in at a tool step, and the prompts OpenCode adds after its service's
        // restart, also during a failed answer's retry, and after an answer the output limit cut,
        // go on with the turn.
        (
            vec![
                &user, &tools, &user, &cut, &synthetic, &failed, &synthetic, &truncated,
                &synthetic, &one, &idle,
            ],
            vec![(completed("a1", "ONE"), span(0, 11))],
        ),
        // A message OpenCode admitted again wakes the session without one.
        (
            vec![&user, &one, &idle, &idle],
            vec![(completed("a1", "ONE"), span(0, 3))],
        ),
        (vec![&user, &idle], vec![(completed("i", ""), span(0, 2))]),
    ] {
        let lines: Vec<Value> = lines.into_iter().cloned().collect();
        let (got, last) = turns(&lines);
        assert_eq!(got, sealed, "{lines:?}");
        assert_eq!(last, sealed.last().unwrap().0, "{lines:?}");
    }
    assert_eq!(turns(&[user.clone(), one]), (vec![], Turn::Open));
}

#[test]
fn a_turn_fails_or_is_cancelled_as_its_idle_says() {
    let user = json!({"id": "u", "type": "user"});
    let answer = json!({"id": "a", "type": "assistant", "finish": "error", "error": "overloaded"});
    let idle = |outcome: &str, error: Option<&str>| json!({"id": "i", "type": "idle", "outcome": outcome, "error": error});
    let failed = |text: &str| Turn::Failed { text: text.into() };
    for (lines, ended) in [
        (
            vec![&user, &answer, &idle("failed", Some("quota"))],
            failed("quota"),
        ),
        (
            vec![&user, &answer, &idle("failed", None)],
            failed("overloaded"),
        ),
        (
            vec![&user, &idle("failed", None)],
            failed("the turn ended with outcome `failed`"),
        ),
        (
            vec![&user, &answer, &idle("interrupted", None)],
            Turn::Cancelled,
        ),
    ] {
        let lines: Vec<Value> = lines.into_iter().cloned().collect();
        assert_eq!(turns(&lines).1, ended, "{lines:?}");
    }
}

#[test]
fn only_a_prompt_the_plugin_delivered_is_received() {
    assert!(received(
        &json!({"id": "msg_d", "type": "user", "received": "[rexume d]"})
    ));
    assert!(!received(&json!({"id": "u", "type": "user"})));
}

#[test]
fn a_session_reopens_by_its_id_without_a_continuation() {
    let c = Conversation {
        native_id: "ses_a".into(),
        ..Conversation::default()
    };
    assert!(Runtime::OpenCode.can_reopen(&c));
    assert_eq!(
        Runtime::OpenCode.resume_argv(&c),
        ["opencode", "--session", "ses_a"]
    );
    assert!(Runtime::OpenCode.resumes_turns());
}

mod plugin;
