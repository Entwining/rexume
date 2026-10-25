use std::collections::HashSet;
use std::io::Write;

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

pub(super) const PROMPT: &str =
    r#"{"type":"user","message":{"role":"user","content":"Write a story"}}"#;
pub(super) const ANSWER: &str = r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Done."}]}}"#;

#[test]
fn a_tracker_reads_only_whole_lines_and_continues_from_them() -> Result<()> {
    let mut file = transcript(&[PROMPT]);
    let mut tracker = Tracker::new(Runtime::Claude, file.path(), 0);
    assert_eq!(tracker.update()?, Turn::Open { since: None });
    // Half of the answer reaches the file first.
    let (head, tail) = ANSWER.split_at(40);
    write!(file, "{head}")?;
    file.flush()?;
    assert_eq!(tracker.update()?, Turn::Open { since: None });
    writeln!(file, "{tail}")?;
    file.flush()?;
    assert_eq!(
        tracker.update()?,
        Turn::Completed {
            reference: "m1".into(),
            text: "Done.".into()
        }
    );
    assert_eq!(tracker.end(), std::fs::metadata(file.path())?.len());
    Ok(())
}

#[test]
fn a_tracker_starts_over_when_the_file_is_replaced_or_cut() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.jsonl");
    std::fs::write(&path, format!("{PROMPT}\n{ANSWER}\n"))?;
    let mut tracker = Tracker::new(Runtime::Claude, &path, 0);
    assert!(matches!(tracker.update()?, Turn::Completed { .. }));
    // Rewritten in place, shorter: only the prompt is left.
    std::fs::write(&path, format!("{PROMPT}\n"))?;
    assert_eq!(tracker.update()?, Turn::Open { since: None });
    // Replaced by another file of the same length.
    let other = dir.path().join("other.jsonl");
    std::fs::write(&other, format!("{PROMPT}\n{ANSWER}\n"))?;
    std::fs::rename(&other, &path)?;
    assert!(matches!(tracker.update()?, Turn::Completed { .. }));
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

#[test]
fn only_received_input_counts_as_a_receipt() -> Result<()> {
    let mut file =
        transcript(&[r#"{"type":"assistant","message":{"content":"I saw [rexume d1] earlier"}}"#]);
    let pending = ["d1".to_owned(), "d2".to_owned()];
    let mut receipt = Receipt::new(Runtime::Claude, file.path());
    assert!(receipt.found(&pending)?.is_empty());
    writeln!(
        file,
        r#"{{"type":"user","origin":{{"kind":"peer"}},"message":{{"content":"Another Claude session sent a message:\n[rexume d1] ready"}}}}"#
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
    let mut tracker = Tracker::new(Runtime::Claude, file.path(), 0);
    assert!(matches!(tracker.update()?, Turn::Completed { .. }));
    Ok(())
}

#[test]
fn a_long_record_reads_the_same_as_a_short_one() -> Result<()> {
    let filler =
        r#"{"type":"user","message":{"role":"user","content":"Write a story"},"isMeta":true}"#;
    let mut file = transcript(&[]);
    for _ in 0..50_000 {
        writeln!(file, "{filler}")?;
    }
    writeln!(file, "{PROMPT}\n{ANSWER}")?;
    file.flush()?;
    let mut tracker = Tracker::new(Runtime::Claude, file.path(), 0);
    assert_eq!(
        tracker.update()?,
        Turn::Completed {
            reference: "m1".into(),
            text: "Done.".into()
        }
    );
    assert_eq!(tracker.end(), std::fs::metadata(file.path())?.len());
    Ok(())
}
