//! The daemon's side of tasks: every pass reads the current state again, so any wake-up, whatever
//! caused it, brings every task up to date.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::evidence::{self, Evidence, Pane, Reading};
use super::outcome;
use super::{OPEN, cleanup, notice, request};
use crate::rex::Rex;
use crate::runtime::{Receipt, Runtime, Taken, Tracker};
use crate::store::{Store, now_ms};

/// What the daemon asks to be woken for: changes to these files and these processes' exits.
#[derive(Default)]
pub struct Interest {
    pub files: HashSet<PathBuf>,
    pub pids: HashSet<i64>,
}

/// Moves tasks along from the daemon's event loop.
#[derive(Default)]
pub struct Follow {
    trackers: HashMap<(String, i64), Tracker>,
    /// One reader per owner record, shared by every notice sent to it.
    receipts: HashMap<PathBuf, Receipt>,
    next_notice: Option<Instant>,
    /// When a round that only time can settle is read again.
    next_round: Option<Instant>,
    /// Released workers whose close was tried since the server last changed a session's view.
    tried: HashSet<String>,
}

/// The rounds that have not finished, with the record each follows and its request.
pub(super) static OPEN_ROUNDS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT r.task_id, r.round, t.result_file, t.worker_lineage, t.worker_pid, t.created_at,
           t.worker_runtime, c.transcript, t.position, q.id
         FROM task_round r JOIN task t ON t.id = r.task_id
         LEFT JOIN conversation c ON c.runtime = t.worker_runtime AND c.native_id = t.worker_native_id
         LEFT JOIN request q ON q.task_id = r.task_id AND q.round = r.round
         WHERE {OPEN}"
    )
});

/// The turns a task's rounds took, by where each starts and by its answer.
pub(super) const TAKEN: &str = "SELECT turn_start, result_ref FROM task_round WHERE task_id = ?1";

struct OpenRound {
    task: String,
    round: i64,
    result_file: Option<String>,
    worker: String,
    worker_pid: Option<i64>,
    created_at: i64,
    runtime: Option<Runtime>,
    transcript: Option<String>,
    position: i64,
    request: Option<String>,
}

impl Follow {
    pub fn deadline(&self) -> Option<Instant> {
        self.next_notice.into_iter().chain(self.next_round).min()
    }

    /// The pane that has focus may have changed, so closes that were put off are tried again.
    pub fn view_changed(&mut self) {
        self.tried.clear();
    }

    /// Brings every task up to date. `connected` is the daemon's connection and the server
    /// instance it observes; without one, no pane is closed.
    pub fn pass(
        &mut self,
        store: &mut Store,
        connected: Option<(&mut Rex, &str)>,
    ) -> Result<Interest> {
        let mut interest = Interest::default();
        self.evaluate_open(store, &mut interest)?;
        request::send_due(store, &mut interest)?;
        self.next_notice = notice::send_due(store, &mut interest)?;
        notice::settle(store, &mut self.receipts, &mut interest)?;
        if let Some((rex, instance)) = connected {
            for id in cleanup::due(store.conn())? {
                if self.tried.contains(&id) {
                    continue;
                }
                if let Some(outcome) = cleanup::close(rex, store, instance, &id)? {
                    log::info!("task {id}: worker pane {outcome}");
                    self.tried.insert(id);
                }
            }
        }
        Ok(interest)
    }

