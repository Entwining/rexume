//! The LaunchAgent that runs the daemon: its plist and launchd's loaded copy of it.

use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::{Install, LABEL, Targets, write};

/// Writes the plist when its parsed content differs, and loads the job when the plist changed or
/// the job is not loaded: a loaded job keeps the definition it was loaded with.
pub(super) fn install(install: &Install, targets: &Targets) -> Result<&'static str> {
    let wanted = plist(install, &targets.logs.join("rexume.log"));
    let current = match plist::Value::from_file(&targets.plist) {
        Ok(current) => Some(current),
        Err(e) if e.as_io().is_some_and(|io| io.kind() == ErrorKind::NotFound) => None,
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to parse `{}`", targets.plist.display()));
        }
    };
    let mut bytes = Vec::new();
    wanted.to_writer_xml(&mut bytes)?;
    bytes.push(b'\n');
    let changed = current.as_ref() != Some(&wanted);
    if changed {
        write(&targets.plist, &bytes)?;
    }
    let loaded = loaded()?;
    if !changed && loaded {
        return Ok("unchanged");
    }
    if loaded {
        launchctl(&["bootout", &service()])?;
    }
    launchctl(&["bootstrap", &domain(), &targets.plist.to_string_lossy()])?;
    Ok(if current.is_none() {
        "created"
    } else {
        "updated"
    })
}

/// Stops the job when launchd has it loaded.
pub(super) fn unload() -> Result<()> {
    if loaded()? {
        launchctl(&["bootout", &service()])?;
    }
    Ok(())
}

pub(super) fn plist(install: &Install, log: &Path) -> plist::Value {
    let mut args = vec![
        install.exe.to_string_lossy().into_owned(),
        "daemon".to_owned(),
        "--rex".to_owned(),
        install.rex.to_string_lossy().into_owned(),
        "--server".to_owned(),
        install.server.clone(),
    ];
    if let Some((foreground, background)) = &install.theme {
        args.extend([
            "--theme-foreground".to_owned(),
            foreground.clone(),
            "--theme-background".to_owned(),
            background.clone(),
        ]);
    }
    let log = log.to_string_lossy().into_owned();
    let exe = install.exe.to_string_lossy().into_owned();
    let dict =
        |pairs: Vec<(&str, plist::Value)>| plist::Value::Dictionary(pairs.into_iter().collect());
    // The job runs only while the executable exists, so removing rexume without uninstall stops
    // it instead of leaving launchd to restart a missing program.
    dict(vec![
        ("Label", LABEL.to_owned().into()),
        (
            "ProgramArguments",
            plist::Value::Array(args.into_iter().map(plist::Value::from).collect()),
        ),
        (
            "EnvironmentVariables",
            dict(vec![("PATH", install.path.clone().into())]),
        ),
        ("ProcessType", "Background".to_owned().into()),
        (
            "KeepAlive",
            dict(vec![("PathState", dict(vec![(exe.as_str(), true.into())]))]),
        ),
        ("ThrottleInterval", 2.into()),
        ("StandardOutPath", log.clone().into()),
        ("StandardErrorPath", log.into()),
    ])
}

fn domain() -> String {
    format!("gui/{}", nix::unistd::getuid())
}

fn service() -> String {
    format!("{}/{LABEL}", domain())
}

fn loaded() -> Result<bool> {
    let status = Command::new("launchctl")
        .args(["print", &service()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run `launchctl`")?;
    Ok(status.success())
}

fn launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .context("failed to run `launchctl`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.trim().is_empty() {
            bail!("`launchctl {}` exited {}", args.join(" "), output.status);
        }
        bail!("`launchctl {}` failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}
