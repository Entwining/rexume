use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::{Open, PANE_WAIT, Plan, millis};
use crate::runtime::{Runtime, Sent, Target};
use crate::store::{Store, now_ms};

pub fn opened_elsewhere(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row(OPENED_ELSEWHERE, params![id], |row| row.get(0))?)
}

/// `o.instance_id = r.instance_id` catches a conversation someone reopened on this instance
/// before the plan. A report in the plan's millisecond counts as later: keeping the shell is the
/// safe side.
pub(crate) const OPENED_ELSEWHERE: &str = "SELECT EXISTS (SELECT 1 FROM restore r JOIN occupant o
  ON o.runtime = r.runtime AND o.native_id = r.native_id
  WHERE r.id = ?1 AND o.block_lineage != r.block_lineage
    AND (o.reported_at >= r.created_at OR o.instance_id = r.instance_id))";

pub(super) const OPENED_IN_ANOTHER_PANE: &str = "the conversation was opened in another pane";

/// The plan's start and runtime, when the conversation reported from the planned pane on this
/// instance.
pub(crate) const OPENED_HERE: &str = "SELECT r.start, r.runtime FROM restore r JOIN occupant o
  ON o.block_lineage = r.block_lineage AND o.instance_id = r.instance_id
    AND o.runtime = r.runtime AND o.native_id = r.native_id
  WHERE r.id = ?1";

/// The conversation's process in the planned pane, and whether it can take a prompt now.
pub(crate) const RECIPIENT: &str = "SELECT r.runtime, r.native_id, r.plan, r.note, o.transport,
    o.native_id IS NOT NULL, b.instance_id = r.instance_id AND b.fg_pid = o.pid,
    COALESCE(b.status IN ('working', 'blocked'), 0)
  FROM restore r
  LEFT JOIN occupant o ON o.block_lineage = r.block_lineage AND o.instance_id = r.instance_id
    AND o.runtime = r.runtime AND o.native_id = r.native_id
  LEFT JOIN block b ON b.lineage = r.block_lineage
  WHERE r.id = ?1";

/// How a restore ends when its conversation runs already: `skipped` in another pane, or `opened`
/// in the planned pane on this instance, as one Rex resumed itself does, since Rex starts a program
/// it resumes before any shell can take a line. A `continue` plan's prompt then goes through the
/// runtime's transport, as `continuation_state` records.
pub(super) struct Found {
    pub state: &'static str,
    pub note: Option<String>,
    pub continuation: Option<&'static str>,
}

