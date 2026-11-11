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
        run(dir.path(), dir.path())?["epoch_error"],
        json!({ "error": "UNIQUE constraint failed", "started_at": 1, "ended_at": 2 })
    );
    insert(&mut store, None)?;
    assert_eq!(run(dir.path(), dir.path())?["epoch_error"], Value::Null);
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
        run(dir.path(), home.path())?["claude_hooks_missing"],
        json!(["UserPromptSubmit", "Stop", "StopFailure"])
    );
    std::fs::write(&settings, "{")?;
    let status = run(dir.path(), home.path())?;
    assert_eq!(status["claude_hooks_missing"], Value::Null);
    assert_eq!(status["restores"], json!([]));
    Ok(())
}
