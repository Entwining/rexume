use std::path::Path;

use super::*;

#[test]
fn classifies_server_answers_apart_from_lost_calls() {
    let rejected = classify(
        "Error: stream response \"control-2\" failed: block not found: block:x (status=404 code=not_found)\n",
    );
    assert!(matches!(rejected, CallError::Rejected { ref code, .. } if code == "not_found"));
    assert!(rejected.not_found());
    let gone = classify(
        "Error: stream response \"control-1\" failed: session session:x not found (status=404 code=session_not_found)\n",
    );
    assert!(gone.not_found());
    let unknown = classify("Error: server went away: reading API control: EOF\n");
    assert!(matches!(unknown, CallError::Unknown(_)));
}

#[test]
fn decodes_the_socket_path() {
    let cli = Cli {
        bin: "rex".into(),
        endpoint: "unix:///Users/me/Library/Application%20Support/rex/server.sock".into(),
    };
    assert_eq!(
        cli.socket_path().unwrap(),
        PathBuf::from("/Users/me/Library/Application Support/rex/server.sock")
    );
}

#[test]
fn an_ambiguous_pane_is_refused_with_its_reason_and_an_unknown_status_is_not_found() {
    let ambiguous = json!({ "results": [{
        "query": " ", "status": "ambiguous", "reason": "multiple sessions are available",
        "candidates": [{ "kind": "session", "session_id": "session:s" }],
    }] });
    let blank = resolved_session(&ambiguous, " ").unwrap_err().to_string();
    assert!(
        blank.contains("ambiguous: multiple sessions are available"),
        "{blank}"
    );
    let unknown = json!({ "results": [{ "status": "pending", "target": { "session_id": "s" } }] });
    assert_eq!(resolved_session(&unknown, "block:b").unwrap(), None);
}

fn quoted(path: &Path) -> String {
    shlex::try_quote(&path.to_string_lossy())
        .unwrap()
        .into_owned()
}

/// A `rex` that runs `body`, a shell script.
fn fake_rex(dir: &Path, body: &str) -> Cli {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("rex");
    std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&bin, PermissionsExt::from_mode(0o755)).unwrap();
    Cli {
        bin,
        endpoint: "unix:///rex.sock".into(),
    }
}

#[test]
fn the_adapters_arguments_go_on_stdin_and_stay_off_argv() {
    let dir = tempfile::tempdir().unwrap();
    let (argv, stdin) = (dir.path().join("argv"), dir.path().join("stdin"));
    let cli = fake_rex(
        dir.path(),
        &format!(
            "printf '%s\\n' \"$@\" > {}\n/bin/cat > {}",
            quoted(&argv),
            quoted(&stdin)
        ),
    );
    let relay = [
        "rexume".to_owned(),
        "relay".to_owned(),
        "/d.sock".to_owned(),
    ];
    let token = "019a0000-0000-7000-8000-000000000000";
    let status = cli.adapter(&relay, token).unwrap().wait().unwrap();
    assert!(status.success());

    let argv = std::fs::read_to_string(&argv).unwrap();
    let words: Vec<&str> = argv.lines().collect();
    let args = words.iter().position(|w| *w == "--args").unwrap();
    assert_eq!(words[args + 1], "-");
    assert!(!argv.contains(token), "{argv}");
    // The fake exits only once `cat` reads to the end, so its exit shows the pipe was closed.
    let sent: Value = serde_json::from_slice(&std::fs::read(&stdin).unwrap()).unwrap();
    assert_eq!(sent, json!({ "relay": relay, "token": token }));
}

#[test]
fn a_rex_do_that_cannot_take_its_arguments_is_ended() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("pid");
    let cli = fake_rex(
        dir.path(),
        &format!(
            "echo $$ > {}\nexec 0<&-\nexec /bin/sleep 600 2>/dev/null",
            quoted(&pid)
        ),
    );
    // More than a pipe holds, so the write is still blocked when `rex do` closes its end.
    let relay = ["x".repeat(1 << 20)];
    cli.adapter(&relay, "token").unwrap_err();
    let pid: i64 = std::fs::read_to_string(&pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!crate::proc::alive(pid));
}

#[test]
fn reads_lua_lists_leniently() {
    let reply = json!({ "windows": {}, "sessions": [{ "session_id": "s" }] });
    assert!(items(&reply, "/windows").is_empty());
    assert!(items(&reply, "/absent").is_empty());
    assert_eq!(items(&reply, "/sessions").len(), 1);
    assert!(listed_blocks(&json!({ "blocks": {} })).is_empty());
    let list = json!({ "blocks": [{ "block_id": "block:a", "placement": "detached" }, {}] });
    assert_eq!(listed_blocks(&list), ["block:a"]);
}
