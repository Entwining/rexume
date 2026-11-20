use anyhow::Result;
use rusqlite::params;
use serde_json::{Value, json};

use super::block::{foreground_pid, recorded, set_foreground, set_status, touch_block};
use crate::rex::{CallError, Rex, is_root, items, listed_blocks, root_status, str_at};
use crate::store::{Store, ensure_lineage, lineage_of, now_ms};
use crate::{proc, task};

pub(super) struct View {
    instance: String,
    new_sessions: Vec<String>,
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
            new_sessions: Vec::new(),
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

    pub(super) fn handle(&mut self, store: &mut Store, event: &Value) -> Result<bool> {
        let payload = event.get("payload").unwrap_or(&Value::Null);
        match str_at(event, "/event") {
            Some("session_created") => {
                if let Some(live) = str_at(payload, "/session/session_id") {
                    self.new_sessions.push(live.to_owned());
                }
                Ok(false)
            }
            // Focus may have left a worker's pane whose close waited for it
            // (`Follow::view_changed`).
            Some("session_view_changed") => Ok(true),
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

    pub(super) fn flush(&mut self, rex: &mut Rex, store: &mut Store) -> Result<()> {
        for live in std::mem::take(&mut self.new_sessions) {
            match rex.call("session.list_blocks", &json!({ "session_id": live })) {
                Ok(list) => {
                    // A pane in a session the stream carries reports itself as it starts, but the
                    // panes a session is created with can report before the stream carries it, so
                    // a pane with nothing recorded on this instance is read once.
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

fn removes_root(payload: &Value) -> bool {
    items(payload, "/ids")
        .iter()
        .any(|id| id.as_str() == Some(""))
}
