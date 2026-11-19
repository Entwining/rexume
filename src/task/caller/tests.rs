use super::*;

#[test]
fn a_worker_is_local_only_when_its_host_equals_the_servers() {
    let list = json!({ "blocks": [
        { "block_id": "block:here", "host_id": "h1" },
        { "block_id": "block:there", "host_id": "h2" },
        { "block_id": "block:unknown" },
    ] });
    assert!(on_host(&list, "block:here", Some("h1")).is_ok());
    assert!(on_host(&list, "block:unknown", None).is_ok());
    for (worker, host) in [
        ("block:there", Some("h1")),
        ("block:unknown", Some("h1")),
        ("block:here", None),
    ] {
        let refused = on_host(&list, worker, host).unwrap_err().to_string();
        assert!(refused.contains("another machine"), "{worker}: {refused}");
    }
    let missing = on_host(&list, "block:gone", Some("h1"))
        .unwrap_err()
        .to_string();
    assert_eq!(missing, "pane block:gone is not on the server");
}

#[test]
fn an_opencode_command_belongs_to_the_recorded_session_that_runs_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    store.conn().execute(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
         VALUES ('opencode', 'ses_a', 1, 0)",
        [],
    )?;
    assert_eq!(recorded_session(store.conn(), "ses_a".into())?, "ses_a");
    assert!(recorded_session(store.conn(), "ses_subagent".into()).is_err());
    Ok(())
}
