//! The process tree, to tell which agent process a command runs under.
#![cfg_attr(
    target_os = "macos",
    expect(
        unsafe_code,
        reason = "libc::proc_pidinfo and libc::sysctl have no safe wrappers in the dependency set; AGENTS.md records this"
    )
)]

#[cfg(target_os = "macos")]
use std::ffi::CStr;
#[cfg(target_os = "macos")]
use std::mem::{MaybeUninit, size_of};

/// Programs that only pass a command along: hooks and tool calls reach this tool through them.
const WRAPPERS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "env", "rexume"];

#[cfg(target_os = "linux")]
pub fn argv(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let bytes = bytes.strip_suffix(&[0])?;
    if bytes.is_empty() {
        return None;
    }
    bytes
        .split(|byte| *byte == 0)
        .map(|arg| std::str::from_utf8(arg).ok().map(str::to_owned))
        .collect()
}

#[cfg(target_os = "macos")]
pub fn argv(pid: u32) -> Option<Vec<String>> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        i32::try_from(pid).ok()?,
    ];
    let mut size = 0;
    // SAFETY: mib names the process arguments, size is writable, and a null buffer queries its size.
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return None;
    }
    let mut bytes = vec![0; size];
    // SAFETY: bytes holds size writable bytes, and mib and size remain valid for this call.
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return None;
    }
    parse_argv(bytes.get(..size)?)
}

#[cfg(target_os = "macos")]
fn parse_argv(bytes: &[u8]) -> Option<Vec<String>> {
    let argc = u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    if argc == 0 {
        return None;
    }
    let mut bytes = bytes.get(4..)?;
    bytes = bytes.get(bytes.iter().position(|byte| *byte == 0)?..)?;
    bytes = bytes.get(bytes.iter().position(|byte| *byte != 0)?..)?;
    let mut args = Vec::new();
    for _ in 0..argc {
        let end = bytes.iter().position(|byte| *byte == 0)?;
        args.push(std::str::from_utf8(&bytes[..end]).ok()?.to_owned());
        bytes = &bytes[end + 1..];
    }
    Some(args)
}

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

    #[test]
    fn reads_this_process_arguments() {
        let args = argv(std::process::id()).expect("this process is visible");
        let executable = std::env::current_exe().expect("this process has an executable");
        assert!(args[0].ends_with(executable.file_name().unwrap().to_str().unwrap()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn process_arguments_stop_before_the_environment_and_reject_incomplete_buffers() {
        let mut bytes = 3_u32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/agent\0\0\0agent\0\0--remote-control\0SECRET=value\0");
        assert_eq!(
            parse_argv(&bytes).unwrap(),
            ["agent", "", "--remote-control"]
        );
        for invalid in [
            &bytes[..3],
            &bytes[..10],
            &bytes[..bytes.len() - 14],
            &0_u32.to_ne_bytes(),
        ] {
            assert!(parse_argv(invalid).is_none());
        }
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
