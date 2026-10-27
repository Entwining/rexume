//! Settling sent notices from their owners' own records.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use rusqlite::params;

use crate::runtime::{Receipt, Runtime, pi};
use crate::store::Store;
use crate::task::Interest;

/// Sent notices with the owner record each is looked for in.
pub(in crate::task) const SENT: &str = "SELECT n.id, n.state, c.runtime, c.native_id, c.transcript
  FROM notice n JOIN task t ON t.id = n.task_id
  JOIN conversation c ON c.runtime = t.owner_runtime AND c.native_id = t.owner_native_id
  WHERE n.state IN ('sent', 'accepted', 'unknown')";

/// The notices sent to one owner record.
struct OwnerRecord {
    runtime: Runtime,
    native_id: String,
    notices: Vec<(String, String)>,
}

/// Marks notices the owner's conversation has taken in, from its own record. Pi's extension
/// renaming a notice to `.accepted` shows only that Pi holds it.
pub(in crate::task) fn settle(
    store: &mut Store,
    receipts: &mut HashMap<PathBuf, Receipt>,
    interest: &mut Interest,
) -> Result<()> {
    let mut records: HashMap<PathBuf, OwnerRecord> = HashMap::new();
    let rows: Vec<(String, String, Runtime, String, Option<String>)> = store
        .conn()
        .prepare_cached(SENT)?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (notice, state, runtime, native_id, transcript) in rows {
        let Some(transcript) = transcript else {
            continue;
        };
        records
            .entry(PathBuf::from(transcript))
            .or_insert_with(|| OwnerRecord {
                runtime,
                native_id,
                notices: Vec::new(),
            })
            .notices
            .push((notice, state));
    }
    receipts.retain(|path, _| records.contains_key(path));
    let mut delivered = HashSet::new();
    let mut accepted = Vec::new();
    for (path, record) in records {
        let receipt = receipts
            .entry(path.clone())
            .or_insert_with(|| Receipt::new(record.runtime, &path));
        let ids: Vec<String> = record.notices.iter().map(|(id, _)| id.clone()).collect();
        match receipt.found(&ids) {
            Ok(found) => {
                delivered.extend(found);
                interest.files.insert(path);
            }
            // The records read before the error may hold receipts, so the next check reads the
            // record from its start; the other owners' records are checked meanwhile. A path that
            // cannot be read may not be watchable either, and a failed watch ends the epoch.
            Err(e) => {
                log::warn!("failed to read `{}` for receipts: {e:#}", path.display());
                receipts.remove(&path);
            }
        }
        if record.runtime == Runtime::Pi {
            interest
                .files
                .insert(pi::inbox(store.dir(), &record.native_id));
            accepted.extend(record.notices.into_iter().filter(|(id, state)| {
                state != "accepted"
                    && !delivered.contains(id)
                    && pi::accepted(store.dir(), &record.native_id, id)
            }));
        }
    }
    if delivered.is_empty() && accepted.is_empty() {
        return Ok(());
    }
    let tx = store.write()?;
    for id in &delivered {
        tx.execute(
            "UPDATE notice SET state = 'delivered'
             WHERE id = ?1 AND state IN ('sent', 'accepted', 'unknown')",
            params![id],
        )?;
    }
    for (id, _) in &accepted {
        tx.execute(
            "UPDATE notice SET state = 'accepted' WHERE id = ?1 AND state IN ('sent', 'unknown')",
            params![id],
        )?;
    }
    tx.commit()?;
    Ok(())
}
