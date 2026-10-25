//! The daemon's side of the adapter connection: one JSON message per line each way. A call waits
//! for the reply with its ID; events that arrive meanwhile are kept for the event loop.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use serde_json::{Value, json};

use super::{CallError, classify};

/// A call the server leaves unanswered this long is treated as lost.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Pipe {
    stream: UnixStream,
    unread: Vec<u8>,
    events: VecDeque<Value>,
    next_id: u64,
    ended: bool,
}

impl Pipe {
    pub fn new(stream: UnixStream) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            unread: Vec::new(),
            events: VecDeque::new(),
            next_id: 0,
            ended: false,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    pub fn call(&mut self, method: &str, params: &Value) -> Result<Value, CallError> {
        log::debug!("call {method}");
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({ "id": id, "method": method, "params": params }).to_string() + "\n";
        let until = Instant::now() + CALL_TIMEOUT;
        self.write_all(line.as_bytes(), until)
            .map_err(|e| CallError::Unknown(format!("failed to send {method}: {e}")))?;
        loop {
            if let Some(reply) = self.take_reply(id) {
                return match reply.get("error").and_then(Value::as_str) {
                    Some(error) => Err(classify(error)),
                    None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
                };
            }
            if self.ended {
                return Err(CallError::Unknown(format!(
                    "the connection ended before {method} was answered"
                )));
            }
            if !wait(&self.stream, PollFlags::POLLIN, until) {
                return Err(CallError::Unknown(format!(
                    "no reply to {method} within {}s",
                    CALL_TIMEOUT.as_secs()
                )));
            }
            self.read_available();
        }
    }

    /// Events received so far, reading what the connection holds without waiting, and whether the
    /// connection has ended.
    pub fn events(&mut self) -> (Vec<Value>, bool) {
        self.read_available();
        self.take_reply(0);
        (self.events.drain(..).collect(), self.ended)
    }

    /// Whether events or lines read during calls are waiting, which the socket no longer signals.
    pub fn pending(&self) -> bool {
        !self.events.is_empty() || self.unread.contains(&b'\n')
    }

    /// Waits for an event matching `wanted` until `until`, keeping the others for the event loop.
    pub fn wait_event(&mut self, until: Instant, wanted: impl Fn(&Value) -> bool) -> Option<Value> {
        loop {
            self.take_reply(0);
            if let Some(i) = self.events.iter().position(&wanted) {
                return self.events.remove(i);
            }
            if self.ended || !wait(&self.stream, PollFlags::POLLIN, until) {
                return None;
            }
            self.read_available();
        }
    }

    fn read_available(&mut self) {
        let mut chunk = [0; 64 * 1024];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    self.ended = true;
                    return;
                }
                Ok(n) => self.unread.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => {
                    self.ended = true;
                    return;
                }
            }
        }
    }

    /// Splits the complete lines read so far into events and replies, and returns the reply to
    /// `id`. A reply to an earlier call that gave up waiting is dropped.
    fn take_reply(&mut self, id: u64) -> Option<Value> {
        let mut reply = None;
        while let Some(newline) = self.unread.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.unread.drain(..=newline).collect();
            let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            match message.get("id").and_then(Value::as_u64) {
                Some(got) if got == id => reply = Some(message),
                Some(_) => {}
                None => self.events.push_back(message),
            }
        }
        reply
    }

    fn write_all(&mut self, mut bytes: &[u8], until: Instant) -> std::io::Result<()> {
        while !bytes.is_empty() {
            match self.stream.write(bytes) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if !wait(&self.stream, PollFlags::POLLOUT, until) {
                        return Err(ErrorKind::TimedOut.into());
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Waits until the stream is ready for `flags` or has hung up, or `until` passes.
fn wait(stream: &UnixStream, flags: PollFlags, until: Instant) -> bool {
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        let timeout = PollTimeout::try_from(left).unwrap_or(PollTimeout::MAX);
        let mut fds = [PollFd::new(stream.as_fd(), flags)];
        match poll(&mut fds, timeout) {
            Ok(0) => return false,
            Ok(_) => return true,
            Err(nix::errno::Errno::EINTR) => {}
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};

    use super::*;

    #[test]
    fn a_call_gets_its_own_reply_and_keeps_the_events_around_it() -> std::io::Result<()> {
        let (ours, theirs) = UnixStream::pair()?;
        let mut pipe = Pipe::new(ours)?;
        let adapter = std::thread::spawn(move || -> std::io::Result<Vec<Value>> {
            let mut lines = BufReader::new(theirs.try_clone()?);
            let mut out = &theirs;
            let mut requests = Vec::new();
            let mut line = String::new();
            lines.read_line(&mut line)?;
            requests.push(serde_json::from_str::<Value>(&line)?);
            // An event, a stale reply, then the reply, as Lua writes them: nulls dropped, an
            // empty list as an object.
            writeln!(
                out,
                r#"{{"event":"session_view_changed","session_id":"s1"}}"#
            )?;
            writeln!(out, r#"{{"id":0,"result":{{}}}}"#)?;
            writeln!(
                out,
                r#"{{"id":{},"result":{{"windows":{{}}}}}}"#,
                requests[0]["id"]
            )?;
            line.clear();
            lines.read_line(&mut line)?;
            requests.push(serde_json::from_str::<Value>(&line)?);
            writeln!(
                out,
                r#"{{"id":{},"error":"no (status=400 code=bad_request)"}}"#,
                requests[1]["id"]
            )?;
            // Dropping this end may not end the connection: a process another test forks can
            // inherit it before it is marked close-on-exec.
            theirs.shutdown(std::net::Shutdown::Both)?;
            Ok(requests)
        });
        let reply = pipe
            .call("session.view", &json!({ "session_id": "s1" }))
            .expect("reply");
        assert_eq!(reply, json!({ "windows": {} }));
        assert!(matches!(
            pipe.call("block.close", &json!({})),
            Err(CallError::Rejected { ref code, .. }) if code == "bad_request"
        ));
        let requests = adapter.join().expect("adapter thread")?;
        assert_eq!(requests[0]["method"], "session.view");
        let (events, ended) = pipe.events();
        assert_eq!(
            events,
            [json!({ "event": "session_view_changed", "session_id": "s1" })]
        );
        assert!(ended);
        assert!(matches!(
            pipe.call("session.list", &json!({})),
            Err(CallError::Unknown(_))
        ));
        Ok(())
    }
}
