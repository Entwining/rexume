use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;
use serde_json::json;

use super::{WAIT, restart, restores};
use crate::common::claude::{self, readable_perl};
use crate::common::{Daemon, REXUME, Server, db, query, shell_block, shell_pids, wait_until};

/// Reports its own command line to Rex with OSC 7577, as Rex's Claude Code hook does for Claude
/// Code, then itself to Rexume with a messaging socket, as Claude Code does, and keeps what
/// reaches its terminal in `runs/PID` and what reaches its socket in `sent/PID`.
const AGENT: &str = r#"use Digest::SHA qw(sha256_hex);
use IO::Socket::UNIX;
use MIME::Base64 qw(encode_base64);
use Socket qw(SOCK_STREAM);
$| = 1;
my $token = $ENV{OSC7577_TOKEN} or die "no OSC7577_TOKEN";
my $argv = join(',', map { encode_base64($_, '') } ($^X, $0, @ARGV));
my $body = "pid=$$:time=" . time() . ":argv=$argv";
print "\e]7577;$body:mac=" . sha256_hex("$body:$token") . "\e\\";
my $me = $$;
my $socket = "SOCKETS/$me";
my $listener = IO::Socket::UNIX->new(Type => SOCK_STREAM, Local => $socket, Listen => 5) or die $!;
$ENV{CLAUDE_CODE_MESSAGING_SOCKET} = $socket;
if (!fork) {
    while (my $c = $listener->accept) {
        local $/;
        my $got = <$c>;
        open(my $f, '>>', "SENT/$me") or die $!;
        print $f $got;
        close $f;
    }
    exit;
}
open(my $r, '|-', 'REXUME', 'report', 'claude', '--state-dir', 'STATE') or die $!;
print $r 'HOOK';
close $r;
open(my $f, '>', "RUNS/$$") or die $!;
close $f;
while (sysread(STDIN, my $got, 4096)) {
    open(my $f, '>>', "RUNS/$$") or die $!;
    print $f $got;
    close $f;
}
"#;

const ID: &str = "0b1e5e2a-7c43-4f5e-9d1a-3f0c2b9e8d71";

fn runs(dir: &Path) -> Vec<(i64, String)> {
    let mut runs: Vec<(i64, String)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            let pid = path.file_name().unwrap().to_str().unwrap().parse().unwrap();
            (pid, std::fs::read_to_string(path).unwrap())
        })
        .collect();
    runs.sort();
    runs
}

