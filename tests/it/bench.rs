//! Measurements, not assertions: run with
//! `cargo test --release --test it bench -- --ignored --nocapture` and read the JSON it prints. `REXUME_BENCH_GOGC` sets `GOGC` for the daemon's `rex do`.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::common::{Daemon, Server, query, shell_block, wait_until};

const SESSIONS: usize = 20;
const WAIT: Duration = Duration::from_secs(60);

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A process's children, by pid and command name.
fn children(pid: u32) -> Vec<(u32, String)> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let child: u32 = fields.next()?.parse().ok()?;
            let parent: u32 = fields.next()?.parse().ok()?;
            (parent == pid).then(|| (child, fields.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

/// A command's stdout; a measurement the command could not make fails the bench.
fn output(program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Resident memory in KiB and CPU time in seconds.
fn usage(pid: u32) -> (u64, f64) {
    let text = output("ps", &["-o", "rss=,time=", "-p", &pid.to_string()]);
    let fields: Vec<&str> = text.split_whitespace().collect();
    let [rss, time] = fields[..] else {
        panic!("ps -p {pid}: {text}");
    };
    let seconds = time
        .rsplit(':')
        .zip([1.0, 60.0, 3600.0])
        .map(|(part, unit)| part.parse::<f64>().unwrap() * unit)
        .sum();
    (rss.parse().unwrap(), seconds)
}

/// The physical footprint in KiB, which `top` shows as MEM: the memory the kernel charges to the
/// process, including compressed pages that resident memory leaves out.
fn footprint(pid: u32) -> u64 {
    let pid = pid.to_string();
    let text = output("top", &["-l", "1", "-pid", &pid, "-stats", "pid,mem"]);
    let mem = text
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == pid).then(|| fields.next())?
        })
        .unwrap_or_else(|| panic!("top -pid {pid}: {text}"));
    let digits = mem.trim_end_matches(['+', '-']);
    let (number, unit) = digits.split_at(digits.len() - 1);
    let kib = match unit {
        "B" => 1.0 / 1024.0,
        "K" => 1.0,
        "M" => 1024.0,
        "G" => 1024.0 * 1024.0,
        _ => panic!("top -pid {pid}: unknown size {mem}"),
    };
    (number.parse::<f64>().unwrap() * kib) as u64
}

/// Resident memory and physical footprint of each process in KiB, the median of three samples
/// a second apart.
fn memory(pids: &[u32]) -> Vec<(u64, u64)> {
    let mut samples: Vec<Vec<(u64, u64)>> = vec![Vec::new(); pids.len()];
    for round in 0..3 {
        if round > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        for (pid, samples) in pids.iter().zip(&mut samples) {
            samples.push((usage(*pid).0, footprint(*pid)));
        }
    }
    samples
        .into_iter()
        .map(|samples| {
            let median = |mut values: Vec<u64>| {
                values.sort_unstable();
                values[1]
            };
            (
                median(samples.iter().map(|s| s.0).collect()),
                median(samples.iter().map(|s| s.1).collect()),
            )
        })
        .collect()
}

/// The idle wakeups `top` counts for a process over `window`.
fn idle_wakeups(pid: u32, window: Duration) -> Option<i64> {
    let output = Command::new("top")
        .args([
            "-l",
            "2",
            "-s",
            &window.as_secs().to_string(),
            "-pid",
            &pid.to_string(),
            "-stats",
            "pid,idlew",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let samples: Vec<i64> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == pid.to_string())
                .then(|| fields.next()?.trim_end_matches('+').parse().ok())?
        })
        .collect();
    samples.last().copied()
}

fn calls(daemon: &Daemon, method: &str) -> usize {
    daemon.log().matches(&format!("call {method}")).count()
}

