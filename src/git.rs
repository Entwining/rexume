//! Running Git for snapshots: the working tree a directory is in, and what `git add -A` would stage
//! there, written as a tree into an object directory of Rexume's own, leaving the repository as it
//! was (`docs/design.md`, "Snapshots"). Git runs only here, outside any transaction: a capture is
//! taken in steps, so that the task layer can reserve space for it between them.

mod command;
mod filter;
mod index;
mod run;
mod scratch;

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use command::Isolation;
use filter::Filters;
use run::Git;
pub use run::{Blocked, Stopped};
use scratch::Scratch;
pub use scratch::{Left, captured, claim, left, object_directory};

/// How long one snapshot may take, all its Git commands together.
pub const BUDGET: Duration = Duration::from_secs(5);
/// The most a snapshot copies: the size of the changed and untracked files together before it
/// takes them, and the size of the objects it wrote after.
pub const CONTENT_LIMIT: u64 = 64 << 20;
/// The most changed and untracked paths a snapshot takes, which bounds what reading the listing of
/// them holds, and the work of every step after it.
pub const PATH_LIMIT: usize = 10_000;

/// The top level of the Git working tree `dir` is in.
pub fn worktree(dir: &Path) -> Result<PathBuf> {
    let mut git = Git::new(BUDGET)?;
    let args = ["rev-parse", "--show-toplevel"];
    let top = git
        .run(Git::git(dir, &[], None), &args, Stdio::null(), false)?
        .output()?;
    Ok(PathBuf::from(text(&top)?.trim_end_matches('\n')))
}

/// A snapshot being taken of what `git add -A` would stage in a working tree: a tree whose objects
/// the repository does not hold go to the task's object directory. A blob or tree the repository's
/// index already names is not copied: the tree names it without holding it, which is why
/// `write-tree` gets `--missing-ok`. Dropped before `keep`, it deletes everything it wrote.
pub struct Capture {
    git: Git,
    scratch: Scratch,
    isolation: Isolation,
    /// The working tree, Git directory, and common directory, which a filter may not write to.
    repository: [PathBuf; 3],
    filters: Filters,
    gitlinks: BTreeMap<Vec<u8>, String>,
    files: Vec<Vec<u8>>,
    estimate: u64,
}

impl Capture {
    /// Lists the changed and untracked files of `worktree`, the top level of a working tree, for a
    /// capture into `task_dir`, the task's snapshot directory, within `budget`. Writes nothing
    /// there but the capture's lock file.
    pub fn list(worktree: &Path, task_dir: &Path, budget: Duration) -> Result<Self> {
        let mut git = Git::new(budget)?;
        let args = [
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--absolute-git-dir",
            "--git-common-dir",
            "--git-path",
            "index",
        ];
        let found = git
            .run(Git::git(worktree, &[], None), &args, Stdio::null(), false)?
            .output()?;
        let found = text(&found)?;
        let [top, git_dir, common_dir, index] = found.lines().collect::<Vec<_>>()[..] else {
            bail!("`git rev-parse` printed {found:?}");
        };
        if Path::new(top) != worktree {
            bail!(
                "`{}` is no longer the top level of a working tree",
                worktree.display()
            );
        }
        refuse_split_index(Path::new(git_dir))?;
        let filters = Filters::configured(&mut git, worktree)?;
        let scratch = Scratch::create(task_dir)?;
        index::copy(Path::new(index), &scratch.index())?;
        let isolation = scratch.reading();
        let prepared = index::prepared(&mut git, worktree, &isolation)?;
        prepared.seed(&mut git, worktree, &scratch.seed())?;
        let (files, estimate) = changed_files(&mut git, worktree, &isolation, &filters.listing())?;
        let paths: HashSet<&[u8]> = files.iter().map(Vec::as_slice).collect();
        let left = CONTENT_LIMIT.saturating_sub(scratch.seeded()?);
        index::seed_blobs(
            &mut git,
            worktree,
            &isolation,
            &paths,
            &scratch.seed(),
            left,
        )?;
        Ok(Self {
            git,
            scratch,
            isolation,
            repository: [top, git_dir, common_dir].map(PathBuf::from),
            filters,
            gitlinks: prepared.gitlinks,
            files,
            estimate,
        })
    }

    /// The capture's ID, which the space reserved for it is recorded under.
    pub fn id(&self) -> &str {
        &self.scratch.id
    }

