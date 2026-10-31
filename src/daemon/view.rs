//! What the daemon keeps of the server's panes during one epoch: each pane's last reported state,
//! and the sessions whose views it reads again when they change, for the panes the stream has not
//! reported yet.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};

use super::block::{foreground_pid, recorded, set_foreground, set_status, touch_block};
use crate::rex::{CallError, Rex, is_root, items, root_status, str_at, view_blocks};
use crate::store::{Store, ensure_lineage, lineage_of, now_ms};
use crate::{proc, task};

/// The least time between two reads of a session's view: a change soon after a read waits for
/// the rest of this interval, so a burst of changes is read once more after its first.
const COALESCE: Duration = Duration::from_millis(300);

pub(super) struct View {
    instance: String,
    /// Sessions whose view changed, by live ID, with when their view is read next.
    dirty: HashMap<String, Instant>,
    /// When each session's view was last read.
    read_at: HashMap<String, Instant>,
    /// The view revision last read for each session.
    revisions: HashMap<String, i64>,
}

/// A pane event the daemon records.
enum Change<'a> {
    Foreground(Option<&'a Value>, Option<i64>),
    Status(Option<&'a Value>),
    Exit(Option<i64>),
}

impl View {
    pub(super) fn new(instance: &str) -> Self {
        Self {
            instance: instance.to_owned(),
            dirty: HashMap::new(),
            read_at: HashMap::new(),
            revisions: HashMap::new(),
        }
    }

    /// Reads every pane the server holds now. On the instance it was observed on, the server is
    /// the authority: panes missing from it were closed while nobody watched. So is a new instance
    /// for a worker's pane, since the server restores every pane it saved: one it does not show was
    /// closed before the earlier instance ended, and its rounds end as when its close is seen. That
    /// holds for a worker registered while no daemon ran, which has no row in `block`, but not for
    /// one registered after the server listed its sessions, whose pane the list may predate. Only
    /// a session the server says does not exist counts as missing; any other refusal aborts the
    /// baseline, since forgetting a pane cannot be undone.
    pub(super) fn baseline(&mut self, rex: &mut Rex, store: &mut Store) -> Result<()> {
        let listed_at = now_ms();
        let list = rex.call("session.list", &json!({}))?;
        let mut blocks = Vec::new();
        for live in items(&list, "/sessions")
            .iter()
            .filter_map(|s| str_at(s, "/session_id"))
        {
            let view = match rex.call("session.view", &json!({ "session_id": live })) {
                Ok(view) => view,
                Err(e) if e.not_found() => continue,
                Err(e) => return Err(e.into()),
            };
            self.read(live, &view);
            for block in view_blocks(&view) {
                self.read_block(rex, store, live, &block)?;
                blocks.push(block);
            }
        }
        let tx = store.write()?;
        let seen: Vec<String> = blocks
            .iter()
            .map(|b| ensure_lineage(&tx, &self.instance, b))
            .collect::<Result<_>>()?;
        let recorded: Vec<String> = tx
            .prepare(
                "SELECT lineage FROM block WHERE instance_id = ?1
                 UNION SELECT worker_lineage FROM task WHERE pane_state != 'closed' AND created_at < ?2",
            )?
            .query_map(params![self.instance, listed_at], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for lineage in recorded.iter().filter(|l| !seen.contains(l)) {
            task::block_gone(&tx, lineage)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn read(&mut self, live: &str, view: &Value) {
        self.read_at.insert(live.to_owned(), Instant::now());
        if let Some(revision) = view.get("revision").and_then(Value::as_i64) {
            self.revisions.insert(live.to_owned(), revision);
        }
    }

    fn read_block(
        &self,
        rex: &mut Rex,
        store: &mut Store,
        session: &str,
        block: &str,
    ) -> Result<()> {
        let process = rex.block_method(session, block, "process").ok();
        let status = rex.block_method(session, block, "program_status").ok();
        let parent = process
            .as_ref()
            .and_then(|p| foreground_pid(p.get("foreground")))
            .and_then(proc::parent);
        let tx = store.write()?;
        let lineage = touch_block(&tx, &self.instance, block)?;
        if let Some(process) = process {
            set_foreground(&tx, &lineage, process.get("foreground"), None, parent)?;
            if let Some(code) = process
                .pointer("/last_exit/exit_code")
                .and_then(Value::as_i64)
            {
                tx.execute(
                    "UPDATE block SET last_exit = ?1 WHERE lineage = ?2",
                    params![code, lineage],
                )?;
            }
        }
        // A failed read says nothing about the status, and a server that dies mid-read must not
        // erase the `working` a restore continues from.
        if let Some(status) = status {
            set_status(&tx, &lineage, root_status(&status))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Schedules a read of the session's view. A change long after the last read is read at once,
    /// so a crash right after a single change loses nothing.
    fn mark_dirty(&mut self, live: &str) {
        let due = next_read(self.read_at.get(live).copied(), Instant::now());
        self.dirty.entry(live.to_owned()).or_insert(due);
    }

    /// Records one event, and returns whether it may have moved a worker task or a pane a restore
    /// waits for.
    pub(super) fn handle(&mut self, store: &mut Store, event: &Value) -> Result<bool> {
        let session = str_at(event, "/session_id");
        let payload = event.get("payload").unwrap_or(&Value::Null);
        match str_at(event, "/event") {
            Some("session_created") => {
                if let Some(live) = str_at(payload, "/session/session_id") {
                    self.mark_dirty(live);
                }
                Ok(false)
            }
            Some("session_view_changed") => {
                let Some(live) = session else {
                    return Ok(false);
                };
                let revision = payload.get("revision").and_then(Value::as_i64);
                // A view already read at this revision, such as one this daemon just read, needs
                // no second read.
                if revision.is_none_or(|r| self.revisions.get(live).is_none_or(|saved| r > *saved))
                {
                    self.mark_dirty(live);
                }
                Ok(true)
            }
            // A close seen on a live stream is a real close: a server that stops ends the stream
            // without any.
            Some("block_closed") => {
                let Some(block) = str_at(payload, "/block_id").or(str_at(event, "/block_id"))
                else {
                    return Ok(false);
                };
                let tx = store.write()?;
                if let Some(lineage) = lineage_of(&tx, &self.instance, block)? {
                    task::block_gone(&tx, &lineage)?;
                }
                tx.commit()?;
                Ok(true)
            }
            Some(name) => self.block_event(store, name, event, payload),
            None => Ok(false),
        }
    }

    /// Records one pane's event, and returns whether it may have moved a worker task or a pane a
    /// restore waits for. An event the daemon does not record opens no transaction.
    pub(super) fn block_event(
        &self,
        store: &mut Store,
        name: &str,
        event: &Value,
        payload: &Value,
    ) -> Result<bool> {
        let change = match name {
            "process_changed" => Change::Foreground(
                payload.get("foreground"),
                payload.get("generation").and_then(Value::as_i64),
            ),
            "program_status_changed" if is_root(payload) => Change::Status(payload.get("record")),
            // The protocol drops `working` and `blocked` at a new shell prompt, and Pi marks
            // prompts inside its own turns, so that drop is not a report from the program: the
            // last state the program reported stays the one that counts.
            "program_status_removed"
                if removes_root(payload) && str_at(payload, "/reason") != Some("prompt") =>
            {
                Change::Status(None)
            }
            "child_exited" => Change::Exit(payload.get("exit_code").and_then(Value::as_i64)),
            _ => return Ok(false),
        };
        let Some(block) = str_at(event, "/block_id") else {
            return Ok(false);
        };
        let parent = match change {
            Change::Foreground(foreground, _) => foreground_pid(foreground).and_then(proc::parent),
            _ => None,
        };
        let tx = store.write()?;
        let lineage = touch_block(&tx, &self.instance, block)?;
        let moves_task = match change {
            Change::Foreground(foreground, generation) => {
                set_foreground(&tx, &lineage, foreground, generation, parent)?;
                true
            }
            Change::Status(record) => {
                set_status(&tx, &lineage, record)?;
                true
            }
            // The pane's own program ended. Rex reports the empty foreground later, if at all, as
            // it did after a program that printed just before it exited, so it is cleared here.
            Change::Exit(code) => {
                tx.execute(
                    "UPDATE block SET last_exit = ?1, fg_pid = NULL WHERE lineage = ?2",
                    params![code, lineage],
                )?;
                true
            }
        };
        tx.commit()?;
        Ok(moves_task)
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.dirty.values().min().copied()
    }

    /// Reads the sessions whose next read is due. A session the server says does not exist has
    /// closed with its panes; another refusal is logged and the session is read again at its next
    /// change.
    pub(super) fn flush(&mut self, rex: &mut Rex, store: &mut Store) -> Result<()> {
        let now = Instant::now();
        let due: Vec<String> = self
            .dirty
            .iter()
            .filter(|(_, due)| **due <= now)
            .map(|(live, _)| live.clone())
            .collect();
        for live in due {
            self.dirty.remove(&live);
            match rex.call("session.view", &json!({ "session_id": live })) {
                Ok(view) => {
                    self.read(&live, &view);
                    // A new pane's first report can reach the stream before the stream carries
                    // its session, so a pane with nothing recorded on this instance is read
                    // once, as the baseline reads every pane.
                    for block in view_blocks(&view) {
                        if !recorded(store.conn(), &self.instance, &block)? {
                            self.read_block(rex, store, &live, &block)?;
                        }
                    }
                }
                Err(e) if e.not_found() => {}
                Err(e @ CallError::Rejected { .. }) => {
                    log::error!("session {live}: failed to read its view: {e}");
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

/// When a session whose view was last read at `read_at` is read again after a change at `now`.
pub(super) fn next_read(read_at: Option<Instant>, now: Instant) -> Instant {
    read_at.map_or(now, |read| (read + COALESCE).max(now))
}

fn removes_root(payload: &Value) -> bool {
    items(payload, "/ids")
        .iter()
        .any(|id| id.as_str() == Some(""))
}
