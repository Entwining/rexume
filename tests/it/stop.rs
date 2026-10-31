use std::process::Command;
use std::time::Duration;

use crate::common::{Daemon, Server, alive, query, wait_until};

const WAIT: Duration = Duration::from_secs(20);

/// Run by hand or by a test rather than by its job, nothing else ends the daemon's `rex do`.
#[test]
#[ignore = "needs a Rex server"]
fn a_stopped_daemon_ends_its_rex_do_first() {
    for signal in ["-TERM", "-INT"] {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let server = Server::start(dir.path(), None);
        let mut daemon = Daemon::start(&server, &state);
        wait_until("the daemon's first epoch", WAIT, || {
            query::<i64>(&state, "SELECT id FROM epoch")
        });
        let adapter = wait_until("rex do", WAIT, || daemon.adapter());
        let status = Command::new("/bin/kill")
            .args([signal, &daemon.pid().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let exited = wait_until("the daemon to exit", Duration::from_secs(5), || {
            daemon.try_wait()
        });
        assert!(exited.success(), "{signal}: {}", daemon.log());
        wait_until("rex do to end", Duration::from_secs(5), || {
            (!alive(adapter)).then_some(())
        });
        assert!(
            daemon.log().contains("stopped by a signal"),
            "{}",
            daemon.log()
        );
        // A stop ends the epoch, and is not an error `status` shows.
        assert_eq!(
            query::<bool>(
                &state,
                "SELECT ended_at IS NOT NULL AND error IS NULL FROM epoch
                 WHERE id = (SELECT MAX(id) FROM epoch)"
            ),
            Some(true)
        );
    }
}
