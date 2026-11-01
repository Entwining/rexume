use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use super::lines::last_record;
use super::*;

pub(super) fn settings(pairs: &[(&str, &str)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), Value::from(*value)))
        .collect()
}

pub(super) fn transcript(lines: &[&str]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
    file
}

const PROMPT: &str = r#"{"event":"prompt","prompt_id":"p1","token":null}"#;
const ANSWER: &str = r#"{"event":"stop","prompt_id":"p1","id":"s1","answer":"Done."}"#;

/// A tracker following a first round from the record's start.
fn tracker(path: &Path) -> Tracker {
    Tracker::new(Runtime::Claude, path, 0, Taken::default(), None)
}

fn chosen_text(tracker: &Tracker) -> Option<String> {
    match tracker.chosen().map(|sealed| &sealed.turn) {
        Some(Turn::Completed { text, .. }) => Some(text.clone()),
        _ => None,
    }
}

#[test]
fn a_tracker_reads_only_whole_lines_and_continues_from_them() -> Result<()> {
    let mut file = transcript(&[PROMPT]);
    let mut tracker = tracker(file.path());
    tracker.update()?;
    assert!(tracker.chosen().is_none());
    // Half of the answer reaches the file first.
    let (head, tail) = ANSWER.split_at(20);
    write!(file, "{head}")?;
    file.flush()?;
    tracker.update()?;
    assert!(tracker.chosen().is_none());
    writeln!(file, "{tail}")?;
    file.flush()?;
    tracker.update()?;
    let answered = std::fs::metadata(file.path())?.len();
    assert_eq!(chosen_text(&tracker).as_deref(), Some("Done."));
    assert_eq!(
        tracker.chosen().map(|sealed| sealed.span),
        Some(Span {
            start: 0,
            end: answered
        })
    );
    Ok(())
}

#[test]
fn a_tracker_stops_when_the_record_is_replaced_or_cut() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.jsonl");
    std::fs::write(&path, format!("{PROMPT}\n"))?;
    let mut cut = tracker(&path);
    cut.update()?;
    std::fs::write(&path, "")?;
    cut.update()?;
    assert!(cut.replaced());
    std::fs::write(&path, format!("{PROMPT}\n{ANSWER}\n"))?;
    cut.update()?;
    assert!(cut.replaced() && cut.chosen().is_none());
    // Replaced by another file of the same length.
    let mut replaced = tracker(&path);
    replaced.update()?;
    let other = dir.path().join("other.jsonl");
    std::fs::write(&other, format!("{PROMPT}\n{ANSWER}\n"))?;
    std::fs::rename(&other, &path)?;
    replaced.update()?;
    assert!(replaced.replaced());
    Ok(())
}

#[test]
fn a_missing_record_has_no_turn_and_other_read_errors_surface() -> Result<()> {
    let dir = tempfile::tempdir()?;
    assert_eq!(
        last_turn(Runtime::Claude, &dir.path().join("absent.jsonl"), 0)?,
        Some(Turn::None)
    );
    assert!(last_turn(Runtime::Claude, dir.path(), 0).is_err());
    Ok(())
}

/// Claude Code's input counts as received once the turn that took it in ends, which may be in a
/// later check than its prompt; a prompt another hook rejected, which the next prompt follows
/// without an end, and an answer that mentions a token are no receipts.
#[test]
fn only_input_a_finished_turn_took_counts_as_a_receipt() -> Result<()> {
    let mut file = transcript(&[
        r#"{"event":"stop","prompt_id":"p1","id":"s1","answer":"I saw [rexume d2] earlier"}"#,
        r#"{"event":"prompt","prompt_id":"p2","token":"[rexume d2]"}"#,
        r#"{"event":"prompt","prompt_id":"p3","token":"[rexume d1]"}"#,
    ]);
    let pending = ["d1".to_owned(), "d2".to_owned()];
    let mut receipt = Receipt::new(Runtime::Claude, file.path());
    assert!(receipt.found(&pending)?.is_empty());
    writeln!(
        file,
        r#"{{"event":"stop","prompt_id":"p3","id":"s2","answer":"Read."}}"#
    )?;
    file.flush()?;
    assert_eq!(receipt.found(&pending)?, HashSet::from(["d1".to_owned()]));
    Ok(())
}

#[test]
fn claude_resumes_by_id_with_the_reported_settings() {
    let c = Conversation {
        native_id: "id".into(),
        settings: settings(&[("model", "claude-haiku-5-5"), ("effort", "low")]),
        ..Conversation::default()
    };
    assert_eq!(
        Runtime::Claude.resume_argv(&c),
        [
            "claude",
            "--resume",
            "id",
            "--model",
            "claude-haiku-5-5",
            "--effort",
            "low"
        ]
    );
}

#[test]
fn a_line_that_is_not_json_leaves_the_last_turn_unknown() -> Result<()> {
    let file = transcript(&[PROMPT, "{not json", ANSWER]);
    assert_eq!(last_turn(Runtime::Claude, file.path(), 0)?, None);
    // A tracker following a round still reads the records around it.
    let mut tracker = tracker(file.path());
    tracker.update()?;
    assert_eq!(chosen_text(&tracker).as_deref(), Some("Done."));
    Ok(())
}

#[test]
fn a_long_record_reads_the_same_as_a_short_one() -> Result<()> {
    let filler = r#"{"event":"other","prompt_id":"p0"}"#;
    let mut file = transcript(&[]);
    for _ in 0..50_000 {
        writeln!(file, "{filler}")?;
    }
    writeln!(file, "{PROMPT}\n{ANSWER}")?;
    file.flush()?;
    let mut tracker = tracker(file.path());
    tracker.update()?;
    assert_eq!(chosen_text(&tracker).as_deref(), Some("Done."));
    Ok(())
}

#[test]
fn the_last_record_is_found_across_read_blocks_and_not_in_an_unfinished_line() -> Result<()> {
    let pick = |r: &Value| r.get("mode").and_then(Value::as_str).map(str::to_owned);
    let pad = "x".repeat(100_000);
    let mut file = transcript(&[
        r#"{"mode":"old"}"#,
        // Longer than a read block, so it is found only by joining blocks.
        &format!(r#"{{"mode":"last","pad":"{pad}"}}"#),
        &format!(r#"{{"other":"{pad}"}}"#),
        r#"{"mode":"broken"#,
    ]);
    // Also longer than a read block, so no newline is found in the first block read.
    write!(file, r#"{{"mode":"unfinished","pad":"{pad}"}}"#)?;
    file.flush()?;
    assert_eq!(
        last_record(file.path(), "\"mode\"", pick)?.as_deref(),
        Some("last")
    );
    let none = transcript(&[&format!(r#"{{"other":"{pad}"}}"#)]);
    assert_eq!(last_record(none.path(), "\"mode\"", pick)?, None);
    let dir = tempfile::tempdir()?;
    assert_eq!(
        last_record(&dir.path().join("absent"), "\"mode\"", pick)?,
        None
    );
    Ok(())
}
