use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A runtime's configuration directory: the one its variable names, else `default` in the home.
struct Location {
    var: &'static str,
    default: &'static str,
    /// Whether the runtime reads `~` and `~/…` in the variable as the home, as Pi does.
    tilde: bool,
}

const CLAUDE: Location = Location {
    var: "CLAUDE_CONFIG_DIR",
    default: ".claude",
    tilde: false,
};
const CODEX: Location = Location {
    var: "CODEX_HOME",
    default: ".codex",
    tilde: false,
};
const PI: Location = Location {
    var: "PI_CODING_AGENT_DIR",
    default: ".pi/agent",
    tilde: true,
};

/// Where one runtime reads its configuration, and whether install may write there.
pub struct Dir {
    /// `None` when the variable holds a value install cannot resolve, such as a relative path,
    /// which the runtime reads against a working directory install does not know.
    path: Option<PathBuf>,
    /// Why install neither writes nor refreshes anything for the runtime, if it does not.
    blocked: Option<String>,
}

impl Dir {
    fn at(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            blocked: None,
        }
    }

    /// The directory install writes and the daemon refreshes in, or why there is none.
    pub fn usable(&self) -> Result<&Path, String> {
        match (&self.blocked, &self.path) {
            (Some(note), _) => Err(note.clone()),
            (None, Some(path)) => Ok(path),
            (None, None) => unreachable!("a directory without a path is blocked"),
        }
    }

    /// The directory uninstall removes install's entries from: a blocked one too, when it is known,
    /// since an earlier install may have written there.
    pub fn known(&self) -> Result<&Path, String> {
        self.path.as_deref().ok_or_else(|| {
            self.blocked
                .clone()
                .unwrap_or_else(|| unreachable!("a directory without a path is blocked"))
        })
    }
}

/// Where each runtime reads its configuration. Install, the daemon, and status resolve it the same
/// way, and the job passes the variables install read on to the daemon, so that the daemon
/// refreshes the files install wrote and not ones in the home's defaults.
pub struct Dirs {
    pub home: PathBuf,
    pub claude: Dir,
    pub codex: Dir,
    pub pi: Dir,
    /// omp reads `~/.omp/agent` unless a profile or a variable it shares with Pi moves it; install
    /// writes only the default (`docs/setup.md`).
    pub omp: Dir,
    vars: Vec<(&'static str, OsString)>,
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

    /// An unset or empty variable leaves its runtime's default. One the runtime would read against
    /// its working directory blocks the runtime, since install cannot know that directory, and so
    /// does `PI_CODING_AGENT_DIR` while omp is installed, because omp loads extensions from the
    /// directory it names too.
    pub fn resolve(home: &Path, var: impl Fn(&str) -> Option<OsString>) -> Self {
        let mut vars = Vec::new();
        let mut dir = |location: Location| {
            let Some(value) = var(location.var).filter(|value| !value.is_empty()) else {
                return Dir::at(home.join(location.default));
            };
            vars.push((location.var, value.clone()));
            match expand(home, &value, location.tilde) {
                Some(path) => Dir::at(path),
                None => Dir {
                    path: None,
                    blocked: Some(unsupported(location, &value)),
                },
            }
        };
        let (claude, codex, mut pi) = (dir(CLAUDE), dir(CODEX), dir(PI));
        let pi_moved = vars.iter().any(|(name, _)| *name == PI.var);
        if pi_moved && omp_installed(home, var("PATH")) {
            pi.blocked.get_or_insert_with(|| {
                format!(
                    "`{}` also moves omp's agent directory, and omp would load Pi's extension there",
                    PI.var
                )
            });
        }
        Self {
            home: home.to_owned(),
            claude,
            codex,
            pi,
            omp: Dir::at(home.join(".omp/agent")),
            vars,
        }
    }

    /// The runtimes' variables that are set, as install read them, for the job, so that the
    /// daemon resolves each directory as install did.
    pub fn vars(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .map(|(var, value)| ((*var).to_owned(), value.to_string_lossy().into_owned()))
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

/// The directory `value` names, if the runtime reads it the same from any working directory.
fn expand(home: &Path, value: &OsString, tilde: bool) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return Some(path);
    }
    if tilde {
        if path == Path::new("~") {
            return Some(home.to_owned());
        }
        if let Some(rest) = value.to_str().and_then(|value| value.strip_prefix("~/")) {
            return Some(home.join(rest));
        }
    }
    None
}

fn unsupported(location: Location, value: &OsString) -> String {
    let wanted = if location.tilde {
        "an absolute path or one starting with `~/`"
    } else {
        "an absolute path"
    };
    format!(
        "`{}` is `{}`, not {wanted}",
        location.var,
        value.to_string_lossy()
    )
}

/// omp is set up when its default directory exists or `omp` is on `PATH`.
fn omp_installed(home: &Path, path: Option<OsString>) -> bool {
    home.join(".omp").is_dir()
        || path
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("omp").is_file()))
}

#[cfg(test)]
mod tests;
