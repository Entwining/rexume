use serde_json::Value;

use super::*;

/// Loads the plugin's hooks module in Node and runs its `session.receive` hook on each delivery
/// with a fake `$.prompt.submit` that enters the prompt, has it dropped, or throws, as the case
/// says. Prints, for each, what the hook returned or that it threw, and which of submit and `next`
/// it called.
const PLUGIN_HARNESS: &str = r#"
    const [plugin, cases] = process.argv.slice(1);
    let hook;
    (await import(plugin)).register((event, h) => event === "session.receive" && (hook = h));
    const out = [];
    for (const { event, submit } of JSON.parse(cases)) {
      const calls = { submitted: [], next: 0 };
      const $ = { prompt: { submit: async (p) => {
        calls.submitted.push(p);
        if (submit === "throw") throw new Error("unsupported");
        return submit === "drop" ? { drop: "the session was stopped" } : { text: p.text };
      } } };
      const result = await hook($, event, (e) => (calls.next++, { passed: e === event }))
        .catch(() => "threw");
      out.push({ result, ...calls });
    }
    console.log(JSON.stringify(out));
"#;

/// A notice or request written to the socket is submitted as the user's prompt and not delivered
/// again; anything else, and one whose prompt did not enter, goes on as Claude Code delivers it,
/// and a submit that throws fails the hook for Claude Code to skip. The plugin takes a text exactly
/// when the turn hook reads a token from it (`leading_token`).
#[test]
fn the_plugin_resubmits_only_rexume_lines_from_the_socket() -> Result<()> {
    let node = crate::runtime::tests::node()?;
    let dir = tempfile::tempdir()?;
    let plugin = dir.path().join("rexume.ts");
    std::fs::write(&plugin, PLUGIN)?;
    let line = |text: &str| json!({ "origin": { "kind": "peer" }, "text": text });
    let notice = "[rexume d1] Worker task t finished.";
    // What the hook returned, how often it called `next`, and what it submitted.
    let outcome = |result: Value, next: u32, text: Option<&str>| {
        let submitted: Vec<Value> = text
            .map(|text| json!({ "text": text, "asUser": true }))
            .into_iter()
            .collect();
        json!({ "result": result, "next": next, "submitted": submitted })
    };
    let passed = || json!({ "passed": true });
    let consumed = json!({ "consumed": "submitted as the user's prompt" });
    let mut cases = vec![
        (line(notice), "drop", outcome(passed(), 1, Some(notice))),
        (
            line(notice),
            "throw",
            outcome(json!("threw"), 0, Some(notice)),
        ),
    ];
    for text in [
        notice,
        "count",
        "see [rexume d1]",
        "[rexume ] x",
        "[rexume a b] x",
    ] {
        let wanted = match leading_token(text) {
            Some(_) => outcome(consumed.clone(), 0, Some(text)),
            None => outcome(passed(), 1, None),
        };
        cases.push((line(text), "entered", wanted));
    }
    for event in [
        json!({ "origin": { "kind": "bridge" }, "text": notice }),
        json!({ "origin": { "kind": "peer", "plugin": "other" }, "text": notice }),
        json!({ "origin": { "kind": "peer", "teammate": "t" }, "text": notice }),
        json!({ "origin": { "kind": "peer" }, "text": notice, "agentId": "a" }),
    ] {
        cases.push((event, "entered", outcome(passed(), 1, None)));
    }
    let input: Vec<Value> = cases
        .iter()
        .map(|(event, submit, _)| json!({ "event": event, "submit": submit }))
        .collect();
    let out = std::process::Command::new(&node)
        .args(["--input-type=module", "-e", PLUGIN_HARNESS])
        .arg(&plugin)
        .arg(Value::from(input).to_string())
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let results: Vec<Value> = serde_json::from_slice(&out.stdout)?;
    assert_eq!(results.len(), cases.len());
    for ((event, submit, wanted), got) in cases.iter().zip(&results) {
        assert_eq!(got, wanted, "{event} {submit}");
    }
    Ok(())
}
