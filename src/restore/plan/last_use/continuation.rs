use anyhow::Result;
use rusqlite::Connection;

use super::super::Start;
use super::{LastSeen, Occupant};
use crate::runtime::{self, Turn};
use crate::{report, task};

/// With `works_after_end`, a server stop between completion and its status update can cause an
/// extra continuation prompt.
pub(super) fn continuation(
    conn: &Connection,
    block: &str,
    occupant: &Occupant,
    seen: &LastSeen,
    server_gone: bool,
) -> Result<(Start, String, Option<u64>)> {
    let agent = |reason: &str| Ok((Start::Agent, reason.to_owned(), None));
    if !server_gone {
        return agent("the server's end was not observed");
    }
    if occupant.runtime.resumes_turns() {
        return agent("the runtime goes on with a cut-off turn itself");
    }
    if task::is_run(conn, block, &seen.instance_id, occupant.pid)? {
        return agent("the conversation was a non-interactive run");
    }
    let same_process = seen.status_pid == Some(occupant.pid)
        && (occupant.pid_name.is_none() || seen.status_name == occupant.pid_name);
    if !same_process {
        return agent("the last status came from another process");
    }
    match seen.status.as_deref() {
        Some("working") => {}
        Some("blocked") => return agent("the conversation was waiting for the user"),
        other => return agent(&format!("the last status was {}", other.unwrap_or("none"))),
    }
    let Some(record) = &occupant.record else {
        return agent("no conversation record to check");
    };
    let length = match report::record_length(record) {
        Ok(length) => length,
        Err(e) => return agent(&format!("the record could not be read: {e:#}")),
    };
    let reason = match runtime::last_turn(occupant.runtime, record, 0) {
        Ok(Some(Turn::Open)) => "the server's end cut off a running turn",
        Ok(Some(Turn::Completed { .. })) if occupant.runtime.works_after_end() => {
            "the server's end cut off work that went on after the turn's end"
        }
        Ok(Some(turn)) => return agent(turn.describe()),
        Ok(None) => return agent("the record has a line that is not JSON"),
        Err(e) => return agent(&format!("the record could not be read: {e:#}")),
    };
    Ok((Start::Continue, reason.into(), Some(length)))
}
