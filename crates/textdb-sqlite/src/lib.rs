//! textdb-sqlite: SQLite binding for textdb (spec §7.1).
//!
//! ```sql
//! CREATE VIRTUAL TABLE kb USING textdb(store='kb_');
//! INSERT INTO kb(path, content) VALUES ('/notes/a.md', '# A');
//! UPDATE kb SET content = replace(content, '# A', '# A!') WHERE path = '/notes/a.md';
//! SELECT * FROM textdb_search('A', '/notes');
//! SELECT textdb_content('/notes/a.md', 1);
//! ```
//! The Rust API ([`TextDb`]) offers the same operations without SQL.

pub mod bulk;
pub mod db;
pub mod functions;
pub mod path_history;
pub mod schema;
pub mod stats;
pub mod storage;
pub mod trash;
pub mod vtab;

pub use db::{normalize_path, AuthorCount, ChangeRow, CommitRow, Entry, Hit, NodeRow, TextDb, WriteResult, DEFAULT_PREFIX};
pub use storage::SqliteStorage;
pub use path_history::PathEventRow;
pub use trash::{PurgeStats, TrashEntry};

use rusqlite::{Connection, Result};
use vtab::{FnKind, FnSpec, FN_MODULE, KB_MODULE};

/// Install the `textdb` virtual table module, the table-valued functions and the scalar
/// functions on a connection. Functions are bound to `prefix` (default `kb_`).
pub fn register(conn: &Connection, prefix: &str) -> Result<()> {
    conn.create_module("textdb", &KB_MODULE, None)?;
    for (name, kind) in [
        ("textdb_ls", FnKind::Ls),
        ("textdb_search", FnKind::Search),
        ("textdb_history", FnKind::History),
        ("textdb_export", FnKind::Export),
        ("textdb_feed", FnKind::Feed),
        ("textdb_hunks", FnKind::Hunks),
        ("textdb_chunks", FnKind::Chunks),
        ("textdb_path_history", FnKind::PathHistory),
    ] {
        conn.create_module(
            name,
            &FN_MODULE,
            Some(FnSpec {
                prefix: prefix.to_string(),
                kind,
            }),
        )?;
    }
    functions::register_functions(conn, prefix)?;
    Ok(())
}

/// Open (or create) a database file with the module registered and WAL enabled.
pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 30000;")?;
    register(&conn, DEFAULT_PREFIX)?;
    Ok(conn)
}

pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    register(&conn, DEFAULT_PREFIX)?;
    Ok(conn)
}
