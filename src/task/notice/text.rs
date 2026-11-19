//! Each text starts with its token, the only part of a prompt that Claude Code's turn hook and the
//! omp extension record.

use anyhow::{Result, bail};

use super::send::Due;
use crate::runtime;

pub(super) fn finished(due: &Due, unread: Option<usize>) -> Result<String> {
    let Due {
        notice,
        task,
        title,
        round,
        round_state,
        note,
        interactive,
        ..
    } = due;
    let (Some(round), Some(round_state)) = (round, round_state.as_deref()) else {
        bail!("finished notice {notice} names no round");
    };
    let token = runtime::token(notice);
    let unread = unread_turns(task, unread);
    let watched = if *interactive {
        format!(
            " Until you release the task with `rexume task release {task}`, Rexume also tells \
             you when its worker waits on a dialog or finishes a turn no round took."
        )
    } else {
        String::new()
    };
    Ok(if round_state == "completed" {
        format!(
            "{token} Worker task \"{title}\" finished round {round}. Read its result with \
             `rexume task show {task} --round {round}`, then acknowledge it with \
             `rexume task ack {task} --round {round}`.{unread}{watched}"
        )
    } else {
        let reason = note.as_ref().map_or(String::new(), |n| format!(" ({n})"));
        format!(
            "{token} Worker task \"{title}\" ended round {round} without a result: \
             {round_state}{reason}. Check it with `rexume task show {task} --round {round}`; once \
             handled, acknowledge it with `rexume task ack {task} --round {round}`.\
             {unread}{watched}"
        )
    })
}

/// Names what the worker waits on only by its kind: the program's own message is the worker's
/// text, which would reach the owner as part of a prompt.
pub(super) fn blocked(due: &Due, unread: Option<usize>) -> String {
    let Due {
        notice,
        task,
        title,
        open_round,
        worker,
        request,
        waited_on,
        ..
    } = due;
    let token = runtime::token(notice);
    let waited_on = match waited_on.as_deref() {
        Some("permission") => "a permission prompt",
        Some("question") => "a question",
        Some("auth") => "a credential prompt",
        _ => "a dialog or question",
    };
    let pane = worker
        .as_ref()
        .map_or(String::new(), |block| format!(" in pane {block}"));
    let unread = unread_turns(task, unread);
    let Some(round) = open_round else {
        return format!(
            "{token} Worker task \"{title}\" is blocked{pane} with no round open, waiting on \
             {waited_on}. The user may have answered it since or be working in that pane, so \
             read the pane before answering; `rexume task show {task}` shows the block.{unread}"
        );
    };
    let request = if request.as_deref() == Some("pending") {
        " The round's request is still pending: Rexume sends it once the worker is idle."
    } else {
        ""
    };
    format!(
        "{token} Worker task \"{title}\" is blocked in round {round}{pane}, waiting on \
         {waited_on} that may have been answered since. Check it with \
         `rexume task show {task} --round {round}`.{request}{unread}"
    )
}

pub(super) fn unread(due: &Due, count: usize) -> String {
    let Due {
        notice,
        task,
        title,
        ..
    } = due;
    let token = runtime::token(notice);
    if count == 1 {
        format!(
            "{token} Worker task \"{title}\" has 1 unread turn, finished while no round was \
             open; it may answer a prompt the user typed in its pane. Take it with \
             `rexume task next {task}`."
        )
    } else {
        format!(
            "{token} Worker task \"{title}\" has {count} unread turns, finished while no round \
             was open; they may answer prompts the user typed in its pane. \
             `rexume task next {task}` takes them in order."
        )
    }
}

fn unread_turns(task: &str, unread: Option<usize>) -> String {
    match unread {
        Some(0) | None => String::new(),
        Some(1) => format!(" The worker has 1 unread turn, which `rexume task next {task}` takes."),
        Some(n) => format!(
            " The worker has {n} unread turns, which `rexume task next {task}` takes in order."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::notice::Kind;

    fn due(round: Option<i64>, waited_on: Option<&str>) -> Due {
        Due {
            notice: "n".into(),
            kind: Kind::Blocked,
            task: "t".into(),
            title: "count".into(),
            round,
            round_state: round.map(|_| "blocked".into()),
            note: None,
            waiter: None,
            since: 0,
            worker: None,
            request: None,
            open_round: round,
            waited_on: waited_on.map(Into::into),
            interactive: true,
        }
    }

    #[test]
    fn a_blocked_notice_names_what_the_worker_waits_on_by_its_kind() {
        for (kind, waited_on) in [
            (Some("permission"), "waiting on a permission prompt that"),
            (Some("question"), "waiting on a question that"),
            (Some("auth"), "waiting on a credential prompt that"),
            (Some("elicitation"), "waiting on a dialog or question that"),
            (None, "waiting on a dialog or question that"),
        ] {
            let text = blocked(&due(Some(1), kind), None);
            assert!(text.contains(waited_on), "{kind:?}: {text}");
        }
    }

    #[test]
    fn a_block_between_rounds_sends_the_owner_to_the_pane_first() {
        let text = blocked(&due(None, Some("question")), Some(2));
        for part in [
            "is blocked with no round open, waiting on a question.",
            "read the pane before answering",
            "`rexume task show t` shows the block",
            "The worker has 2 unread turns",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(!text.contains("round 1"), "{text}");
    }
}
