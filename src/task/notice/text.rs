//! What a notice says. The text starts with the token the owner's record is searched for.

use super::Due;
use crate::runtime;

/// A finished notice's text; `unread` is how many turns of the worker no round took.
pub(super) fn finished(due: &Due, unread: Option<usize>) -> String {
    let Due {
        notice,
        task,
        title,
        round,
        round_state,
        note,
        ..
    } = due;
    let token = runtime::token(notice);
    let unread = match unread {
        Some(0) | None => String::new(),
        Some(1) => format!(" The worker has 1 unread turn, which `rexume task next {task}` takes."),
        Some(n) => format!(
            " The worker has {n} unread turns, which `rexume task next {task}` takes in order."
        ),
    };
    if round_state == "completed" {
        format!(
            "{token} Worker task {task} \"{title}\" finished round {round}. Read its result with \
             `rexume task show {task} --round {round}`, then acknowledge it with \
             `rexume task ack {task} --round {round}`.{unread}"
        )
    } else {
        let reason = note.as_ref().map_or(String::new(), |n| format!(" ({n})"));
        format!(
            "{token} Worker task {task} \"{title}\" ended round {round} without a result: \
             {round_state}{reason}. Check it with `rexume task show {task} --round {round}`; once \
             it is dealt with, acknowledge it with `rexume task ack {task} --round {round}`.{unread}"
        )
    }
}

/// A blocked notice's text. The round has not finished, so there is nothing to acknowledge, and
/// the owner may read this after someone answered the worker.
pub(super) fn blocked(due: &Due) -> String {
    let Due {
        notice,
        task,
        title,
        round,
        worker,
        request,
        ..
    } = due;
    let token = runtime::token(notice);
    let pane = worker
        .as_ref()
        .map_or(String::new(), |block| format!(" in pane {block}"));
    let request = if request.as_deref() == Some("pending") {
        " The round's request is still pending: rexume sends it once the worker is idle."
    } else {
        ""
    };
    format!(
        "{token} Worker task {task} \"{title}\" is blocked in round {round}: the worker{pane} is \
         waiting on a dialog or a question, which may have been answered since. Check it with \
         `rexume task show {task} --round {round}`.{request}"
    )
}