pub(super) fn opened(conn: &Connection, id: &str) -> Result<Option<Found>> {
    if opened_elsewhere(conn, id)? {
        return Ok(Some(Found {
            state: "skipped",
            note: Some(OPENED_IN_ANOTHER_PANE.into()),
            continuation: None,
        }));
    }
    let here: Option<(String, Runtime)> = conn
        .query_row(OPENED_HERE, params![id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    Ok(here.map(|(start, runtime)| {
        let (continuation, note) = match start.as_str() {
            "continue" if runtime.continues_opened() => (Some("pending"), None),
            "continue" => (
                Some("skipped"),
                Some(format!(
                    "Rexume sends no continuation to a {runtime} conversation it did not start"
                )),
            ),
            _ => (None, None),
        };
        Found {
            state: "opened",
            note,
            continuation,
        }
    }))
}

/// Unlike `skip`, this leaves the pane's open rounds: they go on from the conversation's record, in
/// this pane or another.
pub(super) fn record_opened(
    store: &mut Store,
    restore: &Open,
    found: Found,
) -> Result<Option<i64>> {
    let tx = store.write()?;
    tx.execute(
        "UPDATE restore SET state = ?1, note = ?2, continuation_state = ?3
         WHERE id = ?4 AND state = 'planned'",
        params![found.state, found.note, found.continuation, restore.id],
    )?;
    tx.commit()?;
    log::info!(
        "restore {}: {}{}",
        restore.id,
        found.state,
        found
            .note
            .map(|note| format!(": {note}"))
            .unwrap_or_default()
    );
    if found.continuation == Some("pending") {
        return continue_opened(store, restore);
    }
    Ok(None)
}

struct Recipient {
    runtime: Runtime,
    native_id: String,
    plan: String,
    note: Option<String>,
    transport: Option<Value>,
    waits: Option<&'static str>,
}

fn recipient(conn: &Connection, id: &str) -> Result<Recipient> {
    Ok(conn.query_row(RECIPIENT, params![id], |row| {
        let waits = if !row.get::<_, bool>(5)? {
            Some("the conversation no longer runs in the pane")
        } else if row.get::<_, Option<bool>>(6)? != Some(true) {
            Some("the conversation is not the pane's foreground program")
        } else if row.get(7)? {
            Some("the conversation is running a turn or waiting for the user")
        } else {
            None
        };
        Ok(Recipient {
            runtime: row.get(0)?,
            native_id: row.get(1)?,
            plan: row.get(2)?,
            note: row.get(3)?,
            transport: row.get(4)?,
            waits,
        })
    })?)
}

/// Sends the plan's prompt to the conversation found `opened` once it can take it, at most once:
/// the claim is committed before the send, and a send that may have happened is never repeated.
pub(super) fn continue_opened(store: &mut Store, restore: &Open) -> Result<Option<i64>> {
    let until = restore.created_at + millis(PANE_WAIT);
    let to = recipient(store.conn(), &restore.id)?;
    if now_ms() >= until {
        let why = to.waits.map(str::to_owned).or(to.note);
        let why = why
            .as_deref()
            .unwrap_or("the time a pane is waited for passed");
        let note = format!("not sent within 30 seconds: {why}");
        return skip_continuation(store, restore, &note);
    }
    if let Some(waits) = to.waits {
        set_note(store, restore, waits)?;
        return Ok(Some(until));
    }
    let plan: Plan = serde_json::from_str(&to.plan)?;
    let record = to
        .runtime
        .record(store.dir(), &to.native_id, plan.transcript.as_deref());
    if !plan.record_untouched(to.runtime, record.as_deref()) {
        let why = "the conversation's record shows a prompt or a turn's end since the plan";
        return skip_continuation(store, restore, why);
    }
    let tx = store.write()?;
    let claimed = tx.execute(
        "UPDATE restore SET continuation_state = 'sending', note = NULL
         WHERE id = ?1 AND continuation_state = 'pending'",
        params![restore.id],
    )?;
    tx.commit()?;
    if claimed == 0 {
        return Ok(None);
    }
    let transport = to
        .transport
        .and_then(|t| t.as_object().cloned())
        .unwrap_or_default();
    let target = Target {
        native_id: &to.native_id,
        transport: &transport,
    };
    let prompt = plan.prompt.as_deref().unwrap_or_default();
    let sent = to.runtime.send(&target, store.dir(), &restore.id, prompt);
    let (state, note) = match Sent::recorded(sent) {
        ("pending", note) => ("pending", note),
        ("unknown", note) => ("unknown", note),
        (_, receipt) => ("sent", receipt),
    };
    let tx = store.write()?;
    tx.execute(
        "UPDATE restore SET continuation_state = ?1, note = ?2
         WHERE id = ?3 AND continuation_state = 'sending'",
        params![state, note, restore.id],
    )?;
    tx.commit()?;
    log::info!("restore {}: continuation {state}", restore.id);
    Ok((state == "pending").then_some(until))
}

fn set_note(store: &mut Store, restore: &Open, note: &str) -> Result<()> {
    let tx = store.write()?;
    tx.execute(
        "UPDATE restore SET note = ?1 WHERE id = ?2 AND continuation_state = 'pending'",
        params![note, restore.id],
    )?;
    tx.commit()?;
    Ok(())
}

fn skip_continuation(store: &mut Store, restore: &Open, note: &str) -> Result<Option<i64>> {
    let tx = store.write()?;
    tx.execute(
        "UPDATE restore SET continuation_state = 'skipped', note = ?1
         WHERE id = ?2 AND continuation_state = 'pending'",
        params![note, restore.id],
    )?;
    tx.commit()?;
    log::info!("restore {}: continuation skipped: {note}", restore.id);
    Ok(None)
}
