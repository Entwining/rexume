//! The LaunchAgent that runs the daemon: its plist and launchd's loaded copy of it.

use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{Install, LABEL, Targets, write};
use crate::kqueue::Kqueue;

/// How long a stop waits for the daemon to exit. The daemon exits on `SIGTERM` once `rex do` has
/// ended, and launchd kills a job that outlives its exit timeout, so this only bounds a stop
/// that neither ends.
const EXIT_DEADLINE: Duration = Duration::from_secs(30);

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
    let job = loaded()?;
    if !changed && job.is_some() {
        return Ok("unchanged");
    }
    // Stopping the old job before writing means that whichever step fails, the plist still
    // differs or the job is not loaded, and the next install finishes the reload.
    if let Some(job) = job {
        stop(&job)?;
    }
    if changed {
        write(&targets.plist, &bytes)?;
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
    if let Some(job) = loaded()? {
        stop(&job)?;
    }
    Ok(())
}

/// launchd's loaded copy of the job.
struct Loaded {
    /// The daemon's process, while it runs.
    pid: Option<i64>,
}

/// Boots the job out and returns once launchd no longer has it. `bootout` returns as soon as
/// launchd has sent `SIGTERM`, and the job stays loaded until its process exits, so a `bootstrap`
/// before then fails with `5: Input/output error`; a job with no process is gone at once.
fn stop(job: &Loaded) -> Result<()> {
    let kq = Kqueue::new()?;
    // Watched before the bootout, so that an exit in between is not missed.
    let running = match job.pid {
        Some(pid) => kq.exit(pid, 0)?,
        None => false,
    };
    launchctl(&["bootout", &service()])?;
    if running && kq.wait(Some(EXIT_DEADLINE))?.is_empty() {
        bail!(
            "the daemon did not exit within {}s of `launchctl bootout`",
            EXIT_DEADLINE.as_secs()
        );
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

fn loaded() -> Result<Option<Loaded>> {
    let output = Command::new("launchctl")
        .args(["print", &service()])
        .output()
        .context("failed to run `launchctl`")?;
    if !output.status.success() {
        return Ok(None);
    }
    // The service's own properties are indented by one tab, those of its sections by more.
    let pid = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("\tpid = ")?.parse().ok());
    Ok(Some(Loaded { pid }))
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
