//! Shadow tables (spec §5.2, §7.1). `{p}` is the store prefix, e.g. `kb_`.

pub fn create_sql(p: &str) -> String {
    format!(
        r#"
CREATE TABLE IF NOT EXISTS {p}node (
  id          INTEGER PRIMARY KEY,
  parent_id   INTEGER NULL REFERENCES {p}node(id),
  name        TEXT    NOT NULL,
  kind        INTEGER NOT NULL,              -- 0 folder, 1 file
  path        TEXT    NOT NULL,
  root        BLOB    NULL,
  version     INTEGER NOT NULL DEFAULT 0,
  nbytes      INTEGER,
  nlines      INTEGER,
  created_at  TEXT    NOT NULL,
  updated_at  TEXT    NOT NULL,
  updated_by  TEXT,
  deleted_at  TEXT    NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS {p}node_path ON {p}node(path) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS {p}node_parent_name ON {p}node(parent_id, name) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS {p}node_parent ON {p}node(parent_id);
CREATE INDEX IF NOT EXISTS {p}node_deleted ON {p}node(deleted_at) WHERE deleted_at IS NOT NULL;
CREATE TABLE IF NOT EXISTS {p}commit (
  file_id      INTEGER NOT NULL,
  version      INTEGER NOT NULL,
  root         BLOB    NOT NULL,
  parent_root  BLOB    NULL,
  author       TEXT,
  ts           TEXT    NOT NULL,
  message      TEXT,
  nbytes       INTEGER,
  nlines       INTEGER,
  kind         TEXT,                          -- direct, rebased, merged
  base_version INTEGER,                       -- the version the writer started from
  PRIMARY KEY (file_id, version)
);
CREATE TABLE IF NOT EXISTS {p}chunk (
  id     INTEGER PRIMARY KEY,
  hash   BLOB NOT NULL UNIQUE,
  bytes  BLOB NOT NULL,
  nlines INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS {p}tree_node (
  hash     BLOB PRIMARY KEY,
  children BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS {p}chunk_ref (
  chunk_id INTEGER NOT NULL,
  file_id  INTEGER NOT NULL,
  version  INTEGER NOT NULL,
  PRIMARY KEY (chunk_id, file_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS {p}section (
  file_id INTEGER NOT NULL, version INTEGER NOT NULL,
  heading_path TEXT NOT NULL, level INTEGER NOT NULL,
  line_from INTEGER NOT NULL, line_to INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS {p}section_file ON {p}section(file_id, version);
CREATE TABLE IF NOT EXISTS {p}link (
  file_id INTEGER NOT NULL, version INTEGER NOT NULL,
  target_path TEXT NOT NULL, line INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS {p}link_file ON {p}link(file_id, version);
CREATE TABLE IF NOT EXISTS {p}frontmatter (
  file_id INTEGER NOT NULL, version INTEGER NOT NULL, data TEXT,
  PRIMARY KEY (file_id, version)
);
CREATE TABLE IF NOT EXISTS {p}checkpoint (
  name TEXT NOT NULL, file_id INTEGER NOT NULL, path TEXT NOT NULL, root BLOB NOT NULL, version INTEGER NOT NULL,
  PRIMARY KEY (name, file_id)
);
-- Change feed: one row per commit, folder creation, move and delete, written in the same
-- transaction as the change. SQLite cannot notify another process, so a watcher polls
-- `PRAGMA data_version` and reads the rows after the last `seq` it saw. AUTOINCREMENT so a
-- sequence number is never reused, even after rows are pruned.
CREATE TABLE IF NOT EXISTS {p}change (
  seq          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts           TEXT    NOT NULL,
  op           TEXT    NOT NULL,              -- create, commit, mkdir, move, delete, purge
  node_id      INTEGER NOT NULL,
  node_kind    INTEGER NOT NULL,              -- 0 folder, 1 file
  path         TEXT    NOT NULL,              -- the path after the change
  old_path     TEXT    NULL,                  -- move: the path before
  version      INTEGER NULL,                  -- create, commit: the new version
  base_version INTEGER NULL,                  -- commit: the version the writer started from
  commit_kind  TEXT    NULL,                  -- create, commit: direct, rebased, merged
  author       TEXT    NULL,
  message      TEXT    NULL
);
CREATE INDEX IF NOT EXISTS {p}change_node ON {p}change(node_id);
-- Path history (textdb_core::path): one row per node a rename, move or delete touched — the
-- node it named and everything below a folder — next to the versions in `commit`.
CREATE TABLE IF NOT EXISTS {p}path_event (
  id           INTEGER PRIMARY KEY,
  node_id      INTEGER NOT NULL,
  node_kind    INTEGER NOT NULL,              -- 0 folder, 1 file
  op           TEXT    NOT NULL,              -- rename, move, delete
  ts           TEXT    NOT NULL,
  author       TEXT    NULL,
  old_path     TEXT    NOT NULL,
  new_path     TEXT    NULL,                  -- rename, move: the path after
  via          TEXT    NULL,                  -- the folder the operation named, when this node went with it
  version      INTEGER NULL,                  -- a file's version when it happened
  change_seq   INTEGER NULL                   -- the change feed row of the operation
);
CREATE INDEX IF NOT EXISTS {p}path_event_node ON {p}path_event(node_id, id);
-- Store settings, e.g. `path_history` = on | off. A missing row means the default.
CREATE TABLE IF NOT EXISTS {p}setting (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS {p}fts USING fts5(text, content='', tokenize='unicode61');
"#,
        p = p
    )
}

/// Columns added to existing tables after their first release, as `(table, column, type)`.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[("commit", "kind", "TEXT"), ("commit", "base_version", "INTEGER")];

/// Bring a store created by an earlier build up to this schema. Idempotent; returns the
/// number of columns it had to add.
///
/// New tables come from `create_sql`, which is all `IF NOT EXISTS`. A column added to an
/// existing table needs `ALTER TABLE … ADD COLUMN`, which has no such clause, so each one is
/// checked against `pragma_table_info` first.
pub fn migrate(conn: &rusqlite::Connection, p: &str) -> rusqlite::Result<usize> {
    conn.execute_batch(&create_sql(p))?;
    let mut added = 0;
    for (table, column, decl) in ADDED_COLUMNS {
        let present: bool = conn.query_row(
            &format!("SELECT count(*) > 0 FROM pragma_table_info('{p}{table}') WHERE name = ?1"),
            [column],
            |r| r.get(0),
        )?;
        if !present {
            conn.execute_batch(&format!("ALTER TABLE {p}{table} ADD COLUMN {column} {decl}"))?;
            added += 1;
        }
    }
    Ok(added)
}

pub fn drop_sql(p: &str) -> String {
    [
        "node", "commit", "chunk", "tree_node", "chunk_ref", "section", "link", "frontmatter", "checkpoint", "change", "path_event",
        "setting", "fts",
    ]
    .iter()
    .map(|t| format!("DROP TABLE IF EXISTS {}{};", p, t))
    .collect()
}
