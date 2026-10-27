//! What a round's worker showed: its pane as last recorded and a non-interactive run's result
//! file, read before the transaction that judges the round.

use std::fs::File;
use std::io::{ErrorKind, Read};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::runtime::{Tracker, Turn};

/// The largest result file a non-interactive round saves.
const RESULT_LIMIT: u64 = 1 << 20;

/// What a round's worker showed, read before the transaction that judges it.
pub(super) enum Evidence {
    /// What an interactive worker's record shows for the round, or why it could not be read.
    Turn(Result<Reading, String>),
    /// A non-interactive run's pane and, once the pane showed it exited 0, its result file: the
    /// text, or why there is none. Only the daemon writes `block`, so the pane read before the
    /// transaction is the one inside it.
    Run {
        pane: Pane,
        file: Option<Result<String, String>>,
    },
}

/// A turn a round can take, with the task's position once it is taken.
pub(super) struct Take {
    pub turn: Turn,
    pub start: u64,
    pub position: u64,
}

/// An interactive worker's record as the round's tracker read it.
pub(super) struct Reading {
    /// The round's turn, once one finished.
    pub chosen: Option<Take>,
    /// The turn left open where the record ends, when it could be the round's: only the worker's
    /// pane closing or its process going idle ends it. `since` is when it started.
    pub open: Option<(Take, Option<i64>)>,
    /// The record shows the round's request received.
    pub received: bool,
    /// The record was replaced or cut.
    pub replaced: bool,
}

impl Reading {
    pub fn of(tracker: &Tracker) -> Self {
        let chosen = tracker.chosen().map(|sealed| Take {
            turn: sealed.turn.clone(),
            start: sealed.span.start,
            position: tracker.position_after(sealed.span),
        });
        let open = tracker.open().map(|(open, since)| {
            let take = Take {
                turn: open.turn,
                start: open.span.start,
                position: tracker.position_after(open.span),
            };
            (take, since)
        });
        Self {
            chosen,
            open,
            received: tracker.received(),
            replaced: tracker.replaced(),
        }
    }
}

/// Reads a non-interactive run's result file, bounded by `RESULT_LIMIT`.
pub(super) fn read_result(path: &str) -> Result<String, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(format!("exited 0 but `{path}` does not exist"));
        }
        Err(e) => return Err(format!("exited 0 but `{path}` could not be read: {e}")),
    };
    let mut text = String::new();
    if let Err(e) = file.take(RESULT_LIMIT + 1).read_to_string(&mut text) {
        return Err(format!("exited 0 but `{path}` could not be read: {e}"));
    }
    match u64::try_from(text.len()) {
        Ok(0) => Err(format!("exited 0 but `{path}` is empty")),
        Ok(len) if len <= RESULT_LIMIT => Ok(text),
        _ => Err(format!(
            "exited 0 but `{path}` is larger than {RESULT_LIMIT} bytes"
        )),
    }
}

#[derive(Default)]
pub(super) struct Pane {
    pub instance_id: Option<String>,
    pub status: Option<String>,
    pub status_pid: Option<i64>,
    pub status_at: Option<i64>,
    pub fg_pid: Option<i64>,
    pub last_exit: Option<i64>,
}

/// The process a non-interactive run is: the pane's foreground when the task was registered, on
/// the server instance it ran on, `None` for a task from before version 3 that no epoch dated.
pub(super) struct RunProcess<'a> {
    pub instance: Option<&'a str>,
    pub pid: Option<i64>,
}

/// Whether process `pid` on `instance` was registered in pane `lineage` as a non-interactive run.
/// A run registered before the pane had a foreground process may be any process there.
pub fn is_run(conn: &Connection, lineage: &str, instance: &str, pid: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM task WHERE worker_lineage = ?1 AND result_file IS NOT NULL
           AND (worker_pid IS NULL OR worker_pid = ?2)
           AND (worker_instance_id IS NULL OR worker_instance_id = ?3))",
        params![lineage, pid, instance],
        |row| row.get(0),
    )?)
}

impl Pane {
    pub fn read(conn: &Connection, lineage: &str) -> Result<Self> {
        Ok(conn
            .prepare_cached(
                "SELECT instance_id, status, status_pid, status_at, fg_pid, last_exit FROM block
                 WHERE lineage = ?1",
            )?
            .query_row(params![lineage], |row| {
                Ok(Self {
                    instance_id: row.get(0)?,
                    status: row.get(1)?,
                    status_pid: row.get(2)?,
                    status_at: row.get(3)?,
                    fg_pid: row.get(4)?,
                    last_exit: row.get(5)?,
                })
            })
            .optional()?
            .unwrap_or_default())
    }

    /// Whether a non-interactive run has ended with exit status 0.
    pub fn exited_zero(&self, run: &RunProcess) -> bool {
        self.ended(run) && self.last_exit == Some(0)
    }

    /// Whether the record shows the run's process gone. A record of another instance shows
    /// nothing of the run.
    pub fn ended(&self, run: &RunProcess) -> bool {
        run.instance
            .is_none_or(|instance| self.instance_id.as_deref() == Some(instance))
            && (self.fg_pid.is_none() || self.fg_pid != run.pid)
    }
}