    fn evaluate_open(&mut self, store: &mut Store, interest: &mut Interest) -> Result<()> {
        let open: Vec<OpenRound> = store
            .conn()
            .prepare_cached(&OPEN_ROUNDS)?
            .query_map([], |row| {
                Ok(OpenRound {
                    task: row.get(0)?,
                    round: row.get(1)?,
                    result_file: row.get(2)?,
                    worker: row.get(3)?,
                    worker_pid: row.get(4)?,
                    created_at: row.get(5)?,
                    runtime: row.get(6)?,
                    transcript: row.get(7)?,
                    position: row.get(8)?,
                    request: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut kept = HashSet::new();
        let mut evidence = Vec::new();
        for mut round in open {
            let key = (round.task.clone(), round.round);
            let shown = match &round.result_file {
                Some(file) => {
                    // Read only once the run exited 0, and before the transaction.
                    let pane = Pane::read(store.conn(), &round.worker)?;
                    let file = pane
                        .exited_zero(round.worker_pid)
                        .then(|| evidence::read_result(file));
                    Evidence::Run { pane, file }
                }
                None => {
                    if round.runtime.is_none() {
                        bind(store, &mut round)?;
                    }
                    let (Some(runtime), Some(transcript)) = (round.runtime, &round.transcript)
                    else {
                        continue;
                    };
                    let path = PathBuf::from(transcript);
                    let tracker = match self.trackers.get_mut(&key) {
                        Some(tracker) if tracker.path() == path => tracker,
                        _ => {
                            let position = u64::try_from(round.position)?;
                            let taken = taken(store.conn(), &round.task)?;
                            let tracker = Tracker::new(
                                runtime,
                                &path,
                                position,
                                taken,
                                round.request.as_deref(),
                            );
                            self.trackers
                                .entry(key.clone())
                                .insert_entry(tracker)
                                .into_mut()
                        }
                    };
                    tracker.update()?;
                    interest.files.insert(path);
                    kept.insert(key);
                    Evidence::Turn(Reading::of(tracker))
                }
            };
            evidence.push((round.task, round.round, shown));
        }
        self.trackers.retain(|key, _| kept.contains(key));
        if evidence.is_empty() {
            self.next_round = None;
            return Ok(());
        }
        let mut recheck: Option<i64> = None;
        let tx = store.write()?;
        for (task, round, shown) in evidence {
            if let Some(at) = outcome::evaluate(&tx, &task, round, shown)? {
                recheck = Some(recheck.map_or(at, |r| r.min(at)));
            }
        }
        tx.commit()?;
        self.next_round = recheck
            .map(|at| -> Result<Instant> {
                Ok(Instant::now() + Duration::from_millis(u64::try_from((at - now_ms()).max(0))?))
            })
            .transpose()?;
        Ok(())
    }
}

pub(super) fn taken(conn: &Connection, task: &str) -> Result<Taken> {
    let rows: Vec<(Option<i64>, Option<String>)> = conn
        .prepare_cached(TAKEN)?
        .query_map(params![task], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut taken = Taken::default();
    for (start, answer) in rows {
        taken.starts.extend(start.map(u64::try_from).transpose()?);
        taken.answers.extend(answer);
    }
    Ok(taken)
}

/// Binds an interactive task registered before its worker reported to the conversation that
/// reported from the pane since the task was registered, or that runs as its foreground program.
/// The binding is made once; a conversation that takes over the pane later is not the task's.
fn bind(store: &mut Store, round: &mut OpenRound) -> Result<()> {
    let found: Option<(Runtime, String, Option<String>)> = store
        .conn()
        .prepare_cached(
            "SELECT o.runtime, o.native_id, c.transcript FROM occupant o
             JOIN conversation c ON c.runtime = o.runtime AND c.native_id = o.native_id
             LEFT JOIN block b ON b.lineage = o.block_lineage
             WHERE o.block_lineage = ?1 AND (o.reported_at >= ?2 OR o.pid = b.fg_pid)",
        )?
        .query_row(params![round.worker, round.created_at], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?;
    let Some((runtime, native_id, transcript)) = found else {
        return Ok(());
    };
    // The update binds only a task still unbound, so a binding made since the read stands.
    let tx = store.write()?;
    let bound = tx.execute(
        "UPDATE task SET worker_runtime = ?1, worker_native_id = ?2
         WHERE id = ?3 AND worker_runtime IS NULL",
        params![runtime.as_str(), native_id, round.task],
    )?;
    tx.commit()?;
    if bound == 1 {
        round.runtime = Some(runtime);
        round.transcript = transcript;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
