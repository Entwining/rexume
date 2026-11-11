use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};

use super::block::{foreground_pid, recorded, set_foreground, set_status, touch_block};
use crate::rex::{CallError, Rex, is_root, items, listed_blocks, root_status, str_at};
use crate::store::{Store, ensure_lineage, lineage_of, now_ms};
use crate::{proc, task};

const COALESCE: Duration = Duration::from_millis(300);

pub(super) struct View {
    instance: String,
    dirty: HashMap<String, Instant>,
    read_at: HashMap<String, Instant>,
    revisions: HashMap<String, i64>,
}

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

    /// The server restores every pane it saved, so a worker's pane it does not list was closed,
    /// whichever instance recorded it, if any; one registered after the list may be missing.
    /// Forgetting a pane cannot be undone, so a refusal other than a missing session aborts.
    pub(super) fn baseline(&mut self, rex: &mut Rex, store: &mut Store) -> Result<()> {
        let listed_at = now_ms();
        let list = rex.call("session.list", &json!({}))?;
        let mut blocks = Vec::new();
        for live in items(&list, "/sessions")
            .iter()
            .filter_map(|s| str_at(s, "/session_id"))
        {
            let list = match rex.call("session.list_blocks", &json!({ "session_id": live })) {
                Ok(list) => list,
                Err(e) if e.not_found() => continue,
                Err(e) => return Err(e.into()),
            };
            self.read_at.insert(live.to_owned(), Instant::now());
            for block in listed_blocks(&list) {
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
        // A failed read leaves the status as recorded, so a server dying mid-read cannot erase the
        // `working` a restore continues from.
        if let Some(status) = status {
            set_status(&tx, &lineage, root_status(&status))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A change long after the last list is listed at once, so a crash right after a single change
    /// loses nothing.
    fn mark_dirty(&mut self, live: &str) {
        let due = next_read(self.read_at.get(live).copied(), Instant::now());
        self.dirty.entry(live.to_owned()).or_insert(due);
    }

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
                // Rex can send these out of revision order. The pane list carries no revision but
                // is read after the event, so it covers any revision already reported.
                if revision.is_none_or(|r| self.revisions.get(live).is_none_or(|saved| r > *saved))
                {
                    if let Some(r) = revision {
                        self.revisions.insert(live.to_owned(), r);
                    }
                    self.mark_dirty(live);
                }
                Ok(true)
            }
            // A server that stops ends the stream without closing its panes, so this close is real.
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
            // Rex drops `working` and `blocked` at a new shell prompt, and Pi marks prompts inside
            // its own turns, so a removal for a prompt is not the program's report.
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
            // Rex reports the empty foreground late or never, as after a program that printed
            // just before it exited.
            Change::Exit(code) => {
                set_foreground(&tx, &lineage, None, None, None)?;
                tx.execute(
                    "UPDATE block SET last_exit = ?1 WHERE lineage = ?2",
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
            match rex.call("session.list_blocks", &json!({ "session_id": live })) {
                Ok(list) => {
                    self.read_at.insert(live.clone(), Instant::now());
                    // A new pane's first reports can come before the stream carries its session,
                    // so a pane with nothing recorded on this instance is read once.
                    for block in listed_blocks(&list) {
                        if !recorded(store.conn(), &self.instance, &block)? {
                            self.read_block(rex, store, &live, &block)?;
                        }
                    }
                }
                Err(e) if e.not_found() => {}
                Err(e @ CallError::Rejected { .. }) => {
                    log::error!("session {live}: failed to list its panes: {e}");
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

pub(super) fn next_read(read_at: Option<Instant>, now: Instant) -> Instant {
    read_at.map_or(now, |read| (read + COALESCE).max(now))
}

fn removes_root(payload: &Value) -> bool {
    items(payload, "/ids")
        .iter()
        .any(|id| id.as_str() == Some(""))
}
