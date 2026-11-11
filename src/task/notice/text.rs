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
            "{token} Worker task \"{title}\" finished round {round}. Read its result with \
             `rexume task show {task} --round {round}`, then acknowledge it with \
             `rexume task ack {task} --round {round}`.{unread}"
        )
    } else {
        let reason = note.as_ref().map_or(String::new(), |n| format!(" ({n})"));
        format!(
            "{token} Worker task \"{title}\" ended round {round} without a result: \
             {round_state}{reason}. Check it with `rexume task show {task} --round {round}`; once \
             handled, acknowledge it with `rexume task ack {task} --round {round}`.{unread}"
        )
    }
}

/// A blocked notice's text. The round has not finished, so there is nothing to acknowledge, and
/// the owner may read this after someone answered the worker. It names what the worker waits on
/// only by the kind its program reported: the program's own message is the worker's text, which
/// would reach the owner as part of a prompt.
pub(super) fn blocked(due: &Due) -> String {
    let Due {
        notice,
        task,
        title,
        round,
        worker,
        request,
        kind,
        ..
    } = due;
    let token = runtime::token(notice);
    let waited_on = match kind.as_deref() {
        Some("permission") => "a permission prompt",
        Some("question") => "a question",
        Some("auth") => "a credential prompt",
        _ => "a dialog or question",
    };
    let pane = worker
        .as_ref()
        .map_or(String::new(), |block| format!(" in pane {block}"));
    let request = if request.as_deref() == Some("pending") {
        " The round's request is still pending: Rexume sends it once the worker is idle."
    } else {
        ""
    };
    format!(
        "{token} Worker task \"{title}\" is blocked in round {round}{pane}, waiting on \
         {waited_on} that may have been answered since. Check it with \
         `rexume task show {task} --round {round}`.{request}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_notice_names_what_the_worker_waits_on_by_its_kind() {
        let due = |kind: Option<&str>| Due {
            notice: "n".into(),
            blocked: true,
            task: "t".into(),
            title: "count".into(),
            round: 1,
            round_state: "blocked".into(),
            note: None,
            waiter: None,
            since: 0,
            worker: None,
            request: None,
            open: true,
            kind: kind.map(Into::into),
        };
        for (kind, waited_on) in [
            (Some("permission"), "waiting on a permission prompt that"),
            (Some("question"), "waiting on a question that"),
            (Some("auth"), "waiting on a credential prompt that"),
            (Some("elicitation"), "waiting on a dialog or question that"),
            (None, "waiting on a dialog or question that"),
        ] {
            let text = blocked(&due(kind));
            assert!(text.contains(waited_on), "{kind:?}: {text}");
        }
    }
}
