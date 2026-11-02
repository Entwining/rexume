//! The Claude Code plugin through which notices and requests reach a conversation as the user's own
//! prompts (`runtime::claude::PLUGIN`). Its files are Rexume's own directory, which is also the
//! marketplace Claude Code installs it from; a directory marketplace is read in place, so the
//! daemon updates the plugin by rewriting the files. Registering and removing it go through
//! `claude plugin`: Claude Code keeps an installed plugin in a registry of its own besides the
//! settings, and only its commands remove it from there.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{Outcome, Targets, check, read_json, write};
use crate::runtime::claude;

pub(super) const KEY: &str = "claude_plugin";
const MARKETPLACE: &str = "rexume";
const PLUGIN: &str = "rexume@rexume";
/// Where Claude Code's settings name the marketplace and the plugin once they are registered.
const MARKETPLACE_KEY: &str = "/extraKnownMarketplaces/rexume";
const PLUGIN_KEY: &str = "/enabledPlugins/rexume@rexume";

/// The marketplace and the plugin are one directory, whose files are these.
const FILES: [(&str, &str); 4] = [
    (
        ".claude-plugin/marketplace.json",
        r#"{ "name": "rexume", "owner": { "name": "Rexume" }, "plugins": [{ "name": "rexume", "source": "./" }] }
"#,
    ),
    (
        ".claude-plugin/plugin.json",
        concat!(
            r#"{ "name": "rexume", "version": ""#,
            env!("CARGO_PKG_VERSION"),
            r#"", "description": "Delivers Rexume's notices and requests as your own prompts" }
"#
        ),
    ),
    ("hooks/hooks.json", "{ \"modules\": [\"./rexume.ts\"] }\n"),
    ("hooks/rexume.ts", claude::PLUGIN),
];

fn dir(home: &Path) -> PathBuf {
    home.join(".local/share/rexume/claude-plugin")
}

/// Writes the plugin's files and registers what Claude Code's settings lack: the marketplace, then
/// the plugin. A plugin the settings name is left as it is, since `claude plugin install` would
/// enable one the user disabled.
pub(super) fn install(home: &Path, path: &str) -> Result<Outcome> {
    if !home.join(".claude").is_dir() {
        return Ok(("skipped", Some("`~/.claude` does not exist".to_owned())));
    }
    let dir = dir(home);
    let mut outcome = write_files(&dir)?;
    let settings = read_json(&Targets::new(home).claude)?;
    let mut steps: Vec<Vec<&str>> = Vec::new();
    if settings.pointer(MARKETPLACE_KEY).is_none() {
        let dir = dir.to_str().context("the plugin's path is not UTF-8")?;
        steps.push(vec!["marketplace", "add", dir]);
    }
    let enabled = settings.pointer(PLUGIN_KEY);
    if enabled.is_none() {
        steps.push(vec!["install", PLUGIN]);
    }
    // A directory written only to register it would be refreshed by the daemon and reported by
    // uninstall though Claude Code never knew it, so it is removed when no step registers it.
    let mut written_only = outcome == "created";
    for args in steps {
        let ran = run_claude(home, path, &args);
        if written_only && !matches!(ran, Ok(Some(()))) {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to remove `{}`", dir.display()))?;
        }
        if ran?.is_none() {
            return Ok(("skipped", Some("`claude` is not on `PATH`".to_owned())));
        }
        written_only = false;
        outcome = "created";
    }
    let note = (enabled == Some(&Value::Bool(false))).then(|| "disabled in Claude Code".to_owned());
    Ok((outcome, note))
}

/// Removes the plugin and its marketplace from Claude Code, then the directory. Returns `removed`
/// or `absent`.
pub(super) fn uninstall(home: &Path, path: &str) -> Result<&'static str> {
    let settings = read_json(&Targets::new(home).claude)?;
    let mut removed = false;
    for (key, args) in [
        (PLUGIN_KEY, ["uninstall", PLUGIN].as_slice()),
        (MARKETPLACE_KEY, &["marketplace", "remove", MARKETPLACE]),
    ] {
        if settings.pointer(key).is_some() {
            run_claude(home, path, args)?.context("`claude` is not on `PATH`")?;
            removed = true;
        }
    }
    let dir = dir(home);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => removed = true,
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("failed to remove `{}`", dir.display())),
    }
    Ok(if removed { "removed" } else { "absent" })
}

/// Rewrites the plugin's files when they differ from this version's, so that an upgrade reaches
/// Claude Code without another install. A missing directory stays missing, so that uninstall stays
/// final. Returns `updated`, `unchanged`, or `absent`.
pub fn refresh(home: &Path) -> Result<&'static str> {
    let dir = dir(home);
    if !dir.is_dir() {
        return Ok("absent");
    }
    write_files(&dir)
}

/// Returns `created` when the directory was new, `updated` when any file changed, and otherwise
/// `unchanged`.
fn write_files(dir: &Path) -> Result<&'static str> {
    let existed = dir.is_dir();
    let mut changed = false;
    for (name, text) in FILES {
        changed |= write(&dir.join(name), text.as_bytes())? != "unchanged";
    }
    Ok(match (existed, changed) {
        (false, _) => "created",
        (true, true) => "updated",
        (true, false) => "unchanged",
    })
}

/// Runs `claude plugin ARGS` for `home` with `PATH` as given, or returns `None` when no `claude` is
/// there.
fn run_claude(home: &Path, path: &str, args: &[&str]) -> Result<Option<()>> {
    let output = Command::new("claude")
        .arg("plugin")
        .args(args)
        .env("HOME", home)
        .env("PATH", path)
        .stdin(Stdio::null())
        .output();
    match output {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        output => {
            let output = output.context("failed to run `claude`")?;
            check(&format!("claude plugin {}", args.join(" ")), &output)?;
            Ok(Some(()))
        }
    }
}
