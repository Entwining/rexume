use super::*;

fn resolve(home: &Path, vars: &[(&str, &str)]) -> Dirs {
    Dirs::resolve(home, |var| {
        vars.iter()
            .find(|(name, _)| *name == var)
            .map(|(_, value)| OsString::from(value))
    })
}

#[test]
fn an_absolute_variable_moves_its_runtime_and_an_empty_one_leaves_the_default() {
    let home = Path::new("/home/u");
    let dirs = resolve(
        home,
        &[
            ("CLAUDE_CONFIG_DIR", "/home/u/.config/claude"),
            ("PI_CODING_AGENT_DIR", ""),
        ],
    );
    assert_eq!(
        dirs.claude.usable(),
        Ok(Path::new("/home/u/.config/claude"))
    );
    assert_eq!(dirs.codex.usable(), Ok(Path::new("/home/u/.codex")));
    assert_eq!(dirs.pi.usable(), Ok(Path::new("/home/u/.pi/agent")));
    assert_eq!(dirs.omp.usable(), Ok(Path::new("/home/u/.omp/agent")));
    assert_eq!(
        dirs.vars(),
        [(
            "CLAUDE_CONFIG_DIR".to_owned(),
            "/home/u/.config/claude".to_owned()
        )]
    );
    assert_eq!(dirs.show(dirs.claude.usable().unwrap()), "~/.config/claude");
    assert_eq!(dirs.show(Path::new("/srv/pi")), "/srv/pi");
    assert!(Dirs::new(home).vars().is_empty());
}

/// Codex and Claude Code read a relative value against their working directory, and Pi expands
/// only a leading `~`, so any other value names a directory install cannot know.
#[test]
fn a_variable_install_cannot_resolve_blocks_its_runtime_instead_of_leaving_the_default() {
    let home = Path::new("/home/u");
    let dirs = resolve(
        home,
        &[
            ("CLAUDE_CONFIG_DIR", "~/.config/claude"),
            ("CODEX_HOME", "relative/codex"),
            ("PI_CODING_AGENT_DIR", "~/.config/pi/agent"),
        ],
    );
    assert_eq!(
        dirs.claude.usable(),
        Err("`CLAUDE_CONFIG_DIR` is `~/.config/claude`, not an absolute path".to_owned())
    );
    assert_eq!(
        dirs.codex.known(),
        Err("`CODEX_HOME` is `relative/codex`, not an absolute path".to_owned())
    );
    assert_eq!(dirs.pi.usable(), Ok(Path::new("/home/u/.config/pi/agent")));
    assert_eq!(
        resolve(home, &[("PI_CODING_AGENT_DIR", "~")]).pi.usable(),
        Ok(home)
    );
    assert_eq!(
        resolve(home, &[("PI_CODING_AGENT_DIR", "pi")]).pi.usable(),
        Err(
            "`PI_CODING_AGENT_DIR` is `pi`, not an absolute path or one starting with `~/`"
                .to_owned()
        )
    );
    assert_eq!(
        dirs.vars().len(),
        3,
        "the daemon resolves every variable install read, blocked ones included"
    );
}

#[test]
fn a_moved_pi_directory_is_left_alone_while_omp_would_load_from_it_too() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin)?;
    let path = bin.to_str().unwrap();
    let vars = [("PI_CODING_AGENT_DIR", "/srv/pi"), ("PATH", path)];
    assert_eq!(
        resolve(home.path(), &vars).pi.usable(),
        Ok(Path::new("/srv/pi"))
    );

    std::fs::write(bin.join("omp"), "")?;
    let dirs = resolve(home.path(), &vars);
    let note = "`PI_CODING_AGENT_DIR` also moves omp's agent directory, and omp would load Pi's \
                extension there";
    assert_eq!(dirs.pi.usable(), Err(note.to_owned()));
    assert_eq!(
        dirs.pi.known(),
        Ok(Path::new("/srv/pi")),
        "uninstall still removes an extension an earlier install wrote there"
    );
    assert_eq!(
        resolve(home.path(), &[("PATH", path)]).pi.usable(),
        Ok(home.path().join(".pi/agent").as_path()),
        "omp reads its own directory while the variable is unset"
    );

    std::fs::remove_file(bin.join("omp"))?;
    std::fs::create_dir(home.path().join(".omp"))?;
    assert_eq!(
        resolve(home.path(), &[("PI_CODING_AGENT_DIR", "/srv/pi")])
            .pi
            .usable(),
        Err(note.to_owned())
    );
    Ok(())
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
    let dirs = resolve(
        home.path(),
        &[
            ("CLAUDE_CONFIG_DIR", claude.to_str().unwrap()),
            ("PI_CODING_AGENT_DIR", pi.to_str().unwrap()),
        ],
    );
    let hook = serde_json::json!({ "type": "command", "command": "rexume report claude" });
    let settings = Targets::new(&dirs).claude;
    assert_eq!(settings, Ok(claude.join("settings.json")));
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

#[test]
fn install_skips_a_runtime_whose_directory_it_cannot_resolve() -> anyhow::Result<()> {
    use super::super::{PI_EXTENSION, Targets, add_hook, claude_plugin, skill};

    let home = tempfile::tempdir()?;
    for default in [".claude", ".pi/agent"] {
        std::fs::create_dir_all(home.path().join(default))?;
    }
    let dirs = resolve(
        home.path(),
        &[
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("PI_CODING_AGENT_DIR", "pi"),
        ],
    );
    let hook = serde_json::json!({ "type": "command", "command": "rexume report claude" });
    let (outcome, note) = add_hook(
        &dirs,
        &Targets::new(&dirs).claude,
        "claude",
        &["Stop"],
        &hook,
    )?;
    assert_eq!(outcome, "skipped");
    assert_eq!(
        note.as_deref(),
        Some("`CLAUDE_CONFIG_DIR` is `claude`, not an absolute path")
    );
    assert_eq!(PI_EXTENSION.install(&dirs)?.0, "skipped");
    assert_eq!(claude_plugin::install(&dirs, "/nonexistent")?.0, "skipped");
    let [(_, agents), (_, claude_skill)] = skill::install(&dirs)?;
    assert_eq!((agents.0, claude_skill.0), ("skipped", "skipped"));
    assert_eq!(
        std::fs::read_dir(home.path().join(".claude"))?.count()
            + std::fs::read_dir(home.path().join(".pi/agent"))?.count(),
        0,
        "nothing reached the defaults"
    );
    assert!(!home.path().join(".agents").exists());
    Ok(())
}

#[test]
fn opencode_reads_its_own_variable_before_the_xdg_configuration_directory() {
    let home = Path::new("/home/u");
    let opencode =
        |vars: &[(&str, &str)]| resolve(home, vars).opencode.usable().map(Path::to_owned);
    assert_eq!(opencode(&[]), Ok(home.join(".config/opencode")));
    assert_eq!(
        opencode(&[("XDG_CONFIG_HOME", "/xdg")]),
        Ok(PathBuf::from("/xdg/opencode"))
    );
    assert_eq!(
        opencode(&[("XDG_CONFIG_HOME", "/xdg"), ("OPENCODE_CONFIG_DIR", "/oc")]),
        Ok(PathBuf::from("/oc"))
    );
    assert_eq!(
        opencode(&[("XDG_CONFIG_HOME", "/xdg"), ("OPENCODE_CONFIG_DIR", "")]),
        Ok(PathBuf::from("/xdg/opencode"))
    );
    assert_eq!(
        opencode(&[("XDG_CONFIG_HOME", "xdg")]),
        Err("`XDG_CONFIG_HOME` is `xdg`, not an absolute path".to_owned())
    );
}
