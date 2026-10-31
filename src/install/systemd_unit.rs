//! The systemd user service that runs the daemon: its unit file and the user manager's loaded
//! copy of it.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Install, Outcome, remove_file, run_manager, write};

/// The keys of install's and uninstall's output, and the unit's name.
pub(super) const KEY: &str = "systemd_unit";
pub(super) const NAME_KEY: &str = "systemd_unit_name";
pub(super) const NAME: &str = "rexume.service";

/// Where the user manager reads the user's own units.
fn unit_path(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("systemd/user")
        .join(NAME)
}

/// Writes the unit when it differs, and restarts the service unless it is active, enabled, and
/// loaded from that unit: a running service keeps the definition it started with.
pub(super) fn install(install: &Install, home: &Path) -> Result<&'static str> {
    let path = unit_path(home);
    let wanted = unit(install);
    let current = match std::fs::read(&path) {
        Ok(current) => Some(current),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("failed to read `{}`", path.display())),
    };
    let changed = current.as_deref() != Some(wanted.as_bytes());
    let state = state().context(
        "failed to reach the systemd user manager, which runs the daemon; without one, run \
         `rexume daemon` from another service manager",
    )?;
    if !changed && !state.stale && state.active && state.enabled {
        return Ok("unchanged");
    }
    // Stopping the old service before writing means that whichever step fails, the unit still
    // differs, the manager still needs a reload, or the service is not running, and the next
    // install finishes the restart.
    if state.running {
        systemctl(&["stop", NAME])?;
    }
    if changed {
        write(&path, wanted.as_bytes())?;
    }
    // Every start counts against the start limit, install's own too, and a service that reached it
    // refuses starts until the limit's window ends. The manager unloads a stopped service unless it
    // is enabled or failed, and refuses to reset one it has unloaded.
    if state.enabled || state.failed {
        systemctl(&["reset-failed", NAME])?;
    }
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", NAME])?;
    Ok(if current.is_none() {
        "created"
    } else {
        "updated"
    })
}

/// Stops the service and drops it from `default.target` when the manager has it.
pub(super) fn unload(home: &Path) -> Result<()> {
    let state = match state() {
        Ok(state) => state,
        // Install writes the hooks before it fails to reach the manager, so uninstall still
        // removes them when install never wrote a unit.
        Err(_) if !unit_path(home).exists() => return Ok(()),
        Err(e) => return Err(e),
    };
    if state.loaded {
        systemctl(&["disable", "--now", NAME])?;
    } else if state.running {
        systemctl(&["stop", NAME])?;
    }
    Ok(())
}

/// Removes the unit of a service already stopped, and has the manager forget it.
pub(super) fn remove(home: &Path) -> Result<Outcome> {
    let outcome = remove_file(&unit_path(home), |_| true)?;
    if outcome.0 == "removed" {
        systemctl(&["daemon-reload"])?;
    }
    Ok(outcome)
}

pub(super) fn unit(install: &Install) -> String {
    let args = [
        install.exe.to_string_lossy().into_owned(),
        "daemon".to_owned(),
        "--rex".to_owned(),
        install.rex.to_string_lossy().into_owned(),
        "--server".to_owned(),
        install.server.clone(),
    ];
    // systemd substitutes variables in a command's arguments but not in its executable path.
    let command: Vec<String> = args
        .iter()
        .enumerate()
        .map(|(i, arg)| quote(arg, i > 0))
        .collect();
    let path = quote(&format!("PATH={}", install.path), false);
    // The service restarts after any exit, as it does after the daemon exits for a replaced
    // executable. With 2 seconds between starts, six never fall within the default window of 10
    // seconds, so the window is 30 seconds, and the retries end at the sixth start within it, as
    // once the executable is gone.
    format!(
        "[Unit]
Description=rexume, which reopens agent conversations in restored Rex panes
StartLimitIntervalSec=30
StartLimitBurst=5

[Service]
ExecStart={}
Environment={path}
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
",
        command.join(" ")
    )
}

/// Quotes one word of a unit setting. systemd expands `%` specifiers in every setting and, where
/// `variables` says so, `$` variables, and reads C escapes inside quotes.
fn quote(word: &str, variables: bool) -> String {
    let mut quoted = String::from('"');
    for c in word.chars() {
        match c {
            '"' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '\n' => quoted.push_str("\\n"),
            '%' => quoted.push_str("%%"),
            '$' if variables => quoted.push_str("$$"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// The manager's view of the service.
struct State {
    /// The manager has read a unit file for it.
    loaded: bool,
    /// Started and not between restarts.
    active: bool,
    running: bool,
    /// The service stopped on an error or its start limit.
    failed: bool,
    enabled: bool,
    /// The unit file changed since the manager read it.
    stale: bool,
}

fn state() -> Result<State> {
    let output = systemctl(&[
        "show",
        "--property=LoadState,ActiveState,UnitFileState,NeedDaemonReload",
        NAME,
    ])?;
    let value = |key: &str| {
        output
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_default()
    };
    let active = value("ActiveState");
    Ok(State {
        loaded: value("LoadState") == "loaded",
        active: active == "active",
        running: !matches!(active, "inactive" | "failed"),
        failed: active == "failed",
        enabled: value("UnitFileState") == "enabled",
        stale: value("NeedDaemonReload") == "yes",
    })
}

fn systemctl(args: &[&str]) -> Result<String> {
    run_manager("systemctl", &[&["--user"], args].concat())
}
