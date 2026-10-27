//! The process tree, to tell which agent process a command runs under.
#![cfg_attr(
    target_os = "macos",
    expect(
        unsafe_code,
        reason = "libc::proc_pidinfo has no safe wrapper in the dependency set; AGENTS.md records this"
    )
)]

#[cfg(target_os = "macos")]
use std::ffi::CStr;
#[cfg(target_os = "macos")]
use std::mem::{MaybeUninit, size_of};

/// Programs that only pass a command along: hooks and tool calls reach this tool through them.
const WRAPPERS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "env", "rexume"];

/// A process's parent and executable name, or `None` when no such process is visible or it has
/// exited and only waits to be reaped.
#[cfg(target_os = "linux")]
fn info(pid: u32) -> Option<(u32, String)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .map(str::trim)
    };
    if field("State")?.starts_with(['Z', 'X']) {
        return None;
    }
    Some((field("PPid")?.parse().ok()?, field("Name")?.to_owned()))
}

/// A process's parent and executable name, or `None` when no such process is visible.
#[cfg(target_os = "macos")]
fn info(pid: u32) -> Option<(u32, String)> {
    let pid = i32::try_from(pid).ok()?;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: the buffer is exactly `size` bytes of writable memory for one proc_bsdinfo, which
    // PROC_PIDTBSDINFO fills.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the kernel wrote the whole struct, as the returned size shows.
    let info = unsafe { info.assume_init() };
    // SAFETY: pbi_comm is a NUL-terminated C string within the array the kernel filled.
    let name = unsafe { CStr::from_ptr(info.pbi_comm.as_ptr()) };
    Some((info.pbi_ppid, name.to_string_lossy().into_owned()))
}

pub fn alive(pid: i64) -> bool {
    u32::try_from(pid).is_ok_and(|pid| info(pid).is_some())
}

/// The nearest ancestor of `pid` that is not a shell or this tool: the program that ran the hook
/// or tool call.
pub fn program_above(pid: u32) -> Option<u32> {
    above(pid, info)
}

fn above(pid: u32, info: impl Fn(u32) -> Option<(u32, String)>) -> Option<u32> {
    let mut current = info(pid)?.0;
    for _ in 0..64 {
        let (parent, name) = info(current)?;
        if !WRAPPERS.contains(&name.trim_start_matches('-')) {
            return Some(current);
        }
        current = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn skips_shells_between_a_command_and_its_program() {
        let table = HashMap::from([
            (10, (1, "2.1.310".to_owned())),
            (20, (10, "zsh".to_owned())),
            (30, (20, "-zsh".to_owned())),
            (40, (30, "rexume".to_owned())),
        ]);
        let lookup = |pid| table.get(&pid).cloned();
        assert_eq!(above(40, lookup), Some(10));
        assert_eq!(above(10, lookup), None);
    }

    #[test]
    fn reads_this_process_and_its_parent() {
        let (parent, name) = info(std::process::id()).expect("this process is visible");
        assert!(name.starts_with("rexume"));
        assert_eq!(parent, std::os::unix::process::parent_id());
        assert!(!alive(-1));
    }

    /// A `task wait` that exited but was not reaped yet can no longer take its round's result.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_process_is_not_alive_before_it_is_reaped() {
        let mut child = std::process::Command::new("/bin/true")
            .spawn()
            .expect("true starts");
        let pid = i64::from(child.id());
        let status = format!("/proc/{pid}/status");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !std::fs::read_to_string(&status).is_ok_and(|s| s.contains("\nState:\tZ")) {
            assert!(std::time::Instant::now() < until, "the child never exited");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!alive(pid));
        child.wait().expect("the child is reaped");
    }
}
