use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::json;

use crate::common::{
    Daemon, REXUME, Running, Server, children, query, shell_block, shell_pids, wait_until,
};

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn a_line_crosses_the_relay_before_the_connection_ends() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("relay.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut relay = Running(
        Command::new(REXUME)
            .arg("relay")
            .arg(&socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (mut daemon, _) = listener.accept().unwrap();
    daemon.set_read_timeout(Some(WAIT)).unwrap();
    let sent = b"token\n{\"id\":1,\"result\":{}}\n";
    relay.0.stdin.as_mut().unwrap().write_all(sent).unwrap();
    let mut received = vec![0; sent.len()];
    daemon.read_exact(&mut received).unwrap();
    assert_eq!(received, sent);

    let stdout = relay.0.stdout.take().unwrap();
    let (lines, read) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines.send(line.unwrap()).is_err() {
                return;
            }
        }
    });
    daemon.write_all(b"{\"id\":2}\n").unwrap();
    assert_eq!(read.recv_timeout(WAIT).unwrap(), r#"{"id":2}"#);
}

fn command_line(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

#[test]
#[ignore = "needs a Rex server"]
fn the_adapters_token_stays_off_every_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let server = Server::start(dir.path(), None);
    let daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", Duration::from_secs(20), || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    // The daemon records a pane's shell only from an event and a call that cross the relay.
    let created = server.call(
        "session.create",
        &json!({ "label": "s", "initial_windows": [{ "layout": shell_block("a", dir.path()) }] }),
    );
    let session = created["session_id"].as_str().unwrap();
    let block = created["initial_windows"][0]["block_ids"][0]
        .as_str()
        .unwrap();
    shell_pids(&server, &state, session, &[block]);

    let adapter = daemon.adapter().unwrap();
    let (relay, _) = children(adapter)
        .into_iter()
        .find(|(_, name)| name.ends_with("rexume"))
        .expect("the relay");
    let adapter_line = command_line(adapter);
    assert!(
        adapter_line.contains(" do --all --args - -e "),
        "{adapter_line}"
    );
    assert!(!adapter_line.contains("\"token\""), "{adapter_line}");
    assert_eq!(
        command_line(relay),
        format!("{REXUME} relay {}", state.join("daemon.sock").display())
    );
}

const LEGACY_TOKEN: &str = "019a0000-0000-7000-8000-000000000000";

fn quoted(path: impl AsRef<Path>) -> String {
    shlex::try_quote(&path.as_ref().to_string_lossy())
        .unwrap()
        .into_owned()
}

fn read_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("daemon.log")).unwrap_or_default()
}

/// Starts a daemon in `dir` that waits for its server until `server.sock` exists there, and whose
/// `rex do` runs `script` after reading its arguments.
fn daemon_waiting_for_its_server(dir: &Path, script: &str) -> Running {
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::process::CommandExt as _;
    let rex = dir.join("rex");
    std::fs::write(&rex, format!("#!/bin/sh\ncat >/dev/null\n{script}")).unwrap();
    std::fs::set_permissions(&rex, std::fs::Permissions::from_mode(0o755)).unwrap();
    let daemon = Running(
        Command::new(REXUME)
            .arg("daemon")
            .arg("--state-dir")
            .arg(dir.join("state"))
            .arg("--rex")
            .arg(&rex)
            .arg("--server")
            .arg(format!("unix://{}", dir.join("server.sock").display()))
            .env_clear()
            .env("HOME", dir)
            .env("PATH", "/usr/bin:/bin")
            .env("RUST_LOG", "info")
            .process_group(0)
            .stderr(std::fs::File::create(dir.join("daemon.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    // Logged once the daemon listens on its socket and receives signals.
    wait_until("the daemon to wait for its server", WAIT, || {
        read_log(dir)
            .contains("waiting for the server")
            .then_some(())
    });
    daemon
}

/// A connection left pending on the socket would end every wait of the daemon at once.
#[test]
fn a_connection_the_daemon_is_not_waiting_for_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = daemon_waiting_for_its_server(dir.path(), "");
    let mut stream = UnixStream::connect(dir.path().join("state/daemon.sock")).unwrap();
    // Not a read timeout: macOS refuses to set one on a socket whose peer already closed.
    let (read, ended) = mpsc::channel();
    std::thread::spawn(move || read.send(stream.read(&mut [0]).unwrap()));
    assert_eq!(ended.recv_timeout(WAIT).unwrap(), 0);
    wait_until("the refusal in the log", WAIT, || {
        read_log(dir.path())
            .contains("refused a connection the daemon was not waiting for")
            .then_some(())
    });
    assert!(daemon.0.try_wait().unwrap().is_none());
}

/// A daemon up to 0.1.25 that missed the replacement of its executable runs this relay with the
/// token as an argument; the relay stops that daemon so that its job starts this version, and no
/// other process that listens on the socket.
#[test]
fn a_relay_given_a_token_stops_only_the_daemon_that_started_it() {
    let dir = tempfile::tempdir().unwrap();
    let (rexume, socket) = (quoted(REXUME), quoted(dir.path().join("state/daemon.sock")));
    // Stands in for an old `rex do`, which stays the relay's parent. The connection before it,
    // which ends without a line, must not cost the daemon this attempt.
    let mut daemon = daemon_waiting_for_its_server(
        dir.path(),
        &format!(
            "{rexume} relay {socket} </dev/null\n{rexume} relay {socket} {LEGACY_TOKEN}\nexec sleep 10\n"
        ),
    );

    let stranger = Command::new(REXUME)
        .arg("relay")
        .arg(dir.path().join("state/daemon.sock"))
        .arg(LEGACY_TOKEN)
        .output()
        .unwrap();
    // The daemon may close the connection before the relay reads its peer, so which refusal the
    // relay reports varies; `relay::tests` checks the ancestry itself.
    let stderr = String::from_utf8_lossy(&stranger.stderr);
    assert!(!stranger.status.success());
    assert!(!stderr.contains("asked to stop"), "{stderr}");

    // The server's path only has to exist for the daemon to start `rex do`.
    std::fs::write(dir.path().join("server.sock"), "").unwrap();
    let exited = wait_until("the daemon to exit", WAIT, || daemon.0.try_wait().unwrap());
    let log = read_log(dir.path());
    assert!(exited.success(), "{log}");
    assert!(log.contains("stopped by a signal"), "{log}");
    assert!(
        log.contains("refused a connection: the connection ended before its token"),
        "{log}"
    );
    assert_eq!(log.matches("started rex do").count(), 1, "{log}");
}
