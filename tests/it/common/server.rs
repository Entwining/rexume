//! The isolated Rex server a scenario test starts.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use super::wait_until;

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

pub struct Server {
    pub dir: PathBuf,
    child: Option<Child>,
}

/// Whether the Rex CLI's server can restore its own sessions, which an older one cannot.
fn restores_itself() -> bool {
    static RESTORES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *RESTORES.get_or_init(|| {
        let help = Command::new(rex_bin())
            .args(["--autostart=false", "server", "run", "--help"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&help.stdout).contains("--restore")
    })
}

impl Server {
    /// Starts a server whose panes run with `path` first on PATH, which leaves restoring its
    /// sessions to Rexume.
    pub fn start(dir: &Path, path: Option<&Path>) -> Self {
        Self::launch(dir, path, false)
    }

    /// Starts a server that brings back the sessions it saved, with their IDs, as Rex's own
    /// restore does. Fails on a Rex CLI too old to have it.
    pub fn start_restoring(dir: &Path) -> Self {
        assert!(
            restores_itself(),
            "{} has no session restore of its own",
            rex_bin().display()
        );
        Self::launch(dir, None, true)
    }

    fn launch(dir: &Path, path: Option<&Path>, restoring: bool) -> Self {
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
            .args((!restoring && restores_itself()).then_some("--restore=false"))
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
