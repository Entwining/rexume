use super::orchestration::Orchestration;
use super::{message, task};
use crate::common::claude;

#[test]
#[ignore = "needs a Rex server"]
fn a_request_sent_during_a_round_reaches_the_worker_after_it_finishes() {
    let world = Orchestration::start();
    let Orchestration {
        server,
        state,
        owner,
        id,
        worker_listener,
        ..
    } = &world;

    let sent = task(
        server,
        state,
        owner,
        &["send", id, "--message", "Fix the nit"],
    );
    assert_eq!(
        (sent["round"].as_i64(), sent["state"].as_str()),
        (Some(2), Some("queued"))
    );
    let request = sent["request"]["id"].as_str().unwrap();

    claude::turn(
        state,
        "worker",
        "p1",
        "Review the diff",
        Some("Looks good."),
    );
    let text = message(worker_listener, "the queued request");
    assert_eq!(text, format!("[rexume {request}] Fix the nit"));
    claude::turn(state, "worker", "p2", &text, Some("Fixed."));
    let waited = task(server, state, owner, &["wait", id, "--timeout", "20"]);
    assert_eq!(
        (waited["round"].as_i64(), waited["result"].as_str()),
        (Some(2), Some("Fixed."))
    );
    task(server, state, owner, &["release", id]);
    let first = task(server, state, owner, &["show", id, "--round", "1"]);
    assert!(first["acked_at"].is_i64());
}
