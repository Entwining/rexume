use super::*;

/// An epoch that ended with an error shows until a later epoch replaces it, so a daemon that keeps
/// failing to observe the server is seen in `status`, and one that recovered is not.
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
        run(dir.path())?["epoch_error"],
        json!({ "error": "UNIQUE constraint failed", "started_at": 1, "ended_at": 2 })
    );
    insert(&mut store, None)?;
    assert_eq!(run(dir.path())?["epoch_error"], Value::Null);
    Ok(())
}
