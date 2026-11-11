//! A Claude Code conversation started through a wrapper that declares its launch reopens through
//! the declared command, with the choice the wrapper made then rather than its current default,
//! and with the `REXUME_TASK_META` it started with.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::common::{Daemon, REXUME, SHELL, Server, claude, db, query, rex_bin, wait_until};
use crate::restore::{WAIT, agents, restart, restores};

/// A Perl whose start environment another process can read, as Rexume reads Claude Code's. macOS
/// withholds it for its own binaries, `/usr/bin/perl` among them, but not for a copy signed by
/// anyone else, as Claude Code is.
#[cfg(target_os = "macos")]
fn readable_perl(dir: &Path) -> PathBuf {
    let perl = dir.join("perl");
    std::fs::copy("/usr/bin/perl", &perl).unwrap();
    let signed = std::process::Command::new("codesign")
        .args(["--sign", "-", "--force"])
        .arg(&perl)
        .output()
        .unwrap();
    assert!(signed.status.success(), "{signed:?}");
    perl
}

#[cfg(target_os = "linux")]
fn readable_perl(_: &Path) -> PathBuf {
    PathBuf::from("/usr/bin/perl")
}

/// A wrapper named `claude`, as `PATH` finds it: `--a` or `--b` chooses for one call, otherwise the
/// choice in `DEFAULT` holds, and `b` sets a provider variable that `a` removes. `--tag VALUE` is
/// passed to the agent as `WRAPPER_TAG`. It declares the resolved choice and `exec`s the agent.
const WRAPPER: &str = r#"#!/usr/bin/perl
use JSON::PP;
open(my $f, '<', 'DEFAULT') or die $!;
my $choice = <$f>;
my @tag;
while (@ARGV) {
    if ($ARGV[0] eq '--a' || $ARGV[0] eq '--b') { $choice = substr(shift @ARGV, 2) }
    elsif ($ARGV[0] eq '--tag') { shift @ARGV; @tag = ('--tag', shift @ARGV) }
    else { last }
}
if ($choice eq 'b') { $ENV{CLAUDE_CODE_USE_VERTEX} = '1' } else { delete $ENV{CLAUDE_CODE_USE_VERTEX} }
$ENV{WRAPPER_TAG} = $tag[1] if @tag;
$ENV{REXUME_LAUNCH_PID} = $$;
$ENV{REXUME_LAUNCH} = encode_json(['claude', "--$choice", @tag]);
exec('PERL', 'AGENT', @ARGV) or die $!;
"#;

/// The agent: it names its conversation by `--resume`, or by `CONVERSATION` when started anew,
/// records what reached it in `RUN.seen`, reports itself as its session-start hook would, and
/// keeps running.
const AGENT: &str = r#"my $id = $ENV{CONVERSATION};
for my $i (0 .. $#ARGV - 1) { $id = $ARGV[$i + 1] if $ARGV[$i] eq '--resume' }
open(my $f, '>', "$run.seen") or die $!;
print $f join("\n", $id, $ENV{CLAUDE_CODE_USE_VERTEX} // 'unset', $ENV{WRAPPER_TAG} // 'none',
  $ENV{REXUME_TASK_META} || 'none');
close $f;
open(my $r, '|-', 'REXUME_BIN', 'report', 'claude', '--state-dir', 'STATE') or die $!;
print $r qq({"session_id":"$id","transcript_path":"DIR/$id.jsonl"});
close $r;
sleep 600;"#;

/// The wrapper in `dir/bin` with its default `b`, and the directory of the agent's runs.
fn wrapped_agent(dir: &Path, state: &Path) -> (PathBuf, PathBuf) {
    let (native, runs) = claude::stand_in(
        &dir.join("native"),
        &AGENT
            .replace("REXUME_BIN", REXUME)
            .replace("STATE", &state.to_string_lossy())
            .replace("DIR", &dir.to_string_lossy()),
    );
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(dir.join("default"), "b").unwrap();
    let wrapper = bin.join("claude");
    std::fs::write(
        &wrapper,
        WRAPPER
            .replace("DEFAULT", &dir.join("default").to_string_lossy())
            .replace("PERL", &readable_perl(dir).to_string_lossy())
            .replace("AGENT", &native.join("claude").to_string_lossy()),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(rex_bin(), bin.join("rex")).unwrap();
    (bin, runs)
}

/// What reached the latest run of conversation `id` once it has reported itself: the provider
/// variable, the tag, and the task meta.
fn reached(state: &Path, runs: &Path, id: &str) -> (String, String, String) {
    wait_until("the run to report", WAIT, || {
        let (pid, seen) = std::fs::read_dir(runs)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                let pid: i64 = path
                    .file_name()?
                    .to_str()?
                    .strip_suffix(".seen")?
                    .parse()
                    .ok()?;
                let seen = std::fs::read_to_string(&path).ok()?;
                let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
                seen.starts_with(&format!("{id}\n"))
                    .then_some((modified, pid, seen))
            })
            .max()
            .map(|(_, pid, seen)| (pid, seen))?;
        query::<i64>(
            state,
            &format!(
                "SELECT 1 FROM occupant WHERE native_id = '{id}' AND pid = {pid} AND pid_name IS NOT NULL"
            ),
        )?;
        let mut lines = seen.lines().skip(1).map(str::to_owned);
        Some((lines.next()?, lines.next()?, lines.next()?))
    })
}

