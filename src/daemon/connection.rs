use std::convert::Infallible;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::{ADAPTER, Daemon, EXE, Exit, FIRST_FILE, SERVER_DIR, STOP, WAL};
use crate::event::Fired;
use crate::rex::{Pipe, require_str, str_at};
use crate::store::new_id;

const CONNECT_WAIT: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Rex gives no sign of when the adapter's subscription is live and never shows a client that
/// joined before, so short attempts retry soon after it is.
const WITNESS_WAIT: Duration = Duration::from_millis(300);
const WITNESS_ATTEMPTS: usize = 20;
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
            if started.elapsed() > MAX_BACKOFF {
                backoff = Duration::from_secs(1);
            }
            self.sleep(backoff)?;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    pub(super) fn wait(&mut self, timeout: Option<Duration>) -> Result<Vec<Fired>> {
        let fired = self.queue.wait(timeout)?;
        if fired.iter().any(|f| f.token == STOP) {
            return Err(Exit::Stopped.into());
        }
        if fired.iter().any(|f| f.token == EXE && f.gone) {
            return Err(Exit::Replaced.into());
        }
        for f in &fired {
            match f.token {
                WAL if f.gone => {
                    self.queue
                        .file(&self.options.state_dir.join("state.db-wal"), WAL)?;
                }
                token if token >= FIRST_FILE && (f.gone || self.via_dir.remove(&token)) => {
                    self.files.retain(|_, t| *t != token);
                    self.queue.forget_file(token);
                }
                _ => {}
            }
        }
        Ok(fired)
    }

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
            let mut below = path.as_path();
            for dir in path.ancestors().skip(1) {
                if self.queue.file(dir, SERVER_DIR)? {
                    break;
                }
                below = dir;
            }
            // `below` may have appeared before the watch was in place, which fires nothing for it.
            if !below.exists() {
                self.wait(None)?;
            }
        }
        self.queue.forget_file(SERVER_DIR);
        Ok(())
    }

    fn sleep(&mut self, duration: Duration) -> Result<()> {
        if let Some(dir) = self
            .options
            .cli
            .socket_path()
            .as_deref()
            .and_then(Path::parent)
        {
            self.queue.file(dir, SERVER_DIR)?;
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
        self.queue.forget_file(SERVER_DIR);
        Ok(())
    }

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
                self.queue.forget_exit(i64::from(child.id()));
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    fn accept(&mut self, child: &Child, token: &str) -> Result<Pipe> {
        let until = Instant::now() + CONNECT_WAIT;
        if !self.queue.exit(i64::from(child.id()), ADAPTER)? {
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

    pub(super) fn witness(&mut self, pipe: &mut Pipe) -> Result<(String, Option<String>)> {
        for _ in 0..WITNESS_ATTEMPTS {
            let reply = self.options.cli.call(
                "session.bootstrap",
                &json!({ "event_scope": "all", "attach_session_ids": [] }),
            )?;
            let client = require_str(&reply, "/client_id")?;
            let instance = require_str(&reply, "/server_status/instance_id")?;
            let host = str_at(&reply, "/server_status/host_id").map(str::to_owned);
            let joined = pipe.wait_event(Instant::now() + WITNESS_WAIT, |event| {
                str_at(event, "/event") == Some("client_joined")
                    && str_at(event, "/payload/client/client_id") == Some(client.as_str())
            });
            if joined.is_some() {
                return Ok((instance, host));
            }
            if pipe.events().1 {
                bail!("the adapter's connection ended before its subscription was confirmed");
            }
        }
        bail!("the adapter's stream never showed this daemon's own client")
    }
}

/// Byte by byte, so that what follows the token line is left for `Pipe`.
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
