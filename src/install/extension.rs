use std::path::{Path, PathBuf};

use anyhow::Result;

use super::dirs::Dir;
use super::{Dirs, Outcome, refresh_file, remove_file, write};
use crate::runtime::{omp, pi};

/// Every version of either extension starts with this, whatever follows it on the first line.
const WRITTEN_BY: &str = "// Written by `rexume install`";

pub(super) struct Extension {
    name: &'static str,
    agent: fn(&Dirs) -> &Dir,
    text: &'static str,
}

pub(super) const PI_EXTENSION: Extension = Extension {
    name: "Pi",
    agent: |dirs| &dirs.pi,
    text: pi::EXTENSION,
};
pub(super) const OMP_EXTENSION: Extension = Extension {
    name: "omp",
    agent: |dirs| &dirs.omp,
    text: omp::EXTENSION,
};

fn path(agent: &Path) -> PathBuf {
    agent.join("extensions/rexume.ts")
}

impl Extension {
    #[cfg(test)]
    fn path(&self, dirs: &Dirs) -> PathBuf {
        path((self.agent)(dirs).known().unwrap())
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
        Ok((write(&path(agent), self.text.as_bytes())?, None))
    }

    /// Removes an extension an earlier install wrote, also from a directory install now leaves
    /// alone.
    pub(super) fn uninstall(&self, dirs: &Dirs) -> Result<Outcome> {
        match (self.agent)(dirs).known() {
            Ok(agent) => remove_file(&path(agent), |text| text.starts_with(WRITTEN_BY)),
            Err(note) => Ok(("skipped", Some(note))),
        }
    }

    fn refresh(&self, dirs: &Dirs) -> Result<&'static str> {
        let Ok(agent) = (self.agent)(dirs).usable() else {
            return Ok("skipped");
        };
        refresh_file(&path(agent), self.text.as_bytes(), |text| {
            text.starts_with(WRITTEN_BY)
        })
    }
}

pub fn refresh_extensions(dirs: &Dirs) -> [(&'static str, Result<&'static str>); 2] {
    [PI_EXTENSION, OMP_EXTENSION].map(|extension| (extension.name, extension.refresh(dirs)))
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
        let pi_path = PI_EXTENSION.path(&dirs);
        let omp_path = OMP_EXTENSION.path(&dirs);
        assert_eq!(refresh()?, ["absent", "absent"]);
        assert!(
            !pi_path.exists() && !omp_path.exists(),
            "a removed extension is not written back"
        );

        for path in [&pi_path, &omp_path] {
            std::fs::create_dir_all(path.parent().unwrap())?;
        }
        std::fs::write(&pi_path, format!("{}, by an older version\n", WRITTEN_BY))?;
        assert_eq!(refresh()?, ["updated", "absent"]);
        assert_eq!(std::fs::read_to_string(&pi_path)?, pi::EXTENSION);
        assert!(!omp_path.exists());

        std::fs::write(&omp_path, format!("{}, by an older version\n", WRITTEN_BY))?;
        let inode = std::fs::metadata(&pi_path)?.ino();
        assert_eq!(refresh()?, ["unchanged", "updated"]);
        assert_eq!(std::fs::read_to_string(&omp_path)?, omp::EXTENSION);
        assert_eq!(
            std::fs::metadata(&pi_path)?.ino(),
            inode,
            "a current file is not replaced"
        );

        std::fs::write(&omp_path, "// the user's own extension\n")?;
        assert_eq!(refresh()?, ["unchanged", "kept"]);
        assert_eq!(
            std::fs::read_to_string(&omp_path)?,
            "// the user's own extension\n"
        );
        Ok(())
    }
}
