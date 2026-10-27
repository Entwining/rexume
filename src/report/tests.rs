use super::*;

#[test]
fn a_pi_session_without_a_file_is_recorded_but_not_resumable() -> Result<()> {
    let tui = json!({"session_id": "s", "session_file": "/p/s.jsonl", "mode": "tui"});
    assert!(parse(Runtime::Pi, &tui)?.resumable);
    let no_session = json!({"session_id": "s", "mode": "tui"});
    assert!(!parse(Runtime::Pi, &no_session)?.resumable);
    Ok(())
}

#[test]
fn a_resumed_claude_conversation_keeps_its_launch_model() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let process = Process {
        transport: Map::new(),
        keeps_session_env: false,
        instance: "i1".into(),
        pid: 100,
        name: Some("claude".into()),
        own_program: false,
        start_position: 0,
    };
    let model = |store: &mut Store, input: Value| -> Result<Option<String>> {
        let tx = store.write()?;
        let report = parse(Runtime::Claude, &input)?;
        record(&tx, "b", Runtime::Claude, &report, &process)?;
        let model = tx.query_row(
            "SELECT c.settings ->> '$.model' FROM occupant o
             JOIN conversation c USING (runtime, native_id) WHERE o.block_lineage = 'b'",
            [],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(model)
    };
    // Inputs as Claude Code sends them on startup and on resume.
    let startup = json!({"session_id": "s", "source": "startup", "model": "claude-haiku-5-5"});
    assert_eq!(
        model(&mut store, startup)?.as_deref(),
        Some("claude-haiku-5-5")
    );
    let resume = json!({"session_id": "s", "source": "resume"});
    assert_eq!(
        model(&mut store, resume)?.as_deref(),
        Some("claude-haiku-5-5")
    );
    let other = json!({"session_id": "t", "source": "resume"});
    assert_eq!(model(&mut store, other)?, None);
    Ok(())
}

fn process(pid: i64) -> Process {
    Process {
        transport: Map::new(),
        keeps_session_env: false,
        instance: "i1".into(),
        pid,
        name: Some("agent".into()),
        own_program: false,
        start_position: 0,
    }
}

/// A task bound to a report reads the record from where the report found it ending, so it starts
/// again for a new process, as a resumed conversation has, and for another conversation, as after
/// `/clear`, but not for the same process reporting the same conversation again, as after a
/// compaction, which may come in the middle of the turn a task is waiting for.
#[test]
fn a_new_process_or_conversation_moves_the_start_and_the_same_one_keeps_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let mut start = |native_id: &str, instance: &str, pid: i64, at: u64| -> Result<i64> {
        let tx = store.write()?;
        let report = parse(Runtime::Claude, &json!({ "session_id": native_id }))?;
        let process = Process {
            instance: instance.into(),
            start_position: at,
            ..process(pid)
        };
        record(&tx, "b", Runtime::Claude, &report, &process)?;
        let start = tx.query_row("SELECT start_position FROM occupant", [], |row| row.get(0))?;
        tx.commit()?;
        Ok(start)
    };
    assert_eq!(start("s", "i1", 100, 10)?, 10);
    assert_eq!(start("s", "i1", 100, 50)?, 10);
    assert_eq!(start("t", "i1", 100, 60)?, 60);
    assert_eq!(start("t", "i1", 200, 70)?, 70);
    assert_eq!(start("t", "i2", 200, 80)?, 80);
    Ok(())
}

/// A PID names a process only on its own server instance, so a start the runtime does not mark
/// takes the pane from an earlier instance's occupant with the same PID and name.
#[test]
fn an_unmarked_start_on_a_new_instance_is_not_the_old_occupants_inner_session() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let starts = [
        (
            "i1",
            json!({"session_id": "old", "transcript_path": "/x/rollout-old.jsonl"}),
        ),
        (
            "i2",
            json!({"session_id": "new", "transcript_path": null, "source": "startup"}),
        ),
    ];
    for (instance, input) in starts {
        let tx = store.write()?;
        let process = Process {
            instance: instance.into(),
            ..process(100)
        };
        record(
            &tx,
            "b",
            Runtime::Codex,
            &parse(Runtime::Codex, &input)?,
            &process,
        )?;
        tx.commit()?;
    }
    let occupant: String = store
        .conn()
        .query_row("SELECT native_id FROM occupant", [], |row| row.get(0))?;
    assert_eq!(occupant, "new");
    Ok(())
}

/// Reports each input from its process, in order, and returns the conversations recorded and the
/// pane's occupant with whether it can be reopened.
fn report_all(runtime: Runtime, inputs: &[(i64, &Value)]) -> Result<(Vec<String>, String, bool)> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    for (pid, input) in inputs {
        let tx = store.write()?;
        record(&tx, "b", runtime, &parse(runtime, input)?, &process(*pid))?;
        tx.commit()?;
    }
    let conn = store.conn();
    let conversations = conn
        .prepare("SELECT native_id FROM conversation ORDER BY native_id")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let (occupant, resumable) = conn.query_row(
        "SELECT native_id, resumable FROM occupant JOIN conversation USING (runtime, native_id)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((conversations, occupant, resumable))
}

