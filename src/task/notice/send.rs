use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value};

use super::{HOLDER, Kind, WANTED, supersede_unread, text};
use crate::proc;
use crate::runtime::{Runtime, Sent, Target};
use crate::store::{Store, now_ms};
use crate::task::{Interest, OPEN, blocked, turns};

/// Lets a `task wait` running in the owner's turn take the notice before the daemon sends it.
pub(super) const GRACE_MS: i64 = 3000;

pub(super) const ACKED: &str = "the round was acknowledged during a send that failed";
const ROUND_STARTED: &str = "a round started or the task was released during a send that failed";
pub(super) const REPORTED: &str = "its turns were taken or reported before it was sent";

/// A blocked notice goes first: its worker stops until someone answers.
pub(in crate::task) static PENDING: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT n.id, n.kind, t.id, t.title, n.round, nr.state, nr.note, w.waiter_pid, n.created_at,
           l.live_id, q.state, r.round, ep.kind, t.result_file IS NULL
         FROM notice n JOIN task t ON t.id = n.task_id
         LEFT JOIN task_round nr ON nr.task_id = n.task_id AND nr.round = n.round
         LEFT JOIN task_round r ON r.task_id = n.task_id AND {OPEN}
         LEFT JOIN task_round w ON w.task_id = n.task_id AND w.round = {holder}
         LEFT JOIN request q ON q.task_id = r.task_id AND q.round = r.round
         LEFT JOIN blocked_episode ep ON ep.id = n.blocked_episode_id
         LEFT JOIN epoch e ON e.id = (SELECT MAX(id) FROM epoch)
         LEFT JOIN live l ON l.instance_id = e.instance_id AND l.lineage = t.worker_lineage
         WHERE n.state = 'pending'
         ORDER BY n.kind = 'blocked' DESC, n.created_at, n.id",
        holder = *HOLDER
    )
});

pub(super) struct Due {
    pub notice: String,
    pub kind: Kind,
    pub task: String,
    pub title: String,
    pub round: Option<i64>,
    pub round_state: Option<String>,
    pub note: Option<String>,
    pub waiter: Option<i64>,
    pub since: i64,
    pub worker: Option<String>,
    /// The request of the round open now, if any.
    pub request: Option<String>,
    pub open_round: Option<i64>,
    pub waited_on: Option<String>,
    pub interactive: bool,
}

pub(in crate::task) fn send_due(
    store: &mut Store,
    interest: &mut Interest,
) -> Result<Option<Instant>> {
    let pending: Vec<Due> = store
        .conn()
        .prepare_cached(&PENDING)?
        .query_map([], |row| {
            Ok(Due {
                notice: row.get(0)?,
                kind: row.get(1)?,
                task: row.get(2)?,
                title: row.get(3)?,
                round: row.get(4)?,
                round_state: row.get(5)?,
                note: row.get(6)?,
                waiter: row.get(7)?,
                since: row.get(8)?,
                worker: row.get(9)?,
                request: row.get(10)?,
                open_round: row.get(11)?,
                waited_on: row.get(12)?,
                interactive: row.get(13)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let now = now_ms();
    let mut next = None;
    for due in pending {
        let due_at = due.since + GRACE_MS;
        if due_at > now {
            let at = Instant::now() + Duration::from_millis(u64::try_from(due_at - now)?);
            next = Some(next.map_or(at, |n: Instant| n.min(at)));
            continue;
        }
        if let Some(pid) = due.waiter.filter(|pid| proc::alive(*pid)) {
            interest.pids.insert(pid);
            continue;
        }
        let Some(owner) = ready_owner(store.conn(), &due.task)? else {
            continue;
        };
        let composed = compose(store, &due)?;
        let tx = store.write()?;
        let claimed = match &composed {
            Some((_, unread_end)) => {
                claim(&tx, &due.notice, due.waiter, *unread_end, due.open_round)?
            }
            None => {
                supersede_unread(&tx, &due.task, REPORTED, now_ms())?;
                false
            }
        };
        tx.commit()?;
        let Some((text, _)) = composed.filter(|_| claimed) else {
            continue;
        };
        let target = Target {
            native_id: &owner.native_id,
            transport: &owner.transport,
        };
        let sent = Sent::recorded(owner.runtime.send(&target, store.dir(), &due.notice, &text));
        log::info!("notice {} for task {}: {}", due.notice, due.task, sent.0);
        let tx = store.write()?;
        record_send(&tx, &due.notice, sent)?;
        tx.commit()?;
    }
    Ok(next)
}

pub(in crate::task) const REPORTED_END: &str =
    "SELECT MAX(n.unread_end) FROM notice n WHERE n.task_id = ?1";

/// The notice's text and where the unread turns it reports end; `None` for an unread notice whose
/// turns were all taken or reported by then.
fn compose(store: &Store, due: &Due) -> Result<Option<(String, Option<i64>)>> {
    // While a round is open `task next` refuses, so no notice names the unread turns.
    let turns = if due.open_round.is_some() {
        None
    } else {
        turns(store.conn(), store.dir(), &due.task)?
    };
    let unread = turns
        .and_then(|turns| turns.unread_end().map(|end| (turns.unread(), end)))
        .map(|(count, end)| i64::try_from(end).map(|end| (count, end)))
        .transpose()?;
    let count = unread.map(|(count, _)| count);
    let end = unread.map(|(_, end)| end);
    Ok(match due.kind {
        Kind::Finished => Some((text::finished(due, count)?, end)),
        Kind::Blocked => Some((text::blocked(due, count), end)),
        Kind::Unread => {
            let reported: Option<i64> =
                store
                    .conn()
                    .query_row(REPORTED_END, params![due.task], |row| row.get(0))?;
            unread
                .filter(|&(_, end)| reported.is_none_or(|reported| end > reported))
                .map(|(count, end)| (text::unread(due, count), Some(end)))
        }
    })
}

pub(in crate::task) static STILL_WANTED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {}, n.kind
         FROM notice n JOIN task t ON t.id = n.task_id
         LEFT JOIN task_round r ON r.task_id = n.task_id AND r.round = n.round
         WHERE n.id = ?1",
        *WANTED
    )
});

pub(super) fn record_send(
    tx: &Transaction,
    notice: &str,
    sent: (&str, Option<String>),
) -> Result<()> {
    let (mut state, mut note) = sent;
    if state == "pending" {
        let (wanted, kind): (bool, Kind) = tx.query_row(&STILL_WANTED, params![notice], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        if !wanted {
            state = "superseded";
            note = Some(
                match kind {
                    Kind::Finished => ACKED,
                    Kind::Blocked => blocked::ENDED,
                    Kind::Unread => ROUND_STARTED,
                }
                .to_owned(),
            );
        }
    }
    tx.execute(
        "UPDATE notice SET state = ?1, note = ?2,
           sent_at = CASE WHEN ?1 IN ('pending', 'superseded') THEN NULL ELSE sent_at END,
           unread_end = CASE WHEN ?1 IN ('pending', 'superseded') THEN NULL ELSE unread_end END,
           superseded_at = CASE WHEN ?1 = 'superseded' THEN ?4 END
         WHERE id = ?3",
        params![state, note, notice, now_ms()],
    )?;
    Ok(())
}

/// `?2` is the waiter the daemon found dead, or none, before the transaction; any other waiter
/// holds the notice. `?5` is the round open when the text was composed: a task command that opened
/// or ended a round since leaves the notice for the pass its commit wakes.
pub(in crate::task) static CLAIM: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE notice AS n SET state = 'sending', sent_at = ?3, attempts = attempts + 1,
           unread_end = ?4
         WHERE n.id = ?1 AND n.state = 'pending'
           AND NOT EXISTS (SELECT 1 FROM task_round w
                           WHERE w.task_id = n.task_id AND w.round = {holder}
                             AND w.waiter_pid IS NOT NULL AND w.waiter_pid IS NOT ?2)
           AND (SELECT r.round FROM task_round r WHERE r.task_id = n.task_id AND {OPEN}) IS ?5",
        holder = *HOLDER
    )
});

