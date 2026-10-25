//! Building the saved layout from a `session.view` reply.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::{Floating, Node, SessionDoc, WindowDoc};
use crate::rex::{items, str_at};

/// Rex names an unlabeled window "Window " and an unlabeled block "shell" or "Block " plus the end
/// of its ID; keeping those would pin a name that no longer matches the restored ID.
fn chosen_label(
    label: Option<&str>,
    id: &str,
    prefix: &str,
    default: Option<&str>,
) -> Option<String> {
    let label = label?;
    let tail = id.get(id.len().saturating_sub(8)..).unwrap_or(id);
    if Some(label) == default || label == format!("{prefix}{tail}") {
        return None;
    }
    Some(label.to_owned())
}

/// Builds the saved layout from a `session.view` reply, naming each window and block by the
/// lineage `lineage` returns for its live ID.
pub fn from_view(
    view: &Value,
    lineage: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<SessionDoc> {
    let label = str_at(view, "/label").unwrap_or_default().to_owned();
    let active_id = str_at(view, "/active_window_id");
    let mut windows = Vec::new();
    let mut active = None;
    for window in items(view, "/windows") {
        let window_id = str_at(window, "/window_id").context("window without window_id")?;
        let mut labels = HashMap::new();
        let mut tiled = None;
        let mut floating = Vec::new();
        for layer in items(window, "/layers") {
            for block in items(layer, "/blocks") {
                if let Some(id) = str_at(block, "/block_id") {
                    labels.insert(
                        id.to_owned(),
                        chosen_label(str_at(block, "/label"), id, "Block ", Some("shell")),
                    );
                }
            }
            let layout = layer
                .get("layout")
                .filter(|l| !l.is_null())
                .context("layer without layout")?;
            let node = node_from_view(layout, &labels, lineage)?;
            if str_at(layer, "/kind") == Some("tiled") {
                tiled = Some(node);
            } else {
                floating.push(Floating {
                    bounds: bounds(layer)?,
                    layout: node,
                });
            }
        }
        if tiled.is_none() && floating.is_empty() {
            continue;
        }
        if Some(window_id) == active_id {
            active = Some(windows.len());
        }
        let mut block_lineage = |key: &str| -> Result<Option<String>> {
            str_at(window, key).map(&mut *lineage).transpose()
        };
        windows.push(WindowDoc {
            focused: block_lineage("/focused_block_id")?,
            zoomed: block_lineage("/zoomed_block_id")?,
            lineage: lineage(window_id)?,
            label: chosen_label(str_at(window, "/label"), window_id, "Window ", None),
            tiled,
            floating,
        });
    }
    Ok(SessionDoc {
        label,
        active,
        windows,
    })
}

fn bounds(layer: &Value) -> Result<[f64; 4]> {
    let values: Vec<f64> = layer
        .get("bounds")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    match values.as_slice() {
        [x, y, w, h] => Ok([*x, *y, *w, *h]),
        _ => bail!("floating layer without bounds"),
    }
}

fn node_from_view(
    node: &Value,
    labels: &HashMap<String, Option<String>>,
    lineage: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<Node> {
    if let Some(split) = node.get("split") {
        let (Some(before), Some(after)) = (split.get("before"), split.get("after")) else {
            bail!("split without both sides");
        };
        return Ok(Node::Split {
            direction: str_at(split, "/direction")
                .unwrap_or("horizontal")
                .to_owned(),
            ratio: split.get("ratio").and_then(Value::as_f64).unwrap_or(0.5),
            before: Box::new(node_from_view(before, labels, lineage)?),
            after: Box::new(node_from_view(after, labels, lineage)?),
        });
    }
    let Some(id) = str_at(node, "/block_id") else {
        bail!("layout node without block or split")
    };
    Ok(Node::Block {
        lineage: lineage(id)?,
        label: labels.get(id).cloned().flatten(),
    })
}

/// The live IDs of the blocks a `session.view` reply shows.
pub fn view_blocks(view: &Value) -> Vec<String> {
    items(view, "/windows")
        .iter()
        .flat_map(|window| items(window, "/layers"))
        .flat_map(|layer| items(layer, "/blocks"))
        .filter_map(|block| str_at(block, "/block_id").map(str::to_owned))
        .collect()
}
