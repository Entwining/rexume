use std::process::{Command, Stdio};
use std::time::Duration;

use crate::common::{REXUME, Running, wait_until};

#[test]
fn a_daemon_starts_as_soon_as_a_server_appears_under_directories_that_did_not_exist() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rex/run/s.sock");
    let log = dir.path().join("daemon.log");
    let _daemon = Running(
        Command::new(REXUME)
            .arg("daemon")
            .arg("--state-dir")
            .arg(dir.path().join("state"))
            .args(["--rex", "/usr/bin/false", "--server"])
            .arg(format!("unix://{}", socket.display()))
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

    std::fs::create_dir(dir.path().join("rex")).unwrap();
    std::fs::create_dir(dir.path().join("rex/run")).unwrap();
    std::fs::write(&socket, "").unwrap();
    // A daemon woken by the directories, not by a timer, starts its client at once.
    wait_until(
        "the daemon to start its client",
        Duration::from_secs(2),
        || read_log().contains("started rex do").then_some(()),
    );
}
