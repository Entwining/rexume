use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::common::{REXUME, Running, wait_until};

/// Installs a copy of rexume at `to`, as a package manager does: the new file is renamed into
/// place, never written over the old one.
fn install(to: &Path) {
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    let partial = to.with_extension("partial");
    std::fs::copy(REXUME, &partial).unwrap();
    std::fs::rename(&partial, to).unwrap();
}

#[test]
fn a_daemon_exits_when_an_upgrade_replaces_its_executable() {
    // Homebrew relinks a symbolic link to the new version; Cargo replaces the file itself.
    for linked in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bin/rexume");
        install(&dir.path().join("Cellar/1/rexume"));
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        if linked {
            std::os::unix::fs::symlink(dir.path().join("Cellar/1/rexume"), &exe).unwrap();
        } else {
            install(&exe);
        }
        let log = dir.path().join("daemon.log");
        let mut daemon = Running(
            Command::new(&exe)
                .arg("daemon")
                .arg("--state-dir")
                .arg(dir.path().join("state"))
                .args(["--rex", "/usr/bin/false", "--server"])
                .arg(format!("unix://{}", dir.path().join("s.sock").display()))
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        let read_log = || std::fs::read_to_string(&log).unwrap_or_default();
        wait_until(
            "the daemon to wait for its server",
            Duration::from_secs(10),
            || read_log().contains("waiting for the server").then_some(()),
        );

        if linked {
            install(&dir.path().join("Cellar/2/rexume"));
            let link = dir.path().join("bin/rexume.new");
            std::os::unix::fs::symlink(dir.path().join("Cellar/2/rexume"), &link).unwrap();
            std::fs::rename(&link, &exe).unwrap();
        } else {
            install(&exe);
        }
        let status = wait_until("the daemon to exit", Duration::from_secs(10), || {
            daemon.0.try_wait().unwrap()
        });
        assert!(status.success(), "{}", read_log());
        assert!(
            read_log().contains("executable was replaced"),
            "{}",
            read_log()
        );
    }
}
