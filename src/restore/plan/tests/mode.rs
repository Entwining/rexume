use super::*;

#[test]
fn a_claude_conversation_reopens_in_the_mode_its_record_last_shows() -> Result<()> {
    let argv = |records: &[&str]| -> Result<Vec<String>> {
        let dir = tempfile::tempdir()?;
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, records.join("\n") + "\n")?;
        let store = Store::open(dir.path())?;
        conversation(&store, "b", "claude", &transcript)?;
        store.conn().execute_batch(
            "INSERT INTO block (lineage, instance_id, fg_pid, status) VALUES ('b', 'i1', 10, 'idle')",
        )?;
        Ok(candidate(store.conn(), store.dir(), "i2", "b")?.plan.argv)
    };
    let resume = |mode: &[&str]| -> Vec<String> {
        ["claude", "--resume", "b-conversation"]
            .iter()
            .chain(mode)
            .map(|s| (*s).to_owned())
            .collect()
    };
    assert_eq!(
        argv(&[
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s"}"#,
            r#"{"type":"user","permissionMode":"bypassPermissions","message":{"role":"user","content":"Go"}}"#,
        ])?,
        resume(&["--permission-mode", "bypassPermissions"])
    );
    assert_eq!(
        argv(&[
            r#"{"type":"user","permissionMode":"auto","message":{"role":"user","content":"Go"}}"#,
            r#"{"type":"permission-mode","permissionMode":"default","sessionId":"s"}"#,
        ])?,
        resume(&["--permission-mode", "default"])
    );
    assert_eq!(
        argv(&[
            r#"{"type":"user","permissionMode":"default","message":{"role":"user","content":"Go"}}"#,
            r#"{"type":"user","isSidechain":true,"permissionMode":"acceptEdits","message":{"role":"user","content":"Look"}}"#,
        ])?,
        resume(&["--permission-mode", "default"])
    );
    assert_eq!(
        argv(&[r#"{"type":"user","message":{"role":"user","content":"Go"}}"#])?,
        resume(&[])
    );
    Ok(())
}
