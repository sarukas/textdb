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
  deleted_at  TEXT    NULL,
  nwords      INTEGER,                        -- file: words, as wc -w counts them
  nauthors    INTEGER,                        -- file: distinct commit authors (file_author rows)
  -- Folder: totals of every live node below it, kept current by each commit, mkdir, move and
  -- delete. Zero on files; a file's own figures are nbytes, nlines, nwords and version.
  t_files      INTEGER NOT NULL DEFAULT 0,
  t_folders    INTEGER NOT NULL DEFAULT 0,
  t_bytes      INTEGER NOT NULL DEFAULT 0,
  t_lines      INTEGER NOT NULL DEFAULT 0,
  t_words      INTEGER NOT NULL DEFAULT 0,
  t_versions   INTEGER NOT NULL DEFAULT 0,
  t_updated_at TEXT    NULL                   -- the last change anywhere below
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
  batch        TEXT    NULL,                  -- the run that made it (`textdb sql --write`), see bulk.rs
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
  target_path TEXT NOT NULL, line INTEGER NOT NULL,
  kind        TEXT,                           -- wiki, embed, md, image
  anchor      TEXT,                           -- heading or ^block after #
  alias       TEXT,
  external    INTEGER NOT NULL DEFAULT 0,     -- URL, email, query, numbered reference
  target_name TEXT,                           -- last segment, lower case, without .md (see links.rs)
  resolved_id INTEGER,                        -- the file it points to
  status      TEXT                            -- ok, ambiguous, anchor-missing, broken, not-in-store, external
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
  message      TEXT    NULL,
  batch        TEXT    NULL                   -- the run that made it, as on commit
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
-- Who wrote each file: one row per (file, author) with that author's commits. '' is a commit
-- without an author.
CREATE TABLE IF NOT EXISTS {p}file_author (
  file_id  INTEGER NOT NULL,
  author   TEXT    NOT NULL,
  commits  INTEGER NOT NULL,
  first_ts TEXT    NOT NULL,
  last_ts  TEXT    NOT NULL,
  PRIMARY KEY (file_id, author)
) WITHOUT ROWID;
-- textdb sync: what a store folder and a directory held when they were last reconciled, one row
-- per (folder, directory), with the git commit the directory's checkout was at.
CREATE TABLE IF NOT EXISTS {p}sync (
  id         INTEGER PRIMARY KEY,
  prefix     TEXT    NOT NULL,
  dir        TEXT    NOT NULL,
  seq        INTEGER NOT NULL,              -- the store's last change number after the sync
  synced_at  TEXT    NOT NULL,
  author     TEXT,
  git_commit TEXT,
  git_branch TEXT,
  git_remote TEXT,
  git_clean  INTEGER,                       -- 1: no uncommitted changes; NULL: not a git checkout
  rules      TEXT,                          -- the include rules of that sync, as JSON (sync.rs Rules)
  UNIQUE (prefix, dir)
);
-- Each file both sides agreed on at that sync: its version in the store, the git blob id of its
-- content, and its size and modification time on disk (NULL when too recent to trust).
CREATE TABLE IF NOT EXISTS {p}sync_file (
  sync_id    INTEGER NOT NULL,
  rel        TEXT    NOT NULL,
  version    INTEGER,
  blob       TEXT    NOT NULL,
  disk_size  INTEGER,
  disk_mtime INTEGER,
  conflict   INTEGER NOT NULL DEFAULT 0,    -- 1: conflict markers were written to the file on disk
  PRIMARY KEY (sync_id, rel)
) WITHOUT ROWID;
CREATE VIRTUAL TABLE IF NOT EXISTS {p}fts USING fts5(text, content='', tokenize='unicode61');
"#,
        p = p
    )
}

