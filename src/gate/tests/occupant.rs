use super::*;
use crate::task::Follow;
use crate::task::tests::{record, task_world};

type Occupant = (
    String,
    Option<String>,
    i64,
    Option<String>,
    Option<bool>,
    i64,
);

fn occupant(store: &Store) -> Result<Option<Occupant>> {
    Ok(store
        .conn()
        .query_row(
            "SELECT native_id, instance_id, pid, pid_name, own_program, start_position
             FROM occupant WHERE block_lineage = 'b'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?)
}

#[test]
fn a_claim_records_a_conversation_it_starts_as_the_terminals_own_program() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(&transcript, format!("{PROMPT}\n"))?;
    let length = i64::try_from(std::fs::metadata(&transcript)?.len())?;
    let shell_started = tempfile::tempdir()?;
    let mut store = planned(shell_started.path(), &transcript, false)?;
    claim(&mut store, "op", 0, "block:new", "session:new")?;
    assert_eq!(occupant(&store)?, None);

    let mut store = planned(dir.path(), &transcript, true)?;
    claim(&mut store, "op", 0, "block:new", "session:new")?;
    let pid = i64::from(std::process::id());
    assert_eq!(
        occupant(&store)?,
        Some(("c".into(), Some("i2".into()), pid, None, Some(true), length))
    );
    // The conversation's own reports come from the same process, under the name Rex gives it.
    let process = |start_position| Process {
        transport: Map::new(),
        keeps_session_env: false,
        instance: "i2".into(),
        pid,
        name: Some("claude".into()),
        own_program: true,
        start_position,
    };
    let report = |native_id: &str, role| Report {
        native_id: native_id.into(),
        role,
        transcript: None,
        resumable: role == Role::Own,
        settings: Map::new(),
        record_length: None,
    };
    let tx = store.write()?;
    report::record(
        &tx,
        "b",
        Runtime::Claude,
        &report("inner", Role::Unmarked),
        &process(0),
    )?;
    report::record(
        &tx,
        "b",
        Runtime::Claude,
        &report("c", Role::Own),
        &process(900),
    )?;
    tx.commit()?;
    assert_eq!(
        occupant(&store)?,
        Some((
            "c".into(),
            Some("i2".into()),
            pid,
            Some("claude".into()),
            Some(true),
            length
        ))
    );
    Ok(())
}

#[test]
fn a_request_reaches_a_restored_worker_that_has_not_reported() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let plan = Plan {
        transcript: Some(transcript.path().to_string_lossy().into_owned()),
        own_program: true,
        ..Plan::default()
    };
    // Pi worker `w` reported from pane `b` on instance `i1`; the server restarted as `i2`, which
    // the daemon observes, and runs this process as the pane's foreground program.
    store.conn().execute(
        "INSERT INTO conversation (runtime, native_id, transcript, resumable, reported_at)
         VALUES ('pi', 'w', ?1, 1, 50)",
        params![transcript.path().to_string_lossy()],
    )?;
    store.conn().execute_batch(&format!(
        "UPDATE occupant SET runtime = 'pi', native_id = 'w', instance_id = 'i1'
           WHERE block_lineage = 'b';
         UPDATE task SET worker_runtime = 'pi', worker_native_id = 'w';
         UPDATE task_round SET state = 'completed', result = 'Done.', result_ref = 'a0',
           finished_at = 1;
         INSERT INTO task_round (task_id, round, state, created_at) VALUES ('t', 2, 'running', 2);
         INSERT INTO request (id, task_id, round, text) VALUES ('q', 't', 2, 'go');
         UPDATE epoch SET ended_at = 3, end_reason = 'server_gone';
         INSERT INTO epoch (id, instance_id, started_at) VALUES (2, 'i2', 4);
         UPDATE block SET instance_id = 'i2', fg_pid = {}, status = NULL, status_pid = NULL
           WHERE lineage = 'b';
         INSERT INTO restore_op (id, instance_id, session_lineage, state, doc, created_at)
           VALUES ('op', 'i2', 's', 'sent', '{{}}', 5);",
        std::process::id()
    ))?;
    store.conn().execute(
        "INSERT INTO restore_slot (op_id, slot, instance_id, block_lineage, start, runtime,
           native_id, plan) VALUES ('op', 0, 'i2', 'b', 'agent', 'pi', 'w', ?1)",
        params![serde_json::to_string(&plan)?],
    )?;
    let request = |store: &Store| -> Result<(String, Option<String>)> {
        Ok(store
            .conn()
            .query_row("SELECT state, note FROM request", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?)
    };
    let mut follow = Follow::default();
    follow.pass(&mut store, None)?;
    assert_eq!(
        request(&store)?.1.as_deref(),
        Some("waiting for the worker's conversation to run in its pane")
    );
    claim(&mut store, "op", 0, "block:new", "session:new")?;
    follow.pass(&mut store, None)?;
    assert_eq!(request(&store)?, ("sent".into(), None));
    Ok(())
}
