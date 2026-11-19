use std::path::{Path, PathBuf};

use anyhow::Result;

use super::dirs::Dir;
use super::{Dirs, Outcome, refresh_file, remove_file, remove_file_and_dir, write};
use crate::runtime::{omp, opencode, pi};

/// Every version of each extension starts with this, whatever follows it on the first line.
const WRITTEN_BY: &str = "// Written by `rexume install`";

pub(super) struct Extension {
    name: &'static str,
    agent: fn(&Dirs) -> &Dir,
    text: &'static str,
    /// The extension's path in the agent's directory.
    file: &'static str,
    /// Whether the file's directory is install's own, which the runtime reads as one plugin.
    own_dir: bool,
}

pub(super) const PI_EXTENSION: Extension = Extension {
    name: "Pi extension",
    agent: |dirs| &dirs.pi,
    text: pi::EXTENSION,
    file: "extensions/rexume.ts",
    own_dir: false,
};
pub(super) const OMP_EXTENSION: Extension = Extension {
    name: "omp extension",
    agent: |dirs| &dirs.omp,
    text: omp::EXTENSION,
    file: "extensions/rexume.ts",
    own_dir: false,
};
pub(super) const OPENCODE_PLUGIN: Extension = Extension {
    name: "OpenCode plugin",
    agent: |dirs| &dirs.opencode,
    text: opencode::PLUGIN,
    file: "plugins/rexume/tui.ts",
    own_dir: true,
};

impl Extension {
    fn path(&self, agent: &Path) -> PathBuf {
        agent.join(self.file)
    }

    pub(super) fn install(&self, dirs: &Dirs) -> Result<Outcome> {
        let agent = match (self.agent)(dirs).usable() {
            Ok(agent) => agent,
            Err(note) => return Ok(("skipped", Some(note))),
        };
        if !agent.is_dir() {
            return Ok((
                "skipped",
                Some(format!("`{}` does not exist", dirs.show(agent))),
            ));
        }
        Ok((write(&self.path(agent), self.text.as_bytes())?, None))
    }

    /// Removes an extension an earlier install wrote, also from a directory install now leaves
    /// alone.
    pub(super) fn uninstall(&self, dirs: &Dirs) -> Result<Outcome> {
        let agent = match (self.agent)(dirs).known() {
            Ok(agent) => agent,
            Err(note) => return Ok(("skipped", Some(note))),
        };
        let path = self.path(agent);
        let ours = |text: &str| text.starts_with(WRITTEN_BY);
        if self.own_dir {
            remove_file_and_dir(&path, ours)
        } else {
            remove_file(&path, ours)
        }
    }

    fn refresh(&self, dirs: &Dirs) -> Result<&'static str> {
        let Ok(agent) = (self.agent)(dirs).usable() else {
            return Ok("skipped");
        };
        refresh_file(&self.path(agent), self.text.as_bytes(), |text| {
            text.starts_with(WRITTEN_BY)
        })
    }
}

pub fn refresh_extensions(dirs: &Dirs) -> [(&'static str, Result<&'static str>); 3] {
    [PI_EXTENSION, OMP_EXTENSION, OPENCODE_PLUGIN]
        .map(|extension| (extension.name, extension.refresh(dirs)))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[test]
    fn the_daemon_refreshes_only_an_extension_install_wrote() -> Result<()> {
        let home = tempfile::tempdir()?;
        let dirs = Dirs::new(home.path());
        let refresh = || -> Result<Vec<&'static str>> {
            refresh_extensions(&dirs)
                .into_iter()
                .map(|(_, r)| r)
                .collect()
        };
        let path =
            |extension: &Extension| extension.path((extension.agent)(&dirs).known().unwrap());
        let pi_path = path(&PI_EXTENSION);
        let omp_path = path(&OMP_EXTENSION);
        assert_eq!(refresh()?, ["absent", "absent", "absent"]);
        assert!(
            !pi_path.exists() && !omp_path.exists(),
            "a removed extension is not written back"
        );

        for path in [&pi_path, &omp_path] {
            std::fs::create_dir_all(path.parent().unwrap())?;
        }
        std::fs::write(&pi_path, format!("{}, by an older version\n", WRITTEN_BY))?;
        assert_eq!(refresh()?, ["updated", "absent", "absent"]);
        assert_eq!(std::fs::read_to_string(&pi_path)?, pi::EXTENSION);
        assert!(!omp_path.exists());

        std::fs::write(&omp_path, format!("{}, by an older version\n", WRITTEN_BY))?;
        let inode = std::fs::metadata(&pi_path)?.ino();
        assert_eq!(refresh()?, ["unchanged", "updated", "absent"]);
        assert_eq!(std::fs::read_to_string(&omp_path)?, omp::EXTENSION);
        assert_eq!(
            std::fs::metadata(&pi_path)?.ino(),
            inode,
            "a current file is not replaced"
        );

        std::fs::write(&omp_path, "// the user's own extension\n")?;
        assert_eq!(refresh()?, ["unchanged", "kept", "absent"]);
        assert_eq!(
            std::fs::read_to_string(&omp_path)?,
            "// the user's own extension\n"
        );
        Ok(())
    }

    #[test]
    fn uninstall_removes_the_opencode_plugins_directory_once_empty() -> Result<()> {
        let home = tempfile::tempdir()?;
        let dirs = Dirs::new(home.path());
        std::fs::create_dir_all(home.path().join(".config/opencode"))?;
        let path = OPENCODE_PLUGIN.path(dirs.opencode.known().unwrap());
        let dir = path.parent().unwrap();

        OPENCODE_PLUGIN.install(&dirs)?;
        let other = dir.join("notes.md");
        std::fs::write(&other, "the user's\n")?;
        assert_eq!(OPENCODE_PLUGIN.uninstall(&dirs)?.0, "removed");
        assert!(!path.exists() && other.exists());

        std::fs::remove_file(&other)?;
        OPENCODE_PLUGIN.install(&dirs)?;
        assert_eq!(OPENCODE_PLUGIN.uninstall(&dirs)?.0, "removed");
        assert!(
            !dir.exists(),
            "OpenCode would load the directory as a plugin"
        );
        Ok(())
    }
}
