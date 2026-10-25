//! `rexume install` and `rexume uninstall`: the session hooks of Claude Code and Codex, the Pi
//! extension, which the daemon also refreshes, and the LaunchAgent that runs the daemon. Each
//! entry is recognized by its identity (the hook's command, the extension's and plist's paths),
//! so install changes only what is missing or different, and uninstall removes only these
//! entries.

mod launch_agent;

use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::runtime::pi;

pub const LABEL: &str = "io.github.entwining.rexume";

/// What the LaunchAgent runs.
pub struct Install {
    pub exe: PathBuf,
    pub rex: PathBuf,
    pub server: String,
    pub theme: Option<(String, String)>,
    /// The daemon's `PATH`, install's own, so that it starts the same `codex` the user does.
    pub path: String,
}

/// Where install writes, under one home directory.
struct Targets {
    claude: PathBuf,
    codex: PathBuf,
    pi: PathBuf,
    plist: PathBuf,
    logs: PathBuf,
}

impl Targets {
    fn new(home: &Path) -> Self {
        Self {
            claude: home.join(".claude/settings.json"),
            codex: home.join(".codex/hooks.json"),
            pi: home.join(".pi/agent/extensions/rexume.ts"),
            plist: home.join(format!("Library/LaunchAgents/{LABEL}.plist")),
            logs: home.join("Library/Logs"),
        }
    }
}

/// What install or uninstall did to one entry, and why when that needs saying.
type Outcome = (&'static str, Option<String>);

fn outcomes(entries: [(&str, Outcome); 4]) -> Value {
    let mut out = serde_json::Map::from_iter([("version".to_owned(), json!(1))]);
    for (key, (value, note)) in entries {
        out.insert(key.to_owned(), json!(value));
        out.insert(format!("{key}_note"), json!(note));
    }
    Value::Object(out)
}

pub fn install(home: &Path, install: &Install) -> Result<Value> {
    let targets = Targets::new(home);
    let claude = add_hook(
        &targets.claude,
        "claude",
        json!({ "type": "command", "command": "rexume report claude", "timeout": 5 }),
    )?;
    let codex = add_hook(
        &targets.codex,
        "codex",
        json!({ "command": "rexume report codex", "timeout": 5, "type": "command" }),
    )?;
    let extension = match targets.pi.parent().and_then(Path::parent) {
        Some(agent) if agent.is_dir() => (write(&targets.pi, pi::EXTENSION.as_bytes())?, None),
        _ => ("skipped", Some("`~/.pi/agent` does not exist".to_owned())),
    };
    std::fs::create_dir_all(&targets.logs)
        .with_context(|| format!("failed to create `{}`", targets.logs.display()))?;
    let launch_agent = launch_agent::install(install, &targets)?;
    let mut out = outcomes([
        ("claude_hook", claude),
        ("codex_hook", codex),
        ("pi_extension", extension),
        ("launch_agent", (launch_agent, None)),
    ]);
    out["launch_agent_label"] = json!(LABEL);
    Ok(out)
}

pub fn uninstall(home: &Path) -> Result<Value> {
    let targets = Targets::new(home);
    // The daemon rewrites the Pi extension when it starts, so it is stopped before the extension
    // is removed.
    launch_agent::unload()?;
    let claude = remove_hook(&targets.claude, "claude")?;
    let codex = remove_hook(&targets.codex, "codex")?;
    let extension = remove_file(&targets.pi, |text| text.starts_with(pi::WRITTEN_BY))?;
    let launch_agent = remove_file(&targets.plist, |_| true)?;
    Ok(outcomes([
        ("claude_hook", (claude, None)),
        ("codex_hook", (codex, None)),
        ("pi_extension", extension),
        ("launch_agent", launch_agent),
    ]))
}

/// Rewrites the Pi extension install wrote when it differs from this version's, so that an
/// upgrade reaches Pi without another install. A missing file stays missing, so that uninstall
/// stays final, and a file install did not write is left as it is. Returns `updated`,
/// `unchanged`, `absent`, or `kept`.
pub fn refresh_pi_extension(home: &Path) -> Result<&'static str> {
    let path = Targets::new(home).pi;
    match std::fs::read_to_string(&path) {
        Ok(text) if text.starts_with(pi::WRITTEN_BY) => write(&path, pi::EXTENSION.as_bytes()),
        Ok(_) => Ok("kept"),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok("absent"),
        Err(e) => Err(e).with_context(|| format!("failed to read `{}`", path.display())),
    }
}

