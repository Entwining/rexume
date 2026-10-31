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
        Ok(candidate(store.conn(), "i2", "b")?.plan.argv)
    };
    let resume = |mode: &[&str]| -> Vec<String> {
        ["claude", "--resume", "b-conversation"]
            .iter()
            .chain(mode)
            .map(|s| (*s).to_owned())
            .collect()
    };
    // Claude Code's own resume would start this conversation in a new session's mode.
    assert_eq!(
        argv(&[
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s"}"#,
            r#"{"type":"user","permissionMode":"bypassPermissions","message":{"role":"user","content":"Go"}}"#,
        ])?,
        resume(&["--permission-mode", "bypassPermissions"])
    );
    // Switched to Manual after the last prompt; its resume would take a settings `defaultMode`.
    assert_eq!(
        argv(&[
            r#"{"type":"user","permissionMode":"auto","message":{"role":"user","content":"Go"}}"#,
            r#"{"type":"permission-mode","permissionMode":"default","sessionId":"s"}"#,
        ])?,
        resume(&["--permission-mode", "default"])
    );
    // A subagent's records in the same file, as Claude Code 2.1.85 wrote them, are not the
    // conversation's own.
    assert_eq!(
        argv(&[
            r#"{"type":"user","permissionMode":"default","message":{"role":"user","content":"Go"}}"#,
            r#"{"type":"user","isSidechain":true,"permissionMode":"acceptEdits","message":{"role":"user","content":"Look"}}"#,
        ])?,
        resume(&["--permission-mode", "default"])
    );
    // A record that shows no mode leaves it to Claude Code's resume.
    assert_eq!(
        argv(&[r#"{"type":"user","message":{"role":"user","content":"Go"}}"#])?,
        resume(&[])
    );
    Ok(())
}