fn launch_command(state: &Path, id: &str) -> Value {
    let settings: String = query(
        state,
        &format!("SELECT settings FROM conversation WHERE native_id = '{id}'"),
    )
    .unwrap();
    serde_json::from_str::<Value>(&settings).unwrap()["launch_command"].clone()
}

/// Conversation `one` starts with `--a`, a tag of shell metacharacters, and a task meta while the
/// default is `b`; conversation `two` starts on the default `b`, which then becomes `a`, with the
/// task meta its shell exports cleared. Each restart reopens each with its own choice, the tag as
/// given, and its own task meta rather than the shell's, through the declared command: the plan
/// replays no provider variable, and the command does not grow from one restore to the next.
#[test]
#[ignore = "needs a Rex server"]
fn a_declared_launch_reopens_with_the_choice_made_at_launch() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (bin, runs) = wrapped_agent(dir.path(), &state);
    let tag = r#"x y;$(touch z)|&`"q'*"#;
    let meta = r#"{"eval": "it's 1"}"#;
    // A startup file of the interactive shell the gate reopens a conversation through.
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(format!(".{SHELL}rc")),
        r#"export REXUME_TASK_META='{"eval": "stale"}'"#,
    )
    .unwrap();
    let expected = [
        (
            "one",
            json!(["claude", "--a", "--tag", tag]),
            ("unset", tag, meta),
        ),
        ("two", json!(["claude", "--b"]), ("1", "none", "none")),
    ];
    for (id, ..) in &expected {
        std::fs::write(dir.path().join(format!("{id}.jsonl")), "").unwrap();
    }
    let mut server = Server::start(dir.path(), Some(&bin));
    let _daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let (session, blocks, _) = agents(&server, &state, dir.path());
    let typed = [
        format!(
            "REXUME_TASK_META={} CONVERSATION=one claude --a --tag {}",
            shlex::try_quote(meta).unwrap(),
            shlex::try_quote(tag).unwrap()
        ),
        "REXUME_TASK_META= CONVERSATION=two claude".to_owned(),
    ];
    for (block, line) in blocks.iter().zip(&typed) {
        server.call(
            "block.method",
            &json!({ "session_id": session, "block_id": block, "method": "write",
                     "args": { "data": format!("{line}\r") } }),
        );
    }
    let seen = |(provider, tag, meta): &(&str, &str, &str)| {
        ((*provider).into(), (*tag).into(), (*meta).into())
    };
    for (id, command, reaches) in &expected {
        let reached = reached(&state, &runs, id);
        assert_eq!(reached, seen(reaches), "{id}");
        assert_eq!(launch_command(&state, id), *command, "{id}");
    }
    std::fs::write(dir.path().join("default"), "a").unwrap();

    for restarts in 1..=2 {
        restart(&mut server, &state, &bin, restarts, false);
        wait_until("the gates to claim the restores", WAIT, || {
            Some(restores(&server, &state))
                .filter(|r| r.len() == 2 && r.iter().all(|(_, s, _)| s == "claimed"))
        });
        for (id, command, reaches) in &expected {
            let plan: String = db(&state)
                .query_row(
                    "SELECT plan FROM restore WHERE instance_id = ?1 AND native_id = ?2",
                    [server.instance(), (*id).to_owned()],
                    |row| row.get(0),
                )
                .unwrap();
            let plan: Value = serde_json::from_str(&plan).unwrap();
            let mut argv = command.as_array().unwrap().clone();
            argv.extend([json!("--resume"), json!(id)]);
            assert_eq!(plan["argv"], json!(argv), "{id} after {restarts}");
            let meta = if reaches.2 == "none" { "" } else { reaches.2 };
            assert_eq!(
                plan["env"],
                json!([["REXUME_TASK_META", meta]]),
                "{id} after {restarts}"
            );
            let reached = reached(&state, &runs, id);
            assert_eq!(reached, seen(reaches), "{id} after {restarts}");
            assert_eq!(
                launch_command(&state, id),
                *command,
                "{id} after {restarts}"
            );
        }
    }
}
