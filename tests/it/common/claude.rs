use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::{rex_bin, wait_until};

/// A stand-in for Claude Code in `dir/bin`, with the Rex CLI beside it. Each run writes its
/// arguments, one per line, to `dir/runs/PID`, then runs `then`, Perl statements. It is Perl
/// because a shell script runs under its shell's name (`bash` for `/bin/sh` on macOS), which the
/// hook skips as a wrapper on its way to the program that ran it.
pub fn stand_in(dir: &Path, then: &str) -> (PathBuf, PathBuf) {
    let (bin, runs) = (dir.join("bin"), dir.join("runs"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&runs).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        format!(
            "#!/usr/bin/perl\nmy $run = '{}/' . $$;\nopen(my $f, '>', \"$run.new\") or die $!;\n\
             print $f map {{ \"$_\\n\" }} @ARGV;\nclose $f;\nrename(\"$run.new\", $run) or die $!;\n{then}\n",
            runs.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(rex_bin(), bin.join("rex")).unwrap();
    (bin, runs)
}

/// The stand-in's runs once there are at least `count`, by the order they started, each with its
/// pid and arguments.
pub fn runs(dir: &Path, count: usize) -> Vec<(i64, String)> {
    wait_until("the agent to run", Duration::from_secs(20), || {
        let mut runs: Vec<(SystemTime, i64, String)> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                let path = e.unwrap().path();
                let pid = path.file_name()?.to_str()?.parse().ok()?;
                let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
                Some((modified, pid, std::fs::read_to_string(path).unwrap()))
            })
            .collect();
        runs.sort();
        (runs.len() >= count).then(|| runs.into_iter().map(|(_, p, a)| (p, a)).collect())
    })
}
