use std::io::Write;
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::Queue;

const WAIT: Option<Duration> = Some(Duration::from_secs(1));

#[test]
fn fires_for_appends_replacement_and_exit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.jsonl");
    let mut queue = Queue::new()?;
    assert!(!queue.file(&path, 1)?);
    std::fs::write(&path, "a\n")?;
    assert!(queue.file(&path, 1)?);
    assert!(queue.wait(Some(Duration::ZERO))?.is_empty());
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(b"b\n")?;
    let fired = queue.wait(WAIT)?;
    assert!(fired.iter().any(|f| f.token == 1 && !f.gone));
    std::fs::write(dir.path().join("new"), "c\n")?;
    std::fs::rename(dir.path().join("new"), &path)?;
    let fired = queue.wait(WAIT)?;
    assert!(fired.iter().any(|f| f.token == 1 && f.gone));

    let mut child = std::process::Command::new("/bin/sleep")
        .arg("0.1")
        .spawn()?;
    let pid = i64::from(child.id());
    assert!(queue.exit(pid, 2)?);
    let fired = queue.wait(Some(Duration::from_secs(5)))?;
    assert!(fired.iter().any(|f| f.token == 2));
    child.wait()?;
    assert!(!queue.exit(pid, 2)?);
    Ok(())
}

/// The daemon's own executable stays mapped while an upgrade renames a new one over its path.
#[test]
fn a_file_replaced_while_open_is_gone() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("rexume");
    std::fs::write(&path, "old")?;
    let _open = std::fs::File::open(&path)?;
    let mut queue = Queue::new()?;
    assert!(queue.entry(&path, 1)?);
    std::fs::write(dir.path().join("new"), "new")?;
    std::fs::rename(dir.path().join("new"), &path)?;
    let fired = queue.wait(WAIT)?;
    assert!(fired.iter().any(|f| f.token == 1 && f.gone));
    Ok(())
}

#[test]
fn a_replaced_link_fires_its_entry_watch() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let link = dir.path().join("rexume");
    std::fs::write(dir.path().join("target"), "")?;
    std::os::unix::fs::symlink("target", &link)?;
    let mut queue = Queue::new()?;
    assert!(queue.entry(&link, 1)?);
    std::fs::remove_file(&link)?;
    std::os::unix::fs::symlink("target", &link)?;
    let fired = queue.wait(WAIT)?;
    assert!(fired.iter().any(|f| f.token == 1 && f.gone));
    Ok(())
}

/// The daemon waits for the server's socket through its directory, and follows several records
/// not written yet through the same directory.
#[test]
fn watches_of_one_directory_fire_apart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut queue = Queue::new()?;
    assert!(queue.file(dir.path(), 1)?);
    assert!(queue.file(dir.path(), 2)?);
    queue.forget_file(1);
    let _listener = UnixListener::bind(dir.path().join("s.sock"))?;
    let fired = queue.wait(WAIT)?;
    assert!(fired.iter().any(|f| f.token == 2 && !f.gone));
    assert!(fired.iter().all(|f| f.token != 1));
    queue.forget_file(2);
    std::fs::write(dir.path().join("later"), "")?;
    assert!(queue.wait(Some(Duration::from_millis(100)))?.is_empty());
    Ok(())
}

/// A record followed through its directory shares it with other records being written.
#[test]
fn a_directory_watch_ignores_writes_to_its_files() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let other = dir.path().join("other.jsonl");
    std::fs::write(&other, "a\n")?;
    let mut queue = Queue::new()?;
    assert!(queue.file(dir.path(), 1)?);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&other)?
        .write_all(b"b\n")?;
    assert!(queue.wait(Some(Duration::from_millis(100)))?.is_empty());
    std::fs::write(dir.path().join("t.jsonl"), "c\n")?;
    let fired = queue.wait(WAIT)?;
    assert!(fired.iter().any(|f| f.token == 1 && !f.gone));
    Ok(())
}

/// Re-watching a path removes its old watch, whose last event names nothing the queue follows.
#[test]
fn wait_returns_nothing_only_once_its_timeout_passes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut queue = Queue::new()?;
    assert!(queue.file(dir.path(), 1)?);
    assert!(queue.file(dir.path(), 1)?);
    let timeout = Duration::from_millis(100);
    let started = Instant::now();
    assert!(queue.wait(Some(timeout))?.is_empty());
    assert!(started.elapsed() >= timeout);
    Ok(())
}

/// The kernel drops events once its queue is full, so every watch may have missed a change.
#[cfg(target_os = "linux")]
#[test]
fn an_overflow_fires_every_watch() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let [a, b, quiet] = ["a", "b", "quiet"].map(|name| dir.path().join(name));
    let mut queue = Queue::new()?;
    for (token, path) in [(0, &a), (1, &b), (2, &quiet)] {
        std::fs::write(path, "")?;
        assert!(queue.file(path, token)?);
    }
    let limit: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")?
        .trim()
        .parse()?;
    let open = |path| std::fs::OpenOptions::new().append(true).open(path);
    let (mut a, mut b) = (open(&a)?, open(&b)?);
    // Alternating between two files, so that the kernel cannot merge an event into the last one.
    for _ in 0..=limit / 2 {
        a.write_all(b"x")?;
        b.write_all(b"x")?;
    }
    loop {
        let fired = queue.wait(WAIT)?;
        assert!(!fired.is_empty());
        if let Some(f) = fired.iter().find(|f| f.token == 2) {
            assert!(!f.gone);
            return Ok(());
        }
    }
}

/// A group leader that exits before the rest of its group leaves its ID naming the group, which
/// older kernels report to `pidfd_open` as invalid rather than missing.
#[cfg(target_os = "linux")]
#[test]
fn a_reaped_group_leader_has_exited() -> Result<()> {
    use std::os::unix::process::CommandExt;

    let mut leader = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 1 & exit 0"])
        .process_group(0)
        .spawn()?;
    leader.wait()?;
    let mut queue = Queue::new()?;
    assert!(!queue.exit(i64::from(leader.id()), 1)?);
    Ok(())
}
