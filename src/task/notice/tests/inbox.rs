use super::*;
use crate::runtime::{Runtime, pi};

#[test]
fn inbox_notices_are_accepted_once_and_delivered_only_from_the_owners_record() -> Result<()> {
    for runtime in [Runtime::Pi, Runtime::Omp] {
        let dir = tempfile::tempdir()?;
        let mut transcript = tempfile::NamedTempFile::new()?;
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
        let inbox = pi::inbox(store.dir(), "inbox-owner");
        let pending = inbox.join("d.json");
        let accepted = inbox.join("d.accepted");
        std::fs::rename(&pending, &accepted)?;
        settle(&mut store, &mut receipts, &mut interest)?;
        assert_eq!(notice(&store)?.0, "accepted");
        assert!(interest.files.contains(&inbox));
        send_due(&mut store, &mut interest)?;
        assert!(!pending.exists(), "accepted notices must not be sent twice");
        writeln!(
            transcript,
            "{}",
            json!({
                "type": "message", "id": "a1", "message": {
                    "role": "assistant", "stopReason": "stop", "content": [{
                        "type": "text", "text": runtime::token("d")
                    }]
                }
            })
        )?;
        transcript.flush()?;
        settle(&mut store, &mut receipts, &mut interest)?;
        assert_eq!(
            notice(&store)?.0,
            "accepted",
            "mentioning a token is no receipt"
        );
        writeln!(
            transcript,
            "{}",
            json!({
                "type": "custom_message", "id": "n1", "customType": "rexume",
                "content": format!("{} Done.", runtime::token("d")), "display": true
            })
        )?;
        transcript.flush()?;
        settle(&mut store, &mut receipts, &mut interest)?;
        assert_eq!(notice(&store)?.0, "delivered");
    }
    Ok(())
}
