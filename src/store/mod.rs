//! The state database. Every writer (the daemon, hooks, gates, and task commands) is a short
//! transaction through this module, so related records change together and no writer needs the
//! daemon to be running. No transaction waits on Rex, a socket, or a file.

#[cfg(test)]
mod tests;
mod upgrade;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

const SCHEMA: &str = include_str!("schema.sql");
/// The `user_version` that `schema.sql` sets.
pub const SCHEMA_VERSION: i64 = 7;
/// The `application_id` that `schema.sql` sets: 'rexu' in ASCII.
const APPLICATION_ID: i64 = 0x7265_7875;
/// How long finished restores, tasks, and unused conversations are kept.
const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub fn default_dir() -> Result<PathBuf> {
    let home = std::env::home_dir().context("HOME is not set")?;
    Ok(home.join(".local/state/rexume"))
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A time-ordered ID for records this tool creates (RFC 9562 UUIDv7).
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub struct Store {
    conn: Connection,
    dir: PathBuf,
}

impl Store {
    /// Opens the database in `dir`, creating both on first use.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create `{}`", dir.display()))?;
        let path = dir.join("state.db");
        let mut conn = Connection::open(&path)
            .with_context(|| format!("failed to open `{}`", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        if check_version(&conn, &path)? != Found::Current {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Another process may have created or upgraded it between the check and the lock.
            match check_version(&tx, &path)? {
                Found::Current => {}
                Found::Empty => tx
                    .execute_batch(SCHEMA)
                    .context("failed to apply the schema")?,
                Found::Older(version) => upgrade::from(&tx, version)
                    .with_context(|| format!("failed to upgrade `{}`", path.display()))?,
            }
            tx.commit()?;
        }
        Ok(Self {
            conn,
            dir: dir.to_path_buf(),
        })
    }

    /// Opens an existing database without writing to it or creating anything.
    pub fn open_read_only(dir: &Path) -> Result<Connection> {
        let path = dir.join("state.db");
        if !path.exists() {
            bail!("`{}` does not exist; the daemon creates it", path.display());
        }
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("failed to open `{}`", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        match check_version(&conn, &path)? {
            Found::Current => {}
            Found::Empty => bail!("`{}` has no rexume state yet", path.display()),
            Found::Older(_) => bail!(
                "`{}` has an older schema; any other rexume command upgrades it",
                path.display()
            ),
        }
        Ok(conn)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A write transaction that takes the write lock up front, so what it reads cannot change
    /// under another writer before it writes.
    pub fn write(&mut self) -> Result<Transaction<'_>> {
        Ok(self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Returns once no other connection is part way through a commit. A commit writes its frames
    /// to the write-ahead log, which wakes the processes watching it, and becomes visible only
    /// after that, in the memory-mapped index no watch sees; the writer holds the write lock
    /// throughout, so taking the lock makes that commit visible here.
    pub fn settle(&mut self) -> Result<()> {
        self.write()?.rollback()?;
        Ok(())
    }

    /// Changes whenever another connection commits, and not for this one's own commits.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "data_version", |row| row.get(0))?)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Found {
    Current,
    /// A new, empty file.
    Empty,
    /// The schema of an earlier released version, which `upgrade` brings up to this one.
    Older(i64),
}

/// Which schema the database holds. Anything but this version's, an earlier one's, or none is an
/// error, since writing to it could lose another program's or version's data.
fn check_version(conn: &Connection, path: &Path) -> Result<Found> {
    let pragma = |name| conn.pragma_query_value(None, name, |row| row.get::<_, i64>(0));
    let (application_id, version) = (pragma("application_id")?, pragma("user_version")?);
    match (application_id, version) {
        (APPLICATION_ID, SCHEMA_VERSION) => Ok(Found::Current),
        (APPLICATION_ID, found) if (1..SCHEMA_VERSION).contains(&found) => Ok(Found::Older(found)),
        (APPLICATION_ID, found) if found > SCHEMA_VERSION => bail!(
            "`{}` was written by a newer rexume (schema {found}; this version knows \
             {SCHEMA_VERSION}); upgrade rexume",
            path.display()
        ),
        (0, 0)
            if conn.query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| {
                row.get::<_, i64>(0)
            })? == 0 =>
        {
            Ok(Found::Empty)
        }
        _ => bail!(
            "`{}` is not a state database of this rexume version; move it away to start over",
            path.display()
        ),
    }
}

pub fn lineage_of(conn: &Connection, instance: &str, live_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT lineage FROM live WHERE instance_id = ?1 AND live_id = ?2",
            params![instance, live_id],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn live_of(conn: &Connection, instance: &str, lineage: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT live_id FROM live WHERE instance_id = ?1 AND lineage = ?2",
            params![instance, lineage],
            |row| row.get(0),
        )
        .optional()?)
}

/// Binds a live ID to a lineage. Binding it again to the same lineage changes nothing; a
/// different lineage, or a lineage already bound to another live ID, is an error.
pub fn bind_live(tx: &Transaction, instance: &str, live_id: &str, lineage: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO live (instance_id, live_id, lineage) VALUES (?1, ?2, ?3)
         ON CONFLICT (instance_id, live_id) DO NOTHING",
        params![instance, live_id, lineage],
    )
    .with_context(|| format!("{lineage} is already bound to another ID than {live_id}"))?;
    match lineage_of(tx, instance, live_id)? {
        Some(bound) if bound == lineage => Ok(()),
        bound => bail!(
            "{live_id} is bound to {}, not {lineage}",
            bound.as_deref().unwrap_or("nothing")
        ),
    }
}

/// The lineage of a live resource, bound the first time it is seen on this instance. A resource
/// the server brought back under the ID it had on an earlier instance, as Rex's own restore does,
/// keeps the lineage that ID was last bound to, until the sweep forgets the earlier instances; any
/// other takes its own ID. A lineage another resource on this instance holds already, as a copy
/// that an earlier version restored beside Rex's own may, is not taken twice: the resource gets a
/// new one.
pub fn ensure_lineage(tx: &Transaction, instance: &str, live_id: &str) -> Result<String> {
    if let Some(lineage) = lineage_of(tx, instance, live_id)? {
        return Ok(lineage);
    }
    let earlier: Option<String> = tx
        .query_row(
            "SELECT lineage FROM live WHERE instance_id != ?1 AND live_id = ?2
             ORDER BY (SELECT MAX(id) FROM epoch WHERE epoch.instance_id = live.instance_id) DESC
             LIMIT 1",
            params![instance, live_id],
            |row| row.get(0),
        )
        .optional()?;
    let wanted = earlier.unwrap_or_else(|| live_id.to_owned());
    let taken: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM live WHERE instance_id = ?1 AND lineage = ?2)",
        params![instance, wanted],
        |row| row.get(0),
    )?;
    let lineage = if taken { new_id() } else { wanted };
    tx.execute(
        "INSERT INTO live (instance_id, live_id, lineage) VALUES (?1, ?2, ?3)",
        params![instance, live_id, lineage],
    )?;
    Ok(lineage)
}

