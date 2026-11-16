use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{Dirs, Outcome, refresh_file, remove_file, write};
use crate::runtime::{omp, pi};

/// Every version of either extension starts with this, whatever follows it on the first line.
const WRITTEN_BY: &str = "// Written by `rexume install`";

pub(super) struct Extension {
    name: &'static str,
    agent: fn(&Dirs) -> &Path,
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

impl Extension {
    fn path(&self, dirs: &Dirs) -> PathBuf {
        (self.agent)(dirs).join("extensions/rexume.ts")
    }

    pub(super) fn install(&self, dirs: &Dirs) -> Result<Outcome> {
        let agent = (self.agent)(dirs);
        if !agent.is_dir() {
            return Ok((
                "skipped",
                Some(format!("`{}` does not exist", dirs.show(agent))),
            ));
        }
        Ok((write(&self.path(dirs), self.text.as_bytes())?, None))
    }

    pub(super) fn uninstall(&self, dirs: &Dirs) -> Result<Outcome> {
        remove_file(&self.path(dirs), |text| text.starts_with(WRITTEN_BY))
    }

    fn refresh(&self, dirs: &Dirs) -> Result<&'static str> {
        refresh_file(&self.path(dirs), self.text.as_bytes(), |text| {
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
