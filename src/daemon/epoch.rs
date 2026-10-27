//! One epoch: a connection to one server instance, from its witness until it ends. The loop
//! records events, reconciles restores, and runs task passes, and sleeps on the event queue
//! between.

use std::time::Instant;

use anyhow::Result;
use rusqlite::params;
use serde_json::json;

use super::view::View;
use super::{ADAPTER, Daemon, FIRST_FILE, PIPE, WAITER_EXIT, WAL};
use crate::restore::{Restorer, Spec};
use crate::rex::{Pipe, Rex, str_at};
use crate::store::{self, now_ms};
use crate::task::{Follow, Interest};

impl Daemon<'_> {
    /// Observes one connection until it ends, and returns how it ended.
    pub(super) fn observe(&mut self) -> Result<&'static str> {
        let (mut child, mut pipe) = self.connect()?;
        self.queue.read(pipe.fd(), PIPE)?;
        let witnessed = self.witness(&mut pipe);
        let mut rex = Rex::Pipe(pipe);
        let result = witnessed.and_then(|instance| {
            let epoch = {
                let tx = self.store.write()?;
                tx.execute(
                    "INSERT INTO epoch (instance_id, started_at) VALUES (?1, ?2)",
                    params![instance, now_ms()],
                )?;
                let id = tx.last_insert_rowid();
                tx.commit()?;
                id
            };
            log::info!("observing instance {instance} as epoch {epoch}");
            let observed = self.observe_epoch(&mut rex, &instance);
            // The connection ending does not by itself mean the server stopped: only a server
            // that no longer answers as this instance makes the last observations its final state.
            let reason = match self.options.cli.call("server.status", &json!({})) {
                Ok(status) if str_at(&status, "/instance_id") == Some(instance.as_str()) => {
                    "disconnected"
                }
                _ => "server_gone",
            };
            let tx = self.store.write()?;
            tx.execute(
                "UPDATE epoch SET ended_at = ?1, end_reason = ?2 WHERE id = ?3",
                params![now_ms(), reason, epoch],
            )?;
            tx.commit()?;
            observed.map(|()| reason)
        });
        self.queue.forget_exit(i64::from(child.id()));
        let _ = child.kill();
        let _ = child.wait();
        result
    }

    fn observe_epoch(&mut self, rex: &mut Rex, instance: &str) -> Result<()> {
        let spec = Spec {
            gate: vec![
                self.options.exe.to_string_lossy().into_owned(),
                "gate".to_owned(),
                "--state-dir".to_owned(),
                self.options.state_dir.to_string_lossy().into_owned(),
            ],
            theme: self.options.theme.clone(),
        };
        let mut restorer = Restorer::new(spec);
        restorer.start(rex, &mut self.store, instance)?;
        let mut view = View::new(instance);
        view.baseline(rex, &mut self.store)?;
        {
            let tx = self.store.write()?;
            store::sweep(&tx, instance, now_ms())?;
            tx.commit()?;
        }
        let mut follow = Follow::default();
        let mut version = self.store.data_version()?;
        let mut follow_due = true;
        loop {
            let (events, ended) = pipe_of(rex).events();
            for event in &events {
                if str_at(event, "/event") == Some("session_view_changed") {
                    follow.view_changed();
                }
                match view.handle(rex, &mut self.store, event) {
                    Ok(changed) => follow_due |= changed,
                    Err(e) => log::error!(
                        "event {}: {e:#}",
                        str_at(event, "/event").unwrap_or("an unnamed event")
                    ),
                }
            }
            if ended {
                return Ok(());
            }
            let now = Instant::now();
            let changed = self.store.data_version()?;
            if changed != version || restorer.deadline().is_some_and(|at| at <= now) {
                version = changed;
                restorer.reconcile(rex, &mut self.store, instance);
                follow_due = true;
            }
            view.flush(rex, &mut self.store)?;
            if follow_due || follow.deadline().is_some_and(|at| at <= now) {
                follow_due = false;
                match follow.pass(&mut self.store, Some((rex, instance))) {
                    Ok(interest) => follow_due = self.register(interest)?,
                    Err(e) => log::error!("task pass failed: {e:#}"),
                }
            }
            if follow_due || pipe_of(rex).pending() {
                continue;
            }
            let deadline = [view.deadline(), restorer.deadline(), follow.deadline()]
                .into_iter()
                .flatten()
                .min();
            match self.sleep_until(deadline)? {
                Some(woken) => follow_due = woken,
                None => return Ok(()),
            }
        }
    }

    /// Waits for the next wakeup and handles the watches it fired. Returns whether a task may have
    /// moved, or `None` when the adapter exited.
    fn sleep_until(&mut self, deadline: Option<Instant>) -> Result<Option<bool>> {
        let mut follow_due = false;
        for fired in self.wait(deadline.map(|at| at.saturating_duration_since(Instant::now())))? {
            match fired.token {
                ADAPTER => {
                    log::warn!("`rex do` exited; reconnecting");
                    return Ok(None);
                }
                WAL => self.store.settle()?,
                token if token == WAITER_EXIT || token >= FIRST_FILE => follow_due = true,
                _ => {}
            }
        }
        Ok(Some(follow_due))
    }

    /// Follows the files and processes the task module asked for, and returns whether one changed
    /// before it could be followed.
    fn register(&mut self, interest: Interest) -> Result<bool> {
        let mut again = false;
        self.files.retain(|path, token| {
            let keep = interest.files.contains(path);
            if !keep {
                self.queue.forget_file(*token);
                self.via_dir.remove(token);
            }
            keep
        });
        for path in interest.files {
            if self.files.contains_key(&path) {
                continue;
            }
            let token = self.next_file;
            // A file not written yet is followed through its directory until it appears; that
            // watch is replaced by one on the file at the next change.
            let via_dir = if self.queue.file(&path, token)? {
                false
            } else if let Some(dir) = path.parent()
                && self.queue.file(dir, token)?
            {
                true
            } else {
                continue;
            };
            self.next_file += 1;
            if via_dir {
                self.via_dir.insert(token);
            }
            self.files.insert(path, token);
            // A change between the pass's read and this watch would otherwise go unseen.
            again = true;
        }
        for pid in self.pids.difference(&interest.pids) {
            self.queue.forget_exit(*pid);
        }
        for pid in &interest.pids {
            if !self.pids.contains(pid) && !self.queue.exit(*pid, WAITER_EXIT)? {
                again = true;
            }
        }
        self.pids = interest.pids;
        Ok(again)
    }
}

fn pipe_of(rex: &mut Rex) -> &mut Pipe {
    match rex {
        Rex::Pipe(pipe) => pipe,
        Rex::Cli(_) => unreachable!("the daemon observes through its adapter"),
    }
}
