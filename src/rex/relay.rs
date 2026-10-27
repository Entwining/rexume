//! `rexume relay`: the adapter script's child process. Rex's Lua API can start a process and talk
//! to it, but cannot open a socket, so this process carries bytes between its standard streams
//! and the daemon's socket. It exits when either side ends, which ends the other.

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result};

pub fn run(socket: &Path, token: &str) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("failed to connect to `{}`", socket.display()))?;
    writeln!(stream, "{token}")?;
    let mut up = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut up);
        let _ = up.shutdown(Shutdown::Write);
    });
    // Not `std::io::copy`: on Linux it splices the socket into the stdout pipe, and that splice
    // passed nothing on until the daemon's side closed, so no call reached the adapter.
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