    /// The size of the changed and untracked files, which the capture reserves. What it writes can
    /// take more: the trees, Git's overhead on small or incompressible files, and a file that grew.
    pub fn estimate(&self) -> u64 {
        self.estimate
    }

    /// Writes the tree and returns it with the size of the objects the capture wrote, once space is
    /// reserved for them. Refuses a snapshot whose objects take more than `CONTENT_LIMIT`, as when
    /// a file grew after the listing.
    pub fn stage(&mut self) -> Result<(String, u64)> {
        self.isolation = self.scratch.staging()?;
        let Self {
            git,
            scratch,
            isolation,
            repository,
            ..
        } = self;
        let repository = repository.each_ref().map(PathBuf::as_path);
        let worktree = repository[0];
        index::mask_gitlinks(git, worktree, isolation, &self.gitlinks)?;
        let needed = self.filters.needed(git, worktree, isolation, &self.files)?;
        let storage = if self.filters.runs_lfs(&needed)? {
            Some(scratch.lfs()?)
        } else {
            None
        };
        self.filters
            .add(git, isolation, &needed, storage, repository)?;
        index::unmask_gitlinks(git, worktree, isolation, &self.gitlinks)?;
        let command = Git::git(worktree, &[], Some(isolation));
        let args = ["write-tree", "--missing-ok"];
        let tree = git.run(command, &args, Stdio::null(), false)?.output()?;
        let tree = text(&tree)?.trim_end().to_owned();
        let bytes = scratch.staged()?;
        if bytes > CONTENT_LIMIT {
            bail!(
                "the snapshot's objects take {} MiB, over the {} MiB a snapshot copies",
                bytes >> 20,
                CONTENT_LIMIT >> 20
            );
        }
        Ok((tree, bytes))
    }

    /// Moves the objects the capture wrote to the task's object directory, once their space is
    /// recorded as kept; a failed move leaves them for whoever next takes the capture's lock.
    pub fn keep(self) -> Result<()> {
        self.scratch.keep()
    }

    /// The capture as one left, for a capture that failed once it reserved space: its processes
    /// have ended, and whoever finishes it by its row deletes what it wrote.
    pub fn into_left(self) -> Result<Left> {
        self.scratch.into_left()
    }
}

/// Reading a split index refreshes its shared file in the repository, from whichever copy of the
/// index Git reads, so a repository with one gets no snapshot. A shared file left after the
/// repository stopped splitting its index counts too.
fn refuse_split_index(git_dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(git_dir)? {
        if entry?
            .file_name()
            .to_string_lossy()
            .starts_with("sharedindex.")
        {
            bail!("the repository has a split index");
        }
    }
    Ok(())
}

/// The changed and untracked regular files, which are what `git add` reads, and their size: a
/// deleted file is listed as modified and is read by nobody. Refuses a snapshot of more than
/// `PATH_LIMIT` paths or that would copy more than `CONTENT_LIMIT`, as soon as the listing shows
/// it.
fn changed_files(
    git: &mut Git,
    worktree: &Path,
    isolation: &Isolation,
    config: &[std::ffi::OsString],
) -> Result<(Vec<Vec<u8>>, u64)> {
    let (mut files, mut total, mut listed) = (Vec::new(), 0, 0);
    let command = Git::git(worktree, config, Some(isolation));
    let args = [
        "ls-files",
        "-z",
        "--deduplicate",
        "--others",
        "--modified",
        "--exclude-standard",
    ];
    git.stream(command, &args, &mut |path| {
        listed += 1;
        if listed > PATH_LIMIT {
            bail!(
                "more than {PATH_LIMIT} files are changed or untracked, the most a snapshot takes"
            );
        }
        if let Ok(meta) = std::fs::symlink_metadata(worktree.join(OsStr::from_bytes(path)))
            && meta.is_file()
        {
            total += meta.len();
            files.push(path.to_vec());
        }
        if total > CONTENT_LIMIT {
            bail!(
                "the changed and untracked files total more than the {} MiB a snapshot copies",
                CONTENT_LIMIT >> 20
            );
        }
        Ok(())
    })?
    .check()?;
    Ok((files, total))
}

fn text(output: &[u8]) -> Result<&str> {
    std::str::from_utf8(output).context("`git` printed text that is not UTF-8")
}

#[cfg(test)]
pub(crate) mod tests;
