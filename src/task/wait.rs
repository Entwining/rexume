use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Transaction, params};
use serde_json::{Value, json};

use super::blocked::OPEN_EPISODE;
use super::notice::{self, Kind};
use super::{caller, finished, owned, round_state, shown, snapshot};
use crate::event::{Queue, Token};
use crate::proc;
use crate::store::{Store, now_ms};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Returned {
    Finished,
    Blocked,
    TimedOut,
}

const WAL: Token = 0;
/// A waiter that dies writes nothing to the log, so the one behind it watches its exit.
const HOLDER_EXIT: Token = 1;

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
    if returned == Returned::Finished {
        snapshot::after(&mut store, id)?;
    }
    let mut task = shown(store.conn(), store.dir(), &caller.instance, id, Some(round))?;
    task["timed_out"] = json!(returned == Returned::TimedOut);
    Ok((task, returned))
}

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
        // `proc::alive` reads `/proc` on Linux, so it runs before the write transaction, which
        // then replaces only the holder found gone here.
        let seen: Option<i64> = store.conn().query_row(
            "SELECT waiter_pid FROM task_round WHERE task_id = ?1 AND round = ?2",
            params![id, round],
            |row| row.get(0),
        )?;
        let gone = seen.filter(|holder| *holder != pid && !proc::alive(*holder));
        // The write lock settles the store as `Store::settle` does, which the first look needs
        // too: a commit that wrote the log before the watch existed fires nothing.
        let tx = store.write()?;
        let now = stand(&tx, id, round, pid, gone, waiter.as_ref())?;
        let returned = match returns(&tx, id, round, &now)? {
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
        let unwatched = match now {
            Waiter::Behind { holder, .. } if watched != Some(holder) => {
                !queue.exit(holder, HOLDER_EXIT)?
            }
            _ => false,
        };
        waiter = Some(now);
        if unwatched {
            continue;
        }
        let fired = queue.wait(Some(until.saturating_duration_since(Instant::now())))?;
        if fired.iter().any(|f| f.gone) {
            queue.file(wal, WAL)?;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Waiter {
    Registered,
    Behind {
        holder: i64,
        episode: Option<String>,
    },
}

static ROUND: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT r.waiter_pid, r.state,
           (SELECT e.id FROM blocked_episode e
            WHERE e.task_id = r.task_id AND e.round = r.round AND {OPEN_EPISODE})
         FROM task_round r WHERE r.task_id = ?1 AND r.round = ?2"
    )
});

fn round(tx: &Transaction, id: &str, round: i64) -> Result<(Option<i64>, String, Option<String>)> {
    Ok(tx.query_row(&ROUND, params![id, round], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?)
}

pub(super) fn stand(
    tx: &Transaction,
    id: &str,
    round: i64,
    pid: i64,
    gone: Option<i64>,
    before: Option<&Waiter>,
) -> Result<Waiter> {
    let (holder, _, episode) = self::round(tx, id, round)?;
    match holder {
        Some(holder) if holder == pid => Ok(Waiter::Registered),
        Some(holder) if Some(holder) != gone => {
            let episode = match before {
                Some(Waiter::Behind { episode, .. }) => episode.clone(),
                _ => episode,
            };
            Ok(Waiter::Behind { holder, episode })
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

pub(super) fn returns(
    tx: &Transaction,
    id: &str,
    round: i64,
    waiter: &Waiter,
) -> Result<Option<Returned>> {
    let (_, state, episode) = self::round(tx, id, round)?;
    let taken = "taken by task wait";
    let now = now_ms();
    Ok(match waiter {
        Waiter::Registered if finished(&state) => {
            notice::supersede(tx, Kind::Finished, id, round, taken, now)?;
            Some(Returned::Finished)
        }
        Waiter::Registered => notice::supersede(tx, Kind::Blocked, id, round, taken, now)?
            .then_some(Returned::Blocked),
        Waiter::Behind { .. } if finished(&state) => Some(Returned::Finished),
        Waiter::Behind { episode: seen, .. } => {
            (episode.is_some() && episode != *seen).then_some(Returned::Blocked)
        }
    })
}

#[cfg(test)]
mod tests;
