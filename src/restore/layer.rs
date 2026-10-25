//! Floating layers of a restored session. A layer creates blocks, so like the session it is
//! marked as sent before it is requested and, once sent, is only adopted from its gates' claims.

use anyhow::Result;
use rusqlite::{Connection, params};

use super::create::{Layer, Op};
use super::{Restorer, Waiting, hand_over};
use crate::rex::{CallError, Rex};
use crate::store::{Store, now_ms};

/// Where a floating layer goes on the new instance.
pub(super) struct Placement<'a> {
    pub instance: &'a str,
    pub session: &'a str,
    pub window: &'a str,
}

pub(super) enum LayerOutcome {
    Exists,
    Missing(String),
    Waiting,
}

impl Restorer {
    pub(super) fn restore_layer(
        &mut self,
        rex: &mut Rex,
        store: &mut Store,
        op: &Op,
        layer: Layer,
        placement: &Placement,
    ) -> Result<LayerOutcome> {
        let slots = op.layer_slots(layer)?;
        let state = layer_state(store.conn(), &op.id, &slots)?;
        // Its gates bound the blocks when they claimed their slots.
        if state.bound {
            return Ok(LayerOutcome::Exists);
        }
        if state.sent {
            if self.abandoned.contains(&(op.id.clone(), slots[0])) {
                return Ok(LayerOutcome::Missing(
                    "a floating layer may or may not exist; no block of it reported, so it was not \
                     requested again"
                        .into(),
                ));
            }
            self.wait(&op.id, Waiting::Layer(slots));
            return Ok(LayerOutcome::Waiting);
        }
        let request = op.layer_request(
            store.conn(),
            &self.spec,
            placement.session,
            placement.window,
            layer,
        )?;
        mark_layer(store, &op.id, &slots, Some(now_ms()))?;
        match rex.call("session.new_layer", &request) {
            Ok(reply) => {
                let tx = store.write()?;
                let blocks = op.bind_layer(&tx, placement.instance, layer, &reply)?;
                tx.commit()?;
                hand_over(rex, placement.session, &blocks);
                Ok(LayerOutcome::Exists)
            }
            // Nothing was created, so the layer is no longer marked as sent.
            Err(e @ (CallError::Unsent(_) | CallError::Rejected { .. })) => {
                mark_layer(store, &op.id, &slots, None)?;
                Ok(LayerOutcome::Missing(format!(
                    "a floating layer was refused: {}",
                    e.reason()
                )))
            }
            Err(CallError::Unknown(message)) => {
                log::warn!(
                    "restore {}: floating layer unanswered, waiting for its panes to claim: {message}",
                    op.id
                );
                self.wait(&op.id, Waiting::Layer(slots));
                Ok(LayerOutcome::Waiting)
            }
        }
    }
}

fn mark_layer(store: &mut Store, op: &str, slots: &[i64], sent_at: Option<i64>) -> Result<()> {
    let tx = store.write()?;
    for slot in slots {
        tx.execute(
            "UPDATE restore_slot SET layer_sent_at = ?1 WHERE op_id = ?2 AND slot = ?3",
            params![sent_at, op, slot],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) struct LayerState {
    /// A block of the layer is bound on this instance, so the layer exists.
    pub bound: bool,
    /// The layer was requested.
    pub sent: bool,
}

pub(super) fn layer_state(conn: &Connection, op: &str, slots: &[i64]) -> Result<LayerState> {
    let mut state = LayerState {
        bound: false,
        sent: false,
    };
    for slot in slots {
        let (bound, sent_at): (bool, Option<i64>) = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM live l
                            WHERE l.instance_id = s.instance_id AND l.lineage = s.block_lineage),
               s.layer_sent_at
             FROM restore_slot s WHERE s.op_id = ?1 AND s.slot = ?2",
            params![op, slot],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        state.bound |= bound;
        state.sent |= sent_at.is_some();
    }
    Ok(state)
}