#[test]
#[ignore = "measurement; needs a Rex server"]
#[expect(
    clippy::too_many_lines,
    reason = "a measurement reads as one fixture, its steps, and its report"
)]
fn bench() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut server = Server::start(dir.path(), None);
    let gogc = std::env::var("REXUME_BENCH_GOGC").ok();
    let mut vars = vec![("RUST_LOG", "rexume=debug")];
    if let Some(gogc) = &gogc {
        vars.push(("GOGC", gogc));
    }
    let daemon = Daemon::start_with(&server, &state, &vars);
    wait_until("the daemon's first epoch", WAIT, || {
        query::<i64>(&state, "SELECT id FROM epoch")
    });
    let mut sessions = Vec::new();
    for i in 0..SESSIONS {
        let cwd = dir.path().join(format!("s{i}"));
        std::fs::create_dir_all(&cwd).unwrap();
        let created = server.call(
            "session.create",
            &json!({ "label": format!("s{i}"), "initial_windows": [{ "layout": { "split": { "direction": "horizontal",
                "ratio": 0.5, "before": shell_block("a", &cwd), "after": { "split": { "direction": "vertical",
                "ratio": 0.5, "before": shell_block("b", &cwd), "after": shell_block("c", &cwd) } } } } }] }),
        );
        sessions.push(created["session_id"].as_str().unwrap().to_owned());
    }
    wait_until("every pane to be saved", WAIT, || {
        (query::<i64>(&state, "SELECT COUNT(*) FROM block WHERE cwd IS NOT NULL")
            == Some(3 * SESSIONS as i64)
            && query::<i64>(&state, "SELECT COUNT(*) FROM session") == Some(SESSIONS as i64))
        .then_some(())
    });
    std::thread::sleep(Duration::from_secs(2));

    let daemon_pid = daemon.pid();
    let adapter = daemon.adapter().unwrap();
    let relay = children(adapter)
        .first()
        .map(|(pid, _)| *pid)
        .expect("the relay");

    // Idle: CPU time and wakeups of the daemon over 30 s with nothing happening.
    let before = usage(daemon_pid).1;
    let wakeups = idle_wakeups(daemon_pid, Duration::from_secs(30));
    let idle_cpu = usage(daemon_pid).1 - before;
    let memory = memory(&[daemon_pid, adapter, relay]);

    // Title changes in a pane cost the daemon no call.
    let views_before = calls(&daemon, "");
    server
        .cli()
        .args(["send", "-s", &sessions[0], "-b"])
        .arg(server.call("session.view", &json!({ "session_id": sessions[0] }))["windows"][0]["layers"][0]["blocks"][0]["block_id"].as_str().unwrap())
        .arg("for i in $(seq 50); do printf '\\033]0;t%s\\007' $i; sleep 0.02; done\n")
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let title_calls = calls(&daemon, "") - views_before;

    // Renames: from the command until the saved layout holds the new name, and the daemon's children
    // meanwhile.
    let seen = Arc::new(Mutex::new(std::collections::BTreeSet::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (seen, stop) = (Arc::clone(&seen), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                seen.lock().unwrap().extend(children(daemon_pid));
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let views = calls(&daemon, "session.view");
    let mut latencies = Vec::new();
    for i in 0..20 {
        let started = now_ms();
        server
            .cli()
            .args(["session", "rename", &sessions[1], &format!("renamed{i}")])
            .status()
            .unwrap();
        // The saved document itself, not only its time: a read that ran before the server applied
        // the rename would save the old name.
        let renamed = format!(
            "SELECT updated_at FROM session WHERE json_extract(doc, '$.label') = 'renamed{i}'"
        );
        let saved = wait_until("the rename to be saved", WAIT, || {
            query::<i64>(&state, &renamed)
        });
        latencies.push(saved - started);
        std::thread::sleep(Duration::from_millis(400));
    }
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    let rename_views = calls(&daemon, "session.view") - views;
    latencies.sort_unstable();

    // Restore: from the new server's start until every session is complete and every pane's
    // directory is known again.
    server.kill();
    wait_until("the epoch to end", WAIT, || {
        query::<String>(
            &state,
            "SELECT end_reason FROM epoch WHERE end_reason IS NOT NULL",
        )
    });
    let started = Instant::now();
    server = Server::start(dir.path(), None);
    let instance = server.instance();
    wait_until("the restore", WAIT, || {
        (query::<i64>(&state, "SELECT COUNT(*) FROM restore_op WHERE state = 'completed'") == Some(SESSIONS as i64)
            && query::<i64>(&state, &format!("SELECT COUNT(*) FROM block WHERE instance_id = '{instance}' AND cwd IS NOT NULL"))
                == Some(3 * SESSIONS as i64))
        .then_some(())
    });
    let restore = started.elapsed();

    let totals = |measure: fn(&(u64, u64)) -> u64| {
        let [daemon, rex_do, relay] = [0, 1, 2].map(|i| measure(&memory[i]));
        json!({ "daemon": daemon, "rex_do": rex_do, "relay": relay, "total": daemon + rex_do + relay })
    };
    let report = json!({
        "gogc": gogc,
        "idle_30s": { "daemon_cpu_seconds": idle_cpu, "daemon_idle_wakeups": wakeups },
        "memory_kib": { "rss": totals(|m| m.0), "phys_footprint": totals(|m| m.1) },
        "title_changes_50_calls": title_calls,
        "rename_to_disk_ms": { "p50": latencies[latencies.len() / 2], "max": latencies.last(), "all": latencies },
        "rename_views_for_20": rename_views,
        "daemon_children_during_renames": seen.lock().unwrap().iter().map(|(pid, name)| format!("{pid} {name}")).collect::<Vec<_>>(),
        "restore_20_sessions_60_panes_ms": restore.as_millis(),
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