/// Columns added to existing tables after their first release, as `(table, column, type)`.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("commit", "kind", "TEXT"),
    ("commit", "base_version", "INTEGER"),
    ("node", "nwords", "INTEGER"),
    ("node", "nauthors", "INTEGER"),
    ("node", "t_files", "INTEGER NOT NULL DEFAULT 0"),
    ("node", "t_folders", "INTEGER NOT NULL DEFAULT 0"),
    ("node", "t_bytes", "INTEGER NOT NULL DEFAULT 0"),
    ("node", "t_lines", "INTEGER NOT NULL DEFAULT 0"),
    ("node", "t_words", "INTEGER NOT NULL DEFAULT 0"),
    ("node", "t_versions", "INTEGER NOT NULL DEFAULT 0"),
    ("node", "t_updated_at", "TEXT NULL"),
    // Links: how each is written and what it resolves to (links.rs).
    ("link", "kind", "TEXT"),
    ("link", "anchor", "TEXT"),
    ("link", "alias", "TEXT"),
    ("link", "external", "INTEGER NOT NULL DEFAULT 0"),
    ("link", "target_name", "TEXT"),
    ("link", "resolved_id", "INTEGER"),
    ("link", "status", "TEXT"),
    // Batches: the run (`textdb sql --write`) a commit or change belongs to.
    ("commit", "batch", "TEXT NULL"),
    ("change", "batch", "TEXT NULL"),
    // Sync: the include rules each base was made with.
    ("sync", "rules", "TEXT"),
];

/// Indexes on columns an older store gains in `migrate`, so they are created after them.
fn index_sql(p: &str) -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS {p}link_target_name ON {p}link(target_name);
         CREATE INDEX IF NOT EXISTS {p}link_resolved ON {p}link(resolved_id);
         CREATE INDEX IF NOT EXISTS {p}node_lower_name ON {p}node(lower(name)) WHERE deleted_at IS NULL;
         CREATE INDEX IF NOT EXISTS {p}node_lower_path ON {p}node(lower(path)) WHERE deleted_at IS NULL;
         CREATE INDEX IF NOT EXISTS {p}change_batch ON {p}change(batch) WHERE batch IS NOT NULL;"
    )
}

/// Bring a store created by an earlier build up to this schema. Idempotent; returns the
/// number of columns it had to add.
///
/// New tables come from `create_sql`, which is all `IF NOT EXISTS`. A column added to an
/// existing table needs `ALTER TABLE … ADD COLUMN`, which has no such clause, so each one is
/// checked against `pragma_table_info` first. A store that gains the word, author and folder
/// total columns has them computed from its content once, in the same savepoint, so no reader
/// ever sees the columns without their values.
pub fn migrate(conn: &rusqlite::Connection, p: &str) -> rusqlite::Result<usize> {
    conn.execute_batch(&create_sql(p))?;
    let mut missing = Vec::new();
    for (table, column, decl) in ADDED_COLUMNS {
        let present: bool = conn.query_row(
            &format!("SELECT count(*) > 0 FROM pragma_table_info('{p}{table}') WHERE name = ?1"),
            [column],
            |r| r.get(0),
        )?;
        if !present {
            missing.push((table, column, decl));
        }
    }
    if missing.is_empty() {
        conn.execute_batch(&index_sql(p))?;
        return Ok(0);
    }
    let backfill = missing.iter().any(|(table, _, _)| **table == "node");
    let backfill_links = missing.iter().any(|(table, _, _)| **table == "link");
    conn.execute_batch("SAVEPOINT textdb_migrate")?;
    let run = || -> rusqlite::Result<()> {
        for (table, column, decl) in &missing {
            conn.execute_batch(&format!("ALTER TABLE {p}{table} ADD COLUMN {column} {decl}"))?;
        }
        conn.execute_batch(&index_sql(p))?;
        if backfill {
            crate::stats::backfill(conn, p).map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
        }
        if backfill_links {
            crate::links::backfill(conn, p).map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
        }
        Ok(())
    };
    match run() {
        Ok(()) => conn.execute_batch("RELEASE textdb_migrate")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO textdb_migrate; RELEASE textdb_migrate");
            return Err(e);
        }
    }
    Ok(missing.len())
}

pub fn drop_sql(p: &str) -> String {
    [
        "node", "commit", "chunk", "tree_node", "chunk_ref", "section", "link", "frontmatter", "checkpoint", "change", "path_event",
        "setting", "file_author", "sync", "sync_file", "fts",
    ]
    .iter()
    .map(|t| format!("DROP TABLE IF EXISTS {}{};", p, t))
    .collect()
}
