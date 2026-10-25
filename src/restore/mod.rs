//! Recreates sessions saved on an earlier server instance. Each restore is planned in one
//! transaction before anything is created, and every new block runs `rexume gate`, which claims
//! its slot before it starts a program; a create or floating layer whose reply was lost is
//! therefore adopted from the claims instead of being requested again.

mod create;
mod layer;
mod plan;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};

use crate::rex::{CallError, Rex};
use crate::store::{Store, live_of};
use crate::task;
use create::Op;
use layer::{LayerOutcome, Placement, layer_state};
pub use plan::{Plan, Start, plan};

/// How long a create or floating layer whose reply was lost waits for a claim from one of its
/// gates before it is left unknown.
const CLAIM_WAIT: Duration = Duration::from_secs(5);

/// How a restored block's command starts the gate: the executable and state directory, to which
/// each block adds its operation and slot.
pub struct Spec {
    pub gate: Vec<String>,
    pub theme: Option<Value>,
}

enum Waiting {
    /// The create's reply was lost.
    Create,
    /// A floating layer's reply was lost.
    Layer(Vec<i64>),
}

struct Wait {
    op: String,
    waiting: Waiting,
    until: Instant,
}

/// Drives the restores of one server instance from the daemon's event loop. Nothing here
/// sleeps: an operation waiting for its gates' claims is looked at again when the state database
/// changes or its deadline passes.
pub struct Restorer {
    spec: Spec,
    waits: Vec<Wait>,
    /// Layers whose wait ran out, as (operation, first slot); they are not requested again.
    abandoned: HashSet<(String, i64)>,
}

