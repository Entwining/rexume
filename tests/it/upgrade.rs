use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
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
        // The extension an older version's install wrote.
        let extension = dir.path().join("home/.pi/agent/extensions/rexume.ts");
        std::fs::create_dir_all(extension.parent().unwrap()).unwrap();
        std::fs::write(&extension, "// Written by `rexume install`, long ago\n").unwrap();
        // The state directory an earlier version created, open to other users.
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state, Permissions::from_mode(0o755)).unwrap();
        let log = dir.path().join("daemon.log");
        let mut daemon = Running(
            Command::new(&exe)
                .arg("daemon")
                .arg("--state-dir")
                .arg(&state)
                .args(["--rex", "/usr/bin/false", "--server"])
                .arg(format!("unix://{}", dir.path().join("s.sock").display()))
                .env("HOME", dir.path().join("home"))
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
        assert_eq!(
            std::fs::read_to_string(&extension).unwrap(),
            include_str!("../../src/runtime/pi/extension.ts"),
            "a starting daemon brings the extension up to date"
        );
        assert_eq!(
            std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700,
            "a starting daemon restricts the state directory to its user"
        );
        assert!(read_log().contains("restricted"), "{}", read_log());

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
