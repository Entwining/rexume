//! A change to the schema is deliberate: it changes the fixture of its version too.

type Definitions = Vec<(String, String, Option<String>)>;

/// The tables, indexes, and `user_version` a schema creates.
fn definitions(schema: &str) -> (i64, Definitions) {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch(schema).unwrap();
    let version = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let definitions = db
        .prepare("SELECT type, name, sql FROM sqlite_schema ORDER BY type, name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    (version, definitions)
}

#[test]
fn schema_changes_are_deliberate() {
    let (version, schema) = definitions(include_str!("../../src/store/schema.sql"));
    let path = format!(
        "{}/tests/fixtures/schema-v{version}.sql",
        env!("CARGO_MANIFEST_DIR")
    );
    let fixture = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert_eq!(schema, definitions(&fixture).1);
}
