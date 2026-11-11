use std::io::{ErrorKind, Write as _};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::Outcome;

pub(super) fn remove_file(path: &Path, ours: impl Fn(&str) -> bool) -> Result<Outcome> {
    match std::fs::read_to_string(path) {
        Ok(text) if ours(&text) => {
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove `{}`", path.display()))?;
            Ok(("removed", None))
        }
        Ok(_) => Ok(("kept", Some("not written by install".to_owned()))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(("absent", None)),
        Err(e) => Err(e).with_context(|| format!("failed to read `{}`", path.display())),
    }
}

pub(super) fn refresh_file(
    path: &Path,
    bytes: &[u8],
    ours: impl Fn(&str) -> bool,
) -> Result<&'static str> {
    match std::fs::read_to_string(path) {
        Ok(text) if ours(&text) => write(path, bytes),
        Ok(_) => Ok("kept"),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok("absent"),
        Err(e) => Err(e).with_context(|| format!("failed to read `{}`", path.display())),
    }
}

pub(super) fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("failed to parse `{}`", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("failed to read `{}`", path.display())),
    }
}

pub(super) fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write(path, text.as_bytes())?;
    Ok(())
}

/// Also resolves a link whose target does not exist yet, which `canonicalize` refuses, so that
/// writing through the link creates its target instead of replacing the link.
pub(super) fn physical(path: &Path) -> Result<PathBuf> {
    for ancestor in path.ancestors() {
        let rest = path.strip_prefix(ancestor)?;
        match std::fs::canonicalize(ancestor) {
            Ok(mut real) => {
                // Past the last directory that exists, `..` names nothing yet, but would name a
                // file this never read once `write` created the directories before it.
                if rest.components().any(|c| c == Component::ParentDir) {
                    bail!(
                        "failed to resolve `{}`: `..` follows a directory that does not exist",
                        path.display()
                    );
                }
                // `extend` adds no trailing separator for an empty `rest`, unlike `join`.
                real.extend(rest);
                return Ok(real);
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("failed to resolve `{}`", path.display()));
            }
        }
        if ancestor.is_symlink() {
            let mut target = ancestor.to_path_buf();
            target
                .set_file_name(std::fs::read_link(ancestor).with_context(|| {
                    format!("failed to read the link `{}`", ancestor.display())
                })?);
            target.extend(rest);
            return physical(&target);
        }
    }
    Ok(path.to_path_buf())
}

pub(super) fn write(path: &Path, bytes: &[u8]) -> Result<&'static str> {
    let path = physical(path)?;
    let old = match std::fs::read(&path) {
        Ok(old) => Some(old),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("failed to read `{}`", path.display())),
    };
    if old.as_deref() == Some(bytes) {
        return Ok("unchanged");
    }
    let dir = path
        .parent()
        .with_context(|| format!("`{}` has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create `{}`", dir.display()))?;
    let mut partial = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("failed to write in `{}`", dir.display()))?;
    partial.write_all(bytes)?;
    if let Ok(meta) = std::fs::metadata(&path) {
        partial.as_file().set_permissions(meta.permissions())?;
    }
    partial
        .persist(&path)
        .with_context(|| format!("failed to write `{}`", path.display()))?;
    Ok(if old.is_some() { "updated" } else { "created" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_link_is_followed_from_its_real_directory() -> Result<()> {
        let home = tempfile::tempdir()?;
        let home = home.path();
        std::fs::create_dir_all(home.join("dotfiles/claude"))?;
        std::fs::create_dir_all(home.join("dotfiles/shared"))?;
        std::os::unix::fs::symlink("dotfiles/claude", home.join(".claude"))?;
        std::os::unix::fs::symlink(
            "../shared/settings.json",
            home.join("dotfiles/claude/settings.json"),
        )?;
        let path = home.join(".claude/settings.json");
        assert_eq!(write(&path, b"{}\n")?, "created");
        assert_eq!(
            std::fs::read(home.join("dotfiles/shared/settings.json"))?,
            b"{}\n"
        );
        assert!(path.is_symlink() && !home.join("shared").exists());
        Ok(())
    }

    #[test]
    fn a_link_back_out_of_a_missing_directory_is_refused() -> Result<()> {
        let home = tempfile::tempdir()?;
        let home = home.path();
        std::fs::create_dir_all(home.join("dotfiles"))?;
        std::fs::write(home.join("dotfiles/settings.json"), "the user's\n")?;
        std::fs::create_dir_all(home.join(".claude"))?;
        std::os::unix::fs::symlink(
            "../dotfiles/missing/../settings.json",
            home.join(".claude/settings.json"),
        )?;
        assert!(write(&home.join(".claude/settings.json"), b"{}\n").is_err());
        assert_eq!(
            std::fs::read_to_string(home.join("dotfiles/settings.json"))?,
            "the user's\n"
        );
        assert!(!home.join("dotfiles/missing").exists());
        Ok(())
    }
}
