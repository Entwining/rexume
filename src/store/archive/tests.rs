use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};

use super::*;
use crate::store::now_ms;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

/// A time before the retention cutoff.
fn old() -> i64 {
    now_ms() - i64::try_from(RETENTION.as_millis()).unwrap() - 1
}

fn strings(store: &Store, sql: &str) -> Vec<String> {
    let mut statement = store.conn().prepare(sql).unwrap();
    let rows = statement.query_map([], |row| row.get(0)).unwrap();
    rows.collect::<rusqlite::Result<_>>().unwrap()
}

fn archived(store: &Store, entity_type: &str, id: &str) -> Result<(i64, i64, Value)> {
    let (version, archived_at, data): (i64, i64, String) = store.conn().query_row(
        "SELECT version, archived_at, data FROM archive WHERE entity_type = ?1 AND entity_id = ?2",
        params![entity_type, id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok((version, archived_at, serde_json::from_str(&data)?))
}

/// Three tasks of one owner: `old` with every round finished and acknowledged, or finished if
/// never acknowledged, before the cutoff; `open` with an unfinished round; `recent` with a round
/// acknowledged since the cutoff. An old dropped round does not let either of the last two go.
fn tasks(store: &Store) -> Result<()> {
    store.conn().execute_batch(&format!(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'owner', 1, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at)
           VALUES ('old', 'x', 'claude', 'owner', 'b1', 0, 0),
             ('open', 'x', 'claude', 'owner', 'b2', 0, 0),
             ('recent', 'x', 'claude', 'owner', 'b3', 0, 0);
         INSERT INTO task_round (task_id, round, state, finished_at, acked_at, created_at,
           result_ref, result)
           VALUES ('old', 1, 'completed', {old}, {old}, 0, 'm1', 'the answer'),
             ('old', 2, 'dropped', {old}, NULL, 0, NULL, NULL),
             ('open', 1, 'running', NULL, NULL, 0, NULL, NULL),
             ('open', 2, 'dropped', {old}, {old}, 0, NULL, NULL),
             ('recent', 1, 'failed', {old}, {now}, 0, NULL, NULL),
             ('recent', 2, 'dropped', {old}, {old}, 0, NULL, NULL);
         INSERT INTO request (id, task_id, round, state, text)
           VALUES ('q', 'old', 2, 'superseded', 'go on');
         INSERT INTO blocked_episode (id, task_id, round, kind, started_at, ended_at, end_reason)
           VALUES ('e2', 'old', 1, NULL, 2, 3, 'round_finished'),
             ('e1', 'old', 1, 'question', 0, 1, 'status_changed');
         INSERT INTO notice (id, task_id, round, kind, blocked_episode_id, state, created_at,
           sent_at)
           VALUES ('n', 'old', 1, 'finished', NULL, 'accepted', {old}, {old}),
             ('nb', 'old', 1, 'blocked', 'e1', 'sent', 0, 0);",
        old = old(),
        now = now_ms()
    ))?;
    Ok(())
}

/// A task whose rounds are all old becomes one archive row holding its rows as they stood, in the
/// transaction that deletes them; a task still open or recently acknowledged stays as it is.
#[test]
fn sweep_archives_a_task_once_all_its_rounds_are_old() -> Result<()> {
    let (_dir, mut store) = store();
    tasks(&store)?;
    let before = document(store.conn(), EntityType::Task, "old")?;
    let now = now_ms();
    sweep(&mut store, "i2", now)?;
    let (version, archived_at, data) = archived(&store, "task", "old")?;
    assert_eq!((version, archived_at), (SCHEMA_VERSION, now));
    assert_eq!(data, before);
    assert_eq!(data["task_round"][0]["result"], "the answer");
    assert_eq!(data["task_round"][1]["round"], 2);
    assert_eq!(data["request"][0]["text"], "go on");
    assert_eq!(data["notice"][0]["id"], "n");
    assert_eq!(data["blocked_episode"][0]["kind"], "question");
    assert_eq!(data["blocked_episode"][1]["id"], "e2");
    assert_eq!(
        strings(&store, "SELECT id FROM task ORDER BY id"),
        ["open", "recent"]
    );
    assert_eq!(
        strings(&store, "SELECT task_id FROM task_round GROUP BY task_id"),
        ["open", "recent"]
    );
    assert!(strings(&store, "SELECT id FROM request").is_empty());
    assert!(strings(&store, "SELECT id FROM notice").is_empty());
    assert!(strings(&store, "SELECT id FROM blocked_episode").is_empty());
    assert_eq!(strings(&store, "SELECT entity_id FROM archive"), ["old"]);
    Ok(())
}

/// A task the sweep listed but which took a new round before its transaction is left as it is.
#[test]
fn a_task_no_longer_expired_is_not_archived() -> Result<()> {
    let (_dir, mut store) = store();
    tasks(&store)?;
    let tx = store.write()?;
    assert_eq!(
        archive(&tx, EntityType::Task, "open", "i2", old() + 1, now_ms())?,
        None
    );
    tx.commit()?;
    assert!(strings(&store, "SELECT entity_id FROM archive").is_empty());
    assert_eq!(
        strings(&store, "SELECT id FROM task WHERE id = 'open'"),
        ["open"]
    );
    Ok(())
}

/// An earlier instance's old restore is archived, its plan as the text the column holds, and so is
/// an old epoch of an instance no pane is recorded on; the current instance's restore, a recent
/// one, an epoch whose instance still has a pane, and a recent epoch stay. A conversation goes once
/// nothing names it.
#[test]
fn sweep_archives_old_restores_and_epochs_of_earlier_instances() -> Result<()> {
    let (_dir, mut store) = store();
    let (old, now) = (old(), now_ms());
    store.conn().execute_batch(&format!(
        "INSERT INTO epoch (id, instance_id, started_at, ended_at, end_reason)
           VALUES (1, 'i0', 0, {old}, 'server_gone'), (2, 'i1', 0, {old}, 'server_gone'),
             (3, 'i3', 0, {now}, 'disconnected');
         INSERT INTO epoch (id, instance_id, host_id, started_at) VALUES (4, 'i2', 'h', {now});
         INSERT INTO block (lineage, instance_id) VALUES ('b-kept', 'i1');
         INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'unused', 1, 0), ('claude', 'restored', 1, 0),
             ('claude', 'expired', 1, 0), ('claude', 'gone', 1, 0);
         INSERT INTO restore (id, instance_id, block_lineage, start, runtime, native_id, plan, state,
           created_at)
           VALUES ('r-old', 'i1', 'b-gone', 'agent', 'claude', 'gone', '{{\"argv\":[]}}', 'skipped',
               {old}),
             ('r-recent', 'i1', 'b-other', 'agent', 'claude', 'restored', '{{}}', 'skipped', {now}),
             ('r-current', 'i2', 'b-now', 'agent', 'claude', 'expired', '{{}}', 'planned', {old});"
    ))?;
    sweep(&mut store, "i2", now)?;
    assert_eq!(
        strings(
            &store,
            "SELECT entity_type || ' ' || entity_id FROM archive ORDER BY 1"
        ),
        ["epoch 1", "restore r-old"]
    );
    assert_eq!(
        archived(&store, "epoch", "1")?.2,
        json!({ "epoch": { "id": 1, "instance_id": "i0", "host_id": null, "daemon_version": null,
            "started_at": 0, "ended_at": old, "end_reason": "server_gone", "error": null } })
    );
    let restore = archived(&store, "restore", "r-old")?.2;
    assert_eq!(restore["restore"]["plan"], r#"{"argv":[]}"#);
    assert_eq!(restore["restore"]["created_at"], old);
    assert_eq!(
        strings(&store, "SELECT CAST(id AS TEXT) FROM epoch ORDER BY id"),
        ["2", "3", "4"]
    );
    assert_eq!(
        strings(&store, "SELECT id FROM restore ORDER BY id"),
        ["r-current", "r-recent"]
    );
    assert_eq!(
        strings(
            &store,
            "SELECT native_id FROM conversation ORDER BY native_id"
        ),
        ["expired", "restored"]
    );
    Ok(())
}

/// A task whose document is over the limit stays in use, and so does the conversation it names,
/// while the same sweep archives another task and forgets that one's owner.
#[test]
fn a_document_over_the_limit_stays_in_use() -> Result<()> {
    let (_dir, mut store) = store();
    let old = old();
    store.conn().execute_batch(&format!(
        "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
           VALUES ('claude', 'big-owner', 1, 0), ('claude', 'small-owner', 1, 0);
         INSERT INTO task (id, title, owner_runtime, owner_native_id, worker_lineage, keep_pane,
           created_at)
           VALUES ('big', 'x', 'claude', 'big-owner', 'b1', 0, 0),
             ('small', 'x', 'claude', 'small-owner', 'b2', 0, 0);
         INSERT INTO task_round (task_id, round, state, finished_at, acked_at, created_at)
           VALUES ('small', 1, 'cancelled', {old}, {old}, 0);"
    ))?;
    store.conn().execute(
        "INSERT INTO task_round (task_id, round, state, finished_at, acked_at, created_at,
           result_ref, result)
         VALUES ('big', 1, 'completed', ?1, ?1, 0, 'm', ?2)",
        params![old, "x".repeat(DOCUMENT_LIMIT)],
    )?;
    sweep(&mut store, "i2", now_ms())?;
    assert_eq!(strings(&store, "SELECT entity_id FROM archive"), ["small"]);
    assert_eq!(strings(&store, "SELECT id FROM task"), ["big"]);
    assert_eq!(
        strings(&store, "SELECT native_id FROM conversation"),
        ["big-owner"]
    );
    Ok(())
}
