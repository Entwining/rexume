use serde_json::json;

use super::*;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn flags_take_their_values_as_claude_codes_parser_does() {
    for (argv, kept) in [
        (
            &[
                "claude",
                "a launch prompt",
                "--model",
                "big",
                "--resume",
                "id",
                "--dangerously-skip-permissions",
                "--rc",
            ][..],
            &["--rc"][..],
        ),
        (
            &["claude", "--rc", "name", "--remote-control", "--chrome"],
            &["--rc", "name", "--remote-control", "--chrome"],
        ),
        (
            &[
                "claude",
                "--add-dir",
                "/a",
                "/b",
                "--settings",
                "-s.json",
                "--tools",
                "",
            ],
            &[
                "--add-dir",
                "/a",
                "/b",
                "--settings",
                "-s.json",
                "--tools",
                "",
            ],
        ),
        (
            &[
                "claude",
                "--add-dir=/a",
                "prompt",
                "--plugin-dir",
                "p",
                "prompt",
            ],
            &["--add-dir=/a", "--plugin-dir", "p"],
        ),
        (&["claude", "--", "--rc"], &[]),
    ] {
        assert_eq!(flags(&strings(argv)), kept, "{argv:?}");
    }
}

#[test]
fn inline_json_settings_are_dropped_and_a_flag_left_without_a_path_with_them() {
    let token = r#"{"headers":{"Authorization":"synthetic-token"}}"#;
    let attached = format!("--settings={token}");
    let argv = [
        "claude",
        "--mcp-config",
        token,
        "a.json",
        "--settings",
        token,
        &attached,
        "--strict-mcp-config",
        "--rc",
        "[wip] auth",
    ];
    assert_eq!(
        flags(&strings(&argv)),
        [
            "--mcp-config",
            "a.json",
            "--strict-mcp-config",
            "--rc",
            "[wip] auth"
        ]
    );
}

#[test]
fn env_keeps_provider_variables_sorted_and_nothing_else() {
    let entries = strings(&[
        "CLOUD_ML_REGION=global",
        "ANTHROPIC_API_KEY=synthetic-key",
        "AWS_BEARER_TOKEN_BEDROCK=synthetic-token",
        "ANTHROPIC_BEDROCK_BASE_URL=https://user:synthetic-token@gateway.example",
        "ANTHROPIC_MODEL=big",
        "VERTEX_REGION_CLAUDE_HAIKU_4_5=us-east5",
        "AWS_PROFILE=",
        "CLAUDE_CODE_USE_VERTEX=1",
        "ptr_munge=",
    ]);
    assert_eq!(
        env(&entries),
        [
            "AWS_PROFILE=",
            "CLAUDE_CODE_USE_VERTEX=1",
            "CLOUD_ML_REGION=global",
            "VERTEX_REGION_CLAUDE_HAIKU_4_5=us-east5"
        ]
    );
}

fn launch(vars: &[&str]) -> proc::Launch {
    let mut env = strings(&["CLAUDE_CODE_USE_VERTEX=1"]);
    env.extend(strings(vars));
    proc::Launch {
        argv: strings(&["claude", "--rc"]),
        env,
    }
}

#[test]
fn a_declaration_not_naming_the_process_leaves_the_provider_variables() {
    let command = r#"REXUME_LAUNCH=["w","--a"]"#;
    for vars in [
        &[][..],
        &["REXUME_LAUNCH_PID=6", command],
        &["REXUME_LAUNCH_PID=seven", command],
    ] {
        assert_eq!(
            Value::Object(patch(&launch(vars), 7)),
            json!({"launch_command": null, "launch_error": null, "launch_flags": ["--rc"],
                   "launch_env": ["CLAUDE_CODE_USE_VERTEX=1"]}),
            "{vars:?}"
        );
    }
}

#[test]
fn a_declaration_naming_the_process_gives_the_command_in_place_of_the_provider_variables() {
    let vars = [
        "REXUME_LAUNCH_PID=7",
        r#"REXUME_LAUNCH=["w","--tag","x y;$(z)"]"#,
    ];
    assert_eq!(
        Value::Object(patch(&launch(&vars), 7)),
        json!({"launch_command": ["w", "--tag", "x y;$(z)"], "launch_error": null,
               "launch_flags": ["--rc"], "launch_env": null})
    );
}

#[test]
fn a_declaration_naming_the_process_without_a_usable_command_is_an_error() {
    for command in [
        None,
        Some("w --a"),
        Some(r#"["w",1]"#),
        Some("[]"),
        Some(r#"[""]"#),
        Some(r#"["X=1","w"]"#),
        Some(r#"["w","a\u0000b"]"#),
    ] {
        let mut vars = vec!["REXUME_LAUNCH_PID=7".to_owned()];
        vars.extend(command.map(|command| format!("REXUME_LAUNCH={command}")));
        let vars: Vec<&str> = vars.iter().map(String::as_str).collect();
        let patch = patch(&launch(&vars), 7);
        assert!(patch["launch_error"].is_string(), "{command:?}");
        assert_eq!(
            (&patch["launch_command"], &patch["launch_env"]),
            (&Value::Null, &Value::Null),
            "{command:?}"
        );
    }
}
