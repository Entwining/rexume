//! `task wait`: the owner taking a round's result, or learning that its worker is blocked, in its
//! own turn.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Transaction, params};
use serde_json::{Value, json};

use super::notice::{self, Kind};
use super::{caller, finished, owned, round_state, shown};
use crate::event::{Queue, Token};
use crate::proc;
use crate::store::Store;

/// Why a `task wait` returned, as decided in the transaction it returned from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Returned {
    Finished,
    /// It reports a blocked episode of the round's worker.
    Blocked,
    TimedOut,
}

/// Every commit writes the write-ahead log, so a change to it is when the round may have moved.
const WAL: Token = 0;
/// The exit of the waiter this one stands behind, which writes nothing to the log.
const HOLDER_EXIT: Token = 1;

/// Waits in the owner's turn for the task's latest round to finish or its worker to block, and
/// returns that round, with `timed_out`, and why it returned. The round is read after that
/// decision, so it may have moved on since.
pub fn wait(state_dir: &Path, id: &str, timeout: Duration) -> Result<(Value, Returned)> {
    let until = Instant::now() + timeout;
    let mut store = Store::open(state_dir)?;
    let caller = caller(&store)?;
    owned(store.conn(), id, &caller)?;
    let (round, _) = round_state(store.conn(), id, None)?;
    let returned = wait_for(
        &mut store,
        &state_dir.join("state.db-wal"),
        id,
        round,
        until,
    )?;
    let mut task = shown(store.conn(), &caller.instance, id, Some(round))?;
    task["timed_out"] = json!(returned == Returned::TimedOut);
    Ok((task, returned))
}

/// Waits until `until` for the round to finish or for an episode to report, standing towards the
/// round as each look decides. `wal` is the database's write-ahead log.
pub(super) fn wait_for(
    store: &mut Store,
    wal: &Path,
    id: &str,
    round: i64,
    until: Instant,
) -> Result<Returned> {
    let pid = i64::from(std::process::id());
    let mut queue = Queue::new()?;
    queue.file(wal, WAL)?;
    let mut waiter = None;
    loop {
        // Whether the holder lives is read from `/proc` on Linux, so it is checked before the
        // write lock, and the transaction replaces only the holder found gone.
        let seen: Option<i64> = store.conn().query_row(
            "SELECT waiter_pid FROM task_round WHERE task_id = ?1 AND round = ?2",
            params![id, round],
            |row| row.get(0),
        )?;
        let gone = seen.filter(|holder| *holder != pid && !proc::alive(*holder));
        // Also before the first look, as a commit that wrote the log before the watch existed
        // fires nothing. Taking the write lock settles the store, as `Store::settle` does.
        let tx = store.write()?;
        let now = stand(&tx, id, round, pid, gone, waiter)?;
        let returned = match returns(&tx, id, round, now)? {
            None if Instant::now() >= until => Some(Returned::TimedOut),
            returned => returned,
        };
        if returned.is_some() && now == Waiter::Registered {
            tx.execute(
                "UPDATE task_round SET waiter_pid = NULL WHERE task_id = ?1 AND round = ?2",
                params![id, round],
            )?;
        }
        tx.commit()?;
        if let Some(returned) = returned {
            return Ok(returned);
        }
        let watched = match waiter {
            Some(Waiter::Behind { holder, .. }) => Some(holder),
            _ => None,
        };
        waiter = Some(now);
        // A holder that exited before its watch fires nothing, so the next look is at once.
        if let Waiter::Behind { holder, .. } = now
            && watched != Some(holder)
            && !queue.exit(holder, HOLDER_EXIT)?
        {
            continue;
        }
        let fired = queue.wait(Some(until.saturating_duration_since(Instant::now())))?;
        if fired.iter().any(|f| f.gone) {
            queue.file(wal, WAL)?;
        }
    }
}

/// How a `task wait` stands towards its round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Waiter {
    /// The round's registered waiter, which takes the round's notices itself, so the daemon leaves
    /// them to it.
    Registered,
    /// Behind the waiter `holder`, not found gone, having seen `blocked_at` when it came to stand
    /// behind one.
    Behind {
        holder: i64,
        blocked_at: Option<i64>,
    },
}

/// Registers `pid` as the round's waiter when the round has none or still has `gone`, a holder
/// found exited before the transaction, and otherwise stands behind the holder it has, whose exit
/// or return wakes the wait. `before` is how it stood at its previous look.
pub(super) fn stand(
    tx: &Transaction,
    id: &str,
    round: i64,
    pid: i64,
    gone: Option<i64>,
    before: Option<Waiter>,
) -> Result<Waiter> {
    let (holder, blocked_at): (Option<i64>, Option<i64>) = tx.query_row(
        "SELECT waiter_pid, blocked_at FROM task_round WHERE task_id = ?1 AND round = ?2",
        params![id, round],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    match holder {
        Some(holder) if holder == pid => Ok(Waiter::Registered),
        Some(holder) if Some(holder) != gone => {
            let blocked_at = match before {
                Some(Waiter::Behind { blocked_at, .. }) => blocked_at,
                _ => blocked_at,
            };
            Ok(Waiter::Behind { holder, blocked_at })
        }
        _ => {
            tx.execute(
                "UPDATE task_round SET waiter_pid = ?1 WHERE task_id = ?2 AND round = ?3",
                params![pid, id, round],
            )?;
            Ok(Waiter::Registered)
        }
    }
}

/// Whether the wait returns, and why: the round finished, or the registered waiter took a blocked
/// notice still pending, which tells of an episode that nobody reported to the owner yet. A waiter
/// behind another returns on an episode that began after it came to stand behind it.
pub(super) fn returns(
    tx: &Transaction,
    id: &str,
    round: i64,
    waiter: Waiter,
) -> Result<Option<Returned>> {
    let (state, blocked_at): (String, Option<i64>) = tx.query_row(
        "SELECT state, blocked_at FROM task_round WHERE task_id = ?1 AND round = ?2",
        params![id, round],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let taken = "taken by task wait";
    Ok(match waiter {
        Waiter::Registered if finished(&state) => {
            notice::supersede(tx, Kind::Finished, id, round, taken)?;
            Some(Returned::Finished)
        }
        Waiter::Registered => {
            notice::supersede(tx, Kind::Blocked, id, round, taken)?.then_some(Returned::Blocked)
        }
        Waiter::Behind { .. } if finished(&state) => Some(Returned::Finished),
        Waiter::Behind {
            blocked_at: seen, ..
        } => (blocked_at.is_some() && blocked_at != seen).then_some(Returned::Blocked),
    })
}

#[cfg(test)]
mod tests;
