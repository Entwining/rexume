use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};

use super::command::Isolation;
use super::run::Git;
use super::{CONTENT_LIMIT, text};

pub(super) fn copy(index: &Path, copy: &Path) -> Result<()> {
    let mut source = match File::open(index) {
        Ok(source) => source,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("failed to read `{}`", index.display())),
    };
    let modified = source.metadata()?.modified()?;
    let mut target = File::create(copy)?;
    std::io::copy(&mut source, &mut target)?;
    // Git compares the index's modification time with each entry's to tell an entry written in
    // the same instant as its file.
    target.set_modified(modified)?;
    Ok(())
}

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

#[derive(Default)]
pub(super) struct Prepared {
    pub gitlinks: BTreeMap<Vec<u8>, String>,
    /// Git reads these trees to expand the sparse index.
    sparse: Vec<String>,
    /// Git reads these blobs from the index when the working tree lacks the file, and without one
    /// goes on without its rules.
    rules: Vec<String>,
}

pub(super) fn prepared(git: &mut Git, worktree: &Path, isolation: &Isolation) -> Result<Prepared> {
    let mut prepared = Prepared::default();
    // Keeps the index sparse where the repository's settings allow it, since expanding it needs
    // the trees not copied yet; Git expands one they do not allow, and fails to read its trees.
    let config = [
        "index.sparse=true",
        "sparse.expectFilesOutsideOfPatterns=true",
    ];
    entries(git, worktree, isolation, &config, &mut |entry| {
        let name = entry.path.rsplit(|&b| b == b'/').next().unwrap_or_default();
        match (entry.tag, entry.mode) {
            // `git add -A` leaves an assume-unchanged or skip-worktree gitlink as it is, and
            // rewriting the entry would drop the mark.
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

/// Git reads `HEAD:.gitmodules` once neither the working tree nor the index has a `.gitmodules`,
/// as after `git add -A` staged its deletion.
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

/// Git reads a changed file's staged blob to tell whether a file it converts from CRLF had CRLF
/// already, and without the blob converts what it would keep.
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
    // With a name instead of `--stdout`, `pack-objects` would build the pack in the repository.
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

fn list(objects: &[String]) -> Result<File> {
    let mut list = tempfile::tempfile()?;
    for object in objects {
        writeln!(list, "{object}")?;
    }
    std::io::Seek::rewind(&mut list)?;
    Ok(list)
}

fn mask(object: &str) -> String {
    format!("{}1", "0".repeat(object.len().saturating_sub(1)))
}

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

/// `git add -A` runs `git status`, with the submodule's own index, objects, and filters, in a
/// populated submodule whose commit the gitlink names, and records any other by its checked-out
/// commit.
pub(super) fn mask_gitlinks(
    git: &mut Git,
    worktree: &Path,
    isolation: &Isolation,
    gitlinks: &BTreeMap<Vec<u8>, String>,
) -> Result<()> {
    set(git, worktree, isolation, gitlinks, true)
}

/// `git add -A` leaves the mask on a submodule it could not read, such as an empty directory.
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
