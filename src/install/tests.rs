use std::io::ErrorKind;

use super::*;

fn read(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

#[test]
fn another_path_to_rexume_is_the_same_hook() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("settings.json");
    let other = json!({ "hooks": [{ "type": "command", "command": "other", "timeout": 5 }] });
    let installed = json!({ "hooks": [{ "type": "command",
        "command": "/opt/homebrew/bin/rexume report claude", "timeout": 5 }] });
    let hooks = json!({ "SessionStart": [other, installed], "UserPromptSubmit": [installed],
        "Stop": [installed], "StopFailure": [installed] });
    std::fs::write(&path, json!({ "hooks": hooks }).to_string())?;
    let hook = json!({ "type": "command", "command": "rexume report claude", "timeout": 5 });
    assert_eq!(
        add_hook(&path, "claude", &CLAUDE_EVENTS, &hook)?.0,
        "unchanged"
    );
    assert_eq!(remove_hook(&path, "claude", &CLAUDE_EVENTS)?, "removed");
    assert_eq!(
        read(&path)?,
        json!({ "hooks": { "SessionStart": [other], "UserPromptSubmit": [], "Stop": [],
            "StopFailure": [] } })
    );
    assert_eq!(remove_hook(&path, "claude", &CLAUDE_EVENTS)?, "absent");
    Ok(())
}

/// An earlier install or a configuration manager left only the session-start hook. The missing
/// turn hooks are reported, then added once with the same command. Without the session-start hook,
/// no hooks are reported missing or added.
#[test]
fn the_daemon_adds_the_turn_hooks_an_earlier_install_left_out() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = Targets::new(home.path()).claude;
    assert_eq!(refresh_claude_hooks(home.path())?, "absent");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let other = json!({ "hooks": [{ "type": "command", "command": "other" }] });
    std::fs::write(&path, json!({ "hooks": { "Stop": [other] } }).to_string())?;
    assert!(missing_claude_hooks(home.path())?.is_empty());
    assert_eq!(refresh_claude_hooks(home.path())?, "absent");
    let command = "/opt/homebrew/bin/rexume report claude";
    let hook = json!({ "type": "command", "command": command, "timeout": 5 });
    std::fs::write(
        &path,
        json!({ "model": "m", "hooks": { "SessionStart": [{ "hooks": [hook] }],
            "Stop": [other] } })
        .to_string(),
    )?;
    assert_eq!(
        missing_claude_hooks(home.path())?,
        ["UserPromptSubmit", "Stop", "StopFailure"]
    );
    assert_eq!(refresh_claude_hooks(home.path())?, "created");
    assert!(missing_claude_hooks(home.path())?.is_empty());
    assert_eq!(refresh_claude_hooks(home.path())?, "unchanged");
    let ours = json!({ "hooks": [hook] });
    assert_eq!(
        read(&path)?,
        json!({ "model": "m", "hooks": { "SessionStart": [ours], "Stop": [other, ours],
            "UserPromptSubmit": [ours], "StopFailure": [ours] } })
    );
    Ok(())
}

#[test]
fn a_runtime_that_is_not_set_up_gets_no_settings() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join(".codex/hooks.json");
    let (value, note) = add_hook(&path, "codex", &CODEX_EVENTS, &json!({}))?;
    assert_eq!(value, "skipped");
    assert_eq!(note.as_deref(), Some("`~/.codex` does not exist"));
    assert!(!path.exists());
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn the_plist_round_trips_through_its_parsed_form() -> Result<()> {
    let install = Install {
        exe: "/opt/homebrew/bin/rexume".into(),
        rex: "/Applications/Rex.app/Contents/MacOS/rex".into(),
        server: "unix:///tmp/a%20b/server.sock".into(),
        path: "/usr/bin:/bin".into(),
    };
    let wanted = launch_agent::plist(&install, Path::new("/tmp/rexume & co.log"));
    let mut bytes = Vec::new();
    wanted.to_writer_xml(&mut bytes)?;
    assert_eq!(plist::Value::from_reader_xml(bytes.as_slice())?, wanted);
    Ok(())
}

