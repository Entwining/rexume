//! The process tree, to tell which agent process a command runs under, and what a process was
//! started with.
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

/// What a process was started with, as the kernel keeps it: its arguments, and its environment as
/// `NAME=value` entries before the process changed any of them.
pub struct Launch {
    pub argv: Vec<String>,
    pub env: Vec<String>,
}

/// NUL-terminated strings. A byte that is not UTF-8 reads as U+FFFD, so that one such string
/// leaves the others readable.
fn strings(bytes: &[u8]) -> Vec<String> {
    bytes
        .strip_suffix(&[0])
        .unwrap_or(bytes)
        .split(|byte| *byte == 0)
        .map(|string| String::from_utf8_lossy(string).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
pub fn launch(pid: u32) -> Option<Launch> {
    let read = |file| std::fs::read(format!("/proc/{pid}/{file}")).ok();
    Some(Launch {
        argv: strings(&read("cmdline")?),
        env: strings(&read("environ")?),
    })
}

#[cfg(target_os = "macos")]
pub fn launch(pid: u32) -> Option<Launch> {
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
    parse(bytes.get(..size)?)
}

/// `KERN_PROCARGS2` holds `argc`, the executable's path padded with NULs, the arguments, and the
/// environment, which the kernel follows with strings of its own (`ptr_munge=` and the like).
#[cfg(target_os = "macos")]
fn parse(bytes: &[u8]) -> Option<Launch> {
    let argc = usize::try_from(u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?)).ok()?;
    let mut bytes = bytes.get(4..)?;
    bytes = bytes.get(bytes.iter().position(|byte| *byte == 0)?..)?;
    bytes = bytes.get(bytes.iter().position(|byte| *byte != 0)?..)?;
    let mut strings = strings(bytes).into_iter();
    let argv: Vec<String> = strings.by_ref().take(argc).collect();
    (argv.len() == argc).then(|| Launch {
        argv,
        env: strings.filter(|string| !string.is_empty()).collect(),
    })
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

/// A process's parent, or `None` when no such process is visible.
pub fn parent(pid: i64) -> Option<i64> {
    info(u32::try_from(pid).ok()?).map(|(parent, _)| i64::from(parent))
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
    fn reads_what_this_process_was_started_with() {
        let launch = launch(std::process::id()).expect("this process is visible");
        let executable = std::env::current_exe().expect("this process has an executable");
        assert!(launch.argv[0].ends_with(executable.file_name().unwrap().to_str().unwrap()));
        let path = format!(
            "PATH={}",
            std::env::var("PATH").expect("tests run with a PATH")
        );
        assert!(launch.env.contains(&path));
    }

    /// A short read is unknown, which keeps what was recorded; it never reads as no arguments.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_kernel_buffer_splits_into_arguments_and_environment() {
        let mut bytes = 3_u32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/agent\0\0\0agent\0\0--rc\0AWS_REGION=x\0\0ptr_munge=\0\0\0");
        let launch = parse(&bytes).unwrap();
        assert_eq!(launch.argv, ["agent", "", "--rc"]);
        assert_eq!(launch.env, ["AWS_REGION=x", "ptr_munge="]);
        for short in [&bytes[..3], &bytes[..10], &bytes[..23]] {
            assert!(parse(short).is_none());
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
