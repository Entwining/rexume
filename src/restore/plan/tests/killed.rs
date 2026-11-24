use super::*;

const PI_PROMPT: &str = r#"{"type":"message","id":"u1","message":{"role":"user","content":"go"}}"#;
/// Signals' ends of the pane's process, PID 10, and of another that had the same session open.
const PI_KILLED: &str = r#"{"pid":10,"signal":"SIGTERM","at":2000}"#;
const PI_KILLED_EARLIER: &str = r#"{"pid":10,"signal":"SIGHUP","at":500}"#;
const PI_OTHER_KILLED: &str = r#"{"pid":11,"signal":"SIGTERM","at":2500}"#;

/// Where else the conversation was reported, at 3000, after this pane.
#[derive(Clone, Copy)]
enum Elsewhere {
    Nowhere,
    InAnOpenPane,
    InAPaneClosedSince,
}

/// A pane whose conversation, recorded at 1000, left the foreground for PID 30 at the pane's first
/// foreground change, before a server whose end was observed stopped.
struct Left<'a> {
    runtime: &'a str,
    record: &'a [&'a str],
    /// What the Pi extension recorded of the conversation's ends.
    pi_exits: &'a [&'a str],
    fg_pid: Option<i64>,
    fg_changes: i64,
    last_exit: Option<i64>,
    left_fg_changes: Option<i64>,
    elsewhere: Elsewhere,
}

const PI: Left = Left {
    runtime: "pi",
    record: &[PI_PROMPT],
    pi_exits: &[PI_KILLED],
    fg_pid: Some(30),
    fg_changes: 1,
    last_exit: None,
    left_fg_changes: Some(1),
    elsewhere: Elsewhere::Nowhere,
};

fn planned(pane: &Left) -> Result<(Start, String)> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, pane.record.join("\n") + "\n")?;
    let store = Store::open(dir.path())?;
    let exits = crate::runtime::pi::exits(store.dir(), "b-conversation");
    std::fs::create_dir_all(crate::runtime::pi::records(store.dir()))?;
    std::fs::write(exits, pane.pi_exits.join("\n") + "\n")?;
    ended(&store, "server_gone")?;
    conversation(&store, "b", pane.runtime, &transcript)?;
    let conn = store.conn();
    conn.execute(
        "UPDATE occupant SET instance_id = 'i1', own_program = 0, reported_at = 1000,
           left_fg_changes = ?1",
        params![pane.left_fg_changes],
    )?;
    conn.execute(
        "INSERT INTO block (lineage, instance_id, fg_pid, fg_changes, last_exit)
         VALUES ('b', 'i1', ?1, ?2, ?3)",
        params![pane.fg_pid, pane.fg_changes, pane.last_exit],
    )?;
    if !matches!(pane.elsewhere, Elsewhere::Nowhere) {
        conn.execute("UPDATE conversation SET reported_at = 3000", [])?;
    }
    if matches!(pane.elsewhere, Elsewhere::InAnOpenPane) {
        conn.execute_batch(
            "INSERT INTO occupant (block_lineage, runtime, native_id, instance_id, pid,
               reported_at) SELECT 'c', runtime, native_id, 'i1', 40, 3000
             FROM occupant WHERE block_lineage = 'b'",
        )?;
    }
    let planned = candidate(conn, store.dir(), "i2", "b")?;
    Ok((planned.start, planned.plan.reason))
}

#[test]
fn a_conversation_a_signal_ended_reopens_without_a_new_turn() -> Result<()> {
    let (start, reason) = planned(&PI)?;
    assert_eq!(start, Start::Agent, "{reason}");
    let shell_ended = Left {
        fg_pid: None,
        fg_changes: 2,
        last_exit: Some(-1),
        ..PI
    };
    assert_eq!(planned(&shell_ended)?.0, Start::Agent);
    Ok(())
}

#[test]
fn a_conversation_left_without_a_signal_or_with_the_pane_used_since_keeps_the_shell() -> Result<()>
{
    for (case, pane) in [
        (
            "no signal",
            Left {
                pi_exits: &[],
                ..PI
            },
        ),
        (
            "another process's signal",
            Left {
                pi_exits: &[PI_OTHER_KILLED],
                ..PI
            },
        ),
        (
            "an earlier process's signal",
            Left {
                pi_exits: &[PI_KILLED_EARLIER],
                ..PI
            },
        ),
        (
            "another program ran since",
            Left {
                fg_changes: 3,
                ..PI
            },
        ),
        (
            "never seen leaving",
            Left {
                left_fg_changes: None,
                ..PI
            },
        ),
        (
            "reported from another pane since",
            Left {
                elsewhere: Elsewhere::InAnOpenPane,
                ..PI
            },
        ),
        (
            "reported from a pane that closed since",
            Left {
                elsewhere: Elsewhere::InAPaneClosedSince,
                ..PI
            },
        ),
        (
            "another program ran before the pane's end",
            Left {
                fg_pid: None,
                fg_changes: 3,
                last_exit: Some(-1),
                ..PI
            },
        ),
    ] {
        let (start, reason) = planned(&pane)?;
        assert_eq!(start, Start::Shell, "{case}: {reason}");
    }
    Ok(())
}

#[test]
fn each_runtime_reads_a_signals_end_from_its_own_record() -> Result<()> {
    const OMP_SIGNAL: &str = r#"{"type":"custom","customType":"session_exit","data":{"reason":"SIGTERM","kind":"signal","recordedAt":"1970-01-01T00:00:02Z"},"id":"e1"}"#;
    const OMP_NORMAL: &str = r#"{"type":"custom","customType":"session_exit","data":{"reason":"dispose","kind":"normal","recordedAt":"1970-01-01T00:00:03Z"},"id":"e2"}"#;
    const CODEX_STARTED: &str = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
    for (runtime, record, start) in [
        ("omp", &[OMP_SIGNAL][..], Start::Agent),
        ("omp", &[OMP_SIGNAL, OMP_NORMAL][..], Start::Shell),
        ("codex", &[CODEX_STARTED][..], Start::Shell),
    ] {
        let (planned, reason) = planned(&Left {
            runtime,
            record,
            ..PI
        })?;
        assert_eq!(planned, start, "{runtime} {record:?}: {reason}");
    }
    Ok(())
}