/// Every word reaches the daemon as written: systemd would otherwise split it at a space, expand
/// `%` and `$`, and read a backslash as an escape.
#[cfg(target_os = "linux")]
#[test]
fn the_unit_quotes_what_systemd_would_expand() {
    let install = Install {
        exe: "/opt/my $apps/rexume".into(),
        rex: r#"/opt/"rex"\bin/rex"#.into(),
        server: "unix:///tmp/a%20b/$HOME.sock".into(),
        path: "/opt/100%/bin:$PATH".into(),
    };
    let unit = systemd_unit::unit(&install);
    let line = |key: &str| {
        unit.lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap()
    };
    assert_eq!(
        line("ExecStart="),
        r#""/opt/my $apps/rexume" "daemon" "--rex" "/opt/\"rex\"\\bin/rex" "--server" "unix:///tmp/a%%20b/$$HOME.sock""#
    );
    assert_eq!(line("Environment="), r#""PATH=/opt/100%%/bin:$PATH""#);
}

/// A home with `~/.claude` and a `PATH` whose `claude` records each call's arguments in `calls`
/// and exits with `status`.
struct FakeClaude {
    home: tempfile::TempDir,
    path: String,
    calls: PathBuf,
}

impl FakeClaude {
    fn new(status: i32) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir()?;
        std::fs::create_dir(home.path().join(".claude"))?;
        let bin = home.path().join("bin");
        std::fs::create_dir(&bin)?;
        let calls = home.path().join("calls");
        let script = format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\necho refused >&2\nexit {status}\n",
            calls.display()
        );
        std::fs::write(bin.join("claude"), script)?;
        std::fs::set_permissions(bin.join("claude"), PermissionsExt::from_mode(0o755))?;
        let path = bin.to_string_lossy().into_owned();
        Ok(Self { home, path, calls })
    }

    fn settings(&self, value: &Value) -> Result<()> {
        Ok(std::fs::write(
            Targets::new(self.home.path()).claude,
            value.to_string(),
        )?)
    }

    /// The calls since the last time this was read.
    fn calls(&self) -> Result<Vec<String>> {
        let calls = match std::fs::read_to_string(&self.calls) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let _ = std::fs::remove_file(&self.calls);
        Ok(calls)
    }

    fn plugin_dir(&self) -> PathBuf {
        self.home.path().join(".local/share/rexume/claude-plugin")
    }
}

/// Install registers what Claude Code's settings lack and nothing they name, so that a plugin the
/// user disabled stays disabled; uninstall removes what they name, then the files.
#[test]
fn install_registers_the_plugin_once_and_uninstall_removes_it() -> Result<()> {
    let claude = FakeClaude::new(0)?;
    let home = claude.home.path();
    let dir = claude.plugin_dir();
    let (value, note) = claude_plugin::install(home, &claude.path)?;
    assert_eq!((value, note), ("created", None));
    assert_eq!(
        claude.calls()?,
        [
            format!("plugin marketplace add {}", dir.display()),
            "plugin install rexume@rexume".to_owned()
        ]
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("hooks/rexume.ts"))?,
        crate::runtime::claude::PLUGIN
    );

    let registered = |enabled: bool| {
        json!({ "extraKnownMarketplaces": { "rexume": {} },
            "enabledPlugins": { "rexume@rexume": enabled } })
    };
    claude.settings(&registered(true))?;
    assert_eq!(
        claude_plugin::install(home, &claude.path)?,
        ("unchanged", None)
    );
    claude.settings(&registered(false))?;
    assert_eq!(
        claude_plugin::install(home, &claude.path)?,
        ("unchanged", Some("disabled in Claude Code".to_owned()))
    );
    assert_eq!(claude.calls()?, Vec::<String>::new());

    assert_eq!(claude_plugin::uninstall(home, &claude.path)?, "removed");
    assert_eq!(
        claude.calls()?,
        [
            "plugin uninstall rexume@rexume",
            "plugin marketplace remove rexume"
        ]
    );
    assert!(!dir.exists());
    claude.settings(&json!({}))?;
    assert_eq!(claude_plugin::uninstall(home, &claude.path)?, "absent");
    assert_eq!(claude.calls()?, Vec::<String>::new());
    Ok(())
}

#[test]
fn install_skips_the_plugin_without_claude_and_reports_its_failure() -> Result<()> {
    let claude = FakeClaude::new(1)?;
    let home = claude.home.path();
    assert_eq!(
        claude_plugin::install(home, "/nonexistent")?,
        ("skipped", Some("`claude` is not on `PATH`".to_owned()))
    );
    assert!(!claude.plugin_dir().exists());
    let failed = claude_plugin::install(home, &claude.path).unwrap_err();
    assert!(format!("{failed:#}").contains("refused"), "{failed:#}");
    assert!(!claude.plugin_dir().exists());

    std::fs::remove_dir_all(home.join(".claude"))?;
    assert_eq!(
        claude_plugin::install(home, &claude.path)?,
        ("skipped", Some("`~/.claude` does not exist".to_owned()))
    );
    assert!(!claude.plugin_dir().exists());
    Ok(())
}

#[test]
fn the_daemon_refreshes_only_a_plugin_install_wrote() -> Result<()> {
    let claude = FakeClaude::new(0)?;
    let home = claude.home.path();
    assert_eq!(refresh_claude_plugin(home)?, "absent");
    assert!(
        !claude.plugin_dir().exists(),
        "a removed plugin is not written back"
    );
    claude_plugin::install(home, &claude.path)?;
    let module = claude.plugin_dir().join("hooks/rexume.ts");
    std::fs::write(&module, "// an older version\n")?;
    assert_eq!(refresh_claude_plugin(home)?, "updated");
    assert_eq!(
        std::fs::read_to_string(&module)?,
        crate::runtime::claude::PLUGIN
    );
    assert_eq!(refresh_claude_plugin(home)?, "unchanged");
    Ok(())
}
