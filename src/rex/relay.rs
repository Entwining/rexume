//! `rexume relay`: the adapter script's child process. Rex's Lua API can start a process and talk
//! to it, but cannot open a socket, so this process carries bytes between its standard streams
//! and the daemon's socket. It exits when either side ends, which ends the other.

use std::io::Write;
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
    std::io::copy(&mut stream, &mut std::io::stdout().lock())?;
    Ok(())
}
