//! `task wait`: the owner taking a round's result in its own turn.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Transaction, params};
use serde_json::json;

use super::{caller, finished, owned, round_state, shown};
use crate::event::Queue;
use crate::proc;
use crate::store::Store;

/// Waits in the owner's turn for the task's latest round to finish and returns that round, with
/// `timed_out`. While a live `task wait` is registered, the daemon leaves the round's notice to
/// it; a second `task wait` on the same round only waits.
pub fn wait(state_dir: &Path, id: &str, timeout: Duration) -> Result<serde_json::Value> {
    let until = Instant::now() + timeout;
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    let (round, _) = round_state(store.conn(), id, None)?;
    let pid = i64::from(std::process::id());
    let waiter: Option<i64> = store.conn().query_row(
        "SELECT waiter_pid FROM task_round WHERE task_id = ?1 AND round = ?2",
        params![id, round],
        |row| row.get(0),
    )?;
    let stale = waiter.filter(|other| !proc::alive(*other));
    let registered = waiter.is_none() || stale.is_some();
    let registered = registered && {
        let tx = store.write()?;
        let registered = register_waiter(&tx, id, round, pid, stale)?;
        tx.commit()?;
        registered
    };
    // Every commit writes the write-ahead log, so a change to it is when the round may have moved.
    let wal = state_dir.join("state.db-wal");
    let mut queue = Queue::new()?;
    queue.file(&wal, 0)?;
    let mut done = loop {
        // Also before the first look, as a commit that wrote the log before the watch existed
        // fires nothing.
        store.settle()?;
        let (_, state) = round_state(store.conn(), id, Some(round))?;
        if finished(&state) || Instant::now() >= until {
            break finished(&state);
        }
        let fired = queue.wait(Some(until.saturating_duration_since(Instant::now())))?;
        if fired.iter().any(|f| f.gone) {
            queue.file(&wal, 0)?;
        }
    };
    if registered {
        let tx = store.write()?;
        // The round can finish between the last look and this transaction.
        done = finished(&round_state(&tx, id, Some(round))?.1);
        if done {
            tx.execute(
                "UPDATE notice SET state = 'superseded', note = 'taken by task wait'
                 WHERE task_id = ?1 AND round = ?2 AND state = 'pending'",
                params![id, round],
            )?;
        }
        tx.execute(
            "UPDATE task_round SET waiter_pid = NULL
             WHERE task_id = ?1 AND round = ?2 AND waiter_pid = ?3",
            params![id, round, pid],
        )?;
        tx.commit()?;
    }
    let mut task = shown(store.conn(), &caller.instance, id, Some(round))?;
    task["timed_out"] = json!(!done);
    Ok(task)
}

/// Registers `pid` as the round's waiter unless another live waiter took it; `stale` is a dead
/// waiter seen before the transaction, which this one replaces.
pub(super) fn register_waiter(
    tx: &Transaction,
    id: &str,
    round: i64,
    pid: i64,
    stale: Option<i64>,
) -> Result<bool> {
    Ok(tx.execute(
        "UPDATE task_round SET waiter_pid = ?1
         WHERE task_id = ?2 AND round = ?3 AND (waiter_pid IS NULL OR waiter_pid = ?4)",
        params![pid, id, round, stale],
    )? == 1)
}
