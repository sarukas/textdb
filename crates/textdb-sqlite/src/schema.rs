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
CREATE TABLE IF NOT EXISTS {p}commit (
  file_id     INTEGER NOT NULL,
  version     INTEGER NOT NULL,
  root        BLOB    NOT NULL,
  parent_root BLOB    NULL,
  author      TEXT,
  ts          TEXT    NOT NULL,
  message     TEXT,
  nbytes      INTEGER,
  nlines      INTEGER,
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
CREATE VIRTUAL TABLE IF NOT EXISTS {p}fts USING fts5(text, content='', tokenize='unicode61');
"#,
        p = p
    )
}

pub fn drop_sql(p: &str) -> String {
    [
        "node", "commit", "chunk", "tree_node", "chunk_ref", "section", "link", "frontmatter", "checkpoint", "fts",
    ]
    .iter()
    .map(|t| format!("DROP TABLE IF EXISTS {}{};", p, t))
    .collect()
}
