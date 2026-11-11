use anyhow::Context as _;

use super::*;
use crate::runtime::Runtime;
use crate::runtime::tests::{answer_line, append_to_record, receipt_line};

#[test]
fn inbox_notices_are_accepted_once_and_delivered_only_from_the_owners_record() -> Result<()> {
    for runtime in [Runtime::Pi, Runtime::Omp] {
        let dir = tempfile::tempdir()?;
        let transcript = tempfile::NamedTempFile::new()?;
        let (mut store, _listener) = notice_world(dir.path(), transcript.path(), "pending")?;
        store.conn().execute(
            "INSERT INTO conversation (runtime, native_id, transcript, resumable, reported_at)
             VALUES (?1, 'inbox-owner', ?2, 1, 0)",
            params![runtime.as_str(), transcript.path().to_string_lossy()],
        )?;
        store.conn().execute(
            "UPDATE occupant SET runtime = ?1, native_id = 'inbox-owner' WHERE block_lineage = 'a'",
            params![runtime.as_str()],
        )?;
        store.conn().execute(
            "UPDATE task SET owner_runtime = ?1, owner_native_id = 'inbox-owner'",
            params![runtime.as_str()],
        )?;
        let mut interest = Interest::default();
        let mut receipts = HashMap::new();
        send_due(&mut store, &mut interest)?;
        assert_eq!(notice(&store)?.0, "sent");
        let inbox = runtime
            .inbox(store.dir(), "inbox-owner")
            .context("no inbox")?;
        let pending = inbox.join("d.json");
        let accepted = inbox.join("d.accepted");
        std::fs::rename(&pending, &accepted)?;
        settle(&mut store, &mut receipts, &mut interest)?;
        assert_eq!(notice(&store)?.0, "accepted");
        assert!(interest.files.contains(&inbox));
        send_due(&mut store, &mut interest)?;
        assert!(!pending.exists(), "accepted notices must not be sent twice");
        let token = runtime::token("d");
        let (mention, receipt) = (
            answer_line(runtime, "a1", &token),
            receipt_line(runtime, &token),
        );
        let state = store.dir().to_owned();
        let append = |line: Value| {
            append_to_record(runtime, &state, "inbox-owner", transcript.path(), &[line])
        };
        append(mention)?;
        settle(&mut store, &mut receipts, &mut interest)?;
        assert_eq!(
            notice(&store)?.0,
            "accepted",
            "mentioning a token is no receipt"
        );
        append(receipt)?;
        settle(&mut store, &mut receipts, &mut interest)?;
        assert_eq!(notice(&store)?.0, "delivered");
    }
    Ok(())
}
