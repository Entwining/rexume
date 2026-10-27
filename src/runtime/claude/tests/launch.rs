use super::*;

#[test]
fn a_resume_carries_launch_flags_and_sorted_provider_environment() {
    let c = Conversation {
        native_id: "id".into(),
        settings: serde_json::from_value(json!({
            "model": "big",
            "permission_mode": "default",
            "launch_flags": ["--remote-control"],
            "launch_env": {"B": "two words", "A": "1"}
        }))
        .unwrap(),
        ..Conversation::default()
    };
    assert_eq!(
        resume_argv(&c),
        [
            "env",
            "A=1",
            "B=two words",
            "claude",
            "--remote-control",
            "--resume",
            "id",
            "--model",
            "big",
            "--permission-mode",
            "default"
        ]
    );
}

#[test]
fn launch_flags_keep_only_the_allowlist_and_consume_values_once() {
    let argv = [
        "claude",
        "a launch prompt",
        "-p",
        "print prompt",
        "--print",
        "another prompt",
        "--resume",
        "old",
        "-r",
        "old",
        "--continue",
        "-c",
        "--session-id",
        "new",
        "--model",
        "big",
        "--effort",
        "low",
        "--permission-mode",
        "default",
        "--dangerously-skip-permissions",
        "--remote-control",
        "--add-dir",
        "/first",
        "--settings=one.json",
        "--add-dir=/second",
        "--settings",
        "two.json",
        "--mcp-config",
        "one-mcp.json",
        "--strict-mcp-config",
        "--mcp-config=two-mcp.json",
        "--chrome",
        "--no-chrome",
        "--settings",
        "--remote-control",
        "--add-dir",
    ]
    .map(str::to_owned);
    let saved = launch_settings(&argv, []);
    assert_eq!(
        saved,
        serde_json::from_value::<Map<String, Value>>(json!({
            "launch_flags": [
                "--remote-control", "--add-dir", "/first", "--settings", "one.json",
                "--add-dir", "/second", "--settings", "two.json", "--mcp-config", "one-mcp.json",
                "--strict-mcp-config", "--mcp-config", "two-mcp.json", "--chrome", "--no-chrome",
                "--remote-control"
            ]
        }))
        .unwrap()
    );
}

#[test]
fn launch_environment_never_records_credentials_or_unrelated_variables() {
    let env = [
        ("CLAUDE_CODE_USE_VERTEX", "1"),
        ("CLAUDE_CODE_USE_BEDROCK", "1"),
        ("CLAUDE_CODE_USE_FOUNDRY", "1"),
        ("CLOUD_ML_REGION", "global"),
        ("ANTHROPIC_VERTEX_PROJECT_ID", "example-project"),
        ("ANTHROPIC_MODEL", "big"),
        ("ANTHROPIC_SMALL_FAST_MODEL", "small"),
        ("AWS_REGION", "us-east-1"),
        ("AWS_PROFILE", "example"),
        ("ANTHROPIC_API_KEY", "synthetic-key"),
        ("ANTHROPIC_AUTH_TOKEN", "synthetic-token"),
        ("OTHER", "unrelated"),
    ]
    .map(|(name, value)| (name.into(), value.into()));
    assert_eq!(launch_settings(&[], env), serde_json::from_value::<Map<String, Value>>(json!({
        "launch_env": {
            "CLAUDE_CODE_USE_VERTEX": "1", "CLAUDE_CODE_USE_BEDROCK": "1",
            "CLAUDE_CODE_USE_FOUNDRY": "1", "CLOUD_ML_REGION": "global",
            "ANTHROPIC_VERTEX_PROJECT_ID": "example-project", "ANTHROPIC_MODEL": "big",
            "ANTHROPIC_SMALL_FAST_MODEL": "small", "AWS_REGION": "us-east-1", "AWS_PROFILE": "example"
        }
    })).unwrap());
}

#[test]
fn empty_launch_settings_and_positional_flags_are_omitted() {
    let argv = ["--remote-control", "--", "--chrome"].map(str::to_owned);
    assert!(launch_settings(&argv, [("AWS_PROFILE".into(), OsString::new())]).is_empty());
    assert!(launch_settings(&[], []).is_empty());
}

#[test]
fn a_continuation_prompt_follows_only_single_value_flags() {
    let prompt = "Continue the interrupted turn.";
    for flags in [json!(["--remote-control"]), json!(["--add-dir", "/x"])] {
        let c = Conversation {
            native_id: "id".into(),
            settings: serde_json::from_value(json!({"launch_flags": flags})).unwrap(),
            ..Conversation::default()
        };
        let saved = resume_argv(&c);
        let argv: Vec<_> = saved
            .iter()
            .map(String::as_str)
            .chain(Some(prompt))
            .collect();
        assert_eq!(argv.last(), Some(&prompt));
        assert_eq!(&argv[argv.len() - 3..], ["--resume", "id", prompt]);
    }
}

#[test]
fn launch_flags_keep_optional_names_and_all_path_values() {
    let argv = [
        "claude",
        "--rc",
        "first",
        "--remote-control",
        "second",
        "--rc",
        "--add-dir",
        "/a",
        "/b",
        "--mcp-config",
        "a.json",
        "b.json",
        "--chrome",
    ]
    .map(str::to_owned);
    assert_eq!(
        launch_settings(&argv, [])["launch_flags"],
        json!([
            "--rc",
            "first",
            "--remote-control",
            "second",
            "--rc",
            "--add-dir",
            "/a",
            "/b",
            "--mcp-config",
            "a.json",
            "b.json",
            "--chrome"
        ])
    );
}

#[test]
fn launch_flags_drop_inline_json_and_keep_paths() {
    let settings = r#" {"env":{"ANTHROPIC_AUTH_TOKEN":"synthetic-token"}}"#;
    let mcp = r#" [{"headers":{"Authorization":"synthetic-token"}}]"#;
    for argv in [
        vec![
            "claude".into(),
            "--settings".into(),
            settings.into(),
            "--mcp-config".into(),
            mcp.into(),
        ],
        vec![
            "claude".into(),
            format!("--settings={settings}"),
            format!("--mcp-config={mcp}"),
        ],
    ] {
        let saved = launch_settings(&argv, []);
        assert!(saved.is_empty());
        assert!(
            !serde_json::to_string(&saved)
                .unwrap()
                .contains("synthetic-token")
        );
    }
    let argv = [
        "claude",
        "--mcp-config",
        mcp,
        "a.json",
        settings,
        "b.json",
        "--settings",
        "c.json",
    ]
    .map(str::to_owned);
    assert_eq!(
        launch_settings(&argv, [])["launch_flags"],
        json!(["--mcp-config", "a.json", "b.json", "--settings", "c.json"])
    );
}

#[test]
fn launch_environment_skips_non_unicode_names_and_values() {
    use std::os::unix::ffi::OsStringExt;

    let invalid = OsString::from_vec(vec![0xff]);
    let env = [
        (invalid.clone(), "synthetic-token".into()),
        ("OTHER".into(), invalid.clone()),
        ("AWS_PROFILE".into(), invalid),
        ("CLAUDE_CODE_USE_BEDROCK".into(), "1".into()),
    ];
    assert_eq!(
        launch_settings(&[], env)["launch_env"],
        json!({
            "CLAUDE_CODE_USE_BEDROCK": "1"
        })
    );
}
