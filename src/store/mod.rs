pub mod archive;
#[cfg(test)]
mod tests;
mod upgrade;

use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::ValueEnum as _;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::runtime::Runtime;

const SCHEMA: &str = include_str!("schema.sql");
pub const SCHEMA_VERSION: i64 = 10;
const APPLICATION_ID: i64 = 0x7265_7875;
pub(crate) const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub fn default_dir() -> Result<PathBuf> {
    let home = std::env::home_dir().context("HOME is not set")?;
    Ok(home.join(".local/state/rexume"))
}

/// The Pi and omp extensions create their directories in the state directory with this mode too,
/// whichever needs one first.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("failed to create `{}`", dir.display()))
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub struct Store {
    conn: Connection,
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        create_private_dir(dir)?;
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
            tx.execute(
                "INSERT INTO store (id, source_id) VALUES (1, ?1) ON CONFLICT DO NOTHING",
                params![new_id()],
            )?;
            tx.commit()?;
        }
        Ok(Self {
            conn,
            dir: dir.to_path_buf(),
        })
    }

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

    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "data_version", |row| row.get(0))?)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Found {
    Current,
    Empty,
    Older(i64),
}

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

pub fn forget_block(tx: &Transaction, lineage: &str) -> Result<()> {
    tx.execute("DELETE FROM block WHERE lineage = ?1", params![lineage])?;
    tx.execute(
        "DELETE FROM occupant WHERE block_lineage = ?1",
        params![lineage],
    )?;
    Ok(())
}

pub fn sweep_records(store: &Store) -> Result<()> {
    let cutoff = SystemTime::now() - RETENTION;
    let mut known = store.conn().prepare(
        "SELECT EXISTS (SELECT 1 FROM conversation WHERE runtime = ?1 AND native_id = ?2)",
    )?;
    for runtime in Runtime::value_variants() {
        let Some(dir) = runtime.records(store.dir()) else {
            continue;
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("failed to read `{}`", dir.display())),
        };
        for entry in entries {
            let path = entry?.path();
            let Some(native_id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".jsonl"))
            else {
                continue;
            };
            let written = std::fs::metadata(&path).and_then(|meta| meta.modified());
            if written.is_ok_and(|at| at < cutoff)
                && !known.query_row(params![runtime.as_str(), native_id], |row| row.get(0))?
                && let Err(e) = std::fs::remove_file(&path)
            {
                log::warn!("failed to delete `{}`: {e}", path.display());
            }
        }
    }
    Ok(())
}

pub fn sweep(tx: &Transaction, instance: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM live WHERE instance_id != ?1",
        params![instance],
    )?;
    // The baseline has moved every pane this instance shows to it, so an earlier instance's pane
    // was closed, or not saved, while the daemon was not observing.
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
    Ok(())
}
