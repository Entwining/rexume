use anyhow::Result;
use serde_json::{Value, json};

use super::*;

fn same_id(id: &str) -> Result<String> {
    Ok(id.to_owned())
}

fn view() -> Value {
    json!({
        "label": "work",
        "active_window_id": "window:b",
        "windows": [
            {"window_id": "window:aaaaaaaa", "label": "Window aaaaaaaa", "focused_block_id": "block:2",
             "layers": [{"kind": "tiled", "bounds": [0,0,1,1], "layout": {"rect": [0,0,1,1], "split": {
                 "direction": "horizontal", "ratio": 0.3,
                 "before": {"rect": [0,0,0.3,1], "block_id": "block:1"},
                 "after": {"rect": [0.3,0,0.7,1], "block_id": "block:2"}}},
                 "blocks": [{"block_id": "block:1", "label": "shell"}, {"block_id": "block:2", "label": "editor"}]}]},
            {"window_id": "window:b", "label": "logs", "zoomed_block_id": "block:3",
             "layers": [
                 {"kind": "tiled", "bounds": [0,0,1,1], "layout": {"rect": [0,0,1,1], "block_id": "block:3"},
                  "blocks": [{"block_id": "block:3"}]},
                 {"kind": "floating", "bounds": [0.1,0.1,0.5,0.5], "layout": {"rect": [0.1,0.1,0.5,0.5], "block_id": "block:4"},
                  "blocks": [{"block_id": "block:4", "label": "float"}]}]}
        ]
    })
}

#[test]
fn reads_splits_floating_layers_zoom_and_labels() -> Result<()> {
    let doc = from_view(&view(), &mut same_id)?;
    assert_eq!(doc.active, Some(1));
    assert_eq!(
        doc.windows[0].label, None,
        "default window label is dropped"
    );
    assert_eq!(doc.windows[1].label.as_deref(), Some("logs"));
    assert_eq!(
        doc.blocks(),
        vec!["block:1", "block:2", "block:3", "block:4"]
    );
    let Some(Node::Split { ratio, before, .. }) = &doc.windows[0].tiled else {
        panic!("split")
    };
    assert!((ratio - 0.3).abs() < f64::EPSILON);
    assert_eq!(
        **before,
        Node::Block {
            lineage: "block:1".into(),
            label: None
        }
    );
    assert_eq!(doc.windows[0].focused.as_deref(), Some("block:2"));
    assert_eq!(doc.windows[1].zoomed.as_deref(), Some("block:3"));
    assert_eq!(doc.windows[1].floating[0].bounds, [0.1, 0.1, 0.5, 0.5]);
    Ok(())
}

#[test]
fn closing_blocks_promotes_siblings_and_drops_empty_windows() -> Result<()> {
    let mut doc = from_view(&view(), &mut same_id)?;
    assert!(doc.remove_block("block:1"));
    assert_eq!(
        doc.windows[0].tiled,
        Some(Node::Block {
            lineage: "block:2".into(),
            label: Some("editor".into())
        })
    );
    assert!(doc.remove_block("block:2"));
    assert_eq!(
        doc.windows.len(),
        1,
        "the emptied window goes with its last block"
    );
    assert_eq!(doc.active, Some(0), "the active window keeps being active");
    assert!(doc.remove_block("block:3"));
    assert_eq!(doc.windows[0].zoomed, None);
    assert_eq!(
        doc.windows[0].blocks(),
        vec!["block:4"],
        "a window with only a floating layer stays"
    );
    assert!(!doc.remove_block("block:9"));
    Ok(())
}

#[test]
fn a_view_missing_a_window_id_or_a_layout_is_an_error() {
    let mut no_id = view();
    no_id["windows"][0]
        .as_object_mut()
        .map(|w| w.remove("window_id"));
    assert!(from_view(&no_id, &mut same_id).is_err());
    let mut no_layout = view();
    no_layout["windows"][1]["layers"][1]["layout"] = Value::Null;
    assert!(from_view(&no_layout, &mut same_id).is_err());
}

/// The server no longer has a session this instance saved, or one it restored itself and the daemon
/// bound here before the session closed; a session saved on an earlier instance that waits for its
/// restore here is kept.
#[test]
fn sessions_this_instance_had_are_forgotten_once_the_server_lists_them_no_more() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = crate::store::Store::open(dir.path())?;
    let tx = store.write()?;
    tx.execute_batch(
        "INSERT INTO session (lineage, instance_id, position, doc, updated_at)
           VALUES ('own', 'i2', 0, '{}', 0), ('restored', 'i1', 1, '{}', 0),
                  ('kept', 'i1', 2, '{}', 0), ('waiting', 'i1', 3, '{}', 0);
         INSERT INTO live (instance_id, live_id, lineage)
           VALUES ('i2', 'session:a', 'restored'), ('i2', 'session:b', 'kept');",
    )?;
    forget_sessions_except(&tx, "i2", &["kept".to_owned()])?;
    let left: Vec<String> = tx
        .prepare("SELECT lineage FROM session ORDER BY lineage")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(left, ["kept", "waiting"]);
    Ok(())
}
