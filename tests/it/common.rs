//! An isolated Rex server and daemon for scenario tests. Each test gets its own directory with
//! the server's socket, state, and home, so nothing reaches the user's own server.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};

pub const REXUME: &str = env!("CARGO_BIN_EXE_rexume");

/// The shell the test server's panes run, in `/bin`.
#[cfg(target_os = "macos")]
pub const SHELL: &str = "zsh";
#[cfg(target_os = "linux")]
pub const SHELL: &str = "bash";

/// The Rex CLI: `REXUME_TEST_REX`, or where the Rex app installs it.
#[cfg(target_os = "macos")]
pub fn rex_bin() -> PathBuf {
    std::env::var_os("REXUME_TEST_REX").map_or_else(
        || {
            let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            home.join("Library/Application Support/com.superlogical.rex/RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex")
        },
        PathBuf::from,
    )
}

/// The Rex CLI: `REXUME_TEST_REX`, or the first `rex` on PATH.
#[cfg(target_os = "linux")]
pub fn rex_bin() -> PathBuf {
    std::env::var_os("REXUME_TEST_REX").map_or_else(
        || which::which("rex").expect("no `rex` on PATH"),
        PathBuf::from,
    )
}

/// A daemon started without the harness, ended when an assertion fails before it exits.
pub struct Running(pub Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Polls until `check` returns a value; test code may wait, the product never polls.
pub fn wait_until<T>(what: &str, timeout: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + timeout;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub struct Server {
    pub dir: PathBuf,
    child: Option<Child>,
}

impl Server {
    /// Starts a server whose panes run with `path` first on PATH.
    pub fn start(dir: &Path, path: Option<&Path>) -> Self {
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(dir.join("rex-state")).unwrap();
        let config = dir.join("init.lua");
        if !config.exists() {
            std::fs::write(&config, "").unwrap();
        }
        let system = "/usr/bin:/bin:/usr/sbin:/sbin";
        if let Some(first) = path {
            // A Linux login shell sets PATH again from `/etc/profile`, so the panes get the test's
            // directory from `~/.profile`.
            std::fs::write(
                home.join(".profile"),
                format!("PATH='{}':\"$PATH\"\n", first.display()),
            )
            .unwrap();
        }
        let path = path.map_or(system.to_owned(), |p| format!("{}:{system}", p.display()));
        let endpoint = format!("unix://{}", dir.join("s.sock").display());
        let child = Command::new(rex_bin())
            .args(["--autostart=false", "server", "run", "--listen", &endpoint])
            .arg("--state-dir")
            .arg(dir.join("rex-state"))
            .arg("--config")
            .arg(&config)
            .env_clear()
            .env("HOME", &home)
            .env("USER", std::env::var("USER").unwrap_or_default())
            .env("SHELL", format!("/bin/{SHELL}"))
            .env("LANG", "en_US.UTF-8")
            .env("PATH", path)
            .env("TMPDIR", std::env::temp_dir())
            .env("REX_SERVER", &endpoint)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.join("server.log")).unwrap())
            .spawn()
            .unwrap();
        let server = Self {
            dir: dir.to_path_buf(),
            child: Some(child),
        };
        wait_until("the server", Duration::from_secs(10), || {
            server.try_call("server.status", &json!({})).ok()
        });
        server
    }

    pub fn endpoint(&self) -> String {
        format!("unix://{}", self.dir.join("s.sock").display())
    }

    pub fn cli(&self) -> Command {
        let mut command = Command::new(rex_bin());
        command.args(["--autostart=false", "-S", &self.endpoint()]);
        command
    }

    pub fn try_call(&self, method: &str, params: &Value) -> Result<Value, String> {
        let output = self
            .cli()
            .args(["api", "call", method, &params.to_string()])
            .output()
            .unwrap();
        if output.status.success() {
            Ok(serde_json::from_slice(&output.stdout).unwrap())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    pub fn call(&self, method: &str, params: &Value) -> Value {
        self.try_call(method, params)
            .unwrap_or_else(|e| panic!("{method}: {e}"))
    }

    pub fn instance(&self) -> String {
        self.call("server.status", &json!({}))["instance_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Ends the server as a crash would.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    /// Stops the server the way the app does.
    pub fn stop(&mut self) {
        let _ = self.try_call("server.stop", &json!({}));
        if let Some(mut child) = self.child.take() {
            child.wait().unwrap();
        }
    }

    pub fn sessions(&self) -> Vec<String> {
        self.call("session.list", &json!({}))["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_id"].as_str().unwrap().to_owned())
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
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
