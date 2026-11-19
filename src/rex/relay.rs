//! Rex's Lua API can start a process and talk to it but cannot open a socket, so this process
//! carries bytes between its standard streams and the daemon's socket.

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::sys::socket::{getsockopt, sockopt};
use nix::unistd::Pid;

use crate::proc;

pub fn run(socket: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("failed to connect to `{}`", socket.display()))?;
    let mut up = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut up);
        let _ = up.shutdown(Shutdown::Write);
    });
    // Not `std::io::copy`: on Linux it splices the socket into the stdout pipe, and the splice
    // passes nothing on until the daemon's side closes.
    let mut out = std::io::stdout().lock();
    let mut chunk = [0; 64 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                out.write_all(&chunk[..n])?;
                out.flush()?;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// Only a daemon up to 0.1.25 that missed the replacement of its executable passes the token as an
/// argument; it exits 0 on `SIGTERM`, and its job then starts this version. See AGENTS.md.
pub fn hand_over(socket: &Path) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("failed to connect to `{}`", socket.display()))?;
    // Before the daemon accepts, the peer is the process that listens on the socket.
    #[cfg(target_os = "macos")]
    let listener = getsockopt(&stream, sockopt::LocalPeerPid)?;
    #[cfg(target_os = "linux")]
    let listener = getsockopt(&stream, sockopt::PeerCredentials)?.pid();
    if !is_ancestor(i64::from(listener)) {
        bail!(
            "the process listening on `{}` did not start this relay, so it was not signalled",
            socket.display()
        );
    }
    kill(Pid::from_raw(listener), Signal::SIGTERM)?;
    bail!(
        "the daemon (pid {listener}) is from a version before this one; it was asked to stop so that its job starts this one"
    )
}

/// Stopping before PID 1 also refuses a listener reported as 0, which Linux reports for a process
/// outside this PID namespace and which `kill` would take as this process group.
fn is_ancestor(pid: i64) -> bool {
    std::iter::successors(proc::parent(i64::from(std::process::id())), |&parent| {
        proc::parent(parent)
    })
    .take_while(|&parent| parent > 1)
    .take(64)
    .any(|parent| parent == pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_process_above_this_one_and_below_pid_1_is_an_ancestor() {
        let parent = proc::parent(i64::from(std::process::id())).unwrap();
        assert!(parent > 1 && is_ancestor(parent));
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .unwrap();
        assert!(!is_ancestor(i64::from(child.id())));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!is_ancestor(0));
        assert!(!is_ancestor(1));
    }
}
