use std::process::Stdio;

use serde_json::Value;

use super::orchestration::Orchestration;
use super::{DURATION, WAIT, append, failure, run_task, task, task_command};
use crate::common::{query, wait_until};

/// The owner stops waiting for a round the worker has not answered: a running `task wait` returns
/// with it and no notice is sent. The answer the worker gives later is unread, so the task is
/// released only after `task next` takes it.
#[test]
#[ignore = "needs a Rex server"]
fn an_abandoned_round_ends_a_wait_and_leaves_the_late_answer_unread() {
    let world = Orchestration::start();
    let Orchestration {
        server,
        state,
        owner,
        id,
        worker_record,
        listener,
        ..
    } = &world;

    let waiting = task_command(server, state, owner, &["wait", id, "--timeout", "20"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until("the wait to hold the round", WAIT, || {
        query::<i64>(
            state,
            "SELECT waiter_pid FROM task_round WHERE waiter_pid IS NOT NULL",
        )
    });
    let abandoned = task(server, state, owner, &["abandon", id]);
    assert_eq!(
        (abandoned["state"].as_str(), abandoned["acked_at"].is_i64()),
        (Some("abandoned"), true)
    );
    let waited = waiting.wait_with_output().unwrap();
    assert_eq!(waited.status.code(), Some(0));
    let waited: Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(waited["state"].as_str(), Some("abandoned"));
    let (code, stderr) = failure(&run_task(server, state, owner, &["abandon", id]));
    assert_eq!(code, Some(1));
    assert!(stderr.contains("already finished as abandoned"), "{stderr}");

    append(
        worker_record,
        r#"{"type":"user","message":{"role":"user","content":"Review the diff"}}"#,
    );
    append(
        worker_record,
        r#"{"type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Late."}]}}"#,
    );
    append(worker_record, DURATION);
    wait_until("the late answer to be unread", WAIT, || {
        task(server, state, owner, &["show", id])["unread"]
            .as_u64()
            .filter(|unread| *unread == 1)
    });
    let (code, stderr) = failure(&run_task(server, state, owner, &["release", id]));
    assert_eq!(code, Some(1));
    assert!(stderr.contains("unread turns (1)"), "{stderr}");
    assert_eq!(task(server, state, owner, &["next", id])["round"], 2);
    let next = task(server, state, owner, &["wait", id, "--timeout", "20"]);
    assert_eq!(next["result"].as_str(), Some("Late."));
    assert_eq!(
        query::<String>(state, "SELECT note FROM notice WHERE round = 1").as_deref(),
        Some("taken by task abandon")
    );
    assert!(listener.accept().is_err(), "a notice was sent to the owner");
}
