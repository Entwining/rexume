use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A runtime's configuration directory: the one its variable names, else `default` in the home.
struct Location {
    var: &'static str,
    default: &'static str,
}

const CLAUDE: Location = Location {
    var: "CLAUDE_CONFIG_DIR",
    default: ".claude",
};
const CODEX: Location = Location {
    var: "CODEX_HOME",
    default: ".codex",
};
const PI: Location = Location {
    var: "PI_CODING_AGENT_DIR",
    default: ".pi/agent",
};

/// Where each runtime reads its configuration. Install, the daemon, and status resolve it the same
/// way, and the job passes the variables that moved a directory on to the daemon, so that the
/// daemon refreshes the files install wrote and not ones in the home's defaults.
pub struct Dirs {
    pub home: PathBuf,
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub pi: PathBuf,
    /// omp reads `~/.omp/agent` unless a profile or a variable it shares with Pi moves it; install
    /// writes only the default (`docs/setup.md`).
    pub omp: PathBuf,
    moved: Vec<(&'static str, PathBuf)>,
}

impl Dirs {
    /// The home's defaults, whatever the environment says.
    #[cfg(test)]
    pub fn new(home: &Path) -> Self {
        Self::resolve(home, |_| None)
    }

    pub fn from_env(home: &Path) -> Self {
        Self::resolve(home, |var| std::env::var_os(var))
    }

    /// A variable that is unset, empty, or not absolute leaves its runtime's default, as an
    /// `XDG_CONFIG_HOME` that is not absolute does.
    pub fn resolve(home: &Path, var: impl Fn(&str) -> Option<OsString>) -> Self {
        let mut moved = Vec::new();
        let mut dir = |location: Location| match var(location.var).map(PathBuf::from) {
            Some(path) if path.is_absolute() => {
                moved.push((location.var, path.clone()));
                path
            }
            _ => home.join(location.default),
        };
        let (claude, codex, pi) = (dir(CLAUDE), dir(CODEX), dir(PI));
        Self {
            home: home.to_owned(),
            claude,
            codex,
            pi,
            omp: home.join(".omp/agent"),
            moved,
        }
    }

    /// The variables that moved a directory, with the directories they name, for the job.
    pub fn moved(&self) -> Vec<(String, String)> {
        self.moved
            .iter()
            .map(|(var, path)| ((*var).to_owned(), path.to_string_lossy().into_owned()))
            .collect()
    }

    /// `path` for a message: `~/…` inside the home, else the whole path.
    pub fn show(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variable_moves_its_runtime_only_when_it_names_an_absolute_path() {
        let home = Path::new("/home/u");
        let vars = [
            ("CLAUDE_CONFIG_DIR", "/home/u/.config/claude"),
            ("CODEX_HOME", "relative/codex"),
            ("PI_CODING_AGENT_DIR", ""),
        ];
        let dirs = Dirs::resolve(home, |var| {
            vars.iter()
                .find(|(name, _)| *name == var)
                .map(|(_, value)| OsString::from(value))
        });
        assert_eq!(dirs.claude, Path::new("/home/u/.config/claude"));
        assert_eq!(dirs.codex, Path::new("/home/u/.codex"));
        assert_eq!(dirs.pi, Path::new("/home/u/.pi/agent"));
        assert_eq!(dirs.omp, Path::new("/home/u/.omp/agent"));
        assert_eq!(
            dirs.moved(),
            [(
                "CLAUDE_CONFIG_DIR".to_owned(),
                "/home/u/.config/claude".to_owned()
            )]
        );
        assert_eq!(dirs.show(&dirs.claude), "~/.config/claude");
        assert_eq!(dirs.show(Path::new("/srv/pi")), "/srv/pi");
        assert!(Dirs::new(home).moved().is_empty());
    }

    #[test]
    fn install_writes_where_the_variables_point_and_nothing_in_the_home() -> anyhow::Result<()> {
        use super::super::{CLAUDE_EVENTS, PI_EXTENSION, Targets, add_hook, skill};

        let home = tempfile::tempdir()?;
        let (claude, pi) = (
            home.path().join("cfg/claude"),
            home.path().join("cfg/pi/agent"),
        );
        for dir in [&claude, &pi] {
            std::fs::create_dir_all(dir)?;
        }
        let vars = [("CLAUDE_CONFIG_DIR", &claude), ("PI_CODING_AGENT_DIR", &pi)];
        let dirs = Dirs::resolve(home.path(), |var| {
            vars.iter()
                .find(|(name, _)| *name == var)
                .map(|(_, path)| path.as_os_str().to_owned())
        });
        let hook = serde_json::json!({ "type": "command", "command": "rexume report claude" });
        let settings = Targets::new(&dirs).claude;
        assert_eq!(settings, claude.join("settings.json"));
        assert_eq!(
            add_hook(&dirs, &settings, "claude", &CLAUDE_EVENTS, &hook)?.0,
            "created"
        );
        assert_eq!(PI_EXTENSION.install(&dirs)?.0, "created");
        assert!(pi.join("extensions/rexume.ts").is_file());
        let [(_, agents), (_, claude_skill)] = skill::install(&dirs)?;
        assert_eq!((agents.0, claude_skill.0), ("created", "created"));
        assert!(claude.join("skills/rexume/SKILL.md").is_file());
        for default in [".claude", ".pi", ".codex"] {
            assert!(!home.path().join(default).exists(), "{default} was created");
        }
        let codex = Targets::new(&dirs).codex;
        let (_, note) = add_hook(&dirs, &codex, "codex", &["SessionStart"], &hook)?;
        assert_eq!(note.as_deref(), Some("`~/.codex` does not exist"));
        Ok(())
    }
}
