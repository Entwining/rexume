use std::io::Read as _;
use std::process::Stdio;
use std::time::Instant;

use super::*;

/// OSC 7501 reports a program status to Rex, as an agent showing a dialog does.
fn report(world: &Orchestration, pairs: &str) {
    let status = world
        .server
        .cli()
        .args(["send", "-s", &world.session, "-b", &world.worker])
        .arg(format!("printf '\\033]7501;{pairs}\\033\\\\'\n"))
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "needs a Rex server"]
fn a_blocked_worker_is_reported_once_per_episode_and_its_result_still_arrives() {
    let world = Orchestration::start();
    let Orchestration {
        server,
        state,
        owner,
        worker,
        id,
        listener,
        ..
    } = &world;

    let mut waiting = task_command(server, state, owner, &["wait", id, "--timeout", "20"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until("the wait to register", WAIT, || {
        query::<i64>(
            state,
            "SELECT waiter_pid FROM task_round WHERE waiter_pid IS NOT NULL",
        )
    });
    let reported = Instant::now();
    report(&world, "state=blocked:kind=question");
    let exited = wait_until("the wait to return", WAIT, || waiting.try_wait().unwrap());
    assert!(
        reported.elapsed() < Duration::from_secs(3),
        "{:?}",
        reported.elapsed()
    );
    assert_eq!(exited.code(), Some(3));
    let mut printed = String::new();
    waiting
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut printed)
        .unwrap();
    let printed: Value = serde_json::from_str(&printed).unwrap();
    assert_eq!(
        (printed["state"].as_str(), printed["timed_out"].as_bool()),
        (Some("blocked"), Some(false))
    );
    let again = run_task(server, state, owner, &["wait", id, "--timeout", "2"]);
    assert_eq!(failure(&again), (Some(124), String::new()));

    report(&world, "state=working");
    wait_until("the round to run again", WAIT, || {
        query::<String>(state, "SELECT state FROM task_round").filter(|s| s == "running")
    });
    report(&world, "state=blocked:kind=permission");
    let text = message(listener, "the blocked notice");
    assert!(
        text.contains("Worker task \"review\" is blocked in round 1")
            && text.contains(worker.as_str())
            && !text.contains("task ack"),
        "{text}"
    );

    claude::turn(
        state,
        "worker",
        "p1",
        "Review the diff",
        Some("Looks good."),
    );
    let text = message(listener, "the finished notice");
    assert!(text.contains("finished round 1"), "{text}");
    let conn = db(state);
    let blocked: Vec<(String, Option<String>)> = conn
        .prepare("SELECT state, note FROM notice WHERE kind = 'blocked' ORDER BY rowid")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        blocked,
        [
            ("superseded".into(), Some("taken by task wait".into())),
            ("sent".into(), None)
        ]
    );
}

#[test]
#[ignore = "needs a Rex server"]
fn a_worker_between_rounds_is_watched_until_its_task_is_released() {
    let world = Orchestration::start();
    let Orchestration {
        server,
        state,
        owner,
        id,
        listener,
        ..
    } = &world;
    claude::turn(
        state,
        "worker",
        "p1",
        "Review the diff",
        Some("Looks good."),
    );
    let text = message(listener, "the finished notice");
    assert!(text.contains("task release"), "{text}");
    task(server, state, owner, &["ack", id]);

    report(&world, "state=blocked:kind=question");
    let text = message(listener, "the blocked notice");
    assert!(
        text.contains("is blocked") && text.contains("with no round open"),
        "{text}"
    );
    let shown = task(server, state, owner, &["show", id]);
    assert_eq!(shown["worker_blocked"]["kind"], "question");
    assert_eq!(shown["blocked"], Value::Null);

    report(&world, "state=working");
    claude::turn(state, "worker", "p2", "What changed?", Some("Two files."));
    let text = message(listener, "the unread notice");
    assert!(text.contains("has 1 unread turn"), "{text}");
    let listed = task(server, state, owner, &["list"]);
    assert_eq!(listed["version"], 2);

    report(&world, "state=blocked:kind=permission");
    let text = message(listener, "the second blocked notice");
    assert!(text.contains("The worker has 1 unread turn"), "{text}");
    let next = task(server, state, owner, &["next", id]);
    assert_eq!(
        next["state"], "blocked",
        "a round started during an episode"
    );
    let waited = task(server, state, owner, &["wait", id, "--timeout", "20"]);
    assert_eq!(waited["result"], "Two files.", "{waited}");
    task(server, state, owner, &["ack", id]);
    task(server, state, owner, &["release", id]);
    let reason = wait_until("the episode to end", WAIT, || {
        query::<String>(
            state,
            "SELECT end_reason FROM blocked_episode WHERE ended_at IS NOT NULL AND kind = 'permission'",
        )
    });
    assert_eq!(reason, "task_released");
}
