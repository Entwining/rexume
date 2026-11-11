mod claude_plugin;
mod extension;
mod file;
#[cfg(target_os = "macos")]
mod launch_agent;
mod skill;
#[cfg(target_os = "linux")]
mod systemd_unit;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
#[cfg(target_os = "macos")]
use launch_agent as job;
use serde_json::{Value, json};
#[cfg(target_os = "linux")]
use systemd_unit as job;

pub use claude_plugin::refresh as refresh_claude_plugin;
pub use extension::refresh_extensions;
use extension::{OMP_EXTENSION, PI_EXTENSION};
use file::{read_json, refresh_file, remove_file, write, write_json};
pub use skill::refresh_skills;

pub struct Install {
    pub exe: PathBuf,
    pub rex: PathBuf,
    pub server: String,
    /// Install's own `PATH`, so that the daemon starts the same `codex` the user does.
    pub path: String,
}

struct Targets {
    claude: PathBuf,
    codex: PathBuf,
}

impl Targets {
    fn new(home: &Path) -> Self {
        Self {
            claude: home.join(".claude/settings.json"),
            codex: home.join(".codex/hooks.json"),
        }
    }
}

fn run_manager(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run `{program}`"))?;
    check(&format!("{program} {}", args.join(" ")), &output)
}

fn check(command: &str, output: &Output) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.trim().is_empty() {
            bail!("`{command}` exited {}", output.status);
        }
        bail!("`{command}` failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

const CLAUDE_EVENTS: [&str; 4] = ["SessionStart", "UserPromptSubmit", "Stop", "StopFailure"];
const CODEX_EVENTS: [&str; 1] = ["SessionStart"];

type Outcome = (&'static str, Option<String>);

fn outcomes<const N: usize>(entries: [(&str, Outcome); N]) -> Value {
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
        &CLAUDE_EVENTS,
        &json!({ "type": "command", "command": "rexume report claude", "timeout": 5 }),
    )?;
    let codex = add_hook(
        &targets.codex,
        "codex",
        &CODEX_EVENTS,
        &json!({ "command": "rexume report codex", "timeout": 5, "type": "command" }),
    )?;
    let pi = PI_EXTENSION.install(home)?;
    let omp = OMP_EXTENSION.install(home)?;
    let daemon = job::install(install, home)?;
    // After the job, so that a stub that cannot be written does not keep the daemon from running.
    let [agents_skill, claude_skill] = skill::install(home)?;
    // Last: notices and requests reach Claude Code without the plugin, so a failing `claude`
    // leaves only the plugin for the next install.
    let plugin = claude_plugin::install(home, &install.path)?;
    let mut out = outcomes([
        ("claude_hook", claude),
        (claude_plugin::KEY, plugin),
        ("codex_hook", codex),
        ("pi_extension", pi),
        ("omp_extension", omp),
        agents_skill,
        claude_skill,
        (job::KEY, (daemon, None)),
    ]);
    out[job::NAME_KEY] = json!(job::NAME);
    Ok(out)
}

pub fn uninstall(home: &Path, path: &str) -> Result<Value> {
    let targets = Targets::new(home);
    // The daemon rewrites the plugin, the extensions, and the stub when it starts, so it is stopped
    // before they are removed.
    job::unload(home)?;
    let claude = remove_hook(&targets.claude, "claude", &CLAUDE_EVENTS)?;
    let codex = remove_hook(&targets.codex, "codex", &CODEX_EVENTS)?;
    let pi = PI_EXTENSION.uninstall(home)?;
    let omp = OMP_EXTENSION.uninstall(home)?;
    let daemon = job::remove(home)?;
    let [agents_skill, claude_skill] = skill::uninstall(home)?;
    // Last, so that a missing or failing `claude` leaves only the plugin for the next uninstall.
    let plugin = claude_plugin::uninstall(home, path)?;
    Ok(outcomes([
        ("claude_hook", (claude, None)),
        (claude_plugin::KEY, (plugin, None)),
        ("codex_hook", (codex, None)),
        ("pi_extension", pi),
        ("omp_extension", omp),
        agents_skill,
        claude_skill,
        (job::KEY, daemon),
    ]))
}

pub fn refresh_claude_hooks(home: &Path) -> Result<&'static str> {
    let path = Targets::new(home).claude;
    let settings = read_json(&path)?;
    let Some(hook) = ours(&settings, "SessionStart", "claude") else {
        return Ok("absent");
    };
    let hook = json!({ "type": "command", "command": hook["command"], "timeout": 5 });
    Ok(add_hook(&path, "claude", &CLAUDE_EVENTS[1..], &hook)?.0)
}

pub fn missing_claude_hooks(home: &Path) -> Result<Vec<&'static str>> {
    let settings = read_json(&Targets::new(home).claude)?;
    if ours(&settings, "SessionStart", "claude").is_none() {
        return Ok(Vec::new());
    }
    Ok(CLAUDE_EVENTS[1..]
        .iter()
        .copied()
        .filter(|event| ours(&settings, event, "claude").is_none())
        .collect())
}

fn ours<'a>(settings: &'a Value, event: &str, runtime: &str) -> Option<&'a Value> {
    settings
        .get("hooks")?
        .get(event)?
        .as_array()?
        .iter()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .find(|h| is_ours(h, runtime))
}

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

/// Appends the hook in a group of its own: Codex keys a hook's approval by its position.
fn add_hook(path: &Path, runtime: &str, events: &[&str], hook: &Value) -> Result<Outcome> {
    if !path.parent().is_some_and(Path::is_dir) {
        let dir = path.parent().and_then(Path::file_name).unwrap_or_default();
        return Ok((
            "skipped",
            Some(format!("`~/{}` does not exist", dir.display())),
        ));
    }
    let mut settings = read_json(path)?;
    let mut added = false;
    for event in events {
        if ours(&settings, event, runtime).is_none() {
            groups(&mut settings, event)
                .with_context(|| format!("failed to update `{}`", path.display()))?
                .push(json!({ "hooks": [hook] }));
            added = true;
        }
    }
    if !added {
        return Ok(("unchanged", None));
    }
    write_json(path, &settings)?;
    Ok(("created", None))
}

fn groups<'a>(settings: &'a mut Value, event: &str) -> Result<&'a mut Vec<Value>> {
    settings
        .as_object_mut()
        .context("the file is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`hooks` is not an object")?
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .with_context(|| format!("`hooks.{event}` is not an array"))
}

fn remove_hook(path: &Path, runtime: &str, events: &[&str]) -> Result<&'static str> {
    if !path.exists() {
        return Ok("absent");
    }
    let mut settings = read_json(path)?;
    let mut removed = false;
    for event in events {
        let Some(groups) = settings
            .pointer_mut(&format!("/hooks/{event}"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        let mut here = false;
        for group in groups.iter_mut() {
            if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let count = hooks.len();
                hooks.retain(|h| !is_ours(h, runtime));
                here |= hooks.len() != count;
            }
        }
        if here {
            groups.retain(|group| {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_none_or(|hooks| !hooks.is_empty())
            });
        }
        removed |= here;
    }
    if !removed {
        return Ok("absent");
    }
    write_json(path, &settings)?;
    Ok("removed")
}

#[cfg(test)]
mod tests;
