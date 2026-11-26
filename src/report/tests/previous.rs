use super::*;

/// The occupant's `pid` from the last instance survives a report from the next one only while the
/// pane's `block` row still describes the last instance, as before the daemon plans the pane.
#[test]
fn a_report_before_the_plan_keeps_the_last_instances_process() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::open(dir.path())?;
    store
        .conn()
        .execute_batch("INSERT INTO block (lineage, instance_id) VALUES ('b', 'i1')")?;
    // The pane's `block` row is on `seen` when the report comes.
    let mut report = |native_id: &str, instance: &str, pid: i64, seen: &str| {
        let tx = store.write()?;
        tx.execute("UPDATE block SET instance_id = ?1", [seen])?;
        let process = Process {
            instance: instance.into(),
            ..process(pid)
        };
        let report = parse(Runtime::Claude, &json!({ "session_id": native_id }))?;
        record(&tx, "b", Runtime::Claude, &report, &process)?;
        let previous: Option<i64> =
            tx.query_row("SELECT previous_pid FROM occupant", [], |row| row.get(0))?;
        tx.commit()?;
        anyhow::Ok(previous)
    };
    assert_eq!(report("s", "i1", 100, "i1")?, None);
    assert_eq!(report("s", "i2", 300, "i1")?, Some(100));
    assert_eq!(
        report("s", "i2", 300, "i1")?,
        Some(100),
        "a later report on the instance"
    );
    assert_eq!(report("t", "i2", 400, "i1")?, None, "another conversation");
    assert_eq!(report("t", "i3", 500, "i2")?, Some(400));
    assert_eq!(
        report("t", "i4", 600, "i4")?,
        None,
        "after the baseline moved the pane"
    );
    Ok(())
}
