//! The copy of the repository's index a capture works on, and what it copies from the repository
//! for Git to read. Isolated, Git cannot read the repository's objects, and some of what `git add
//! -A` reads is there: the blobs of skip-worktree `.gitattributes`, `.gitignore`, and
//! `.gitmodules` files, which hold rules, the trees of a sparse index's directories, which Git
//! reads to expand it, and the blobs the index names for changed files, from which Git tells
//! whether a file it converts from CRLF had CRLF already. Without them Git goes on without the
//! rules or the entries, or converts what it would keep, and records another tree. A submodule's
//! gitlink is masked during `git add`, which otherwise runs `git status` in a populated submodule
//! whose commit the gitlink names, with that submodule's own index, objects, and filters.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};

use super::command::Isolation;
use super::run::Git;
use super::{CONTENT_LIMIT, text};

/// Copies the repository's index to `copy` with its modification time, which Git compares with
/// each entry's to tell an entry written in the same instant as its file, so that the copy finds
/// the same files changed. A repository without an index has none to copy.
pub(super) fn copy(index: &Path, copy: &Path) -> Result<()> {
    let mut source = match File::open(index) {
        Ok(source) => source,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("failed to read `{}`", index.display())),
    };
    let modified = source.metadata()?.modified()?;
    let mut target = File::create(copy)?;
    std::io::copy(&mut source, &mut target)?;
    target.set_modified(modified)?;
    Ok(())
}

/// One entry of `git ls-files -s -v`: its tag, mode, object, stage, and path.
struct Entry<'a> {
    tag: &'a [u8],
    mode: &'a [u8],
    object: &'a str,
    stage: &'a [u8],
    path: &'a [u8],
}

fn entry(record: &[u8]) -> Result<Entry<'_>> {
    let tab = record.iter().position(|&b| b == b'\t');
    let parsed = tab.and_then(|tab| {
        let (head, path) = (&record[..tab], &record[tab + 1..]);
        let mut fields = head.split(|&b| b == b' ');
        Some(Entry {
            tag: fields.next()?,
            mode: fields.next()?,
            object: std::str::from_utf8(fields.next()?).ok()?,
            stage: fields.next()?,
            path,
        })
    });
    parsed.with_context(|| {
        format!(
            "`git ls-files` printed {:?}",
            String::from_utf8_lossy(record)
        )
    })
}