pub(super) fn claim(
    tx: &Transaction,
    notice: &str,
    seen_waiter: Option<i64>,
    unread_end: Option<i64>,
    open_round: Option<i64>,
) -> Result<bool> {
    Ok(tx.prepare_cached(&CLAIM)?.execute(params![
        notice,
        seen_waiter,
        now_ms(),
        unread_end,
        open_round
    ])? == 1)
}

/// An owner gets one notice at a time: Codex, Pi, and omp run one queued message per turn and
/// Claude Code holds what arrives during a turn, so sending more saves no turn and only moves them
/// from this outbox, where `task wait`, `task ack`, and `task release` can still drop them, into a
/// queue Codex replays on resume and Pi and omp lose on exit. A status the pane reports after the
/// send releases the next without a receipt, since a notice the runtime dropped, as one held during
/// a turn the user interrupts can be, never shows in its record.
pub(in crate::task) const READY_OWNER: &str = "SELECT o.runtime, o.native_id, o.transport
  FROM task t
  JOIN occupant o ON o.runtime = t.owner_runtime AND o.native_id = t.owner_native_id
  JOIN block b ON b.lineage = o.block_lineage
  JOIN epoch e ON e.id = (SELECT MAX(id) FROM epoch)
  WHERE t.id = ?1 AND e.ended_at IS NULL AND e.instance_id = b.instance_id AND b.fg_pid = o.pid
    AND (o.instance_id IS NULL OR o.instance_id = b.instance_id)
    AND (b.status IS NULL OR b.status NOT IN ('working', 'blocked'))
    AND NOT EXISTS (SELECT 1 FROM task s JOIN notice n ON n.task_id = s.id
                    WHERE s.owner_runtime = o.runtime AND s.owner_native_id = o.native_id
                      AND n.state IN ('sending', 'sent', 'accepted') AND n.sent_at > b.status_at)
  ORDER BY o.reported_at DESC LIMIT 1";

struct Owner {
    runtime: Runtime,
    native_id: String,
    transport: Map<String, Value>,
}

fn ready_owner(conn: &Connection, task: &str) -> Result<Option<Owner>> {
    Ok(conn
        .prepare_cached(READY_OWNER)?
        .query_row(params![task], |row| {
            let transport: Option<Value> = row.get(2)?;
            Ok(Owner {
                runtime: row.get(0)?,
                native_id: row.get(1)?,
                transport: transport
                    .and_then(|t| t.as_object().cloned())
                    .unwrap_or_default(),
            })
        })
        .optional()?)
}
