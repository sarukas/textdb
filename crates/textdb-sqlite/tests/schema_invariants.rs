//! What has to stay true of the schema itself, asserted where a break is legible.
//!
//! Each of these is a failure that happened: the test says what should have been true instead.

use rusqlite::Connection;
use textdb_sqlite::{open_in_memory, schema, DEFAULT_PREFIX};

fn fresh() -> Connection {
    let conn = open_in_memory().unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE kb USING textdb(store='kb_');").unwrap();
    conn
}

/// A store this build just created is already current, so `migrate` has nothing to add.
///
/// A column added to `ADDED_COLUMNS` but not to the table in `create_sql` is missing from every
/// *fresh* store too, so every store takes the migration path — which opens a savepoint, and a
/// savepoint cannot be opened while `CREATE VIRTUAL TABLE` is running. `textdb init` on a new
/// store failed with "cannot open savepoint - SQL statements in progress" until the column went
/// into the table as well. Adding one column in one place is an easy thing to do again, so this
/// says so in one line rather than through every other test failing at once.
#[test]
fn a_new_store_needs_no_migration() {
    let conn = fresh();
    let added = schema::migrate(&conn, DEFAULT_PREFIX).unwrap();
    assert_eq!(added, 0, "a store this build created should already have every column it knows about");

    // And again, because a migration that is not idempotent is the same bug one run later.
    assert_eq!(schema::migrate(&conn, DEFAULT_PREFIX).unwrap(), 0);
}

/// Opening a store twice does not migrate it the second time either: the check above holds for a
/// store written to disk and reopened, not only for one in memory.
#[test]
fn reopening_a_store_needs_no_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kb.db");
    {
        let conn = Connection::open(&path).unwrap();
        textdb_sqlite::register(&conn, "kb_").unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE kb USING textdb(store='kb_');").unwrap();
        conn.execute("INSERT INTO kb(path, content) VALUES ('/a.md', '# A\n')", []).unwrap();
    }
    let conn = Connection::open(&path).unwrap();
    textdb_sqlite::register(&conn, "kb_").unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_');").unwrap();
    assert_eq!(schema::migrate(&conn, DEFAULT_PREFIX).unwrap(), 0);
    let content: String = conn.query_row("SELECT content FROM kb WHERE path = '/a.md'", [], |r| r.get(0)).unwrap();
    assert_eq!(content, "# A\n");
}

/// A store written by an older build still migrates — the path the savepoint fix must not have
/// broken. The columns are dropped by rebuilding the table without them, as an older build had it.
#[test]
fn a_store_missing_a_column_is_migrated_once() {
    let conn = fresh();
    let p = DEFAULT_PREFIX;
    // `dir_id` and `generation` are what the sync base gained; take them away again.
    conn.execute_batch(&format!(
        "ALTER TABLE {p}sync DROP COLUMN dir_id; ALTER TABLE {p}sync DROP COLUMN generation;"
    ))
    .unwrap();
    assert_eq!(schema::migrate(&conn, p).unwrap(), 2, "both columns should be added back");
    assert_eq!(schema::migrate(&conn, p).unwrap(), 0, "and not again");

    // The migrated columns are usable, not just present.
    conn.execute_batch(&format!(
        "INSERT INTO {p}sync(prefix, dir, seq, synced_at, generation, dir_id) VALUES ('/', '/d', 1, 'now', 2, 'abc')"
    ))
    .unwrap();
    let (gen, id): (i64, String) = conn
        .query_row(&format!("SELECT generation, dir_id FROM {p}sync"), [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((gen, id.as_str()), (2, "abc"));
}