/// Lists the index entries, with a sparse index's directories as entries of their own, passing each
/// to `each`.
fn entries(
    git: &mut Git,
    worktree: &Path,
    isolation: &Isolation,
    config: &[&str],
    each: &mut dyn FnMut(Entry<'_>),
) -> Result<()> {
    let config: Vec<_> = config.iter().map(Into::into).collect();
    let command = Git::git(worktree, &config, Some(isolation));
    let args = ["ls-files", "-z", "-s", "-v", "--sparse"];
    git.stream(command, &args, &mut |record| {
        each(entry(record)?);
        Ok(())
    })?
    .check()
}

/// What the index holds that a capture prepares for.
#[derive(Default)]
pub(super) struct Prepared {
    /// Each gitlink at stage 0 that `git add -A` reads: its path and the commit it names. One
    /// marked assume-unchanged or skip-worktree, which `-v` tags in lowercase or `S`, Git leaves as
    /// it is, and so does the capture, since rewriting the entry would drop the mark.
    pub gitlinks: BTreeMap<Vec<u8>, String>,
    /// The trees of the sparse directories.
    sparse: Vec<String>,
    /// The blobs of the `.gitattributes` and `.gitmodules` files and of skip-worktree `.gitignore`
    /// files, which Git reads from the index when the working tree lacks the file.
    rules: Vec<String>,
}

/// Reads what the index holds that `git add -A` would need from the repository, without
/// expanding a sparse index, which would need the trees not copied yet: the index stays sparse
/// where the repository's settings allow one, and a sparse index they do not allow fails the
/// read, as Git cannot read its trees.
pub(super) fn prepared(git: &mut Git, worktree: &Path, isolation: &Isolation) -> Result<Prepared> {
    let mut prepared = Prepared::default();
    let config = [
        "index.sparse=true",
        "sparse.expectFilesOutsideOfPatterns=true",
    ];
    entries(git, worktree, isolation, &config, &mut |entry| {
        let name = entry.path.rsplit(|&b| b == b'/').next().unwrap_or_default();
        match (entry.tag, entry.mode) {
            (b"H", b"160000") if entry.stage == b"0" => {
                prepared
                    .gitlinks
                    .insert(entry.path.to_vec(), entry.object.to_owned());
            }
            (b"S" | b"s", b"040000") => prepared.sparse.push(entry.object.to_owned()),
            (tag, _)
                if name == b".gitattributes"
                    || entry.path == b".gitmodules"
                    || (matches!(tag, b"S" | b"s") && name == b".gitignore") =>
            {
                prepared.rules.push(entry.object.to_owned());
            }
            _ => {}
        }
    })?;
    Ok(prepared)
}

impl Prepared {
    /// Copies the rules and the sparse directories' trees, with every tree under them, from the
    /// repository to the capture's seed, before anything else.
    pub fn seed(&self, git: &mut Git, worktree: &Path, seed: &Path) -> Result<()> {
        let mut objects = self.rules.clone();
        objects.extend(head_gitmodules(git, worktree)?);
        if !self.sparse.is_empty() {
            let command = Git::git(worktree, &[], None);
            let args = [
                "rev-list",
                "--objects",
                "--no-object-names",
                "--filter=blob:none",
                "--stdin",
            ];
            let input = Stdio::from(list(&self.sparse)?);
            let trees = git.run(command, &args, input, false)?.output()?;
            objects.extend(text(&trees)?.lines().map(str::to_owned));
        }
        copy_objects(git, worktree, &objects, seed, CONTENT_LIMIT)
    }
}

/// The objects through which Git reads `HEAD:.gitmodules`, where it looks once neither the working
/// tree nor the index has a `.gitmodules`, as after `git add -A` staged its deletion: the commit
/// `HEAD` names, its tree, and the file. None when `HEAD` has no `.gitmodules` or no commit.
fn head_gitmodules(git: &mut Git, worktree: &Path) -> Result<Vec<String>> {
    let names = ["HEAD", "HEAD^{tree}", "HEAD:.gitmodules"].map(str::to_owned);
    let command = Git::git(worktree, &[], None);
    let args = ["cat-file", "--batch-check=%(objectname)"];
    let found = git
        .run(command, &args, Stdio::from(list(&names)?), false)?
        .output()?;
    // A name that resolves to no object is printed back with ` missing`.
    let objects: Vec<String> = text(&found)?.lines().map(str::to_owned).collect();
    if objects.iter().any(|object| object.contains(' ')) {
        return Ok(Vec::new());
    }
    Ok(objects)
}

/// Copies the blobs the index names for `files`, the changed files `git add` reads, from the
/// repository to the capture's seed, taking at most `limit` bytes.
pub(super) fn seed_blobs(
    git: &mut Git,
    worktree: &Path,
    isolation: &Isolation,
    files: &HashSet<&[u8]>,
    seed: &Path,
    limit: u64,
) -> Result<()> {
    let mut objects = Vec::new();
    entries(git, worktree, isolation, &[], &mut |entry| {
        if entry.mode != b"160000" && files.contains(entry.path) {
            objects.push(entry.object.to_owned());
        }
    })?;
    copy_objects(git, worktree, &objects, seed, limit)
}

/// Packs `objects` from the repository and unpacks them into `seed`, as a pack of at most `limit`
/// bytes, of which the seeds of a capture take `CONTENT_LIMIT` together. `pack-objects` writes to
/// its standard output, since with a name it would build the pack in the repository.
fn copy_objects(
    git: &mut Git,
    worktree: &Path,
    objects: &[String],
    seed: &Path,
    limit: u64,
) -> Result<()> {
    if objects.is_empty() {
        return Ok(());
    }
    let pack = Git::git(worktree, &[], None);
    let mut unpack = Git::git(worktree, &[], None);
    unpack.env("GIT_OBJECT_DIRECTORY", seed);
    // `index-pack` takes 0 for no limit.
    let limit = format!("--max-input-size={}", limit.max(1));
    git.pipe(
        (pack, &["pack-objects", "-q", "--stdout"]),
        Stdio::from(list(objects)?),
        (unpack, &["index-pack", "--stdin", &limit]),
    )
    .with_context(|| {
        format!(
            "failed to copy the {} objects of the repository Git reads for the snapshot, which may \
             total more than {} MiB",
            objects.len(),
            CONTENT_LIMIT >> 20
        )
    })
}

/// A file listing `objects` one per line, for a Git command to read.
fn list(objects: &[String]) -> Result<File> {
    let mut list = tempfile::tempfile()?;
    for object in objects {
        writeln!(list, "{object}")?;
    }
    std::io::Seek::rewind(&mut list)?;
    Ok(list)
}

/// The object a gitlink is masked with: none a commit can have, of the length of `object`'s.
fn mask(object: &str) -> String {
    format!("{}1", "0".repeat(object.len().saturating_sub(1)))
}

/// Points each of `gitlinks` at its mask, or back at its commit.
fn set(
    git: &mut Git,
    worktree: &Path,
    isolation: &Isolation,
    gitlinks: &BTreeMap<Vec<u8>, String>,
    masked: bool,
) -> Result<()> {
    if gitlinks.is_empty() {
        return Ok(());
    }
    let mut info = tempfile::tempfile()?;
    for (path, object) in gitlinks {
        let object = if masked { mask(object) } else { object.clone() };
        write!(info, "160000 {object}\t")?;
        info.write_all(path)?;
        info.write_all(b"\0")?;
    }
    std::io::Seek::rewind(&mut info)?;
    let command = Git::git(worktree, &[], Some(isolation));
    let args = ["update-index", "-z", "--index-info"];
    git.run(command, &args, Stdio::from(info), false)?.check()
}

/// Masks every gitlink, so that `git add -A` takes each populated submodule for one whose commit
/// differs and records its commit without asking whether its working tree is changed.
pub(super) fn mask_gitlinks(
    git: &mut Git,
    worktree: &Path,
    isolation: &Isolation,
    gitlinks: &BTreeMap<Vec<u8>, String>,
) -> Result<()> {
    set(git, worktree, isolation, gitlinks, true)
}

/// Points each gitlink `git add -A` left masked, a submodule it could not read such as an empty
/// directory, back at its commit.
pub(super) fn unmask_gitlinks(
    git: &mut Git,
    worktree: &Path,
    isolation: &Isolation,
    gitlinks: &BTreeMap<Vec<u8>, String>,
) -> Result<()> {
    if gitlinks.is_empty() {
        return Ok(());
    }
    let mut left = BTreeMap::new();
    entries(git, worktree, isolation, &[], &mut |entry| {
        if entry.mode == b"160000"
            && let Some(object) = gitlinks.get(entry.path)
            && entry.object == mask(object)
        {
            left.insert(entry.path.to_vec(), object.clone());
        }
    })?;
    set(git, worktree, isolation, &left, false)
}
