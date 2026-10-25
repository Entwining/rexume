use super::launch_agent::plist;
use super::*;

#[test]
fn another_path_to_rexume_is_the_same_hook() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("settings.json");
    let other = json!({ "hooks": [{ "type": "command", "command": "other", "timeout": 5 }] });
    let installed = json!({ "hooks": [{ "type": "command",
        "command": "/opt/homebrew/bin/rexume report claude", "timeout": 5 }] });
    std::fs::write(
        &path,
        json!({ "hooks": { "SessionStart": [other, installed] } }).to_string(),
    )?;
    let hook = json!({ "type": "command", "command": "rexume report claude", "timeout": 5 });
    assert_eq!(add_hook(&path, "claude", hook)?.0, "unchanged");
    assert_eq!(remove_hook(&path, "claude")?, "removed");
    let left: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(left, json!({ "hooks": { "SessionStart": [other] } }));
    assert_eq!(remove_hook(&path, "claude")?, "absent");
    Ok(())
}

#[test]
fn a_runtime_that_is_not_set_up_gets_no_settings() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join(".codex/hooks.json");
    let (value, note) = add_hook(&path, "codex", json!({}))?;
    assert_eq!(value, "skipped");
    assert_eq!(note.as_deref(), Some("`~/.codex` does not exist"));
    assert!(!path.exists());
    Ok(())
}

#[test]
fn the_plist_round_trips_through_its_parsed_form() -> Result<()> {
    let install = Install {
        exe: "/opt/homebrew/bin/rexume".into(),
        rex: "/Applications/Rex.app/Contents/MacOS/rex".into(),
        server: "unix:///tmp/a%20b/server.sock".into(),
        theme: Some(("#ffffff".into(), "#000000".into())),
        path: "/usr/bin:/bin".into(),
    };
    let wanted = plist(&install, Path::new("/tmp/rexume & co.log"));
    let mut bytes = Vec::new();
    wanted.to_writer_xml(&mut bytes)?;
    assert_eq!(plist::Value::from_reader_xml(bytes.as_slice())?, wanted);
    Ok(())
}

#[test]
fn the_daemon_refreshes_only_an_extension_install_wrote() -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let home = tempfile::tempdir()?;
    let path = Targets::new(home.path()).pi;
    assert_eq!(refresh_pi_extension(home.path())?, "absent");
    assert!(!path.exists(), "a removed extension is not written back");

    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, format!("{}, by an older version\n", pi::WRITTEN_BY))?;
    assert_eq!(refresh_pi_extension(home.path())?, "updated");
    assert_eq!(std::fs::read_to_string(&path)?, pi::EXTENSION);

    let inode = std::fs::metadata(&path)?.ino();
    assert_eq!(refresh_pi_extension(home.path())?, "unchanged");
    assert_eq!(
        std::fs::metadata(&path)?.ino(),
        inode,
        "a current file is not replaced"
    );

    std::fs::write(&path, "// the user's own extension\n")?;
    assert_eq!(refresh_pi_extension(home.path())?, "kept");
    assert_eq!(
        std::fs::read_to_string(&path)?,
        "// the user's own extension\n"
    );
    Ok(())
}