/// Each runtime starts sessions of its own inside the pane's agent process, so their session starts
/// come from the pane's foreground program too. Inputs as each runtime sends them: Claude Code for
/// a subagent and an in-process teammate, Codex for a guardian review and a side conversation, and
/// Pi for sessions an extension creates through the SDK in each mode but `tui`.
#[test]
fn an_agents_inner_sessions_do_not_replace_the_pane_conversation() -> Result<()> {
    let cases = [
        (
            Runtime::Claude,
            json!({"session_id": "main", "transcript_path": "/c/main.jsonl", "source": "startup"}),
            vec![
                json!({"session_id": "inner", "transcript_path": "/c/inner.jsonl",
                       "source": "startup", "agent_id": "a1", "agent_type": "Explore"}),
                json!({"session_id": "inner", "transcript_path": "/c/inner.jsonl",
                       "source": "startup", "agent_id": "lead@team", "agent_type": "lead"}),
            ],
        ),
        (
            Runtime::Codex,
            json!({"session_id": "main", "transcript_path": "/x/rollout-main.jsonl",
                   "hook_event_name": "SessionStart", "model": "big", "permission_mode": "default",
                   "source": "startup"}),
            vec![
                json!({"session_id": "inner", "transcript_path": null,
                       "hook_event_name": "SessionStart", "model": "small",
                       "permission_mode": "bypassPermissions", "source": "startup"}),
                json!({"session_id": "inner", "transcript_path": null,
                       "hook_event_name": "SessionStart", "model": "big",
                       "permission_mode": "default", "source": "fork"}),
            ],
        ),
        (
            Runtime::Pi,
            json!({"session_id": "main", "session_file": "/p/main.jsonl", "mode": "tui"}),
            ["print", "json", "rpc"]
                .map(|mode| json!({"session_id": "inner", "mode": mode, "model": "small"}))
                .to_vec(),
        ),
    ];
    for (runtime, main, inner) in cases {
        let inputs: Vec<_> = std::iter::once(&main)
            .chain(&inner)
            .map(|i| (100, i))
            .collect();
        let (conversations, occupant, resumable) = report_all(runtime, &inputs)?;
        assert_eq!(
            (conversations, occupant.as_str(), resumable),
            (vec!["main".to_owned()], "main", true),
            "{runtime}"
        );
    }
    Ok(())
}

/// A start the runtime marks neither way is the agent's own conversation when it comes from a new
/// process, such as an ephemeral `codex exec` or a `pi -p` run after the pane's conversation ended,
/// and so is a Claude Code session started with `--agent`, which carries only `agent_type`.
#[test]
fn a_new_process_takes_the_pane_even_without_the_runtimes_mark() -> Result<()> {
    let cases = [
        (
            Runtime::Claude,
            json!({"session_id": "old", "transcript_path": "/c/old.jsonl"}),
            json!({"session_id": "new", "transcript_path": "/c/new.jsonl", "agent_type": "lead"}),
            true,
        ),
        (
            Runtime::Codex,
            json!({"session_id": "old", "transcript_path": "/x/rollout-old.jsonl"}),
            json!({"session_id": "new", "transcript_path": null, "source": "startup"}),
            false,
        ),
        (
            Runtime::Pi,
            json!({"session_id": "old", "session_file": "/p/old.jsonl", "mode": "tui"}),
            json!({"session_id": "new", "session_file": "/p/new.jsonl", "mode": "print"}),
            false,
        ),
    ];
    for (runtime, old, new, reopens) in cases {
        let (_, occupant, resumable) = report_all(runtime, &[(100, &old), (200, &new)])?;
        assert_eq!(
            (occupant.as_str(), resumable),
            ("new", reopens),
            "{runtime}"
        );
    }
    Ok(())
}

#[test]
fn a_claude_report_replaces_launch_settings_and_merges_model_and_effort() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let tx = store.write()?;
    let mut report = parse(
        Runtime::Claude,
        &json!({"session_id": "s", "model": "big", "effort": "high"}),
    )?;
    let argv = ["claude", "--remote-control"].map(str::to_owned);
    report.settings.extend(claude::launch_settings(
        &argv,
        [
            ("CLAUDE_CODE_USE_VERTEX".into(), "1".into()),
            ("CLOUD_ML_REGION".into(), "global".into()),
        ],
    ));
    record(&tx, "b", Runtime::Claude, &report, &process(100))?;
    let saved = || -> Result<Value> {
        Ok(tx.query_row(
            "SELECT settings FROM conversation WHERE runtime = 'claude' AND native_id = 's'",
            [],
            |row| row.get(0),
        )?)
    };
    assert_eq!(
        saved()?,
        json!({
            "model": "big", "effort": "high", "launch_flags": ["--remote-control"],
            "launch_env": {"CLAUDE_CODE_USE_VERTEX": "1", "CLOUD_ML_REGION": "global"}
        })
    );
    let partial = Report {
        replace_settings: &[],
        ..parse(Runtime::Claude, &json!({"session_id": "s"}))?
    };
    record(&tx, "b", Runtime::Claude, &partial, &process(100))?;
    assert_eq!(saved()?["launch_flags"], json!(["--remote-control"]));
    assert_eq!(saved()?["launch_env"]["CLAUDE_CODE_USE_VERTEX"], "1");
    let mut resume = parse(
        Runtime::Claude,
        &json!({"session_id": "s", "source": "resume"}),
    )?;
    resume.settings.extend(claude::launch_settings(
        &[],
        [("CLAUDE_CODE_USE_BEDROCK".into(), "1".into())],
    ));
    record(&tx, "b", Runtime::Claude, &resume, &process(100))?;
    assert_eq!(
        saved()?,
        json!({
            "model": "big", "effort": "high", "launch_env": {"CLAUDE_CODE_USE_BEDROCK": "1"}
        })
    );
    resume.settings = claude::launch_settings(&[], []);
    record(&tx, "b", Runtime::Claude, &resume, &process(100))?;
    assert_eq!(saved()?, json!({"model": "big", "effort": "high"}));
    tx.commit()?;
    Ok(())
}
