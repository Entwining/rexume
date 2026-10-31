//! Reopens conversations in the panes the server restored itself. Rex brings every session back
//! under its IDs with a login shell in each pane; for a pane whose record on the earlier instance
//! shows a conversation to reopen, the daemon types `rexume gate ID` into that shell, once, after
//! reading that the shell is the pane's foreground with nothing typed at its prompt. The gate
//! claims the pane's row before it starts anything, so the line starts the conversation at most
//! once however often it runs.

mod pane;
mod plan;
mod retired;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;

use crate::rex::{CallError, Rex, items, str_at, view_blocks};
use crate::store::{Store, ensure_lineage, live_of, now_ms};
use crate::task;
use pane::Pane;
pub use plan::{Plan, Start, plan};

/// How long a pane waits for the server's shell to be its foreground with nothing typed, from the
/// plan; past it, the pane is the user's and nothing is typed there.
const PANE_WAIT: Duration = Duration::from_secs(30);
/// How long a typed line waits for its gate's claim before the restore is left unknown.
const CLAIM_WAIT: Duration = Duration::from_secs(5);

/// The restores of instance `?1` whose line the daemon may still type into their pane, or whose
/// gate has not claimed them since the line was typed, oldest first.
pub(crate) const OPEN_RESTORES: &str = "SELECT id, block_lineage, typed_at, created_at FROM restore
  WHERE instance_id = ?1 AND state IN ('planned', 'typed') ORDER BY created_at";
/// A pane's recorded foreground, which a waiting restore compares with the one it last read.
pub(crate) const FOREGROUND: &str = "SELECT fg_pid, fg_generation FROM block WHERE lineage = ?1";

/// One restore that is planned or typed, as the database holds it.
struct Open {
    id: String,
    block: String,
    typed_at: Option<i64>,
    created_at: i64,
}

/// Drives the restores of one server instance from the daemon's event loop. Nothing here sleeps:
/// a pane that is not ready is read again when its recorded foreground changes or its wait ends,
/// and a typed line waits for its gate's claim until its deadline.
pub struct Restorer {
    /// The gate's command, to which each restore adds its ID.
    gate: Vec<String>,
    /// Whether a restore of this instance may still be planned or typed, or a retired worker's pane
    /// is held.
    open: bool,
    /// The recorded foreground (PID, generation) of each pane a restore waits for when it last
    /// read the pane, by pane.
    read: HashMap<String, Foreground>,
    /// Retired workers' panes held for a close.
    retired: Vec<retired::Retired>,
    deadline: Option<Instant>,
}

type Foreground = (Option<i64>, Option<i64>);

