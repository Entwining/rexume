//! The clean and process filters a snapshot's `git add` may run, each only for a changed file whose
//! attributes name it. Git LFS's runs as Git LFS installs it, with its storage in a directory
//! deleted with the snapshot; any other filter, Git LFS's in another form included, runs only
//! denied writes to the repository, since it is a program Rexume knows nothing of. Every filter is
//! required, because Git stores a file unfiltered when a filter that is not required fails, and
//! Rexume keeps no file a filter has not cleaned.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Seek, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

use super::command::Isolation;
use super::run::Git;

pub(super) struct Filters {
    /// Each driver with a clean or process command, and whether it is Git LFS's.
    drivers: BTreeMap<String, bool>,
    /// Whether the repository's own configuration sets `lfs.repositoryformatversion` to 0, the
    /// only value with which Git LFS runs there without writing it: Git LFS writes it when its
    /// last value there is missing or empty, and fails on any other.
    lfs_set_up: bool,
}

/// Each filter the changed files' attributes name, with the number of files that name it.
pub(super) type Needed = BTreeMap<String, usize>;

impl Filters {
    /// The filters configured for the repository at `worktree`. Git LFS's is a driver named `lfs`
    /// whose every command is one Git LFS installs, while no Git LFS extension, which runs a
    /// program of its own inside Git LFS's, is configured.
    pub fn configured(git: &mut Git, worktree: &Path) -> Result<Self> {
        let pattern = r"^(filter\..*\.(clean|process)|lfs\.extension\..*\.clean|lfs\.repositoryformatversion)$";
        let args = ["config", "-z", "--show-scope", "--get-regexp", pattern];
        let ran = git.run(Git::git(worktree, &[], None), &args, Stdio::null(), false)?;
        let mut filters = Self {
            drivers: BTreeMap::new(),
            lfs_set_up: false,
        };
        // `git config` exits 1 when no key matches.
        if ran.status.code() == Some(1) {
            return Ok(filters);
        }
        let found = ran.output()?;
        let mut extension = false;
        // Each entry is its scope, then the key and its value on two lines, each ended by NUL.
        for [scope, entry] in found.split(|&b| b == 0).collect::<Vec<_>>().as_chunks().0 {
            let entry = String::from_utf8_lossy(entry);
            let (key, value) = entry.split_once('\n').unwrap_or((&entry, ""));
            if key == "lfs.repositoryformatversion" {
                if *scope == b"local" {
                    filters.lfs_set_up = value == "0";
                }
            } else if key.starts_with("lfs.extension.") {
                extension |= !value.is_empty();
            } else if let Some((name, kind)) = key
                .strip_prefix("filter.")
                .and_then(|rest| rest.rsplit_once('.'))
                && !value.is_empty()
            {
                let lfs = name == "lfs" && lfs_command(kind, value);
                *filters.drivers.entry(name.to_owned()).or_insert(true) &= lfs;
            }
        }
        if extension && let Some(lfs) = filters.drivers.get_mut("lfs") {
            *lfs = false;
        }
        Ok(filters)
    }

    /// Settings for listing the changed files: every filter off, and not required, since a
    /// required filter that does not run fails the listing.
    pub fn listing(&self) -> Vec<OsString> {
        let mut config = Vec::new();
        for name in self.drivers.keys() {
            config.push(setting(name, "clean="));
            config.push(setting(name, "process="));
            config.push(setting(name, "required=false"));
        }
        config
    }

    /// The filters the attributes of `files` name.
    pub fn needed(
        &self,
        git: &mut Git,
        worktree: &Path,
        isolation: &Isolation,
        files: &[Vec<u8>],
    ) -> Result<Needed> {
        let mut needed = Needed::new();
        if files.is_empty() || self.drivers.is_empty() {
            return Ok(needed);
        }
        let mut input = tempfile::tempfile()?;
        for path in files {
            input.write_all(path)?;
            input.write_all(b"\0")?;
        }
        input.rewind()?;
        let command = Git::git(worktree, &[], Some(isolation));
        let args = ["check-attr", "-z", "--stdin", "filter"];
        let found = git
            .run(command, &args, Stdio::from(input), false)?
            .output()?;
        // Each path is printed with the attribute and its value, each ended by NUL.
        for [_, _, value] in found.split(|&b| b == 0).collect::<Vec<_>>().as_chunks().0 {
            let value = String::from_utf8_lossy(value);
            if self.drivers.contains_key(value.as_ref()) {
                *needed.entry(value.into_owned()).or_default() += 1;
            }
        }
        Ok(needed)
    }

    /// Whether Git LFS's filter runs for `needed`. Refuses when Git LFS would write the
    /// repository's configuration or fail.
    pub fn runs_lfs(&self, needed: &Needed) -> Result<bool> {
        let runs = needed.keys().any(|name| self.drivers[name]);
        if runs && !self.lfs_set_up {
            bail!(
                "{}, which writes `lfs.repositoryformatversion` to the repository's configuration, \
                 or fails, unless that sets it to 0",
                needs(needed.iter())
            );
        }
        Ok(runs)
    }

