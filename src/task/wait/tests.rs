use std::process::{Child, Command};
use std::thread;

use super::*;
use crate::task::tests::{PROMPT, record, task_world};

/// Task `t`'s round 1 with `change` applied, and whether a waiter in `waiter`'s place returns.
fn returns_after(store: &mut Store, change: &str, waiter: Waiter) -> Result<Option<Returned>> {
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

/// Round 1 begins a blocked episode at `at`, with its notice `id` pending.
fn episode(at: i64, id: &str) -> String {
    format!(
        "UPDATE task_round SET state = 'blocked', blocked_at = {at};
         INSERT INTO notice (id, task_id, round, kind, state, created_at)
           VALUES ('{id}', 't', 1, 'blocked', 'pending', {at});"
    )
}

const RESUMED: &str = "UPDATE task_round SET state = 'running', blocked_at = NULL;
  UPDATE notice SET state = 'superseded' WHERE state = 'pending'";

const FINISHED: &str = "UPDATE task_round SET state = 'completed', result = 'Done.',
    result_ref = 'm1', finished_at = 9, blocked_at = NULL;
  INSERT INTO notice (id, task_id, round, kind, state, created_at)
    VALUES ('f', 't', 1, 'finished', 'pending', 9);";

#[test]
fn the_registered_waiter_returns_on_each_notice_it_takes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let waiter = Waiter::Registered;
    assert_eq!(returns_after(&mut store, "", waiter)?, None);
    assert_eq!(
        returns_after(&mut store, &episode(5, "b1"), waiter)?,
        Some(Returned::Blocked)
    );
    assert_eq!(notice_states(&store)?, [taken("blocked")]);
    // The owner has been told of this episode, so a later wait waits for what comes next.
    assert_eq!(returns_after(&mut store, "", waiter)?, None);
    // An episode whose notice the daemon sent does not end a wait either.
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

/// A waiter behind another live one takes no notice, and returns on an episode that began after it
/// came to stand behind it.
#[test]
fn a_waiter_behind_another_returns_on_a_later_episode() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let behind = |blocked_at| Waiter::Behind {
        holder: 1,
        blocked_at,
    };
    assert_eq!(
        returns_after(&mut store, &episode(5, "b1"), behind(Some(5)))?,
        None
    );
    assert_eq!(returns_after(&mut store, RESUMED, behind(Some(5)))?, None);
    assert_eq!(
        returns_after(&mut store, &episode(7, "b2"), behind(Some(5)))?,
        Some(Returned::Blocked)
    );
    assert_eq!(
        returns_after(&mut store, "", behind(None))?,
        Some(Returned::Blocked)
    );
    let notices = notice_states(&store)?;
    assert!(!notices.contains(&taken("blocked")), "{notices:?}");
    assert_eq!(
        returns_after(
            &mut store,
            &format!("{RESUMED}; {FINISHED}"),
            behind(Some(5))
        )?,
        Some(Returned::Finished)
    );
    Ok(())
}

/// A wait stands behind the holder its transaction reads unless that is the one it found gone
/// before the transaction, and keeps the episode it saw when it came to stand behind one, also
/// once another holder took the round meanwhile.
#[test]
fn a_wait_replaces_only_the_holder_it_found_gone() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let transcript = record(&[PROMPT])?;
    let mut store = task_world(dir.path(), transcript.path(), None)?;
    let stand_after = |store: &mut Store, change: &str, gone, before| -> Result<Waiter> {
        store.conn().execute_batch(change)?;
        let tx = store.write()?;
        let waiter = stand(&tx, "t", 1, 222, gone, before)?;
        tx.commit()?;
        Ok(waiter)
    };
    let behind = |holder| Waiter::Behind {
        holder,
        blocked_at: Some(5),
    };
    let holder = format!(
        "{}; UPDATE task_round SET waiter_pid = 111",
        episode(5, "b1")
    );
    let first = stand_after(&mut store, &holder, None, None)?;
    assert_eq!(first, behind(111));
    let later = format!("{RESUMED}; {}", episode(7, "b2"));
    assert_eq!(stand_after(&mut store, &later, None, Some(first))?, first);
    // Another wait replaced the holder this one found gone before its transaction.
    let replaced = "UPDATE task_round SET waiter_pid = 333";
    assert_eq!(
        stand_after(&mut store, replaced, Some(111), Some(first))?,
        behind(333)
    );
    assert_eq!(
        stand_after(&mut store, "", Some(333), Some(first))?,
        Waiter::Registered
    );
    let waiter: Option<i64> =
        store
            .conn()
            .query_row("SELECT waiter_pid FROM task_round", [], |row| row.get(0))?;
    assert_eq!(waiter, Some(222));
    Ok(())
}

/// A `task wait` on round 1 of the store in `dir` while its episode's notice is pending and a live
/// `sleep` holds the round, which `then` changes once the wait stands behind it: the wait takes the
/// notice and returns on it before its deadline.
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

/// The waiter ahead returned on its timeout without the notice, which is now this one's to take.
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

/// A waiter that dies writes nothing to the store; the one behind it learns of the death from the
/// exit itself.
#[test]
fn a_wait_behind_one_that_died_takes_the_notice_it_left() -> Result<()> {
    let dir = tempfile::tempdir()?;
    takes_over(dir.path(), |_, holder| Ok(holder.kill()?))
}
