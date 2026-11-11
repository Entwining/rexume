mod pane;
mod plan;
mod retired;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;

use crate::rex::{CallError, Rex, items, listed_blocks, str_at};
use crate::store::{Store, ensure_lineage, live_of, now_ms};
use crate::task;
use pane::Pane;
pub use plan::{Plan, Start, plan};

const PANE_WAIT: Duration = Duration::from_secs(30);
const CLAIM_WAIT: Duration = Duration::from_secs(5);

pub(crate) const OPEN_RESTORES: &str = "SELECT id, block_lineage, typed_at, created_at FROM restore
  WHERE instance_id = ?1 AND state IN ('planned', 'typed') ORDER BY created_at";
pub(crate) const FOREGROUND: &str = "SELECT fg_pid, fg_generation FROM block WHERE lineage = ?1";

struct Open {
    id: String,
    block: String,
    typed_at: Option<i64>,
    created_at: i64,
}

pub struct Restorer {
    gate: Vec<String>,
    open: bool,
    read: HashMap<String, Foreground>,
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

    /// Runs before the baseline moves the panes' `block` rows to this instance: a pane recorded on
    /// an earlier instance is found by its row's `instance_id`.
    pub fn start(&mut self, rex: &mut Rex, store: &mut Store, instance: &str) -> Result<()> {
        {
            let tx = store.write()?;
            // A pane an earlier epoch held may have been used since, so only the check below
            // holds it again.
            task::release_held(&tx)?;
            tx.commit()?;
        }
        let listed = rex.call("session.list", &json!({}))?;
        let mut panes = Vec::new();
        for session in items(&listed, "/sessions") {
            let Some(live) = str_at(session, "/session_id") else {
                continue;
            };
            let list = match rex.call("session.list_blocks", &json!({ "session_id": live })) {
                Ok(list) => list,
                Err(e) if e.not_found() => continue,
                Err(e) => return Err(e.into()),
            };
            let label = str_at(session, "/label").map(str::to_owned);
            let tx = store.write()?;
            ensure_lineage(&tx, instance, live)?;
            for block in listed_blocks(&list) {
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
        let retired = task::retired_workers(store.conn(), store.dir())?;
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
            // Unlike `skip`, this leaves the pane's open rounds: they go on from the conversation's
            // record in the other pane.
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
            Pane::Shell { .. } => {}
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

    /// Rex reports no change of a pane's screen, so a pane is read again only when its recorded
    /// foreground changes or at `until`.
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

/// Past the wait the restore is `unknown` rather than typed again: the line may have reached the
/// shell, and a gate that starts later still claims it.
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

pub fn opened_elsewhere(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row(OPENED_ELSEWHERE, params![id], |row| row.get(0))?)
}

/// `o.instance_id = r.instance_id` catches a conversation someone reopened on this instance
/// before the plan. A report in the plan's millisecond counts as later: keeping the shell is the
/// safe side.
pub(crate) const OPENED_ELSEWHERE: &str = "SELECT EXISTS (SELECT 1 FROM restore r JOIN occupant o
  ON o.runtime = r.runtime AND o.native_id = r.native_id
  WHERE r.id = ?1 AND o.block_lineage != r.block_lineage
    AND (o.reported_at >= r.created_at OR o.instance_id = r.instance_id))";

const OPENED_IN_ANOTHER_PANE: &str = "the conversation was opened in another pane";

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

/// Each other pane of an earlier instance that showed this conversation and has no restore here
/// left its open rounds to this restore's record; with nothing started, they end too.
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
