//! The Pi and omp extensions, each written into its runtime's agent directory under home and
//! recognized by the first line every version of either starts with.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Outcome, remove_file, write};
use crate::runtime::{omp, pi};

/// A runtime's extension, which install writes into the runtime's agent directory under home.
pub(super) struct Extension {
    /// The runtime's name, for the daemon's log.
    name: &'static str,
    agent: &'static str,
    text: &'static str,
}

pub(super) const PI_EXTENSION: Extension = Extension {
    name: "Pi",
    agent: ".pi/agent",
    text: pi::EXTENSION,
};
pub(super) const OMP_EXTENSION: Extension = Extension {
    name: "omp",
    agent: ".omp/agent",
    text: omp::EXTENSION,
};

impl Extension {
    pub(super) fn path(&self, home: &Path) -> PathBuf {
        home.join(self.agent).join("extensions/rexume.ts")
    }

    /// Writes the extension when the runtime's agent directory exists.
    pub(super) fn install(&self, home: &Path) -> Result<Outcome> {
        if !home.join(self.agent).is_dir() {
            return Ok((
                "skipped",
                Some(format!("`~/{}` does not exist", self.agent)),
            ));
        }
        Ok((write(&self.path(home), self.text.as_bytes())?, None))
    }

    /// Removes the extension when install wrote it, in this version or an earlier one.
    pub(super) fn uninstall(&self, home: &Path) -> Result<Outcome> {
        remove_file(&self.path(home), |text| text.starts_with(pi::WRITTEN_BY))
    }

    /// Rewrites the extension install wrote when it differs from this version's, so that an
    /// upgrade reaches the runtime without another install. A missing file stays missing, so that
    /// uninstall stays final, and a file install did not write is left as it is. Returns
    /// `updated`, `unchanged`, `absent`, or `kept`.
    fn refresh(&self, home: &Path) -> Result<&'static str> {
        let path = self.path(home);
        match std::fs::read_to_string(&path) {
            Ok(text) if text.starts_with(pi::WRITTEN_BY) => write(&path, self.text.as_bytes()),
            Ok(_) => Ok("kept"),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok("absent"),
            Err(e) => Err(e).with_context(|| format!("failed to read `{}`", path.display())),
        }
    }
}

/// Refreshes the Pi and omp extensions, each on its own, and returns each runtime's name with what
/// `Extension::refresh` returned.
pub fn refresh_extensions(home: &Path) -> [(&'static str, Result<&'static str>); 2] {
    [PI_EXTENSION, OMP_EXTENSION].map(|extension| (extension.name, extension.refresh(home)))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    /// Each extension is refreshed on its own: a missing or user-owned file at one target stays
    /// as it is while the other is updated.
    #[test]
    fn the_daemon_refreshes_only_an_extension_install_wrote() -> Result<()> {
        let home = tempfile::tempdir()?;
        let refresh = || -> Result<Vec<&'static str>> {
            refresh_extensions(home.path())
                .into_iter()
                .map(|(_, r)| r)
                .collect()
        };
        let pi_path = PI_EXTENSION.path(home.path());
        let omp_path = OMP_EXTENSION.path(home.path());
        assert_eq!(refresh()?, ["absent", "absent"]);
        assert!(
            !pi_path.exists() && !omp_path.exists(),
            "a removed extension is not written back"
        );

        for path in [&pi_path, &omp_path] {
            std::fs::create_dir_all(path.parent().unwrap())?;
        }
        std::fs::write(
            &pi_path,
            format!("{}, by an older version\n", pi::WRITTEN_BY),
        )?;
        assert_eq!(refresh()?, ["updated", "absent"]);
        assert_eq!(std::fs::read_to_string(&pi_path)?, pi::EXTENSION);
        assert!(!omp_path.exists());

        std::fs::write(
            &omp_path,
            format!("{}, by an older version\n", pi::WRITTEN_BY),
        )?;
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
