//! A fake of the service manager install drives. It records its calls in `calls.log` and keeps
//! the job in files under one directory instead of reaching the real manager: `job` is the
//! program it starts, which writes its pid to `loaded` and removes it on exit, and `fail-stop`
//! makes the next stop fail.

use std::path::Path;

/// launchd: `bootstrap` starts the job's process, `bootout` only sends it `SIGTERM`, the job stays
/// loaded until that process exits, and `bootstrap` fails while it is loaded.
#[cfg(target_os = "macos")]
mod platform {
    use std::path::{Path, PathBuf};

    pub const KEY: &str = "launch_agent";
    pub const NAME_KEY: &str = "launch_agent_label";
    pub const NAME: &str = "io.github.entwining.rexume";
    pub const TOOL: &str = "launchctl";
    pub const STARTS: &[&str] = &["bootstrap"];
    pub const STOPS: &[&str] = &["bootout"];

    pub fn definition(home: &Path) -> PathBuf {
        home.join(format!("Library/LaunchAgents/{NAME}.plist"))
    }

    pub fn quoted(word: &str) -> String {
        format!("<string>{word}</string>")
    }

    pub fn script(dir: &Path, _home: &Path) -> String {
        format!(
            r#"#!/bin/sh
cd '{dir}'
echo "$*" >> calls.log
case "$1" in
  print) test -e loaded && printf '\tpid = %s\n' "$(cat loaded)" ;;
  bootstrap)
    if test -e loaded; then echo 'Bootstrap failed: 5: Input/output error' >&2; exit 5; fi
    ./job loaded < /dev/null > /dev/null 2>&1 &
    until test -e loaded; do sleep 0.01; done ;;
  bootout)
    if test -e fail-stop; then rm fail-stop; echo 'Boot-out failed: 5: Input/output error' >&2; exit 5; fi
    kill -TERM "$(cat loaded)" ;;
esac
"#,
            dir = dir.display()
        )
    }
}

/// The systemd user manager: `read` is its copy of the unit, which `daemon-reload` refreshes,
/// `enabled` marks the unit enabled, `stop` returns once the job has exited, `restarting` shows the
/// service waiting to restart, `start-limit` keeps it failed and refusing starts until
/// `reset-failed`, and `no-manager` makes every call fail as on a host without a user manager.
#[cfg(target_os = "linux")]
mod platform {
    use std::path::{Path, PathBuf};

    pub const KEY: &str = "systemd_unit";
    pub const NAME_KEY: &str = "systemd_unit_name";
    pub const NAME: &str = "rexume.service";
    pub const TOOL: &str = "systemctl";
    pub const STARTS: &[&str] = &["enable"];
    pub const STOPS: &[&str] = &["stop", "disable"];

    pub fn definition(home: &Path) -> PathBuf {
        home.join(".config/systemd/user").join(NAME)
    }

    pub fn quoted(word: &str) -> String {
        format!("\"{word}\"")
    }

    pub fn script(dir: &Path, home: &Path) -> String {
        format!(
            r#"#!/bin/sh
cd '{dir}'
unit='{unit}'
test "$1" = --user || exit 2
shift
echo "$*" >> calls.log
if test -e no-manager; then echo 'Failed to connect to user scope bus' >&2; exit 1; fi
start() {{
  if test -e start-limit; then echo 'Job for rexume.service failed.' >&2; exit 1; fi
  test -e loaded && return
  ./job loaded < /dev/null > /dev/null 2>&1 &
  until test -e loaded; do sleep 0.01; done
}}
stop() {{
  if test -e fail-stop; then rm fail-stop; echo 'Job for rexume.service failed.' >&2; exit 1; fi
  rm -f restarting
  test -e loaded || return 0
  kill -TERM "$(cat loaded)"
  while test -e loaded; do sleep 0.01; done
}}
case "$1" in
  show)
    if test -e read; then echo LoadState=loaded; else echo LoadState=not-found; fi
    if test -e loaded; then echo ActiveState=active
    elif test -e restarting; then echo ActiveState=activating
    elif test -e start-limit; then echo ActiveState=failed
    else echo ActiveState=inactive; fi
    if test -e enabled; then echo UnitFileState=enabled; else echo UnitFileState=; fi
    if test -e read -o -e "$unit" && ! cmp -s read "$unit"
    then echo NeedDaemonReload=yes; else echo NeedDaemonReload=no; fi ;;
  daemon-reload) if test -e "$unit"; then cp "$unit" read; else rm -f read; fi ;;
  enable)
    test -e read || {{ echo 'Unit file rexume.service does not exist.' >&2; exit 1; }}
    touch enabled; start ;;
  disable) rm -f enabled; stop ;;
  stop) stop ;;
  reset-failed)
    test -e enabled -o -e start-limit || {{ echo 'Unit rexume.service not loaded.' >&2; exit 1; }}
    rm -f start-limit ;;
  *) exit 2 ;;
esac
"#,
            dir = dir.display(),
            unit = definition(home).display()
        )
    }
}

pub use platform::{KEY, NAME, NAME_KEY, STARTS, STOPS, TOOL, definition, quoted, script};

/// The calls whose first word is one of `verbs`.
pub fn calls(dir: &Path, verbs: &[&str]) -> usize {
    std::fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            line.split(' ')
                .next()
                .is_some_and(|verb| verbs.contains(&verb))
        })
        .count()
}
