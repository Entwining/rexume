//! The pane of a released disposable worker that nothing used after the worker's result, which
//! the server restored with the rest. Rexume would have closed it had the server not stopped, so
//! the daemon closes it once it reads that the pane shows only the shell the server started there
//! with nothing typed and is not its session's focused pane, as cleanup closes one; a pane in use,
//! focused, or not checked when the wait ends is kept.

use super::{PANE_WAIT, Restorer, located, millis, pane};
use crate::rex::Rex;
use crate::store::{Store, now_ms};
use crate::task;
use anyhow::Result;
use pane::Pane;

pub(super) struct Retired {
    task: String,
    pub(super) block: String,
    pub(super) until: i64,
}

/// The panes held for a close, each waited for as long as a restore waits for its pane.
pub(super) fn held(store: &Store) -> Result<Vec<Retired>> {
    let until = now_ms() + millis(PANE_WAIT);
    Ok(task::held(store.conn())?
        .into_iter()
        .map(|(task, block)| Retired { task, block, until })
        .collect())
}

impl Restorer {
    /// Closes the held panes that are ready, keeps those found in use past their wait, and returns
    /// when to look at the rest again.
    pub(super) fn close_retired(
        &mut self,
        rex: &mut Rex,
        store: &mut Store,
        instance: &str,
    ) -> Vec<i64> {
        let mut due = Vec::new();
        for retired in std::mem::take(&mut self.retired) {
            match self.try_close(rex, store, instance, &retired) {
                Ok(false) => {
                    due.push(retired.until);
                    self.retired.push(retired);
                }
                Ok(true) => {}
                Err(e) if now_ms() < retired.until => {
                    log::error!("task {}: closing its worker's pane: {e:#}", retired.task);
                    due.push(retired.until);
                    self.retired.push(retired);
                }
                // Reading again at once would fail the same way, with nothing to wake it later.
                Err(e) => {
                    let reason = format!("the pane could not be checked: {e:#}");
                    match keep(store, &retired.task, &reason) {
                        Ok(()) => log::warn!("task {}: worker pane kept: {reason}", retired.task),
                        Err(e) => {
                            log::error!("task {}: keeping its worker's pane: {e:#}", retired.task)
                        }
                    }
                }
            }
        }
        due
    }

    /// Returns whether the pane is settled: closed, gone, or kept.
    fn try_close(
        &mut self,
        rex: &mut Rex,
        store: &mut Store,
        instance: &str,
        retired: &Retired,
    ) -> Result<bool> {
        let now = now_ms();
        if self.unchanged(store, &retired.block, retired.until, now)? {
            return Ok(false);
        }
        let located = located(rex, store, instance, &retired.block)?;
        let pane = match &located {
            Some((session, live)) => pane::read(rex, session, live)?,
            None => Pane::Gone,
        };
        let kept = match (pane, located) {
            (Pane::Shell, Some((session, live))) => task::close_unfocused(rex, &session, &live)?,
            (Pane::Busy(reason), _) => Some(reason),
            _ => None,
        };
        // A pane still there is read again until the wait ends, and then left to the user.
        if kept.is_some() && now < retired.until {
            return Ok(false);
        }
        match &kept {
            Some(reason) => keep(store, &retired.task, reason)?,
            None => {
                let tx = store.write()?;
                task::block_gone(&tx, &retired.block)?;
                tx.commit()?;
            }
        }
        match kept {
            Some(reason) => log::info!("task {}: worker pane kept: {reason}", retired.task),
            None => log::info!("task {}: worker pane closed", retired.task),
        }
        Ok(true)
    }
}

fn keep(store: &mut Store, task: &str, reason: &str) -> Result<()> {
    let tx = store.write()?;
    task::keep(&tx, task, reason)?;
    tx.commit()?;
    Ok(())
}
