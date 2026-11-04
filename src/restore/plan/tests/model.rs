use super::*;

#[test]
fn a_claude_conversation_reopens_with_the_model_and_effort_its_record_last_shows() -> Result<()> {
    // The conversation reported `claude-sonnet-5-5` when it started.
    let argv = |records: &[&str]| -> Result<Vec<String>> {
        let dir = tempfile::tempdir()?;
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, records.join("\n") + "\n")?;
        let store = Store::open(dir.path())?;
        conversation(&store, "b", "claude", &transcript)?;
        store.conn().execute_batch(
            "UPDATE conversation SET settings = '{\"model\":\"claude-sonnet-5-5\"}';
             INSERT INTO block (lineage, instance_id, fg_pid, status) VALUES ('b', 'i1', 10, 'idle')",
        )?;
        Ok(candidate(store.conn(), store.dir(), "i2", "b")?.plan.argv)
    };
    let resume = |settings: &[&str]| -> Vec<String> {
        ["claude", "--resume", "b-conversation"]
            .iter()
            .chain(settings)
            .map(|s| (*s).to_owned())
            .collect()
    };
    let opus = r#"{"type":"assistant","requestedModel":"claude-opus-5-5[1m]","effort":"xhigh","message":{"model":"claude-opus-5-5","role":"assistant","content":[]}}"#;
    // Claude Code's own resume drops the effort, and the reported model is the one it started
    // with. The requested model keeps the context-window suffix the response's model drops.
    assert_eq!(
        argv(&[opus])?,
        resume(&["--model", "claude-opus-5-5[1m]", "--effort", "xhigh"])
    );
    // A model without effort levels after a switch reopens without the effort of the one before.
    assert_eq!(
        argv(&[
            opus,
            r#"{"type":"assistant","requestedModel":"claude-haiku-4-5","message":{"model":"claude-haiku-4-5","role":"assistant","content":[]}}"#,
        ])?,
        resume(&["--model", "claude-haiku-4-5"])
    );
    // A synthetic answer, as for an API error, a subagent's records, and a skill's turn, which
    // may run at the skill's model and effort, are not what the conversation runs at.
    assert_eq!(
        argv(&[
            opus,
            r#"{"type":"assistant","isSidechain":true,"requestedModel":"claude-haiku-5-5","effort":"low","message":{"model":"claude-haiku-5-5","role":"assistant","content":[]}}"#,
            r#"{"type":"assistant","attributionSkill":"quick","requestedModel":"claude-haiku-5-5","effort":"low","message":{"model":"claude-haiku-5-5","role":"assistant","content":[]}}"#,
            r#"{"type":"assistant","message":{"model":"<synthetic>","role":"assistant","content":[]}}"#,
        ])?,
        resume(&["--model", "claude-opus-5-5[1m]", "--effort", "xhigh"])
    );
    // A record with no answer yet, or only answers in skills' turns, leaves the reported model.
    assert_eq!(
        argv(&[r#"{"type":"user","message":{"role":"user","content":"Go"}}"#])?,
        resume(&["--model", "claude-sonnet-5-5"])
    );
    assert_eq!(
        argv(&[
            r#"{"type":"assistant","attributionSkill":"quick","requestedModel":"claude-haiku-5-5","effort":"low","message":{"model":"claude-haiku-5-5","role":"assistant","content":[]}}"#,
        ])?,
        resume(&["--model", "claude-sonnet-5-5"])
    );
    Ok(())
}
