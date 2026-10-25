//! The process tree, to tell which agent process a command runs under.
#![expect(
    unsafe_code,
    reason = "libc::proc_pidinfo has no safe wrapper in the dependency set; AGENTS.md records this"
)]

use std::ffi::CStr;
use std::mem::{MaybeUninit, size_of};

/// Programs that only pass a command along: hooks and tool calls reach this tool through them.
const WRAPPERS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "env", "rexume"];

/// A process's parent and executable name, or `None` when no such process is visible.
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
}
