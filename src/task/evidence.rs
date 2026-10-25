//! What a round's worker showed: its pane as last recorded and a non-interactive run's result
//! file, read before the transaction that judges the round.

use std::fs::File;
use std::io::{ErrorKind, Read};

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::runtime::Turn;

/// The largest result file a non-interactive round saves.
const RESULT_LIMIT: u64 = 1 << 20;

/// What a round's worker showed, read before the transaction that judges it.
pub(super) enum Evidence {
    /// An interactive worker's last turn after the round's cursor, with the offset after it.
    Turn { turn: Turn, end: u64 },
    /// A non-interactive run's pane and, once the pane showed it exited 0, its result file: the
    /// text, or why there is none. Only the daemon writes `block`, so the pane read before the
    /// transaction is the one inside it.
    Run {
        pane: Pane,
        file: Option<Result<String, String>>,
    },
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
    pub status: Option<String>,
    pub status_pid: Option<i64>,
    pub status_at: Option<i64>,
    pub fg_pid: Option<i64>,
    pub last_exit: Option<i64>,
}

impl Pane {
    pub fn read(conn: &rusqlite::Connection, lineage: &str) -> Result<Self> {
        Ok(conn
            .prepare_cached(
                "SELECT status, status_pid, status_at, fg_pid, last_exit FROM block WHERE lineage = ?1",
            )?
            .query_row(
                params![lineage],
                |row| {
                    Ok(Self {
                        status: row.get(0)?,
                        status_pid: row.get(1)?,
                        status_at: row.get(2)?,
                        fg_pid: row.get(3)?,
                        last_exit: row.get(4)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Whether a non-interactive run, registered with `worker_pid` as the pane's foreground,
    /// has ended with exit status 0.
    pub fn exited_zero(&self, worker_pid: Option<i64>) -> bool {
        self.ended(worker_pid) && self.last_exit == Some(0)
    }

    pub fn ended(&self, worker_pid: Option<i64>) -> bool {
        self.fg_pid.is_none() || self.fg_pid != worker_pid
    }
}
