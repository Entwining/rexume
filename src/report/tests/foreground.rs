use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};

use super::*;

/// Starts a program under a process named `-perl`, as Rex starts a resumed program under a login
/// shell, and returns both pids. The launcher takes its script from `-e`, or from its input when
/// `args` is false, as a login shell at a prompt has no arguments. Both end once the launcher's
/// input closes, when the returned child drops.
fn launched(args: bool) -> (Child, u32, u32) {
    let script = "$| = 1; my $pid = open(my $c, '|-', 'cat') or die; print \"$pid\\n\"; \
                  1 while <STDIN>;\n__END__\n";
    let mut command = Command::new("perl");
    command
        .arg0("-perl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if args {
        command.args(["-e", script]);
    }
    let mut launcher = command.spawn().unwrap();
    if !args {
        launcher
            .stdin
            .as_mut()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
    }
    let mut line = String::new();
    BufReader::new(launcher.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let (pid, program) = (launcher.id(), line.trim().parse().unwrap());
    (launcher, pid, program)
}

fn shown(pid: u32, argv0: &str) -> Value {
    json!({ "foreground": { "pid": pid, "argv0": argv0 } })
}

#[test]
fn the_foreground_is_the_program_or_the_launcher_rex_resumed_it_through() {
    let (_resumed, launcher, program) = launched(true);
    assert_eq!(
        foreground_of(&shown(program, "cat"), program),
        Some(program)
    );
    assert_eq!(
        foreground_of(&shown(launcher, "-perl"), program),
        Some(launcher)
    );
    // Rex names a login shell by the argv0 its login gave it.
    assert_eq!(foreground_of(&shown(launcher, "perl"), program), None);

    // A login shell without arguments runs a program as a job, not as a resume; and a launcher is
    // not another program's.
    let (_shell, prompt, job) = launched(false);
    assert_eq!(foreground_of(&shown(prompt, "-perl"), job), None);
    assert_eq!(foreground_of(&shown(launcher, "-perl"), job), None);
}

#[test]
fn the_terminals_own_program_is_the_agent_not_its_launcher() {
    let (launcher, program) = (10, 11);
    assert!(!own_program(
        &json!({ "child": { "pid": launcher } }),
        program
    ));
    assert!(own_program(
        &json!({ "child": { "pid": program } }),
        program
    ));
}
