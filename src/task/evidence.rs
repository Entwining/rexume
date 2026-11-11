use std::fs::File;
use std::io::{ErrorKind, Read};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::runtime::{Tracker, Turn};

const RESULT_LIMIT: u64 = 1 << 20;

pub(super) enum Evidence {
    Turn(Result<Reading, String>),
    /// Only the daemon writes `block`, so `pane`, read before the transaction, holds inside it.
    Run {
        pane: Pane,
        file: Option<Result<String, String>>,
    },
}

pub(super) struct Take {
    pub turn: Turn,
    pub start: u64,
    pub position: u64,
}

pub(super) struct Reading {
    pub chosen: Option<Take>,
    pub received: bool,
    pub replaced: bool,
}

impl Reading {
    pub fn of(tracker: &Tracker) -> Self {
        let chosen = tracker.chosen().map(|sealed| Take {
            turn: sealed.turn.clone(),
            start: sealed.span.start,
            position: tracker.position_after(sealed.span),
        });
        Self {
            chosen,
            received: tracker.received(),
            replaced: tracker.replaced(),
        }
    }
}

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
    pub status_kind: Option<String>,
    pub status_message: Option<String>,
    pub status_pid: Option<i64>,
    pub status_at: Option<i64>,
    pub fg_pid: Option<i64>,
    pub last_exit: Option<i64>,
    pub current: bool,
}

pub(super) struct RunProcess<'a> {
    pub instance: Option<&'a str>,
    pub pid: Option<i64>,
}

pub fn is_run(conn: &Connection, lineage: &str, instance: &str, pid: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM task WHERE worker_lineage = ?1 AND result_file IS NOT NULL
           AND (worker_pid IS NULL OR worker_pid = ?2)
           AND (worker_instance_id IS NULL OR worker_instance_id = ?3))",
        params![lineage, pid, instance],
        |row| row.get(0),
    )?)
}

pub(super) const PANE: &str = "SELECT b.instance_id, b.status, b.status_kind, b.status_message,
    b.status_pid, b.status_at, b.fg_pid, b.last_exit, b.instance_id IS e.instance_id
  FROM block b LEFT JOIN epoch e ON e.id = (SELECT MAX(id) FROM epoch)
  WHERE b.lineage = ?1";

impl Pane {
    pub fn read(conn: &Connection, lineage: &str) -> Result<Self> {
        Ok(conn
            .prepare_cached(PANE)?
            .query_row(params![lineage], |row| {
                Ok(Self {
                    instance_id: row.get(0)?,
                    status: row.get(1)?,
                    status_kind: row.get(2)?,
                    status_message: row.get(3)?,
                    status_pid: row.get(4)?,
                    status_at: row.get(5)?,
                    fg_pid: row.get(6)?,
                    last_exit: row.get(7)?,
                    current: row.get(8)?,
                })
            })
            .optional()?
            .unwrap_or_default())
    }

    pub fn exited_zero(&self, run: &RunProcess) -> bool {
        self.ended(run) && self.last_exit == Some(0)
    }

    pub fn ended(&self, run: &RunProcess) -> bool {
        run.instance
            .is_none_or(|instance| self.instance_id.as_deref() == Some(instance))
            && (self.fg_pid.is_none() || self.fg_pid != run.pid)
    }
}
