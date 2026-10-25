//! Rebuilds a saved layout on the server: the session with its tiled layouts, then floating layers
//! and display settings, binding each created resource to its lineage. This is the only part of
//! the restore that knows the layout's form.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::layout::{Node, SessionDoc, WindowDoc, bind_restored};
use crate::rex::{items, require_str, str_at};
use crate::store::live_of;

use super::{Spec, Start};

const SHELL_FLAVOR: &str = "com.superlogical.terminal.shell";

/// A saved session's layout, ready to plan from.
pub(super) struct Saved {
    /// The layout without the left-out blocks, as `restore_op.doc` holds it.
    pub doc: String,
    /// Its blocks in slot order.
    pub blocks: Vec<String>,
    /// The instance it was saved on.
    pub instance: String,
}

/// A saved session's layout without the given blocks; `None` when nothing of it is left to show.
pub(super) fn saved(conn: &Connection, session: &str, without: &[String]) -> Result<Option<Saved>> {
    let (doc, instance): (String, String) = conn.query_row(
        "SELECT doc, instance_id FROM session WHERE lineage = ?1",
        params![session],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut doc: SessionDoc = serde_json::from_str(&doc)
        .with_context(|| format!("failed to parse the saved layout of session {session}"))?;
    for block in without {
        doc.remove_block(block);
    }
    if doc.windows.is_empty() {
        return Ok(None);
    }
    let blocks = doc.blocks().into_iter().map(str::to_owned).collect();
    Ok(Some(Saved {
        doc: serde_json::to_string(&doc)?,
        blocks,
        instance,
    }))
}

/// One planned restore: its layout and its slots' blocks, in slot order.
pub(super) struct Op {
    pub id: String,
    doc: SessionDoc,
    pub session: String,
    slots: Vec<String>,
}

/// A floating layer: its window's position in the layout and its own among the window's layers.
#[derive(Clone, Copy)]
pub(super) struct Layer {
    window: usize,
    index: usize,
}

impl Op {
    pub fn load(conn: &Connection, id: &str) -> Result<Self> {
        let (doc, session): (String, String) = conn.query_row(
            "SELECT doc, session_lineage FROM restore_op WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let slots = conn
            .prepare("SELECT block_lineage FROM restore_slot WHERE op_id = ?1 ORDER BY slot")?
            .query_map(params![id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Self {
            id: id.to_owned(),
            doc: serde_json::from_str(&doc)?,
            session,
            slots,
        })
    }

    fn slot_of(&self, lineage: &str) -> Result<i64> {
        let slot = self
            .slots
            .iter()
            .position(|b| b == lineage)
            .with_context(|| format!("block {lineage} of the layout has no slot"))?;
        Ok(i64::try_from(slot)?)
    }

    pub fn create_request(&self, conn: &Connection, spec: &Spec) -> Result<Value> {
        let mut windows = Vec::new();
        for window in &self.doc.windows {
            let mut entry = json!({ "layout": self.node(conn, spec, tiled_of(window))? });
            if let Some(label) = &window.label {
                entry["window_label"] = json!(label);
            }
            windows.push(entry);
        }
        Ok(json!({ "label": self.doc.label, "initial_windows": windows }))
    }

    /// Binds the session, windows, and tiled blocks a create reply names, and returns the new
    /// session and blocks.
    pub fn bind_created(
        &self,
        tx: &Transaction,
        instance: &str,
        reply: &Value,
    ) -> Result<(String, Vec<String>)> {
        let live = require_str(reply, "/session_id")?;
        bind_restored(tx, instance, &live, &self.session)?;
        let mut blocks = Vec::new();
        for (window, created) in self
            .doc
            .windows
            .iter()
            .zip(items(reply, "/initial_windows"))
        {
            if let Some(id) = str_at(created, "/window_id") {
                bind_restored(tx, instance, id, &window.lineage)?;
            }
            blocks.extend(bind_leaves(tx, instance, tiled_of(window), created)?);
        }
        Ok((live, blocks))
    }

    /// Binds the windows of a session whose create reply was lost, from the session's view.
    pub fn bind_windows(&self, tx: &Transaction, instance: &str, view: &Value) -> Result<()> {
        let ids = items(view, "/windows")
            .iter()
            .filter_map(|w| str_at(w, "/window_id"));
        for (window, id) in self.doc.windows.iter().zip(ids) {
            bind_restored(tx, instance, id, &window.lineage)?;
        }
        Ok(())
    }

    /// The floating layers still to create: a window without a tiled layout used its first one
    /// as the layout, because a new window needs one.
    pub fn layers(&self) -> Vec<Layer> {
        let mut layers = Vec::new();
        for (w, window) in self.doc.windows.iter().enumerate() {
            let skip = usize::from(window.tiled.is_none());
            layers.extend((skip..window.floating.len()).map(|index| Layer { window: w, index }));
        }
        layers
    }

    pub fn window_lineage(&self, layer: Layer) -> &str {
        &self.doc.windows[layer.window].lineage
    }

    fn layout(&self, layer: Layer) -> &Node {
        &self.doc.windows[layer.window].floating[layer.index].layout
    }

    pub fn layer_slots(&self, layer: Layer) -> Result<Vec<i64>> {
        self.layout(layer)
            .leaves()
            .into_iter()
            .map(|lineage| self.slot_of(lineage))
            .collect()
    }

    pub fn layer_request(
        &self,
        conn: &Connection,
        spec: &Spec,
        live_session: &str,
        window: &str,
        layer: Layer,
    ) -> Result<Value> {
        let bounds = self.doc.windows[layer.window].floating[layer.index].bounds;
        Ok(json!({
            "session_id": live_session, "window_id": window, "bounds": bounds,
            "layout": self.node(conn, spec, self.layout(layer))?,
        }))
    }

    pub fn bind_layer(
        &self,
        tx: &Transaction,
        instance: &str,
        layer: Layer,
        reply: &Value,
    ) -> Result<Vec<String>> {
        bind_leaves(tx, instance, self.layout(layer), reply)
    }

    /// The focus, zoom, and active window calls that finish the session's display.
    pub fn display(
        &self,
        conn: &Connection,
        instance: &str,
        live_session: &str,
    ) -> Result<Vec<(&'static str, Value)>> {
        let mut calls = Vec::new();
        for window in &self.doc.windows {
            if let Some(block) = window
                .focused
                .as_deref()
                .map(|b| live_of(conn, instance, b))
                .transpose()?
                .flatten()
            {
                calls.push((
                    "session.focus_block",
                    json!({ "session_id": live_session, "block_id": block }),
                ));
            }
            if let Some(block) = window
                .zoomed
                .as_deref()
                .map(|b| live_of(conn, instance, b))
                .transpose()?
                .flatten()
            {
                calls.push((
                    "session.zoom_block",
                    json!({ "session_id": live_session, "block_id": block, "zoom": true }),
                ));
            }
        }
        if let Some(window) = self.doc.active.and_then(|i| self.doc.windows.get(i))
            && let Some(window) = live_of(conn, instance, &window.lineage)?
        {
            calls.push((
                "session.focus_window",
                json!({ "session_id": live_session, "window_id": window }),
            ));
        }
        Ok(calls)
    }

    fn node(&self, conn: &Connection, spec: &Spec, node: &Node) -> Result<Value> {
        Ok(match node {
            Node::Split {
                direction,
                ratio,
                before,
                after,
            } => json!({ "split": {
                "direction": direction, "ratio": ratio,
                "before": self.node(conn, spec, before)?, "after": self.node(conn, spec, after)?,
            }}),
            Node::Block { lineage, label } => {
                let slot = self.slot_of(lineage)?;
                let (cwd, columns, rows): (Option<String>, Option<i64>, Option<i64>) = conn
                    .query_row(
                        "SELECT cwd, columns, rows FROM block WHERE lineage = ?1",
                        params![lineage],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?
                    .unwrap_or_default();
                let (start, argv): (String, String) = conn.query_row(
                    "SELECT start, plan ->> '$.argv' FROM restore_slot WHERE op_id = ?1 AND slot = ?2",
                    params![self.id, slot],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let mut command = spec.gate.clone();
                command.extend([self.id.clone(), slot.to_string()]);
                let mut options = json!({ "command": command });
                if let Some(cwd) = cwd {
                    options["cwd"] = json!(cwd);
                }
                if let (Some(columns), Some(rows)) = (columns, rows) {
                    options["size"] = json!({ "columns": columns, "rows": rows });
                }
                if let Some(theme) = &spec.theme {
                    options["theme"] = theme.clone();
                }
                if start == Start::Typed.as_str() {
                    let argv: Vec<String> = serde_json::from_str(&argv)?;
                    options["initial_input"] =
                        json!(shlex::try_join(argv.iter().map(String::as_str))?);
                }
                let mut block = json!({ "flavor": SHELL_FLAVOR, "options": options });
                if let Some(label) = label {
                    block["label"] = json!(label);
                }
                json!({ "block": block })
            }
        })
    }
}

/// Binds the blocks a create or layer reply lists to `node`'s leaves, in order, and returns them.
fn bind_leaves(
    tx: &Transaction,
    instance: &str,
    node: &Node,
    reply: &Value,
) -> Result<Vec<String>> {
    let mut blocks = Vec::new();
    for (lineage, id) in node.leaves().into_iter().zip(items(reply, "/block_ids")) {
        if let Some(id) = id.as_str() {
            bind_restored(tx, instance, id, lineage)?;
            blocks.push(id.to_owned());
        }
    }
    Ok(blocks)
}

fn tiled_of(window: &WindowDoc) -> &Node {
    window
        .tiled
        .as_ref()
        .unwrap_or_else(|| &window.floating[0].layout)
}
