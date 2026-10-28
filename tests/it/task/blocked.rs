//! A worker that blocks on a dialog or a question: its owner learns of each episode once, in a
//! `task wait` or through a notice, and still gets the round's result.

use std::io::Read as _;
use std::process::Stdio;
use std::time::Instant;

use super::*;

/// Has the worker's shell report a program status, as an agent showing a dialog does.
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
        worker_record,
        listener,
        ..
    } = &world;

    // The owner is inside `task wait` when the worker blocks.
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
    // That episode has been reported, so the next wait waits for what comes after it.
    let again = run_task(server, state, owner, &["wait", id, "--timeout", "2"]);
    assert_eq!(failure(&again), (Some(124), String::new()));

    // The worker gets past the dialog and blocks again while its owner is idle.
    report(&world, "state=working");
    wait_until("the round to run again", WAIT, || {
        query::<String>(state, "SELECT state FROM task_round").filter(|s| s == "running")
    });
    report(&world, "state=blocked:kind=permission");
    let text = message(listener, "the blocked notice");
    assert!(
        text.contains(&format!(
            "Worker task {id} \"review\" is blocked in round 1"
        )) && text.contains(worker.as_str())
            && !text.contains("task ack"),
        "{text}"
    );

    append(
        worker_record,
        r#"{"type":"user","message":{"role":"user","content":"Review the diff"}}"#,
    );
    append(
        worker_record,
        r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Looks good."}]}}"#,
    );
    append(worker_record, DURATION);
    // The next message is the finished notice, so the blocked one went once.
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
