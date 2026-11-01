use serde_json::json;

use super::*;
use crate::runtime::claude;
use crate::task::Follow;
use crate::task::tests::{record, task_world, turn_record};

/// Lines of a Claude Code turn record, as its turn hooks write them.
const PROMPT: &str = r#"{"event":"prompt","prompt_id":"p1","token":null}"#;
const ANSWER: &str = r#"{"event":"stop","prompt_id":"p1","id":"s1","answer":"Done."}"#;

/// A store with restore `r` on instance `i2`, typed into pane `b`, which Rex restored under its
/// own ID, to continue conversation `c`, whose turn record is `transcript`, as the plan read it now.
fn typed(dir: &Path, transcript: &Path) -> Result<Store> {
    turn_record(dir, "c", transcript)?;
    let mut store = Store::open(dir)?;
    let plan = Plan {
        argv: vec!["claude".into(), "--resume".into(), "c".into()],
        prompt: Some("go on".into()),
        transcript: Some(transcript.to_string_lossy().into_owned()),
        record_length: Some(std::fs::metadata(transcript)?.len()),
        ..Plan::default()
    };
    let tx = store.write()?;
    tx.execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'c', 1, 0);
         INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'b', 'b');
         INSERT INTO block (lineage, instance_id, fg_pid) VALUES ('b', 'i2', 7);",
    )?;
    tx.execute(
        "INSERT INTO restore (id, instance_id, block_lineage, start, runtime, native_id, plan,
           state, created_at, typed_at)
         VALUES ('r', 'i2', 'b', 'continue', 'claude', 'c', ?1, 'typed', 100, 101)",
        params![serde_json::to_string(&plan)?],
    )?;
    tx.commit()?;
    Ok(store)
}

fn started(store: &Store) -> Result<(String, Option<String>)> {
    Ok(store.conn().query_row(
        "SELECT state, started FROM restore WHERE id = 'r'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

/// The claim runs once, from a typed line or one whose gate was late, keeps the gate's process,
/// and leaves the pane's record as it was: Rex restored the pane under the ID its lineage is.
#[test]
fn a_claim_continues_a_turn_still_open_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let mut store = typed(dir.path(), &transcript)?;
    store
        .conn()
        .execute("UPDATE restore SET state = 'unknown'", [])?;
    let (start, plan) = claim(&mut store, "r", "b", 42)?;
    assert_eq!(
        (start, plan.prompt.as_deref()),
        (Start::Continue, Some("go on"))
    );
    assert_eq!(
        started(&store)?,
        ("claimed".into(), Some("continue".into()))
    );
    let pid: Option<i64> =
        store
            .conn()
            .query_row("SELECT pid FROM restore WHERE id = 'r'", [], |row| {
                row.get(0)
            })?;
    assert_eq!(pid, Some(42));
    let fg: Option<i64> =
        store
            .conn()
            .query_row("SELECT fg_pid FROM block WHERE lineage = 'b'", [], |row| {
                row.get(0)
            })?;
    assert_eq!(fg, Some(7));
    let again = claim(&mut store, "r", "b", 43);
    assert!(again.unwrap_err().to_string().contains("has run already"));

    // A line run before the daemon typed it, as from another pane's history, claims nothing.
    let other = tempfile::tempdir()?;
    let mut store = typed(other.path(), &transcript)?;
    store
        .conn()
        .execute("UPDATE restore SET state = 'planned', typed_at = NULL", [])?;
    assert!(claim(&mut store, "r", "b", 42).is_err());
    assert_eq!(started(&store)?, ("planned".into(), None));
    Ok(())
}

#[test]
fn a_turn_that_ended_after_the_plan_reopens_without_the_prompt() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let mut store = typed(dir.path(), &transcript)?;
    std::fs::write(&transcript, format!("{PROMPT}\n{ANSWER}\n"))?;
    let (start, _) = claim(&mut store, "r", "b", 42)?;
    assert_eq!(start, Start::Agent);
    assert_eq!(started(&store)?, ("claimed".into(), Some("agent".into())));
    Ok(())
}

/// Work a Stop hook made Claude Code go on with after the end its record shows continues while
/// the record shows nothing after what the plan read.
#[test]
fn work_after_a_turn_end_the_plan_read_continues() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n{ANSWER}\n"))?;
    let mut store = typed(dir.path(), &transcript)?;
    let (start, _) = claim(&mut store, "r", "b", 42)?;
    assert_eq!(start, Start::Continue);
    Ok(())
}