/// Whether a hook runs `rexume report <runtime>`, by whatever path to rexume.
fn is_ours(hook: &Value, runtime: &str) -> bool {
    let Some(argv) = hook
        .get("command")
        .and_then(Value::as_str)
        .and_then(shlex::split)
    else {
        return false;
    };
    matches!(argv.as_slice(), [exe, report, r]
        if Path::new(exe).file_name().is_some_and(|n| n == "rexume") && report == "report" && r == runtime)
}

/// Appends the hook in a group of its own: Codex keys a hook's approval by its position, so
/// existing groups are never rewritten.
fn add_hook(path: &Path, runtime: &str, hook: Value) -> Result<Outcome> {
    if !path.parent().is_some_and(Path::is_dir) {
        let dir = path.parent().and_then(Path::file_name).unwrap_or_default();
        return Ok((
            "skipped",
            Some(format!("`~/{}` does not exist", dir.display())),
        ));
    }
    let mut settings = read_json(path)?;
    let groups = session_start(&mut settings)
        .with_context(|| format!("failed to update `{}`", path.display()))?;
    let present = groups
        .iter()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .any(|h| is_ours(h, runtime));
    if present {
        return Ok(("unchanged", None));
    }
    groups.push(json!({ "hooks": [hook] }));
    write_json(path, &settings)?;
    Ok(("created", None))
}

fn session_start(settings: &mut Value) -> Result<&mut Vec<Value>> {
    settings
        .as_object_mut()
        .context("the file is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`hooks` is not an object")?
        .entry("SessionStart")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("`hooks.SessionStart` is not an array")
}

fn remove_hook(path: &Path, runtime: &str) -> Result<&'static str> {
    if !path.exists() {
        return Ok("absent");
    }
    let mut settings = read_json(path)?;
    let Some(groups) = settings
        .pointer_mut("/hooks/SessionStart")
        .and_then(Value::as_array_mut)
    else {
        return Ok("absent");
    };
    let mut removed = false;
    for group in groups.iter_mut() {
        if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            let count = hooks.len();
            hooks.retain(|h| !is_ours(h, runtime));
            removed |= hooks.len() != count;
        }
    }
    if !removed {
        return Ok("absent");
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|hooks| !hooks.is_empty())
    });
    write_json(path, &settings)?;
    Ok("removed")
}

fn remove_file(path: &Path, ours: impl Fn(&str) -> bool) -> Result<Outcome> {
    match std::fs::read_to_string(path) {
        Ok(text) if ours(&text) => {
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove `{}`", path.display()))?;
            Ok(("removed", None))
        }
        Ok(_) => Ok(("kept", Some("not written by install".to_owned()))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(("absent", None)),
        Err(e) => Err(e).with_context(|| format!("failed to read `{}`", path.display())),
    }
}

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("failed to parse `{}`", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("failed to read `{}`", path.display())),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write(path, text.as_bytes())?;
    Ok(())
}

/// Replaces a file through a rename, keeping its permissions and leaving it whole if this stops
/// midway, and returns `created`, `updated`, or `unchanged`.
fn write(path: &Path, bytes: &[u8]) -> Result<&'static str> {
    // A file linked from elsewhere is changed where it lives.
    let path = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(e) if e.kind() == ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e).with_context(|| format!("failed to resolve `{}`", path.display())),
    };
    let old = match std::fs::read(&path) {
        Ok(old) => Some(old),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("failed to read `{}`", path.display())),
    };
    if old.as_deref() == Some(bytes) {
        return Ok("unchanged");
    }
    let dir = path
        .parent()
        .with_context(|| format!("`{}` has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create `{}`", dir.display()))?;
    let mut partial = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("failed to write in `{}`", dir.display()))?;
    partial.write_all(bytes)?;
    if let Ok(meta) = std::fs::metadata(&path) {
        partial.as_file().set_permissions(meta.permissions())?;
    }
    partial
        .persist(&path)
        .with_context(|| format!("failed to write `{}`", path.display()))?;
    Ok(if old.is_some() { "updated" } else { "created" })
}

#[cfg(test)]
mod tests;
