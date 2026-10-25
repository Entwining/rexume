//! Reaching the server: waiting for its socket, starting `rex do`, and proving the adapter's
//! connection and subscription before anything is read through it.

use std::convert::Infallible;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::{ADAPTER, Daemon, EXE, Exit, SERVER_DIR, STOP};
use crate::kqueue::Fired;
use crate::rex::{Pipe, require_str, str_at};
use crate::store::new_id;

const CONNECT_WAIT: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// The adapter's subscription gives no sign of when it is live, and a client that joined before
/// then never shows up, so short attempts retry soon after it is.
const WITNESS_WAIT: Duration = Duration::from_millis(300);
const WITNESS_ATTEMPTS: usize = 20;
/// The longest token line a relay may send; a token is a UUID.
const MAX_TOKEN_LINE: usize = 128;

impl Daemon<'_> {
    pub(super) fn serve(&mut self) -> Result<Infallible> {
        let mut backoff = Duration::from_secs(1);
        loop {
            self.wait_for_server()?;
            let started = Instant::now();
            match self.observe() {
                Ok(reason) => log::info!("epoch ended: {reason}"),
                Err(e) if e.is::<Exit>() => return Err(e),
                Err(e) => log::error!("epoch ended with an error: {e:#}"),
            }
            // A connection that held for a while is not a failing one.
            if started.elapsed() > MAX_BACKOFF {
                backoff = Duration::from_secs(1);
            }
            self.sleep(backoff)?;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    pub(super) fn wait(&self, timeout: Option<Duration>) -> Result<Vec<Fired>> {
        let fired = self.kq.wait(timeout)?;
        if fired.iter().any(|f| f.token == STOP) {
            return Err(Exit::Stopped.into());
        }
        if fired.iter().any(|f| f.token == EXE && f.gone) {
            return Err(Exit::Replaced.into());
        }
        Ok(fired)
    }

    /// Returns once the server's socket file exists, watching its directory meanwhile. Checking
    /// for the file starts no client.
    fn wait_for_server(&mut self) -> Result<()> {
        let Some(path) = self.options.cli.socket_path() else {
            return Ok(());
        };
        let mut waiting = false;
        while !path.exists() {
            if !waiting {
                log::info!("waiting for the server at {}", path.display());
                waiting = true;
            }
            // The nearest directory that exists changes when the next one down is created.
            let mut below = path.as_path();
            for dir in path.ancestors().skip(1) {
                if self.kq.file(dir, SERVER_DIR)? {
                    break;
                }
                below = dir;
            }
            // An entry created below the watched directory before the watch was in place fires
            // nothing, so it is checked once the watch is.
            if !below.exists() {
                self.wait(None)?;
            }
        }
        self.kq.forget_file(SERVER_DIR);
        Ok(())
    }

    /// Waits without observing anything, returning early when the server's socket reappears.
    fn sleep(&mut self, duration: Duration) -> Result<()> {
        if let Some(dir) = self
            .options
            .cli
            .socket_path()
            .as_deref()
            .and_then(Path::parent)
        {
            self.kq.file(dir, SERVER_DIR)?;
        }
        let until = Instant::now() + duration;
        while Instant::now() < until {
            if self
                .wait(Some(until.saturating_duration_since(Instant::now())))?
                .iter()
                .any(|f| f.token == SERVER_DIR)
            {
                break;
            }
        }
        self.kq.forget_file(SERVER_DIR);
        Ok(())
    }

    /// Starts the adapter and takes the connection its relay makes back, proven by its token.
    pub(super) fn connect(&mut self) -> Result<(Child, Pipe)> {
        let token = new_id();
        let relay = [
            self.options.exe.to_string_lossy().into_owned(),
            "relay".to_owned(),
            self.socket.to_string_lossy().into_owned(),
        ];
        let mut child = self
            .options
            .cli
            .adapter(&relay, &token)
            .context("failed to start `rex do`")?;
        log::info!("started rex do as pid {}", child.id());
        match self.accept(&child, &token) {
            Ok(pipe) => Ok((child, pipe)),
            Err(e) => {
                self.kq.forget_exit(i64::from(child.id()));
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    fn accept(&mut self, child: &Child, token: &str) -> Result<Pipe> {
        let until = Instant::now() + CONNECT_WAIT;
        if !self.kq.exit(i64::from(child.id()), ADAPTER)? {
            bail!("`rex do` exited at once");
        }
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if first_line(&stream)? == token {
                        return Ok(Pipe::new(stream)?);
                    }
                    log::warn!("refused a connection without this adapter's token");
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "the adapter did not connect within {}s",
                    CONNECT_WAIT.as_secs()
                );
            }
            if self.wait(Some(left))?.iter().any(|f| f.token == ADAPTER) {
                bail!("`rex do` exited before connecting");
            }
        }
    }

    /// Proves the adapter's subscription is live before anything is read: a client started after
    /// it subscribed must show up on its stream. Reads before that could miss changes.
    pub(super) fn witness(&mut self, pipe: &mut Pipe) -> Result<String> {
        for _ in 0..WITNESS_ATTEMPTS {
            let reply = self.options.cli.call(
                "session.bootstrap",
                &json!({ "event_scope": "all", "attach_session_ids": [] }),
            )?;
            let client = require_str(&reply, "/client_id")?;
            let instance = require_str(&reply, "/server_status/instance_id")?;
            let joined = pipe.wait_event(Instant::now() + WITNESS_WAIT, |event| {
                str_at(event, "/event") == Some("client_joined")
                    && str_at(event, "/payload/client/client_id") == Some(client.as_str())
            });
            if joined.is_some() {
                return Ok(instance);
            }
            if pipe.events().1 {
                bail!("the adapter's connection ended before its subscription was confirmed");
            }
        }
        bail!("the adapter's stream never showed this daemon's own client")
    }
}

/// The token line a relay sends first, read byte by byte so nothing after it is consumed.
fn first_line(stream: &UnixStream) -> Result<String> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut line = Vec::new();
    let mut byte = [0];
    loop {
        if (&*stream).read(&mut byte)? == 0 {
            bail!("the connection ended before its token");
        }
        match byte[0] {
            b'\n' => break,
            byte if line.len() < MAX_TOKEN_LINE => line.push(byte),
            _ => bail!("the token line is longer than {MAX_TOKEN_LINE} bytes"),
        }
    }
    stream.set_read_timeout(None)?;
    Ok(String::from_utf8_lossy(&line).into_owned())
}
