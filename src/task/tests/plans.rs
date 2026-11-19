use anyhow::Result;

use crate::restore;
use crate::store::Store;
use crate::task::follow::{OPEN_ROUNDS, TAKEN};
use crate::task::gone::WORKER_ROUNDS;
use crate::task::{blocked, cleanup, evidence, notice, outcome, queue, request, watch};

/// Release, a closed pane, and retention bound `task` and `notice`, so their scans are allowed.
#[test]
fn every_pass_statement_uses_an_index() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    let (open, finished, blocked, unread, episode) = (
        "task_round_open",
        "notice_finished",
        "notice_blocked_unsent",
        "notice_unread_pending",
        "blocked_episode_open",
    );
    let statements: [(&str, &str, &[&str]); 32] = [
        ("OPEN_ROUNDS", &OPEN_ROUNDS, &[open]),
        ("outcome::ROUND", &outcome::ROUND, &[]),
        ("evidence::PANE", &evidence::PANE, &[]),
        ("watch::WATCHED", &watch::WATCHED, &[open, episode]),
        ("WORKER_ROUNDS", &WORKER_ROUNDS, &[open]),
        ("blocked::BEGIN", &blocked::BEGIN, &[open]),
        ("blocked::RESUME", &blocked::RESUME, &[open]),
        ("blocked::END", &blocked::END, &[episode]),
        ("blocked::ON_PANE", &blocked::ON_PANE, &[episode]),
        ("cleanup::CLAIM", &cleanup::CLAIM, &[open]),
        ("cleanup::DUE", &cleanup::DUE, &[open]),
        ("notice::send::PENDING", &notice::send::PENDING, &[open]),
        ("notice::receipt::SENT", notice::receipt::SENT, &[]),
        ("notice::send::READY_OWNER", notice::send::READY_OWNER, &[]),
        ("notice::send::CLAIM", &notice::send::CLAIM, &[open]),
        (
            "notice::send::STILL_WANTED",
            &notice::send::STILL_WANTED,
            &[open],
        ),
        (
            "notice::send::REPORTED_END",
            notice::send::REPORTED_END,
            &[],
        ),
        (
            "notice::SUPERSEDE_FINISHED",
            &notice::SUPERSEDE_FINISHED,
            &[finished],
        ),
        (
            "notice::SUPERSEDE_BLOCKED",
            &notice::SUPERSEDE_BLOCKED,
            &[blocked],
        ),
        (
            "notice::SUPERSEDE_UNREAD",
            &notice::SUPERSEDE_UNREAD,
            &[unread],
        ),
        ("TAKEN", TAKEN, &[]),
        ("queue::STANDING", &queue::STANDING, &[open]),
        ("queue::START_NEXT", &queue::START_NEXT, &[episode]),
        ("queue::QUEUED_ROUNDS", queue::QUEUED_ROUNDS, &[]),
        ("request::PENDING", &request::PENDING, &[open]),
        ("request::WORKER", request::WORKER, &[]),
        ("request::CLAIM", &request::CLAIM, &[]),
        ("request::ROUND_OPEN", &request::ROUND_OPEN, &[]),
        ("request::SENT", &request::SENT, &[open]),
        ("restore::OPEN_RESTORES", restore::OPEN_RESTORES, &[]),
        ("restore::OPENED_ELSEWHERE", restore::OPENED_ELSEWHERE, &[]),
        ("restore::FOREGROUND", restore::FOREGROUND, &[]),
    ];
    for (name, sql, partial) in statements {
        let mut plan = store.conn().prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let mut rows = plan.raw_query();
        let mut details = Vec::new();
        while let Some(row) = rows.next()? {
            details.push(row.get::<_, String>(3)?);
        }
        for detail in &details {
            let Some(scanned) = detail
                .strip_prefix("SCAN ")
                .filter(|scanned| *scanned != "CONSTANT ROW")
            else {
                continue;
            };
            let table = scanned.split(' ').next().unwrap_or_default();
            assert!(
                detail.contains("task_round_open") || ["t", "task", "n", "notice"].contains(&table),
                "{name}: {detail}"
            );
        }
        for index in [open, finished, blocked, unread, episode] {
            assert_eq!(
                details.iter().any(|d| d.contains(index)),
                partial.contains(&index),
                "{name}: {details:?}"
            );
        }
    }
    Ok(())
}
