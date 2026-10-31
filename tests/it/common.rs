//! An isolated Rex server and daemon for scenario tests. Each test gets its own directory with
//! the server's socket, state, and home, so nothing reaches the user's own server.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};

pub mod claude;
mod server;

pub use server::{SHELL, Server, rex_bin};

pub const REXUME: &str = env!("CARGO_BIN_EXE_rexume");

/// A daemon started without the harness, ended when an assertion fails before it exits.
pub struct Running(pub Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Polls until `check` returns a value or `timeout` passes; test code may wait, the product never
/// polls.
pub fn poll<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let until = Instant::now() + timeout;
    loop {
        if let Some(value) = check() {
            return Some(value);
        }
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn wait_until<T>(what: &str, timeout: Duration, check: impl FnMut() -> Option<T>) -> T {
    poll(timeout, check).unwrap_or_else(|| panic!("timed out waiting for {what}"))
}

/// A daemon in its own process group, so ending the group ends its `rex do`, as its job would.
pub struct Daemon {
    child: Option<Child>,
    pub log: PathBuf,
}

impl Daemon {
    pub fn start(server: &Server, state: &Path) -> Self {
        Self::start_with(server, state, &[("RUST_LOG", "info")])
    }

    pub fn start_with(server: &Server, state: &Path, vars: &[(&str, &str)]) -> Self {
        use std::os::unix::process::CommandExt;
        let log = state.with_extension("log");
        let child = Command::new(REXUME)
            .arg("daemon")
            .arg("--state-dir")
            .arg(state)
            .arg("--rex")
            .arg(rex_bin())
            .args(["--server", &server.endpoint()])
            .env_clear()
            .env("HOME", server.dir.join("home"))
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .envs(vars.iter().copied())
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log)
                    .unwrap(),
            )
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            log,
        }
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("a running daemon").id()
    }

    /// The daemon's exit status once it has exited on its own.
    pub fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut()?.try_wait().unwrap()
    }

    /// The `rex do` the daemon started last.
    pub fn adapter(&self) -> Option<u32> {
        self.log().lines().rev().find_map(|line| {
            line.split("started rex do as pid ")
                .nth(1)?
                .trim()
                .parse()
                .ok()
        })
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", child.id())])
                // A daemon that already exited has no group left.
                .stderr(Stdio::null())
                .status();
            let _ = child.wait();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
        if std::thread::panicking() {
            eprintln!("daemon log:\n{}", self.log());
        }
    }
}

pub fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The state database, opened as the product opens it, so a test's own writes are checked by the
/// same foreign keys.
pub fn db(state: &Path) -> Connection {
    let conn = Connection::open(state.join("state.db")).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn
}

/// One value from the state database, or `None` while the database or the row does not exist yet.
/// Any other error is a broken query and fails the test.
pub fn query<T: rusqlite::types::FromSql>(state: &Path, sql: &str) -> Option<T> {
    use rusqlite::OptionalExtension;
    if !state.join("state.db").exists() {
        return None;
    }
    let conn = db(state);
    // The daemon creates the file before it applies the schema.
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    if version == 0 {
        return None;
    }
    conn.query_row(sql, [], |row| row.get(0))
        .optional()
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Each pane's shell pid, once the shell is its foreground program and the daemon recorded it. A
/// login shell runs its profile's commands in the foreground first.
pub fn shell_pids(server: &Server, state: &Path, session: &str, blocks: &[&str]) -> Vec<i64> {
    wait_until("the shells to be recorded", Duration::from_secs(20), || {
        blocks
            .iter()
            .map(|block| {
                let process = server
                    .try_call(
                        "block.method",
                        &json!({ "session_id": session, "block_id": block, "method": "process" }),
                    )
                    .ok()?;
                let foreground = &process["foreground"];
                let pid = foreground["pid"]
                    .as_i64()
                    .filter(|_| foreground["name"] == SHELL)?;
                query::<i64>(
                    state,
                    &format!(
                        "SELECT fg_pid FROM block WHERE lineage = '{block}' AND fg_pid IS NOT NULL"
                    ),
                )
                .filter(|recorded| *recorded == pid)
            })
            .collect()
    })
}

/// A terminal block for a create request.
pub fn shell_block(label: &str, cwd: &Path) -> Value {
    json!({ "block": { "flavor": "com.superlogical.terminal.shell", "label": label,
                       "options": { "cwd": cwd } } })
}
