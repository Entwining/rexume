mod claude_plugin;
mod dirs;
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
pub use dirs::Dirs;
pub use extension::refresh_extensions;
use extension::{OMP_EXTENSION, OPENCODE_PLUGIN, PI_EXTENSION};
use file::{read_json, refresh_file, remove_file, remove_file_and_dir, write, write_json};
pub use skill::refresh_skills;

pub struct Install {
    pub exe: PathBuf,
    pub rex: PathBuf,
    pub server: String,
    /// Install's own `PATH`, so that the daemon starts the same `codex` the user does.
    pub path: String,
    /// The runtimes' variables install read, so that the daemon resolves their directories as
    /// install did and refreshes the files install wrote there.
    pub env: Vec<(String, String)>,
}

/// Each runtime's hook settings, or why install leaves the runtime alone.
type Target = std::result::Result<PathBuf, String>;

struct Targets {
    claude: Target,
    codex: Target,
}

impl Targets {
    fn new(dirs: &Dirs) -> Self {
        Self {
            claude: dirs.claude.usable().map(|dir| dir.join("settings.json")),
            codex: dirs.codex.usable().map(|dir| dir.join("hooks.json")),
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

pub fn install(dirs: &Dirs, install: &Install) -> Result<Value> {
    let home = &dirs.home;
    let targets = Targets::new(dirs);
    let claude = add_hook(
        dirs,
        &targets.claude,
        "claude",
        &CLAUDE_EVENTS,
        &json!({ "type": "command", "command": "rexume report claude", "timeout": 5 }),
    )?;
    let codex = add_hook(
        dirs,
        &targets.codex,
        "codex",
        &CODEX_EVENTS,
        &json!({ "command": "rexume report codex", "timeout": 5, "type": "command" }),
    )?;
    let pi = PI_EXTENSION.install(dirs)?;
    let omp = OMP_EXTENSION.install(dirs)?;
    let opencode = OPENCODE_PLUGIN.install(dirs)?;
    let daemon = job::install(install, home)?;
    // After the job, so that a stub that cannot be written does not keep the daemon from running.
    let [agents_skill, claude_skill] = skill::install(dirs)?;
    // Last: notices and requests reach Claude Code without the plugin, so a failing `claude`
    // leaves only the plugin for the next install.
    let plugin = claude_plugin::install(dirs, &install.path)?;
    let mut out = outcomes([
        ("claude_hook", claude),
        (claude_plugin::KEY, plugin),
        ("codex_hook", codex),
        ("pi_extension", pi),
        ("omp_extension", omp),
        ("opencode_plugin", opencode),
        agents_skill,
        claude_skill,
        (job::KEY, (daemon, None)),
    ]);
    out[job::NAME_KEY] = json!(job::NAME);
    Ok(out)
}

pub fn uninstall(dirs: &Dirs, path: &str) -> Result<Value> {
    let home = &dirs.home;
    let targets = Targets::new(dirs);
    // The daemon rewrites the plugin, the extensions, and the stub when it starts, so it is stopped
    // before they are removed.
    job::unload(home)?;
    let claude = remove_hook(&targets.claude, "claude", &CLAUDE_EVENTS)?;
    let codex = remove_hook(&targets.codex, "codex", &CODEX_EVENTS)?;
    let pi = PI_EXTENSION.uninstall(dirs)?;
    let omp = OMP_EXTENSION.uninstall(dirs)?;
    let opencode = OPENCODE_PLUGIN.uninstall(dirs)?;
    let daemon = job::remove(home)?;
    let [agents_skill, claude_skill] = skill::uninstall(dirs)?;
    // Last, so that a missing or failing `claude` leaves only the plugin for the next uninstall.
    let plugin = claude_plugin::uninstall(dirs, path)?;
    Ok(outcomes([
        ("claude_hook", claude),
        (claude_plugin::KEY, (plugin, None)),
        ("codex_hook", codex),
        ("pi_extension", pi),
        ("omp_extension", omp),
        ("opencode_plugin", opencode),
        agents_skill,
        claude_skill,
        (job::KEY, daemon),
    ]))
}

pub fn refresh_claude_hooks(dirs: &Dirs) -> Result<&'static str> {
    let Ok(path) = Targets::new(dirs).claude else {
        return Ok("skipped");
    };
    let settings = read_json(&path)?;
    let Some(hook) = ours(&settings, "SessionStart", "claude") else {
        return Ok("absent");
    };
    let hook = json!({ "type": "command", "command": hook["command"], "timeout": 5 });
    Ok(add_hook(dirs, &Ok(path), "claude", &CLAUDE_EVENTS[1..], &hook)?.0)
}

pub fn missing_claude_hooks(dirs: &Dirs) -> Result<Vec<&'static str>> {
    let path = Targets::new(dirs).claude.map_err(anyhow::Error::msg)?;
    let settings = read_json(&path)?;
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
fn add_hook(
    dirs: &Dirs,
    path: &Target,
    runtime: &str,
    events: &[&str],
    hook: &Value,
) -> Result<Outcome> {
    let path = match path {
        Ok(path) => path,
        Err(note) => return Ok(("skipped", Some(note.clone()))),
    };
    let dir = path.parent().unwrap_or(path);
    if !dir.is_dir() {
        return Ok((
            "skipped",
            Some(format!("`{}` does not exist", dirs.show(dir))),
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

fn remove_hook(path: &Target, runtime: &str, events: &[&str]) -> Result<Outcome> {
    let path = match path {
        Ok(path) => path,
        Err(note) => return Ok(("skipped", Some(note.clone()))),
    };
    if !path.exists() {
        return Ok(("absent", None));
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
        return Ok(("absent", None));
    }
    write_json(path, &settings)?;
    Ok(("removed", None))
}

#[cfg(test)]
mod tests;
