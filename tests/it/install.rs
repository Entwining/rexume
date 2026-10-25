use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

use crate::common::REXUME;

const LABEL: &str = "io.github.entwining.rexume";

/// A home directory with Claude Code, Codex, and Pi set up, and a `launchctl` that records its
/// calls and keeps the job in a file instead of reaching launchd. It behaves as launchd does:
/// `bootstrap` starts the job's process, `bootout` only sends it `SIGTERM`, the job stays loaded
/// until that process exits, and `bootstrap` fails while it is loaded.
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
        let job = dir.path().join("job");
        let launchctl = home.join("bin/launchctl");
        for (path, script) in [
            (
                &job,
                r#"#!/bin/sh
trap 'kill $sleeper; sleep 0.3; rm "$1"; exit 0' TERM
echo $$ > "$1"
while :; do sleep 60 & sleeper=$!; wait $sleeper; done
"#
                .to_owned(),
            ),
            (
                &launchctl,
                format!(
                    r#"#!/bin/sh
echo "$*" >> '{log}'
case "$1" in
  print) test -e '{loaded}' && printf '\tpid = %s\n' "$(cat '{loaded}')" ;;
  bootstrap)
    if test -e '{loaded}'; then echo 'Bootstrap failed: 5: Input/output error' >&2; exit 5; fi
    '{job}' '{loaded}' < /dev/null > /dev/null 2>&1 &
    until test -e '{loaded}'; do sleep 0.01; done ;;
  bootout)
    if test -e '{fail}'; then rm '{fail}'; echo 'Boot-out failed: 5: Input/output error' >&2; exit 5; fi
    kill -TERM "$(cat '{loaded}')" ;;
esac
"#,
                    log = dir.path().join("launchctl.log").display(),
                    loaded = dir.path().join("loaded").display(),
                    job = job.display(),
                    fail = dir.path().join("fail-bootout").display(),
                ),
            ),
        ] {
            std::fs::write(path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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

    fn install_args<'a>(theme: &[&'a str]) -> Vec<&'a str> {
        let mut args = vec![
            "install",
            "--rex",
            "/opt/rex/bin/rex",
            "--server",
            "unix:///tmp/rex.sock",
        ];
        args.extend(theme);
        args
    }

    fn install(&self, theme: &[&str]) -> Value {
        self.run(&Self::install_args(theme))
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
        .chain([home.join(format!("Library/LaunchAgents/{LABEL}.plist"))])
        .map(|path| {
            let bytes = std::fs::read(&path).ok();
            (path, bytes)
        })
        .collect()
    }

    fn launchctl(&self, verb: &str) -> usize {
        std::fs::read_to_string(self.dir.path().join("launchctl.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with(verb))
            .count()
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
    ["claude_hook", "codex_hook", "pi_extension", "launch_agent"].map(|key| {
        (
            out[key].as_str().unwrap(),
            out[format!("{key}_note")].as_str(),
        )
    })
}

#[test]
fn install_converges_and_uninstall_removes_only_its_own_entries() {
    let home = Home::new();
    let first = home.install(&[]);
    assert!(outcomes(&first).iter().all(|o| *o == ("created", None)));
    assert_eq!(first["launch_agent_label"], LABEL);
    let installed = home.targets();
    assert!(installed.iter().all(|(_, bytes)| bytes.is_some()));
    assert_eq!(
        session_start(&home.home().join(".claude/settings.json")),
        ["rex-hook", "rexume report claude"]
    );
    let plist = String::from_utf8(installed[3].1.clone().unwrap()).unwrap();
    assert!(plist.contains("<string>daemon</string>") && plist.contains("/opt/rex/bin/rex"));
    let path = format!(
        "<string>{}:/usr/bin:/bin</string>",
        home.home().join("bin").display()
    );
    assert!(plist.contains(&path), "the daemon runs with install's PATH");

    // Running it again changes nothing and loads the job no second time.
    let again = home.install(&[]);
    assert!(outcomes(&again).iter().all(|o| *o == ("unchanged", None)));
    assert_eq!(home.targets(), installed);
    assert_eq!(home.launchctl("bootstrap"), 1);

    // A partial install is completed without touching what is already right.
    std::fs::remove_file(home.home().join(".pi/agent/extensions/rexume.ts")).unwrap();
    std::fs::write(
        home.home().join(".codex/hooks.json"),
        json!({ "hooks": {} }).to_string(),
    )
    .unwrap();
    let repaired = home.install(&[]);
    let repaired = outcomes(&repaired);
    assert_eq!(
        repaired[..3],
        [("unchanged", None), ("created", None), ("created", None)]
    );
    assert_eq!(home.targets()[0], installed[0]);
    assert_eq!(home.launchctl("bootstrap"), 1);

    // A changed job definition is loaded again.
    let themed = home.install(&[
        "--theme-foreground",
        "#eeeeee",
        "--theme-background",
        "#222222",
    ]);
    assert_eq!(themed["launch_agent"], "updated");
    assert_eq!(
        (home.launchctl("bootout"), home.launchctl("bootstrap")),
        (1, 2)
    );

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
    assert_eq!(home.launchctl("bootout"), 2);
    let twice = home.run(&["uninstall"]);
    assert!(outcomes(&twice).iter().all(|o| *o == ("absent", None)));
}

#[test]
fn a_runtime_that_is_not_set_up_is_skipped_with_a_note() {
    let home = Home::new();
    std::fs::remove_dir_all(home.home().join(".pi")).unwrap();
    let out = home.install(&[]);
    let out = outcomes(&out);
    assert_eq!(out[2], ("skipped", Some("`~/.pi/agent` does not exist")));
}

/// A reload that fails part way leaves the plist differing or the job not loaded, so the next
/// install finishes it instead of finding nothing to do.
#[test]
fn the_next_install_finishes_a_reload_that_failed() {
    let home = Home::new();
    home.install(&[]);
    let theme = [
        "--theme-foreground",
        "#eeeeee",
        "--theme-background",
        "#222222",
    ];
    std::fs::write(home.dir.path().join("fail-bootout"), "").unwrap();
    let failed = home.output(&Home::install_args(&theme));
    assert!(!failed.status.success());
    assert_eq!(home.install(&theme)["launch_agent"], "updated");
    assert_eq!(
        (home.launchctl("bootout"), home.launchctl("bootstrap")),
        (2, 2)
    );
}
