use std::process::{Child, Command};
use std::thread;

use super::*;
use crate::task::tests::{PROMPT, record, task_world};

fn returns_after(store: &mut Store, change: &str, waiter: &Waiter) -> Result<Option<Returned>> {
    store.conn().execute_batch(change)?;
    let tx = store.write()?;
    let returned = returns(&tx, "t", 1, waiter)?;
    tx.commit()?;
    Ok(returned)
}

fn notice_states(store: &Store) -> Result<Vec<(String, String, Option<String>)>> {
    Ok(store
        .conn()
        .prepare("SELECT kind, state, note FROM notice ORDER BY rowid")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn taken(kind: &str) -> (String, String, Option<String>) {
    (
        kind.into(),
        "superseded".into(),
        Some("taken by task wait".into()),
    )
}

fn episode(at: i64, id: &str) -> String {
    format!(
        "UPDATE task_round SET state = 'blocked';
         INSERT INTO blocked_episode (id, task_id, round, started_at) VALUES ('e{id}', 't', 1, {at});
         INSERT INTO notice (id, task_id, round, kind, blocked_episode_id, state, created_at)
           VALUES ('{id}', 't', 1, 'blocked', 'e{id}', 'pending', {at});"
    )
}

fn seen(id: &str) -> Option<String> {
    Some(format!("e{id}"))
}

const RESUMED: &str = "UPDATE task_round SET state = 'running';
  UPDATE blocked_episode SET ended_at = 8, end_reason = 'status_changed' WHERE ended_at IS NULL;
  UPDATE notice SET state = 'superseded' WHERE state = 'pending'";

const FINISHED: &str = "UPDATE task_round SET state = 'completed', result = 'Done.',
    result_ref = 'm1', finished_at = 9;
  INSERT INTO notice (id, task_id, round, kind, state, created_at)
    VALUES ('f', 't', 1, 'finished', 'pending', 9);";

#[test]
fn the_registered_waiter_returns_on_each_notice_it_takes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let waiter = &Waiter::Registered;
    assert_eq!(returns_after(&mut store, "", waiter)?, None);
    assert_eq!(
        returns_after(&mut store, &episode(5, "b1"), waiter)?,
        Some(Returned::Blocked)
    );
    assert_eq!(notice_states(&store)?, [taken("blocked")]);
    let taken_at: Option<i64> =
        store
            .conn()
            .query_row("SELECT superseded_at FROM notice", [], |row| row.get(0))?;
    assert!(
        taken_at.is_some(),
        "the wait did not record when it took the notice"
    );
    assert_eq!(returns_after(&mut store, "", waiter)?, None);
    let sent = format!(
        "{RESUMED}; {} UPDATE notice SET state = 'sent' WHERE id = 'b2'",
        episode(7, "b2")
    );
    assert_eq!(returns_after(&mut store, &sent, waiter)?, None);
    assert_eq!(
        returns_after(&mut store, &format!("{RESUMED}; {FINISHED}"), waiter)?,
        Some(Returned::Finished)
    );
    assert_eq!(
        notice_states(&store)?,
        [
            taken("blocked"),
            ("blocked".into(), "sent".into(), None),
            taken("finished")
        ]
    );
    Ok(())
}

#[test]
fn a_waiter_behind_another_returns_on_a_later_episode() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let behind = |episode| Waiter::Behind { holder: 1, episode };
    assert_eq!(
        returns_after(&mut store, &episode(5, "b1"), &behind(seen("b1")))?,
        None
    );
    assert_eq!(
        returns_after(&mut store, RESUMED, &behind(seen("b1")))?,
        None
    );
    assert_eq!(
        returns_after(&mut store, &episode(7, "b2"), &behind(seen("b1")))?,
        Some(Returned::Blocked)
    );
    assert_eq!(
        returns_after(&mut store, "", &behind(None))?,
        Some(Returned::Blocked)
    );
    let notices = notice_states(&store)?;
    assert!(!notices.contains(&taken("blocked")), "{notices:?}");
    assert_eq!(
        returns_after(
            &mut store,
            &format!("{RESUMED}; {FINISHED}"),
            &behind(seen("b1"))
        )?,
        Some(Returned::Finished)
    );
    Ok(())
}

#[test]
fn a_wait_replaces_only_the_holder_it_found_gone() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let stand_after =
        |store: &mut Store, change: &str, gone, before: Option<&Waiter>| -> Result<Waiter> {
            store.conn().execute_batch(change)?;
            let tx = store.write()?;
            let waiter = stand(&tx, "t", 1, 222, gone, before)?;
            tx.commit()?;
            Ok(waiter)
        };
    let behind = |holder| Waiter::Behind {
        holder,
        episode: seen("b1"),
    };
    let holder = format!(
        "{}; UPDATE task_round SET waiter_pid = 111",
        episode(5, "b1")
    );
    let first = stand_after(&mut store, &holder, None, None)?;
    assert_eq!(first, behind(111));
    let later = format!("{RESUMED}; {}", episode(7, "b2"));
    assert_eq!(stand_after(&mut store, &later, None, Some(&first))?, first);
    let replaced = "UPDATE task_round SET waiter_pid = 333";
    assert_eq!(
        stand_after(&mut store, replaced, Some(111), Some(&first))?,
        behind(333)
    );
    assert_eq!(
        stand_after(&mut store, "", Some(333), Some(&first))?,
        Waiter::Registered
    );
    let waiter: Option<i64> =
        store
            .conn()
            .query_row("SELECT waiter_pid FROM task_round", [], |row| row.get(0))?;
    assert_eq!(waiter, Some(222));
    Ok(())
}

fn takes_over(dir: &Path, then: impl FnOnce(&Store, &mut Child) -> Result<()>) -> Result<()> {
    let transcript = record(&[PROMPT])?;
    let store = task_world(dir, transcript.path(), None)?;
    let mut holder = Command::new("sleep").arg("30").spawn()?;
    store.conn().execute_batch(&format!(
        "{}; UPDATE task_round SET waiter_pid = {}",
        episode(5, "b1"),
        holder.id()
    ))?;
    let state_dir = dir.to_owned();
    let waiting = thread::spawn(move || -> Result<(Returned, bool)> {
        let mut store = Store::open(&state_dir)?;
        let until = Instant::now() + Duration::from_secs(5);
        let returned = wait_for(&mut store, &state_dir.join("state.db-wal"), "t", 1, until)?;
        Ok((returned, Instant::now() < until))
    });
    thread::sleep(Duration::from_millis(300));
    let changed = then(&store, &mut holder);
    holder.kill()?;
    holder.wait()?;
    changed?;
    let (returned, early) = waiting
        .join()
        .map_err(|_| anyhow::anyhow!("the wait panicked"))??;
    assert_eq!(returned, Returned::Blocked);
    assert!(early, "the wait returned only at its deadline");
    assert_eq!(notice_states(&store)?, [taken("blocked")]);
    Ok(())
}

#[test]
fn a_wait_behind_one_that_timed_out_takes_the_round_and_its_notice() -> Result<()> {
    let dir = tempfile::tempdir()?;
    takes_over(dir.path(), |store, _| {
        store
            .conn()
            .execute_batch("UPDATE task_round SET waiter_pid = NULL")?;
        Ok(())
    })
}

#[test]
fn a_wait_behind_one_that_died_takes_the_notice_it_left() -> Result<()> {
    let dir = tempfile::tempdir()?;
    takes_over(dir.path(), |_, holder| Ok(holder.kill()?))
}