/// Forgets what the server said about a pane and the conversation that ran in it.
pub fn forget_block(tx: &Transaction, lineage: &str) -> Result<()> {
    tx.execute("DELETE FROM block WHERE lineage = ?1", params![lineage])?;
    tx.execute(
        "DELETE FROM occupant WHERE block_lineage = ?1",
        params![lineage],
    )?;
    Ok(())
}

/// Deletes what no restore, task, or notice can read any more: rows of earlier instances that
/// nothing names, and finished records older than `RETENTION`. Run by the daemon once the
/// current instance's baseline is saved. The order follows the foreign keys.
pub fn sweep(tx: &Transaction, instance: &str, now: i64) -> Result<()> {
    let cutoff = now - i64::try_from(RETENTION.as_millis())?;
    tx.execute(
        "DELETE FROM live WHERE instance_id != ?1",
        params![instance],
    )?;
    // The baseline has moved every pane this instance shows to it, so an earlier instance's pane
    // was closed, or not saved, while the daemon was not observing. A task's worker pane stays, and
    // so does an occupant reported on this instance before its pane's row.
    tx.execute(
        "DELETE FROM block WHERE instance_id != ?1
           AND lineage NOT IN (SELECT worker_lineage FROM task WHERE pane_state != 'closed')",
        params![instance],
    )?;
    tx.execute(
        "DELETE FROM occupant WHERE (instance_id IS NULL OR instance_id != ?1)
           AND NOT EXISTS (SELECT 1 FROM block WHERE block.lineage = occupant.block_lineage)",
        params![instance],
    )?;
    tx.execute(
        "DELETE FROM restore WHERE instance_id != ?1 AND created_at < ?2",
        params![instance, cutoff],
    )?;
    tx.execute(
        "DELETE FROM task
          WHERE id IN (SELECT t.id FROM task t JOIN task_round r ON r.task_id = t.id
                       WHERE r.round = (SELECT MAX(round) FROM task_round WHERE task_id = t.id)
                         AND COALESCE(r.acked_at, r.finished_at) < ?1)
            AND NOT EXISTS (SELECT 1 FROM notice n WHERE n.task_id = task.id
                            AND n.state IN ('pending', 'sending', 'sent', 'accepted', 'unknown'))",
        params![cutoff],
    )?;
    tx.execute(
        "DELETE FROM epoch
          WHERE ended_at < ?1 AND instance_id NOT IN (SELECT instance_id FROM block)",
        params![cutoff],
    )?;
    tx.execute(
        "DELETE FROM conversation
          WHERE reported_at < ?1
            AND NOT EXISTS (SELECT 1 FROM occupant o
                            WHERE o.runtime = conversation.runtime
                              AND o.native_id = conversation.native_id)
            AND NOT EXISTS (SELECT 1 FROM task t
                            WHERE t.owner_runtime = conversation.runtime
                              AND t.owner_native_id = conversation.native_id)
            AND NOT EXISTS (SELECT 1 FROM restore r
                            WHERE r.runtime = conversation.runtime
                              AND r.native_id = conversation.native_id)
            AND NOT EXISTS (SELECT 1 FROM task t
                            WHERE t.worker_runtime = conversation.runtime
                              AND t.worker_native_id = conversation.native_id)",
        params![cutoff],
    )?;
    Ok(())
}
