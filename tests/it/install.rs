use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

use crate::common::REXUME;

const SERVER: &str = "unix:///tmp/rex.sock";
/// Another server, which changes the job's definition.
const OTHER_SERVER: &str = "unix:///tmp/other-rex.sock";

mod manager;

/// A home directory with Claude Code, Codex, and Pi set up, and a fake of the service manager on
/// its PATH.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        for sub in [".claude", ".codex", ".pi/agent", "bin"] {
            std::fs::create_dir_all(home.join(sub)).unwrap();
        }
        let rex_hook =
            json!({ "hooks": [{ "type": "command", "command": "rex-hook", "timeout": 5 }] });
        std::fs::write(
            home.join(".claude/settings.json"),
            serde_json::to_string_pretty(
                &json!({ "model": "opus", "hooks": { "SessionStart": [rex_hook] } }),
            )
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            home.join(".codex/hooks.json"),
            json!({ "hooks": { "SessionStart": [rex_hook] } }).to_string(),
        )
        .unwrap();
        // The job takes a moment to exit after `SIGTERM`, as the daemon does, and is unloaded
        // only then.
        for (path, script) in [
            (
                dir.path().join("job"),
                r#"#!/bin/sh
trap 'kill $sleeper; sleep 0.3; rm "$1"; exit 0' TERM
echo $$ > "$1"
while :; do sleep 60 & sleeper=$!; wait $sleeper; done
"#
                .to_owned(),
            ),
            (
                home.join("bin").join(manager::TOOL),
                manager::script(dir.path(), &home),
            ),
        ] {
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    fn job(&self) -> Option<i32> {
        let pid = std::fs::read_to_string(self.dir.path().join("loaded")).ok()?;
        pid.trim().parse().ok()
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn output(&self, args: &[&str]) -> Output {
        Command::new(REXUME)
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.home().join("bin").display()),
            )
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Value {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Install's arguments, with the job's server given.
    fn install_args(server: &str) -> [&str; 5] {
        ["install", "--rex", "/opt/rex/bin/rex", "--server", server]
    }

    fn install(&self, server: &str) -> Value {
        self.run(&Self::install_args(server))
    }

    fn targets(&self) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let home = self.home();
        [
            ".claude/settings.json",
            ".codex/hooks.json",
            ".pi/agent/extensions/rexume.ts",
        ]
        .iter()
        .map(|p| home.join(p))
        .chain([manager::definition(&home)])
        .map(|path| {
            let bytes = std::fs::read(&path).ok();
            (path, bytes)
        })
        .collect()
    }

    /// How many times the job was started and stopped.
    fn starts_and_stops(&self) -> (usize, usize) {
        (
            manager::calls(self.dir.path(), manager::STARTS),
            manager::calls(self.dir.path(), manager::STOPS),
        )
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        if let Some(pid) = self.job() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

fn session_start(path: &Path) -> Vec<String> {
    let settings: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    settings["hooks"]["SessionStart"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| {
            group["hooks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["command"].as_str().unwrap().to_owned())
        })
        .collect()
}

/// The value and note install or uninstall reported for each entry.
fn outcomes(out: &Value) -> [(&str, Option<&str>); 4] {
    ["claude_hook", "codex_hook", "pi_extension", manager::KEY].map(|key| {
        (
            out[key].as_str().unwrap(),
            out[format!("{key}_note")].as_str(),
        )
    })
}

#[test]
fn install_converges_and_uninstall_removes_only_its_own_entries() {
    let home = Home::new();
    let first = home.install(SERVER);
    assert!(outcomes(&first).iter().all(|o| *o == ("created", None)));
    assert_eq!(first[manager::NAME_KEY], manager::NAME);
    let installed = home.targets();
    assert!(installed.iter().all(|(_, bytes)| bytes.is_some()));
    assert_eq!(
        session_start(&home.home().join(".claude/settings.json")),
        ["rex-hook", "rexume report claude"]
    );
    let job = String::from_utf8(installed[3].1.clone().unwrap()).unwrap();
    assert!(job.contains(&manager::quoted("daemon")) && job.contains("/opt/rex/bin/rex"));
    let path = format!("{}:/usr/bin:/bin", home.home().join("bin").display());
    assert!(job.contains(&path), "the daemon runs with install's PATH");

    // Running it again changes nothing and loads the job no second time.
    let again = home.install(SERVER);
    assert!(outcomes(&again).iter().all(|o| *o == ("unchanged", None)));
    assert_eq!(home.targets(), installed);
    assert_eq!(home.starts_and_stops(), (1, 0));

    // A partial install is completed without touching what is already right.
    std::fs::remove_file(home.home().join(".pi/agent/extensions/rexume.ts")).unwrap();
    std::fs::write(
        home.home().join(".codex/hooks.json"),
        json!({ "hooks": {} }).to_string(),
    )
    .unwrap();
    let repaired = home.install(SERVER);
    let repaired = outcomes(&repaired);
    assert_eq!(
        repaired[..3],
        [("unchanged", None), ("created", None), ("created", None)]
    );
    assert_eq!(home.targets()[0], installed[0]);
    assert_eq!(home.starts_and_stops(), (1, 0));

    // A changed job definition is loaded again.
    let moved = home.install(OTHER_SERVER);
    assert_eq!(moved[manager::KEY], "updated");
    assert_eq!(home.starts_and_stops(), (2, 1));

    let removed = home.run(&["uninstall"]);
    assert!(outcomes(&removed).iter().all(|o| *o == ("removed", None)));
    assert_eq!(
        session_start(&home.home().join(".claude/settings.json")),
        ["rex-hook"]
    );
    let settings: Value =
        serde_json::from_slice(&std::fs::read(home.home().join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(settings["model"], "opus");
    let left = home.targets();
    assert!(left[2].1.is_none() && left[3].1.is_none());
    assert_eq!(home.starts_and_stops(), (2, 2));
    let twice = home.run(&["uninstall"]);
    assert!(outcomes(&twice).iter().all(|o| *o == ("absent", None)));
}

#[test]
fn a_runtime_that_is_not_set_up_is_skipped_with_a_note() {
    let home = Home::new();
    std::fs::remove_dir_all(home.home().join(".pi")).unwrap();
    let out = home.install(SERVER);
    let out = outcomes(&out);
    assert_eq!(out[2], ("skipped", Some("`~/.pi/agent` does not exist")));
}

/// A reload that fails part way leaves the definition differing or the job stopped, so the next
/// install finishes it instead of finding nothing to do.
#[test]
fn the_next_install_finishes_a_reload_that_failed() {
    let home = Home::new();
    home.install(SERVER);
    std::fs::write(home.dir.path().join("fail-stop"), "").unwrap();
    let failed = home.output(&Home::install_args(OTHER_SERVER));
    assert!(!failed.status.success());
    assert_eq!(home.install(OTHER_SERVER)[manager::KEY], "updated");
    assert_eq!(home.starts_and_stops(), (2, 2));
}

/// Install starts a service that is not running: one that gave up at its start limit, as when its
/// executable was missing, and one waiting to restart, which the manager shows as activating.
#[cfg(target_os = "linux")]
#[test]
fn install_starts_a_service_that_is_not_running() {
    for state in ["start-limit", "restarting"] {
        let home = Home::new();
        home.install(SERVER);
        let stopped = Command::new(home.home().join("bin").join(manager::TOOL))
            .args(["--user", "stop", manager::NAME])
            .status()
            .unwrap();
        assert!(stopped.success());
        std::fs::write(home.dir.path().join(state), "").unwrap();
        assert_eq!(home.install(SERVER)[manager::KEY], "updated", "{state}");
        assert!(home.job().is_some(), "{state}");
    }
}

/// Install writes the hooks before it finds no user manager, so uninstall removes them without one.
#[cfg(target_os = "linux")]
#[test]
fn uninstall_removes_what_an_install_without_a_user_manager_wrote() {
    let home = Home::new();
    std::fs::write(home.dir.path().join("no-manager"), "").unwrap();
    let failed = home.output(&Home::install_args(SERVER));
    assert!(!failed.status.success());
    let settings = home.home().join(".claude/settings.json");
    assert!(session_start(&settings).contains(&"rexume report claude".to_owned()));
    let out = home.run(&["uninstall"]);
    assert_eq!(out["claude_hook"], "removed");
    assert_eq!(out[manager::KEY], "absent");
    assert_eq!(session_start(&settings), ["rex-hook"]);
}
