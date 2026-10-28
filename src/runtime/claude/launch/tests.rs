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

#[test]
fn a_patch_names_both_keys_and_nulls_the_empty_one() {
    let launch = proc::Launch {
        argv: strings(&["claude", "--rc"]),
        env: strings(&["HOME=/home/user"]),
    };
    assert_eq!(
        Value::Object(patch(&launch)),
        json!({"launch_flags": ["--rc"], "launch_env": null})
    );
}
