//! Install and uninstall reach the Claude Code plugin through the `claude` on their own PATH.

use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};

use super::{Home, SERVER, session_start};

/// Install and uninstall each find `claude` on their own PATH to register and remove the plugin.
#[test]
fn install_and_uninstall_register_the_plugin_through_claude() {
    let home = Home::new();
    let calls = home.dir.path().join("claude-calls");
    let claude = home.home().join("bin/claude");
    std::fs::write(
        &claude,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\n", calls.display()),
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let take_calls = || {
        let text = std::fs::read_to_string(&calls).unwrap_or_default();
        let _ = std::fs::remove_file(&calls);
        text.lines().map(str::to_owned).collect::<Vec<_>>()
    };
    let plugin = home.home().join(".local/share/rexume/claude-plugin");

    assert_eq!(home.install(SERVER)["claude_plugin"], "created");
    assert_eq!(
        take_calls(),
        [
            format!("plugin marketplace add {}", plugin.display()),
            "plugin install rexume@rexume".to_owned()
        ]
    );
    assert!(plugin.join("hooks/rexume.ts").exists());

    // Claude Code names what it registered in the settings.
    let path = home.home().join(".claude/settings.json");
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    settings["extraKnownMarketplaces"] = json!({ "rexume": {} });
    settings["enabledPlugins"] = json!({ "rexume@rexume": true });
    std::fs::write(&path, settings.to_string()).unwrap();

    assert_eq!(home.run(&["uninstall"])["claude_plugin"], "removed");
    assert_eq!(
        take_calls(),
        [
            "plugin uninstall rexume@rexume",
            "plugin marketplace remove rexume"
        ]
    );
    assert!(!plugin.exists());
}

/// Uninstall needs `claude` only for a plugin the settings name, and removes everything else
/// first, so a machine without it is left with only that registration.
#[test]
fn uninstall_without_claude_removes_everything_else() {
    let home = Home::new();
    home.install(SERVER);
    let path = home.home().join(".claude/settings.json");
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    settings["enabledPlugins"] = json!({ "rexume@rexume": true });
    std::fs::write(&path, settings.to_string()).unwrap();

    let failed = home.output(&["uninstall"]);
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("`claude` is not on `PATH`"), "{stderr}");
    assert_eq!(session_start(&path), ["rex-hook"]);
    let left = home.targets();
    assert!(left[2].1.is_none() && left[3].1.is_none());
    assert_eq!(home.starts_and_stops(), (1, 1));
}
