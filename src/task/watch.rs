//! Follows an interactive task's worker from `task add` until `task release`, between rounds as well
//! as during them: a dialog it waits on, and the turns it finishes that no round took.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;

use super::blocked::{self, OPEN_EPISODE};
use super::evidence::{self, PANE_COLUMNS, Pane};
use super::notice::{self, Kind};
use super::{Interest, OPEN};
use crate::runtime::{Runtime, Tracker};
use crate::store::{Store, now_ms};

/// The tasks the daemon watches, with the pane `b` and process `o` of each one's conversation.
pub(super) static WATCHED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {PANE_COLUMNS}, t.id, t.worker_runtime, t.worker_native_id, c.transcript,
           t.position, o.pid, r.round, e.id IS NOT NULL,
           (SELECT MAX(m.round) FROM task_round m WHERE m.task_id = t.id),
           (SELECT MAX(n.unread_end) FROM notice n WHERE n.task_id = t.id),
           EXISTS (SELECT 1 FROM notice n WHERE n.task_id = t.id AND n.state = 'pending')
         FROM task t
         JOIN conversation c ON c.runtime = t.worker_runtime AND c.native_id = t.worker_native_id
         LEFT JOIN block b ON b.lineage = t.worker_lineage
         LEFT JOIN epoch i ON i.id = (SELECT MAX(id) FROM epoch)
         LEFT JOIN occupant o ON o.block_lineage = t.worker_lineage
           AND o.runtime = t.worker_runtime AND o.native_id = t.worker_native_id
           AND (o.instance_id IS NULL OR o.instance_id = b.instance_id)
         LEFT JOIN task_round r ON r.task_id = t.id AND {OPEN}
         LEFT JOIN blocked_episode e ON e.task_id = t.id AND {OPEN_EPISODE}
         WHERE t.result_file IS NULL AND t.released_at IS NULL AND t.pane_state != 'closed'"
    )
});

struct Watched {
    pane: Pane,
    task: String,
    runtime: Runtime,
    native_id: String,
    transcript: Option<String>,
    position: i64,
    process: Option<i64>,
    round: Option<i64>,
    blocked: bool,
    latest: i64,
    reported: Option<i64>,
    pending: bool,
}

#[derive(Default)]
pub(super) struct Watch {
    /// Keyed by the task's latest round: the rounds the tracker knows to have taken turns are those
    /// that existed when it was made.
    trackers: HashMap<(String, i64), Tracker>,
    next: Option<Instant>,
}

/// What a pass read before its transaction.
struct Observed {
    /// Where the unread turns of each task with no open round end, keyed as the trackers are.
    ends: HashMap<(String, i64), i64>,
    /// Whether an episode or an unread notice may need writing.
    write: bool,
}

impl Watch {
    pub fn deadline(&self) -> Option<Instant> {
        self.next
    }

    pub fn pass(&mut self, store: &mut Store, interest: &mut Interest) -> Result<()> {
        let observed = self.observe(store, interest)?;
        self.apply(store, &observed)
    }

    /// Reads the watched tasks and, for each with no open round, its worker's record, outside any
    /// transaction.
    fn observe(&mut self, store: &Store, interest: &mut Interest) -> Result<Observed> {
        let watched = read(store.conn())?;
        let mut kept = HashSet::new();
        let mut ends = HashMap::new();
        for task in &watched {
            if task.round.is_none()
                && let Some(end) = self.unread_end(store, task, interest, &mut kept)?
            {
                ends.insert((task.task.clone(), task.latest), end);
            }
        }
        self.trackers.retain(|key, _| kept.contains(key));
        let write = watched
            .iter()
            .any(|task| changing(task) || unread_untold(task, &ends));
        Ok(Observed { ends, write })
    }

    fn apply(&mut self, store: &mut Store, observed: &Observed) -> Result<()> {
        if !observed.write {
            self.next = None;
            return Ok(());
        }
        let mut recheck: Option<i64> = None;
        let tx = store.write()?;
        // A task command may have released the task, started a round, or ended the episode since
        // the read.
        for task in &read(&tx)? {
            let id = &task.task;
            if changing(task)
                && let Some(at) =
                    blocked::follow(&tx, id, task.round, task.blocked, &task.pane, task.process)?
            {
                recheck = Some(recheck.map_or(at, |r| r.min(at)));
            }
            if unread_untold(task, &observed.ends) {
                notice::write(&tx, Kind::Unread, id, None, now_ms(), None)?;
            }
        }
        tx.commit()?;
        self.next = recheck
            .map(|at| -> Result<Instant> {
                Ok(Instant::now() + Duration::from_millis(u64::try_from((at - now_ms()).max(0))?))
            })
            .transpose()?;
        Ok(())
    }

    /// Where the last finished turn no round took ends in the worker's record.
    fn unread_end(
        &mut self,
        store: &Store,
        task: &Watched,
        interest: &mut Interest,
        kept: &mut HashSet<(String, i64)>,
    ) -> Result<Option<i64>> {
        let Some(path) =
            task.runtime
                .record(store.dir(), &task.native_id, task.transcript.as_deref())
        else {
            return Ok(None);
        };
        let key = (task.task.clone(), task.latest);
        let tracker = match self.trackers.get_mut(&key) {
            Some(tracker) if tracker.path() == path => tracker,
            _ => {
                let position = u64::try_from(task.position)?;
                let taken = evidence::taken(store.conn(), &task.task)?;
                let tracker = Tracker::new(task.runtime, &path, position, taken, None);
                self.trackers
                    .entry(key.clone())
                    .insert_entry(tracker)
                    .into_mut()
            }
        };
        if let Err(e) = tracker.update() {
            log::debug!(
                "task {}: the worker's record could not be read: {e:#}",
                task.task
            );
            return Ok(None);
        }
        interest.files.insert(path);
        kept.insert(key);
        let Some(end) = tracker.unread_end().filter(|_| !tracker.replaced()) else {
            return Ok(None);
        };
        Ok(Some(i64::try_from(end)?))
    }
}

fn read(conn: &Connection) -> Result<Vec<Watched>> {
    Ok(conn
        .prepare_cached(&WATCHED)?
        .query_map([], |row| {
            Ok(Watched {
                pane: Pane::from_row(row)?,
                task: row.get(9)?,
                runtime: row.get(10)?,
                native_id: row.get(11)?,
                transcript: row.get(12)?,
                position: row.get(13)?,
                process: row.get(14)?,
                round: row.get(15)?,
                blocked: row.get(16)?,
                latest: row.get(17)?,
                reported: row.get(18)?,
                pending: row.get(19)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Whether the task's episode begins or ends with what its pane reports.
fn changing(task: &Watched) -> bool {
    task.blocked != blocked::reported(&task.pane, task.process)
}

/// Whether the task, with no round open and no notice pending, has turns past those its notices
/// reported. A notice still pending reports them itself when it is sent.
fn unread_untold(task: &Watched, ends: &HashMap<(String, i64), i64>) -> bool {
    task.round.is_none()
        && !task.pending
        && ends
            .get(&(task.task.clone(), task.latest))
            .is_some_and(|&end| task.reported.is_none_or(|reported| end > reported))
}

#[cfg(test)]
mod tests;
