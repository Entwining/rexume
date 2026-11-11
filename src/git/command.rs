//! How each of a snapshot's Git commands is set up: the environment it keeps, the settings that
//! keep Git from writing in the repository or running the user's programs, and where a command
//! that reads the index finds it and its objects.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::run::Git;

/// Removes every `GIT_` variable but those that choose Git's configuration: one can point Git at
/// another repository, index, or object directory, or name a file Git or Git LFS writes, such as a
/// trace or Git LFS's progress. Configuration from the environment stays, since a filter configured
/// only there would otherwise not run, and Git would store its file unfiltered.
pub(super) fn scrub(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let bytes = name.as_encoded_bytes();
        let kept = matches!(
            bytes,
            b"GIT_CONFIG_GLOBAL"
                | b"GIT_CONFIG_SYSTEM"
                | b"GIT_CONFIG_NOSYSTEM"
                | b"GIT_CONFIG_COUNT"
                | b"GIT_CONFIG_PARAMETERS"
                | b"GIT_ATTR_NOSYSTEM"
                | b"GIT_EXEC_PATH"
        ) || bytes.starts_with(b"GIT_CONFIG_KEY_")
            || bytes.starts_with(b"GIT_CONFIG_VALUE_");
        if bytes.starts_with(b"GIT_") && !kept {
            command.env_remove(name);
        }
    }
}

/// Where an isolated command finds its index and objects: a copy of the repository's index, and
/// an object directory of Rexume's with Rexume's other directories as its alternates, never the
/// repository's own, since Git refreshes the modification time of an object it finds there.
pub(super) struct Isolation {
    pub index: PathBuf,
    pub objects: PathBuf,
    pub alternates: OsString,
}

impl Git {
    /// `program`, which is `git` or a program that runs the `git` its arguments end with, set to
    /// run in `dir` with Rexume's settings, `config`, and `isolation`.
    pub fn command(
        program: Command,
        dir: &Path,
        config: &[OsString],
        isolation: Option<&Isolation>,
    ) -> Command {
        let mut command = program;
        scrub(&mut command);
        // Optional locks off keeps Git from refreshing an index it only reads; a fetch of a partial
        // clone's missing objects would write to the repository. Git's messages stay untranslated,
        // since a note quotes them and `Ran::check` reads one; `LANGUAGE` changes only messages.
        command.envs([
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("LANGUAGE", "en"),
        ]);
        // Hooks, such as `post-index-change`, and a file system monitor run the user's programs or
        // write in the repository's Git directory, and a split index written from the copy would
        // put its shared file there.
        command.arg("-C").arg(dir).args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.splitIndex=false",
        ]);
        if let Some(isolation) = isolation {
            // A sparse index written back would need the trees of its sparse directories.
            command.args([
                "-c",
                "index.sparse=false",
                "-c",
                "advice.sparseIndexExpanded=false",
            ]);
            command
                .env("GIT_INDEX_FILE", &isolation.index)
                .env("GIT_OBJECT_DIRECTORY", &isolation.objects)
                .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &isolation.alternates);
        }
        for setting in config {
            command.arg("-c").arg(setting);
        }
        command
    }

    pub fn git(dir: &Path, config: &[OsString], isolation: Option<&Isolation>) -> Command {
        Self::command(Command::new("git"), dir, config, isolation)
    }
}
