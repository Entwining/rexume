//! `rexume relay` carries each line as it arrives, both ways, while the connection stays open.

use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::common::{REXUME, Running};

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
            .arg("token")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (mut daemon, _) = listener.accept().unwrap();
    daemon.set_read_timeout(Some(WAIT)).unwrap();
    let mut from_relay = BufReader::new(daemon.try_clone().unwrap());
    let mut line = String::new();
    from_relay.read_line(&mut line).unwrap();
    assert_eq!(line, "token\n");

    let stdout = relay.0.stdout.take().unwrap();
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sent.send(line.unwrap()).is_err() {
                return;
            }
        }
    });
    daemon.write_all(b"{\"id\":1}\n").unwrap();
    assert_eq!(received.recv_timeout(WAIT).unwrap(), r#"{"id":1}"#);

    let stdin = relay.0.stdin.as_mut().unwrap();
    stdin.write_all(b"{\"id\":1,\"result\":{}}\n").unwrap();
    line.clear();
    from_relay.read_line(&mut line).unwrap();
    assert_eq!(line, "{\"id\":1,\"result\":{}}\n");
}
