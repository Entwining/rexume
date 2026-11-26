use anyhow::Result;
use rusqlite::Connection;

use super::{Candidate, LastSeen, Occupant, continuation, prompt, server_gone};
use crate::restore::{Plan, Start};

/// The conversation reported from the pane on this instance before the plan, as a program Rex
/// resumed can, and its report replaced the last instance's occupant. Nothing reopens it; the plan
/// decides only whether its cut-off turn gets the continuation, judged as for a conversation still
/// recorded from the last instance, with `previous_pid` as its process there.
pub(super) fn reported_first(
    conn: &Connection,
    instance: &str,
    block: &str,
    seen: &LastSeen,
    mut occupant: Occupant,
) -> Result<Candidate> {
    let opened = "a conversation was opened in the pane on this server instance";
    let mut candidate = Candidate {
        start: Start::Agent,
        conversation: Some((occupant.runtime, occupant.conversation.native_id.clone())),
        reopens: false,
        plan: Plan {
            reason: opened.into(),
            ..Plan::default()
        },
    };
    let Some(previous) = occupant
        .previous_pid
        .filter(|pid| seen.last_exit.is_none() && seen.fg_pid == Some(*pid))
    else {
        return Ok(candidate);
    };
    // The name the report gives is the new process's.
    occupant.pid = previous;
    occupant.pid_name = None;
    let server_gone = server_gone(conn, &seen.instance_id)?;
    let (start, reason, _) = continuation(conn, block, &occupant, seen, server_gone)?;
    candidate.plan.reason = format!("{opened}, and {reason}");
    if start == Start::Continue {
        candidate.start = start;
        candidate.plan.prompt = Some(prompt(conn, instance, block, &occupant)?);
        // The record's length when the new process reported, so that the late check sees a prompt
        // typed into it before the plan.
        candidate.plan.record_length = Some(u64::try_from(occupant.start_position)?);
    }
    Ok(candidate)
}
