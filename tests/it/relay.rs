use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixListener;
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