    /// Runs `git add -A` with every filter required and `storage`, Git LFS's when it runs. A
    /// filter that no file needs is replaced by a command that fails, so that it cannot run should
    /// a file change meanwhile; when a file needs one other than Git LFS's, `git add` runs denied
    /// writes to `repository`, the working tree and the Git and common directories.
    pub fn add(
        &self,
        git: &mut Git,
        isolation: &Isolation,
        needed: &Needed,
        storage: Option<OsString>,
        repository: [&Path; 3],
    ) -> Result<()> {
        let mut config = Vec::new();
        for name in self.drivers.keys() {
            config.push(setting(name, "required=true"));
            if !needed.contains_key(name) {
                config.push(setting(name, "clean=false"));
                config.push(setting(name, "process="));
            }
        }
        config.extend(storage);
        let untrusted: Vec<_> = needed
            .iter()
            .filter(|(name, _)| !self.drivers[*name])
            .collect();
        let filtered = !needed.is_empty();
        let ran = if untrusted.is_empty() {
            let command = Git::git(repository[0], &config, Some(isolation));
            git.run(command, &["add", "-A"], Stdio::null(), filtered)?
        } else {
            let needs = needs(untrusted.into_iter());
            let command = add_denied(&config, isolation, &needs, repository)?;
            let ran = git.run(command, &["add", "-A"], Stdio::null(), filtered)?;
            if ran.status.code() == Some(71) {
                bail!(
                    "{needs}, which Rexume runs only denied writes to the repository, and a \
                     process started in a sandbox cannot deny them"
                );
            }
            ran
        };
        if ran.status.success() || !filtered {
            return ran.check();
        }
        bail!(
            "{}, and `git add` exited {}; its message is not kept, since a filter can print a \
             file's content in it",
            needs(needed.iter()),
            ran.status
        )
    }
}

/// Whether `command`, a driver's `kind` of command, is one Git LFS installs: `git-lfs`, or an
/// absolute path to a program named so, and the arguments Git LFS writes, each word separated by
/// one space. Git runs the command through the shell, so anything else, a character the shell
/// would expand included, makes it another program.
fn lfs_command(kind: &str, command: &str) -> bool {
    let words: Vec<&str> = command.split(' ').collect();
    let Some((&program, args)) = words.split_first() else {
        return false;
    };
    let plain = program
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._+-".contains(&b));
    let named = program == "git-lfs"
        || (program.starts_with('/')
            && plain
            && Path::new(program).file_name() == Some(OsStr::new("git-lfs")));
    let forms: &[&[&str]] = match kind {
        "clean" => &[&["clean", "--", "%f"]],
        "process" => &[&["filter-process"], &["filter-process", "--skip"]],
        _ => &[],
    };
    named && forms.contains(&args)
}

fn setting(name: &str, rest: &str) -> OsString {
    format!("filter.{name}.{rest}").into()
}

/// Says which filters the changed files need, as a snapshot's note begins.
fn needs<'a>(needed: impl Iterator<Item = (&'a String, &'a usize)>) -> String {
    let (names, files): (Vec<String>, Vec<usize>) = needed
        .map(|(name, &files)| (format!("`{name}`"), files))
        .unzip();
    let files: usize = files.iter().sum();
    let files = if files == 1 {
        "1 changed file needs".to_owned()
    } else {
        format!("{files} changed files need")
    };
    let filters = if names.len() == 1 {
        "filter"
    } else {
        "filters"
    };
    format!("{files} the {filters} {}", names.join(", "))
}

/// Seatbelt's profile for `git add`: anything but writing under the paths it is given.
#[cfg(target_os = "macos")]
const DENY_WRITES: &str = r#"(version 1)
(allow default)
(deny file-write* (subpath (param "WORKTREE")) (subpath (param "GIT_DIR")) (subpath (param "COMMON_DIR")))"#;

/// `git add -A` under `sandbox-exec`, which Seatbelt rules by physical path. A process that is
/// sandboxed already cannot sandbox its children further, and `sandbox-exec` then exits 71 without
/// running Git.
#[cfg(target_os = "macos")]
fn add_denied(
    config: &[OsString],
    isolation: &Isolation,
    _needs: &str,
    repository: [&Path; 3],
) -> Result<Command> {
    let mut sandbox = Command::new("/usr/bin/sandbox-exec");
    sandbox.args(["-p", DENY_WRITES]);
    for (key, path) in ["WORKTREE", "GIT_DIR", "COMMON_DIR"]
        .into_iter()
        .zip(repository)
    {
        let mut define = OsString::from(format!("{key}="));
        define.push(std::fs::canonicalize(path)?);
        sandbox.arg("-D").arg(define);
    }
    sandbox.arg("git");
    Ok(Git::command(
        sandbox,
        repository[0],
        config,
        Some(isolation),
    ))
}

#[cfg(not(target_os = "macos"))]
fn add_denied(
    _config: &[OsString],
    _isolation: &Isolation,
    needs: &str,
    _repository: [&Path; 3],
) -> Result<Command> {
    bail!(
        "{needs}, which Rexume runs only denied writes to the repository, and on Linux it cannot \
         deny them"
    )
}

#[cfg(test)]
mod tests;
