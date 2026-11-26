use super::*;

#[test]
fn status_shows_the_error_of_the_newest_epoch_only() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let insert = |store: &mut Store, error: Option<&str>| -> Result<()> {
        let tx = store.write()?;
        tx.execute(
            "INSERT INTO epoch (instance_id, started_at, ended_at, end_reason, error)
             VALUES ('i1', 1, 2, 'disconnected', ?1)",
            [error],
        )?;
        tx.commit()?;
        Ok(())
    };
    insert(&mut store, Some("UNIQUE constraint failed"))?;
    assert_eq!(
        run(dir.path(), &crate::install::Dirs::new(dir.path()))?["epoch_error"],
        json!({ "error": "UNIQUE constraint failed", "started_at": 1, "ended_at": 2 })
    );
    insert(&mut store, None)?;
    assert_eq!(
        run(dir.path(), &crate::install::Dirs::new(dir.path()))?["epoch_error"],
        Value::Null
    );
    Ok(())
}

#[test]
fn status_lists_missing_claude_hooks_unless_the_settings_cannot_be_read() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    Store::open(dir.path())?;
    let settings = home.path().join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap())?;
    let hook = json!({ "type": "command", "command": "rexume report claude" });
    std::fs::write(
        &settings,
        json!({ "hooks": { "SessionStart": [{ "hooks": [hook] }] } }).to_string(),
    )?;
    assert_eq!(
        run(dir.path(), &crate::install::Dirs::new(home.path()))?["claude_hooks_missing"],
        json!(["UserPromptSubmit", "Stop", "StopFailure"])
    );
    std::fs::write(&settings, "{")?;
    let status = run(dir.path(), &crate::install::Dirs::new(home.path()))?;
    assert_eq!(status["claude_hooks_missing"], Value::Null);
    assert_eq!(status["restores"], json!([]));
    Ok(())
}

#[test]
fn status_leaves_out_claimed_restores_and_opened_ones_whose_continuation_went() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let tx = store.write()?;
    tx.execute_batch(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'a', 1, 0), ('claude', 'b', 1, 0), ('claude', 'c', 1, 0),
             ('claude', 'd', 1, 0), ('claude', 'e', 1, 0), ('claude', 'f', 1, 0);
         INSERT INTO restore (id, instance_id, block_lineage, start, runtime, native_id, plan,
           state, note, created_at, typed_at, claimed_at, started, pid, continuation_state)
           VALUES ('claimed', 'i', 'a', 'agent', 'claude', 'a', '{}', 'claimed', NULL, 1, 1, 1,
               'agent', 7, NULL),
             ('opened', 'i', 'b', 'agent', 'claude', 'b', '{}', 'opened', NULL, 2, NULL, NULL,
               NULL, NULL, NULL),
             ('continued', 'i', 'c', 'continue', 'claude', 'c', '{}', 'opened', NULL, 3,
               NULL, NULL, NULL, NULL, 'sent'),
             ('not continued', 'i', 'd', 'continue', 'claude', 'd', '{}', 'opened', 'typed since',
               4, NULL, NULL, NULL, NULL, 'skipped'),
             ('skipped', 'i', 'e', 'agent', 'claude', 'e', '{}', 'skipped', 'closed', 5, NULL,
               NULL, NULL, NULL, NULL),
             ('waiting', 'i', 'f', 'continue', 'claude', 'f', '{}', 'opened', NULL, 6, NULL,
               NULL, NULL, NULL, 'pending');",
    )?;
    tx.commit()?;
    let status = run(dir.path(), &crate::install::Dirs::new(dir.path()))?;
    let listed: Vec<(&str, &Value)> = status["restores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|restore| (restore["id"].as_str().unwrap(), &restore["continuation"]))
        .collect();
    assert_eq!(
        listed,
        [
            ("not continued", &json!("skipped")),
            ("skipped", &Value::Null),
            ("waiting", &json!("pending"))
        ]
    );
    Ok(())
}
