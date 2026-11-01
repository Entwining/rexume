//! The orchestrator and worker most task scenarios start from.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use rusqlite::params;
use serde_json::json;

use super::{WAIT, task};
use crate::common::{Daemon, Server, claude, db, query, shell_block, shell_pids, wait_until};

/// Two shells standing in for Claude Code, as their session-start hooks would have reported them:
/// the owner in pane `owner` with an empty turn record, and the worker in pane `worker`, which
/// resumed a conversation whose turn record already holds a turn, each with a messaging socket; a
/// daemon following them, and the worker registered as task `id`, titled "review". Fields drop in
/// order, so the daemon goes before its server.
pub(super) struct Orchestration {
    pub(super) _daemon: Daemon,
    pub(super) server: Server,
    pub(super) state: PathBuf,
    pub(super) session: String,
    pub(super) owner: String,
    pub(super) worker: String,
    pub(super) id: String,
    pub(super) listener: UnixListener,
    pub(super) worker_listener: UnixListener,
    pub(super) _dir: tempfile::TempDir,
}

impl Orchestration {
    pub(super) fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let socket = dir.path().join("owner.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker_socket = dir.path().join("worker.sock");
        let worker_listener = UnixListener::bind(&worker_socket).unwrap();
        worker_listener.set_nonblocking(true).unwrap();

        let server = Server::start(dir.path(), None);
        let daemon = Daemon::start(&server, &state);
        wait_until("the daemon's first epoch", WAIT, || {
            query::<i64>(&state, "SELECT id FROM epoch")
        });
        claude::turn(
            &state,
            "worker",
            "p0",
            "Review the plan",
            Some("Plan approved."),
        );
        let earlier = std::fs::metadata(state.join("claude/worker.jsonl"))
            .unwrap()
            .len();
        let created = server.call(
            "session.create",
            &json!({ "label": "orchestrate", "initial_windows": [{ "layout": { "split": { "direction": "horizontal",
                "ratio": 0.5, "before": shell_block("owner", dir.path()), "after": shell_block("worker", dir.path()) } } }] }),
        );
        let session = created["session_id"].as_str().unwrap();
        let ids = created["initial_windows"][0]["block_ids"]
            .as_array()
            .unwrap();
        let (owner, worker) = (ids[0].as_str().unwrap(), ids[1].as_str().unwrap());
        server.call(
            "session.focus_block",
            &json!({ "session_id": session, "block_id": owner }),
        );
        let pids = shell_pids(&server, &state, session, &[owner, worker]);

        let conn = db(&state);
        for (native_id, block, pid, socket, start) in [
            ("owner", owner, pids[0], &socket, 0),
            ("worker", worker, pids[1], &worker_socket, earlier),
        ] {
            conn.execute(
                "INSERT INTO conversation (runtime, native_id, resumable, reported_at)
                 VALUES ('claude', ?1, 1, 0)",
                params![native_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO occupant (block_lineage, runtime, native_id, pid, keeps_session_env, reported_at,
                   transport, start_position)
                 VALUES (?1, 'claude', ?2, ?3, 1, 0, json_object('messaging_socket', ?4), ?5)",
                params![
                    block,
                    native_id,
                    pid,
                    socket.to_string_lossy(),
                    i64::try_from(start).unwrap()
                ],
            )
            .unwrap();
        }
        let id = task(
            &server,
            &state,
            owner,
            &["add", "--worker", worker, "--title", "review"],
        )["id"]
            .as_str()
            .unwrap()
            .to_owned();
        Self {
            _daemon: daemon,
            server,
            state,
            session: session.to_owned(),
            owner: owner.to_owned(),
            worker: worker.to_owned(),
            id,
            listener,
            worker_listener,
            _dir: dir,
        }
    }
}