impl Restorer {
    pub fn new(spec: Spec) -> Self {
        Self {
            spec,
            waits: Vec::new(),
            abandoned: HashSet::new(),
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.waits.iter().map(|w| w.until).min()
    }

    /// Continues this instance's unfinished restores, then restores every session saved on an
    /// earlier instance that has none yet.
    pub fn start(&mut self, rex: &mut Rex, store: &mut Store, instance: &str) -> Result<()> {
        let unfinished: Vec<(String, String)> = store
            .conn()
            .prepare(
                "SELECT id, state FROM restore_op WHERE instance_id = ?1
                 AND state IN ('planned', 'sent', 'unknown', 'created') ORDER BY created_at",
            )?
            .query_map(params![instance], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (op, state) in unfinished {
            self.advance(rex, store, instance, &op, &state);
        }
        let saved: Vec<String> = store
            .conn()
            .prepare(
                "SELECT lineage FROM session s WHERE instance_id != ?1 AND NOT EXISTS
                   (SELECT 1 FROM restore_op o WHERE o.instance_id = ?1 AND o.session_lineage = s.lineage)
                 ORDER BY position",
            )?
            .query_map(params![instance], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let retired = task::retired_workers(store.conn())?;
        for session in saved {
            match plan(store, instance, &session, &retired) {
                Ok(Some(op)) => self.advance(rex, store, instance, &op, "planned"),
                Ok(None) => {}
                Err(e) => log::error!("session {session}: failed to plan its restore: {e:#}"),
            }
        }
        Ok(())
    }

    /// Looks again at the operations waiting for claims.
    pub fn reconcile(&mut self, rex: &mut Rex, store: &mut Store, instance: &str) {
        let now = Instant::now();
        for wait in std::mem::take(&mut self.waits) {
            let op = wait.op.clone();
            if let Err(e) = self.check(rex, store, instance, wait, now) {
                log::error!("restore {op}: {e:#}");
            }
        }
    }

    fn check(
        &mut self,
        rex: &mut Rex,
        store: &mut Store,
        instance: &str,
        wait: Wait,
        now: Instant,
    ) -> Result<()> {
        match &wait.waiting {
            Waiting::Create => {
                let op = Op::load(store.conn(), &wait.op)?;
                // The first gate to claim its slot binds the session it runs in.
                if let Some(live) = live_of(store.conn(), instance, &op.session)? {
                    return self.adopt(rex, store, instance, &wait.op, &live);
                }
                if now >= wait.until {
                    return set_state(
                        store,
                        &wait.op,
                        "unknown",
                        Some("no block of the session reported; not created again"),
                    );
                }
            }
            Waiting::Layer(slots) => {
                if layer_state(store.conn(), &wait.op, slots)?.bound {
                    return self.after_create(rex, store, instance, &wait.op);
                }
                if now >= wait.until {
                    self.abandoned.insert((wait.op.clone(), slots[0]));
                    return self.after_create(rex, store, instance, &wait.op);
                }
            }
        }
        self.waits.push(wait);
        Ok(())
    }

    fn advance(&mut self, rex: &mut Rex, store: &mut Store, instance: &str, op: &str, state: &str) {
        let result = match state {
            "planned" => self.create(rex, store, instance, op),
            "sent" | "unknown" => {
                self.wait(op, Waiting::Create);
                self.reconcile(rex, store, instance);
                Ok(())
            }
            _ => self.after_create(rex, store, instance, op),
        };
        if let Err(e) = result {
            log::error!("restore {op}: {e:#}");
        }
    }

    fn wait(&mut self, op: &str, waiting: Waiting) {
        self.waits.push(Wait {
            op: op.to_owned(),
            waiting,
            until: Instant::now() + CLAIM_WAIT,
        });
    }

    fn create(&mut self, rex: &mut Rex, store: &mut Store, instance: &str, id: &str) -> Result<()> {
        let op = Op::load(store.conn(), id)?;
        let request = op.create_request(store.conn(), &self.spec)?;
        set_state(store, id, "sent", None)?;
        match rex.call("session.create", &request) {
            Ok(reply) => {
                let tx = store.write()?;
                let (live, blocks) = op.bind_created(&tx, instance, &reply)?;
                tx.execute(
                    "UPDATE restore_op SET state = 'created' WHERE id = ?1",
                    params![id],
                )?;
                tx.commit()?;
                hand_over(rex, &live, &blocks);
                self.after_create(rex, store, instance, id)
            }
            Err(e @ (CallError::Unsent(_) | CallError::Rejected { .. })) => {
                set_state(store, id, "failed", Some(&e.reason()))
            }
            Err(CallError::Unknown(message)) => {
                set_state(store, id, "unknown", Some(&message))?;
                self.wait(id, Waiting::Create);
                Ok(())
            }
        }
    }

    /// A session whose create reply was lost, found through its gates' claims.
    fn adopt(
        &mut self,
        rex: &mut Rex,
        store: &mut Store,
        instance: &str,
        id: &str,
        live: &str,
    ) -> Result<()> {
        let op = Op::load(store.conn(), id)?;
        let view = rex.call("session.view", &json!({ "session_id": live }))?;
        let tx = store.write()?;
        crate::layout::bind_restored(&tx, instance, live, &op.session)?;
        op.bind_windows(&tx, instance, &view)?;
        tx.execute(
            "UPDATE restore_op SET state = 'created' WHERE id = ?1",
            params![id],
        )?;
        tx.commit()?;
        hand_over(rex, live, &crate::layout::view_blocks(&view));
        self.after_create(rex, store, instance, id)
    }

    /// Floating layers, then focus, zoom, and the active window. A layer creates blocks, so like
    /// the session it is marked as sent before it is requested and, once sent, is only ever
    /// adopted from its gates' claims. The operation is complete only when every layer exists;
    /// display settings that fail are logged and do not hold it back.
    fn after_create(
        &mut self,
        rex: &mut Rex,
        store: &mut Store,
        instance: &str,
        id: &str,
    ) -> Result<()> {
        let op = Op::load(store.conn(), id)?;
        let Some(live_session) = live_of(store.conn(), instance, &op.session)? else {
            return set_state(
                store,
                id,
                "unknown",
                Some("the created session is not bound"),
            );
        };
        let mut missing = Vec::new();
        for layer in op.layers() {
            let Some(window) = live_of(store.conn(), instance, op.window_lineage(layer))? else {
                missing.push(format!("window {} is not bound", op.window_lineage(layer)));
                continue;
            };
            let placement = Placement {
                instance,
                session: &live_session,
                window: &window,
            };
            match self.restore_layer(rex, store, &op, layer, &placement)? {
                LayerOutcome::Exists => {}
                LayerOutcome::Missing(reason) => missing.push(reason),
                LayerOutcome::Waiting => return Ok(()),
            }
        }
        for (method, request) in op.display(store.conn(), instance, &live_session)? {
            if let Err(e) = rex.call(method, &request) {
                log::warn!("restore {id}: {method}: {e}");
            }
        }
        if missing.is_empty() {
            set_state(store, id, "completed", None)
        } else {
            set_state(store, id, "partial", Some(&missing.join("; ")))
        }
    }
}

/// Blocks created over the daemon's connection belong to it, and a connected owner keeps a block
/// at its own size and leaves programs' color queries unanswered; handing them over lets the app
/// take them when it shows them.
fn hand_over(rex: &mut Rex, session: &str, blocks: &[String]) {
    if blocks.is_empty() {
        return;
    }
    let result = rex
        .call("session.attach", &json!({ "session_id": session }))
        .and_then(|_| {
            rex.call(
                "session.disown_block",
                &json!({ "session_id": session, "block_ids": blocks }),
            )
        });
    if let Err(e) = result {
        log::warn!("session {session}: failed to hand over its blocks: {e}");
    }
}

fn set_state(store: &mut Store, op: &str, state: &str, note: Option<&str>) -> Result<()> {
    let tx = store.write()?;
    tx.execute(
        "UPDATE restore_op SET state = ?1, note = COALESCE(?2, note) WHERE id = ?3",
        params![state, note, op],
    )?;
    tx.commit()?;
    match note {
        Some(note) => log::info!("restore {op}: {state}: {note}"),
        None => log::info!("restore {op}: {state}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