impl Restorer {
    pub fn new(gate: Vec<String>) -> Self {
        Self {
            gate,
            open: true,
            read: HashMap::new(),
            retired: Vec::new(),
            deadline: None,
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Binds the sessions and panes the server shows, plans a restore for each pane recorded on an
    /// earlier instance that has none yet on this one, and holds the panes of retired workers for a
    /// close. A pane an earlier epoch held, which someone may have used since, is let go first and
    /// held again only by this check. Run before the baseline moves the panes' records to this
    /// instance; the panes are read once the baseline has recorded them, by the next `reconcile`,
    /// since the baseline's own writes wake nothing.
    pub fn start(&mut self, rex: &mut Rex, store: &mut Store, instance: &str) -> Result<()> {
        {
            let tx = store.write()?;
            task::release_held(&tx)?;
            tx.commit()?;
        }
        let listed = rex.call("session.list", &json!({}))?;
        let mut panes = Vec::new();
        for session in items(&listed, "/sessions") {
            let Some(live) = str_at(session, "/session_id") else {
                continue;
            };
            let view = match rex.call("session.view", &json!({ "session_id": live })) {
                Ok(view) => view,
                Err(e) if e.not_found() => continue,
                Err(e) => return Err(e.into()),
            };
            let label = str_at(session, "/label").map(str::to_owned);
            let tx = store.write()?;
            ensure_lineage(&tx, instance, live)?;
            for block in view_blocks(&view) {
                let lineage = ensure_lineage(&tx, instance, &block)?;
                let earlier: Option<String> = tx
                    .query_row(
                        "SELECT instance_id FROM block WHERE lineage = ?1",
                        params![lineage],
                        |row| row.get(0),
                    )
                    .optional()?;
                if earlier.is_some_and(|i| i != instance) {
                    panes.push((lineage, label.clone()));
                }
            }
            tx.commit()?;
        }
        let retired = task::retired_workers(store.conn())?;
        for (block, label) in panes {
            if let Some((task, _)) = retired.iter().find(|(_, worker)| *worker == block)
                && task::hold(store, task)?
            {
                continue;
            }
            if let Err(e) = plan(store, instance, &block, label.as_deref()) {
                log::error!("pane {block}: failed to plan its restore: {e:#}");
            }
        }
        self.retired = retired::held(store)?;
        Ok(())
    }

    /// Looks again at the restores of this instance that are planned or typed.
    pub fn reconcile(&mut self, rex: &mut Rex, store: &mut Store, instance: &str) {
        if !self.open {
            return;
        }
        if let Err(e) = self.advance(rex, store, instance) {
            log::error!("restores: {e:#}");
        }
    }

    fn advance(&mut self, rex: &mut Rex, store: &mut Store, instance: &str) -> Result<()> {
        let open: Vec<Open> = store
            .conn()
            .prepare(OPEN_RESTORES)?
            .query_map(params![instance], |row| {
                Ok(Open {
                    id: row.get(0)?,
                    block: row.get(1)?,
                    typed_at: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut due = self.close_retired(rex, store, instance);
        self.open = !open.is_empty() || !self.retired.is_empty();
        self.read.retain(|block, _| {
            open.iter().any(|o| &o.block == block) || self.retired.iter().any(|r| &r.block == block)
        });
        for restore in open {
            let result = match restore.typed_at {
                Some(typed_at) => claim_wait(store, &restore, typed_at),
                None => self.try_type(rex, store, instance, &restore),
            };
            match result {
                Ok(Some(at)) => due.push(at),
                Ok(None) => {}
                Err(e) => log::error!("restore {}: {e:#}", restore.id),
            }
        }
        let now = now_ms();
        self.deadline = due
            .into_iter()
            .min()
            .map(|at| Instant::now() + Duration::from_millis(u64::try_from(at - now).unwrap_or(0)));
        Ok(())
    }

    /// Types the gate's line into a planned restore's pane once the pane is ready for it, and
    /// returns when to look again.
    fn try_type(
        &mut self,
        rex: &mut Rex,
        store: &mut Store,
        instance: &str,
        restore: &Open,
    ) -> Result<Option<i64>> {
        let until = restore.created_at + millis(PANE_WAIT);
        let now = now_ms();
        if opened_elsewhere(store.conn(), &restore.id)? {
            let tx = store.write()?;
            tx.execute(
                "UPDATE restore SET state = 'skipped', note = ?1 WHERE id = ?2 AND state = 'planned'",
                params![OPENED_IN_ANOTHER_PANE, restore.id],
            )?;
            tx.commit()?;
            log::info!("restore {}: skipped: {OPENED_IN_ANOTHER_PANE}", restore.id);
            return Ok(None);
        }
        if self.unchanged(store, &restore.block, until, now)? {
            return Ok(Some(until));
        }
        let Some((session, live)) = located(rex, store, instance, &restore.block)? else {
            return skip(store, instance, restore, "the pane closed");
        };
        match pane::read(rex, &session, &live)? {
            Pane::Shell => {}
            Pane::Gone => return skip(store, instance, restore, "the pane closed"),
            Pane::Busy(reason) if now >= until => {
                return skip(store, instance, restore, &reason);
            }
            Pane::Busy(reason) => {
                let tx = store.write()?;
                tx.execute(
                    "UPDATE restore SET note = ?1 WHERE id = ?2 AND state = 'planned'",
                    params![reason, restore.id],
                )?;
                tx.commit()?;
                return Ok(Some(until));
            }
        };
        let mut gate = self.gate.clone();
        gate.push(restore.id.clone());
        let line = pane::line(&gate)?;
        let typed_at = now_ms();
        let tx = store.write()?;
        let marked = tx.execute(
            "UPDATE restore SET state = 'typed', typed_at = ?1, note = NULL
             WHERE id = ?2 AND state = 'planned'",
            params![typed_at, restore.id],
        )?;
        tx.commit()?;
        if marked == 0 {
            return Ok(None);
        }
        log::info!("restore {}: typing its gate into pane {live}", restore.id);
        match rex.block_method_with(&session, &live, "write", &json!({ "data": line })) {
            Ok(_) | Err(CallError::Unknown(_)) => Ok(Some(typed_at + millis(CLAIM_WAIT))),
            Err(e @ (CallError::Unsent(_) | CallError::Rejected { .. })) => {
                let tx = store.write()?;
                tx.execute(
                    "UPDATE restore SET state = 'failed', typed_at = NULL, note = ?1
                     WHERE id = ?2 AND state = 'typed'",
                    params![e.reason(), restore.id],
                )?;
                end_left(&tx, instance, restore)?;
                tx.commit()?;
                log::warn!("restore {}: the server refused the line", restore.id);
                Ok(None)
            }
        }
    }

    /// Whether a pane waited for shows the foreground it showed when last read, before `until`:
    /// reading it again could only show the same. Otherwise records the foreground read now.
    fn unchanged(&mut self, store: &Store, block: &str, until: i64, now: i64) -> Result<bool> {
        let foreground: Foreground = store
            .conn()
            .query_row(FOREGROUND, params![block], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?
            .unwrap_or_default();
        if now < until && self.read.get(block) == Some(&foreground) {
            return Ok(true);
        }
        self.read.insert(block.to_owned(), foreground);
        Ok(false)
    }
}

/// The session and live ID of a pane on this instance, or `None` when the server no longer shows
/// it.
fn located(
    rex: &mut Rex,
    store: &Store,
    instance: &str,
    block: &str,
) -> Result<Option<(String, String)>> {
    Ok(match live_of(store.conn(), instance, block)? {
        Some(live) => rex.block_session(&live)?.map(|session| (session, live)),
        None => None,
    })
}

/// A typed line whose gate has not claimed its restore yet: past its wait, the restore is left
/// unknown, which a gate that starts later still claims; nothing is typed again.
fn claim_wait(store: &mut Store, restore: &Open, typed_at: i64) -> Result<Option<i64>> {
    let until = typed_at + millis(CLAIM_WAIT);
    if now_ms() < until {
        return Ok(Some(until));
    }
    let tx = store.write()?;
    let unclaimed = tx.execute(
        "UPDATE restore SET state = 'unknown', note = ?1 WHERE id = ?2 AND state = 'typed'",
        params![
            "the line was typed into the pane, and its gate has not started",
            restore.id
        ],
    )?;
    tx.commit()?;
    if unclaimed == 1 {
        log::warn!("restore {}: unknown: no gate claimed it", restore.id);
    }
    Ok(None)
}

/// Whether restore `id`'s conversation reported from another pane since the plan, or from another
/// pane on the restore's instance, as when someone reopened it before the daemon planned: it runs
/// there, so this pane keeps its shell. A report in the same millisecond as the plan counts as
/// later: leaving the shell is the safe side.
pub fn opened_elsewhere(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row(OPENED_ELSEWHERE, params![id], |row| row.get(0))?)
}

pub(crate) const OPENED_ELSEWHERE: &str = "SELECT EXISTS (SELECT 1 FROM restore r JOIN occupant o
  ON o.runtime = r.runtime AND o.native_id = r.native_id
  WHERE r.id = ?1 AND o.block_lineage != r.block_lineage
    AND (o.reported_at >= r.created_at OR o.instance_id = r.instance_id))";

/// Why a restore whose conversation runs in another pane typed nothing. A round its pane's worker
/// left open goes on there, from the conversation's record.
const OPENED_IN_ANOTHER_PANE: &str = "the conversation was opened in another pane";

/// Skips a restore that typed nothing, and ends the rounds the earlier instance left open in its
/// pane, which nothing takes up again.
fn skip(store: &mut Store, instance: &str, restore: &Open, note: &str) -> Result<Option<i64>> {
    let tx = store.write()?;
    tx.execute(
        "UPDATE restore SET state = 'skipped', note = ?1 WHERE id = ?2 AND state = 'planned'",
        params![note, restore.id],
    )?;
    end_left(&tx, instance, restore)?;
    tx.commit()?;
    log::info!("restore {}: skipped: {note}", restore.id);
    Ok(None)
}

/// Ends the rounds the earlier instance left open in the pane of a restore that starts nothing,
/// and in each pane that showed its conversation there and left the conversation to this restore,
/// whose record would have taken them up.
fn end_left(tx: &Transaction, instance: &str, restore: &Open) -> Result<()> {
    task::end_left_to_server(tx, instance, &restore.block)?;
    let deferred: Vec<String> = tx
        .prepare(
            "SELECT o.block_lineage FROM restore r JOIN occupant o
               ON o.runtime = r.runtime AND o.native_id = r.native_id
             WHERE r.id = ?1 AND o.block_lineage != r.block_lineage AND o.instance_id IS NOT ?2
               AND NOT EXISTS (SELECT 1 FROM restore p
                               WHERE p.instance_id = ?2 AND p.block_lineage = o.block_lineage)",
        )?
        .query_map(params![restore.id, instance], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for block in deferred {
        task::end_left_to_server(tx, instance, &block)?;
    }
    Ok(())
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