/// The pane's occupant on the server's current instance: its `pid`, `agent_pid`, and whether
/// `pid` is the pane's recorded foreground.
fn occupant(server: &Server, state: &Path) -> Option<(i64, Option<i64>, bool)> {
    crate::common::db(state)
        .query_row(
            "SELECT o.pid, o.agent_pid, o.pid IS b.fg_pid FROM occupant o
               JOIN block b ON b.lineage = o.block_lineage
             WHERE o.instance_id = ?1",
            [server.instance()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .unwrap_or_else(|e| panic!("the occupant: {e}"))
}

/// A server that trusts the agent's command for Program Resume, with a daemon and a pane where the
/// agent reported.
struct Launched {
    dir: PathBuf,
    state: PathBuf,
    server: Server,
    daemon: Daemon,
    session: String,
    block: String,
    first: i64,
}

fn launch(dir: &Path) -> Launched {
    let dir = dir.canonicalize().unwrap();
    let state = dir.join("state");
    for sub in ["runs", "sent", "sockets"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let transcript = dir.join("story.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let perl = readable_perl(&dir);
    let hook = json!({ "hook_event_name": "SessionStart", "session_id": ID,
        "transcript_path": transcript });
    let agent = dir.join("agent");
    let path = |sub: &str| dir.join(sub).to_string_lossy().into_owned();
    std::fs::write(
        &agent,
        AGENT
            .replace("REXUME", REXUME)
            .replace("STATE", &state.to_string_lossy())
            .replace("HOOK", &hook.to_string())
            .replace("RUNS", &path("runs"))
            .replace("SENT", &path("sent"))
            .replace("SOCKETS", &path("sockets")),
    )
    .unwrap();
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "rex.set(\"resume.trust.agent\", {{ command = {{ \"{}\", rex.match.under(\"{}\"), \"--resume\", rex.match.uuid }} }})\n",
            perl.display(),
            dir.display()
        ),
    )
    .unwrap();

    let server = Server::start(&dir, None);
    let revision = server.call("server.status", &json!({}))["api_revision"].as_i64();
    assert!(revision >= Some(24), "a server without Program Resume");
    let daemon = Daemon::start(&server, &state);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let created = server.call(
        "session.create",
        &json!({ "label": "agents", "initial_windows": [{ "layout": shell_block("agent", &dir) }] }),
    );
    let session = created["session_id"].as_str().unwrap().to_owned();
    let block = created["initial_windows"][0]["block_ids"][0]
        .as_str()
        .unwrap()
        .to_owned();
    shell_pids(&server, &state, &session, &[&block]);
    let command = format!("{} {} --resume {ID}\n", perl.display(), agent.display());
    let typed = server
        .cli()
        .args(["send", "-s", &session, "-b", &block, &command])
        .status()
        .unwrap();
    assert!(typed.success());
    let (first, _, _) = wait_until("the agent to report", WAIT, || {
        occupant(&server, &state).filter(|(_, agent, foreground)| agent.is_none() && *foreground)
    });
    let records = server.call(
        "block.method",
        &json!({ "session_id": session, "block_id": block, "method": "program_resume_records" }),
    );
    assert_eq!(records["records"][0]["trusted"], true, "{records}");
    Launched {
        dir,
        state,
        server,
        daemon,
        session,
        block,
        first,
    }
}

/// Rex names as the foreground either the program or the login shell it started it in, which
/// Rexume then records as the occupant with the agent beside it. The report can end the restore
/// before the daemon reads the pane's new foreground.
fn resumed(server: &Server, state: &Path, first: i64) -> i64 {
    let (shown, agent, _) = wait_until("the occupant to be the foreground", WAIT, || {
        occupant(server, state).filter(|(_, _, foreground)| *foreground)
    });
    let running = agent.unwrap_or(shown);
    assert_ne!(running, first);
    running
}

#[test]
#[ignore = "needs a Rex server"]
fn a_conversation_rex_resumed_gets_no_line_and_stays_tracked_until_it_exits() {
    let dir = tempfile::tempdir().unwrap();
    let Launched {
        dir,
        state,
        mut server,
        daemon,
        session,
        block,
        first,
    } = launch(dir.path());

    restart(&mut server, &state, &dir, 1, false);
    let ended = wait_until("the restore to end", WAIT, || {
        query::<String>(
            &state,
            "SELECT state || ': ' || IFNULL(note, '-') FROM restore
             WHERE typed_at IS NULL AND state != 'planned'",
        )
    });
    assert_eq!(ended, "opened: -");
    let running = resumed(&server, &state, first);
    std::thread::sleep(std::time::Duration::from_secs(2));
    let read = runs(&dir.join("runs"));
    assert_eq!(read.len(), 2);
    assert!(read.iter().all(|(_, got)| got.is_empty()), "{read:?}");
    assert!(runs(&dir.join("sent")).is_empty());

    let interrupted = server
        .cli()
        .args(["send-key", "-s", &session, "-b", &block, "ctrl+c"])
        .status()
        .unwrap();
    assert!(interrupted.success());
    wait_until("the program's exit to count", WAIT, || {
        occupant(&server, &state).filter(|o| *o == (running, None, false))
    });

    restart(&mut server, &state, &dir, 2, false);
    wait_until("the third epoch", WAIT, || {
        query::<i64>(&state, "SELECT COUNT(*) FROM epoch").filter(|n| *n == 3)
    });
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(restores(&server, &state).is_empty());
    assert_eq!(runs(&dir.join("runs")).len(), 2);
    drop(daemon);
}

#[test]
#[ignore = "needs a Rex server"]
fn a_turn_cut_off_in_a_conversation_rex_resumed_continues_once_through_its_socket() {
    let dir = tempfile::tempdir().unwrap();
    let Launched {
        dir,
        state,
        mut server,
        daemon,
        block,
        first,
        ..
    } = launch(dir.path());
    claude::turn(&state, ID, "p1", "Write a story", None);
    db(&state)
        .execute(
            "UPDATE block SET status = 'working', status_pid = fg_pid,
               status_name = (SELECT pid_name FROM occupant WHERE block_lineage = ?1)
             WHERE lineage = ?1",
            [&block],
        )
        .unwrap();

    restart(&mut server, &state, &dir, 1, true);
    let ended = wait_until("the continuation to be sent", WAIT, || {
        query::<String>(
            &state,
            "SELECT state || ' ' || start || ' ' || continuation_state FROM restore
             WHERE continuation_state NOT IN ('pending', 'sending')",
        )
    });
    assert_eq!(ended, "opened continue sent");
    let running = resumed(&server, &state, first);
    std::thread::sleep(std::time::Duration::from_secs(2));
    let sent = runs(&dir.join("sent"));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, running);
    assert_eq!(sent[0].1.lines().count(), 1, "{sent:?}");
    assert!(sent[0].1.contains("[rexume "), "{sent:?}");
    let read = runs(&dir.join("runs"));
    assert!(read.iter().all(|(_, got)| got.is_empty()), "{read:?}");
    drop(daemon);
}