#[test]
fn a_conversation_reported_elsewhere_since_the_plan_starts_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let mut store = typed(dir.path(), &transcript)?;
    // Reported in the same millisecond as the plan: counted as later.
    store.conn().execute(
        "INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env,
           reported_at) VALUES ('other', 'claude', 'c', 1, 0, 100)",
        [],
    )?;
    let (start, _) = claim(&mut store, "r", "b", 42)?;
    assert_eq!(start, Start::Shell);
    assert_eq!(started(&store)?, ("claimed".into(), Some("shell".into())));
    let occupants: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM occupant WHERE block_lineage = 'b'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(occupants, 0);
    Ok(())
}

#[test]
fn a_claim_refuses_a_pane_bound_to_another_lineage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let mut store = typed(dir.path(), &transcript)?;
    store.conn().execute(
        "INSERT INTO live (instance_id, live_id, lineage) VALUES ('i2', 'block:other', 'other')",
        [],
    )?;
    let claimed = claim(&mut store, "r", "block:other", 42);
    assert!(claimed.unwrap_err().to_string().contains("bound to other"));
    assert_eq!(started(&store)?, ("typed".into(), None));
    Ok(())
}

#[test]
fn a_reopen_command_takes_its_variables_as_assignments() -> Result<()> {
    let plan = Plan {
        argv: vec!["claude".into(), "--resume".into(), "c".into()],
        env: vec![("CLOUD_ML_REGION".into(), "us east".into())],
        ..Plan::default()
    };
    assert_eq!(
        command_line(&plan, Some("go on"))?,
        "CLOUD_ML_REGION='us east' claude --resume c 'go on'"
    );
    Ok(())
}

/// A turn the server's end cut off goes on under the continuation prompt the restore sends, so
/// the round that waited for it, also one waiting for a request the turn took in, takes the
/// answer the continuation finishes with.
#[test]
fn the_round_a_cut_off_turn_left_takes_the_answer_its_continuation_finishes() -> Result<()> {
    for request in [None, Some("[rexume q]")] {
        let dir = tempfile::tempdir()?;
        let prompt = json!({ "event": "prompt", "prompt_id": "p1", "token": request });
        let transcript = record(&[&prompt.to_string()])?;
        let mut store = task_world(dir.path(), transcript.path(), None)?;
        if request.is_some() {
            store.conn().execute_batch(
                "INSERT INTO request (id, task_id, round, text, state)
                   VALUES ('q', 't', 1, 'Count.', 'sent')",
            )?;
        }
        store.conn().execute_batch(
            "UPDATE epoch SET ended_at = 60, end_reason = 'server_gone';
             INSERT INTO epoch (id, instance_id, started_at) VALUES (2, 'i2', 70);",
        )?;
        let mut follow = Follow::default();
        follow.instance_started(&mut store, "i2")?;
        let id = restore::plan(&mut store, "i2", "b", None)?.unwrap();
        store.conn().execute(
            "UPDATE restore SET state = 'typed', typed_at = 80 WHERE id = ?1",
            [&id],
        )?;
        let (start, plan) = claim(&mut store, &id, "b", 200)?;
        assert_eq!(start, Start::Continue);
        for input in [
            json!({ "hook_event_name": "UserPromptSubmit", "prompt_id": "p2", "prompt": plan.prompt }),
            json!({ "hook_event_name": "Stop", "prompt_id": "p2", "last_assistant_message": "Counted." }),
        ] {
            claude::append(dir.path(), "worker", &claude::turn_event(&input).unwrap())?;
            follow.pass(&mut store, None)?;
        }
        let round: (String, Option<String>) = store.conn().query_row(
            "SELECT state, result FROM task_round WHERE task_id = 't'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            round,
            ("completed".into(), Some("Counted.".into())),
            "{request:?}"
        );
    }
    Ok(())
}

mod occupant;
