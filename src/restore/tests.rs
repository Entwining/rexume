use super::*;
use crate::layout::{SessionDoc, bind_restored, from_view, save_view};

/// The live IDs of a restored session and the lineages its restore planned for them.
const RESTORED: [(&str, &str); 7] = [
    ("block:1", "b1"),
    ("session:new", "s"),
    ("window:a", "w1"),
    ("window:b", "w2"),
    ("block:2", "b2"),
    ("block:3", "b3"),
    ("block:4", "b4"),
];

fn restored(live: &str) -> Result<String> {
    let (_, lineage) = RESTORED.iter().find(|(id, _)| *id == live).unwrap();
    Ok((*lineage).to_owned())
}

/// Two windows with a split, a focused pane, a zoomed pane, and a floating layer.
fn view() -> Value {
    json!({
        "label": "work",
        "active_window_id": "window:b",
        "windows": [
            {"window_id": "window:a", "label": "code", "focused_block_id": "block:2",
             "layers": [{"kind": "tiled", "layout": {"split": {"direction": "horizontal", "ratio": 0.3,
                 "before": {"block_id": "block:1"}, "after": {"block_id": "block:2"}}},
                 "blocks": [{"block_id": "block:1"}, {"block_id": "block:2"}]}]},
            {"window_id": "window:b", "label": "logs", "zoomed_block_id": "block:3",
             "layers": [
                 {"kind": "tiled", "layout": {"block_id": "block:3"}, "blocks": [{"block_id": "block:3"}]},
                 {"kind": "floating", "bounds": [0.1, 0.1, 0.5, 0.5], "layout": {"block_id": "block:4"},
                  "blocks": [{"block_id": "block:4"}]}]}
        ]
    })
}

fn saved(store: &Store) -> Result<Vec<(String, String, SessionDoc)>> {
    let rows: Vec<(String, String, String)> = store
        .conn()
        .prepare("SELECT lineage, instance_id, doc FROM session")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(lineage, instance, doc)| Ok((lineage, instance, serde_json::from_str(&doc)?)))
        .collect()
}

fn pairs(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<(String, String)>> {
    Ok(conn
        .prepare(sql)?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

#[test]
fn a_lost_create_reply_ends_as_if_the_reply_had_arrived() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    let planned = from_view(&view(), &mut restored)?;
    let tx = store.write()?;
    // What the earlier instance left: the session's layout, and a pane's record and conversation.
    tx.execute(
        "INSERT INTO session (lineage, instance_id, position, doc, updated_at)
         VALUES ('s', 'i1', 0, ?1, 1)",
        params![serde_json::to_string(&planned)?],
    )?;
    tx.execute_batch(
        "INSERT INTO block (lineage, instance_id, cwd, columns, rows) VALUES ('b1', 'i1', '/old', 80, 24);
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'old', 1, 0), ('claude', 'new', 1, 0), ('claude', 'owner', 1, 0);
         INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at)
           VALUES ('b1', 'claude', 'old', 3, 0, 0), ('b2', 'claude', 'old', 4, 0, 0);",
    )?;
    tx.execute(
        "INSERT INTO restore_op (id, instance_id, session_lineage, state, doc, created_at)
         VALUES ('op', 'i2', 's', 'unknown', ?1, 1)",
        params![serde_json::to_string(&planned)?],
    )?;
    // The create's reply was lost. Before any gate claimed, the daemon read the new session and
    // events of two of its panes, one pane's conversation reported, and an orchestrator registered
    // that pane as a worker.
    save_view(&tx, "i2", "session:new", 0, &view())?;
    tx.execute_batch(
        "INSERT INTO block (lineage, instance_id, cwd) VALUES ('block:1', 'i2', '/new'),
           ('block:2', 'i2', '/two');
         INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at)
           VALUES ('block:1', 'claude', 'new', 7, 0, 5);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at) VALUES ('t', 'count', 'claude', 'owner', 'block:1', 0, 5);",
    )?;
    // The gates bind their panes and the session; the restore adopts the session's windows.
    for (live, lineage) in RESTORED.iter().filter(|(id, _)| !id.starts_with("window:")) {
        bind_restored(&tx, "i2", live, lineage)?;
    }
    Op::load(&tx, "op")?.bind_windows(&tx, "i2", &view())?;

    let mut expected: Vec<(String, String)> = RESTORED
        .iter()
        .map(|(id, lineage)| ((*id).to_owned(), (*lineage).to_owned()))
        .collect();
    expected.sort();
    let bindings = "SELECT live_id, lineage FROM live WHERE instance_id = 'i2' ORDER BY live_id";
    assert_eq!(pairs(&tx, bindings)?, expected);
    // A pane recorded on both sides merges, one recorded only under its own ID moves.
    let panes = pairs(&tx, "SELECT lineage, cwd FROM block ORDER BY lineage")?;
    assert_eq!(
        panes,
        [("b1".into(), "/new".into()), ("b2".into(), "/two".into())]
    );
    // A conversation reported under its own ID replaces the earlier one; without one, it stays.
    let occupants = pairs(
        &tx,
        "SELECT block_lineage, native_id FROM occupant ORDER BY 1",
    )?;
    assert_eq!(
        occupants,
        [("b1".into(), "new".into()), ("b2".into(), "old".into())]
    );
    assert_eq!(
        pairs(&tx, "SELECT id, worker_lineage FROM task")?,
        [("t".into(), "b1".into())]
    );
    tx.commit()?;
    assert_eq!(saved(&store)?, [("s".into(), "i2".into(), planned.clone())]);

    // The daemon's next read of the session saves the same layout.
    let tx = store.write()?;
    save_view(&tx, "i2", "session:new", 0, &view())?;
    tx.commit()?;
    assert_eq!(saved(&store)?, [("s".into(), "i2".into(), planned)]);
    Ok(())
}
