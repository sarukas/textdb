//! Namespace layer over the shadow tables: folders, files, commits, structure rows,
//! search. Every algorithm comes from `textdb-core`; this module only persists.

use rusqlite::{params, Connection, OptionalExtension};
use textdb_core::commit::{commit, commit_append, CommitKind, Committed};
use textdb_core::myers::byte_edits;
use textdb_core::storage::Result;
use textdb_core::tree::{leaves, materialize, totals};
use textdb_core::{unified_diff, ChunkParams, Edit, Hash, Storage, StructureExtractor, TextdbError};
use textdb_md::MarkdownExtractor;

use crate::storage::{sql_err, SqliteStorage};

pub const DEFAULT_PREFIX: &str = "kb_";

#[derive(Clone, Debug)]
pub struct NodeRow {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub kind: i64,
    pub path: String,
    pub root: Option<Hash>,
    pub version: i64,
    pub nbytes: Option<i64>,
    pub nlines: Option<i64>,
    pub updated_at: String,
    pub updated_by: Option<String>,
    pub deleted_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub kind: i64,
    pub nbytes: Option<i64>,
    pub nlines: Option<i64>,
    pub updated_at: String,
}

#[derive(Clone, Debug)]
pub struct CommitRow {
    pub version: i64,
    pub author: Option<String>,
    pub ts: String,
    pub message: Option<String>,
    pub nbytes: Option<i64>,
    pub root: Hash,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub line: i64,
    pub snippet: String,
    pub rank: f64,
}

#[derive(Clone, Debug)]
pub struct WriteResult {
    pub version: u64,
    pub kind: CommitKind,
}

pub struct TextDb<'c> {
    pub conn: &'c Connection,
    pub p: String,
    pub params: ChunkParams,
    pub extractor: Option<Box<dyn StructureExtractor>>,
    /// Wrap each operation in `BEGIN IMMEDIATE … COMMIT` when the connection is in
    /// autocommit mode. Disabled when running inside a virtual-table callback.
    pub manage_tx: bool,
    pub retries: usize,
}

fn to_hash(v: &[u8]) -> Result<Hash> {
    if v.len() != 32 {
        return Err(TextdbError::Storage("bad hash length".into()));
    }
    let mut h = [0u8; 32];
    h.copy_from_slice(v);
    Ok(h)
}

/// Normalise to `/a/b/c`. Rejects empty segments, `.`/`..`, NUL.
pub fn normalize_path(p: &str) -> Result<String> {
    if p.contains('\0') {
        return Err(TextdbError::InvalidEdit("path contains NUL".into()));
    }
    let mut out = String::from("/");
    let mut segs = Vec::new();
    for seg in p.split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg == "." || seg == ".." {
            return Err(TextdbError::InvalidEdit(format!("invalid path segment '{}' in {}", seg, p)));
        }
        segs.push(seg);
    }
    out.push_str(&segs.join("/"));
    Ok(out)
}

pub fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

pub fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

impl<'c> TextDb<'c> {
    /// Create the shadow tables if missing and return a handle.
    pub fn open(conn: &'c Connection, prefix: &str) -> Result<Self> {
        conn.execute_batch(&crate::schema::create_sql(prefix)).map_err(sql_err)?;
        let db = Self::attach(conn, prefix, true);
        db.ensure_root()?;
        Ok(db)
    }

    /// Handle over existing tables. `manage_tx = false` inside SQLite callbacks.
    pub fn attach(conn: &'c Connection, prefix: &str, manage_tx: bool) -> Self {
        TextDb {
            conn,
            p: prefix.to_string(),
            params: ChunkParams::DEFAULT,
            extractor: Some(Box::new(MarkdownExtractor)),
            manage_tx,
            retries: textdb_core::DEFAULT_RETRIES,
        }
    }

    fn storage(&self) -> SqliteStorage<'c> {
        SqliteStorage::new(self.conn, &self.p)
    }

    fn now() -> String {
        SqliteStorage::now()
    }

    /// Run `f` inside a write transaction when this handle manages transactions.
    pub fn tx<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        if !self.manage_tx || !self.conn.is_autocommit() {
            return f(self);
        }
        self.conn.execute_batch("BEGIN IMMEDIATE").map_err(sql_err)?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT").map_err(sql_err)?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn ensure_root(&self) -> Result<i64> {
        if let Some(n) = self.node_by_path("/")? {
            return Ok(n.id);
        }
        let now = Self::now();
        self.conn
            .execute(
                &format!(
                    "INSERT INTO {}node(parent_id, name, kind, path, created_at, updated_at) VALUES (NULL, '', 0, '/', ?1, ?1)",
                    self.p
                ),
                params![now],
            )
            .map_err(sql_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    fn row_from(r: &rusqlite::Row) -> rusqlite::Result<NodeRow> {
        let root: Option<Vec<u8>> = r.get(5)?;
        Ok(NodeRow {
            id: r.get(0)?,
            parent_id: r.get(1)?,
            name: r.get(2)?,
            kind: r.get(3)?,
            path: r.get(4)?,
            root: root.and_then(|v| to_hash(&v).ok()),
            version: r.get(6)?,
            nbytes: r.get(7)?,
            nlines: r.get(8)?,
            updated_at: r.get(9)?,
            updated_by: r.get(10)?,
            deleted_at: r.get(11)?,
        })
    }

    const NODE_COLS: &'static str =
        "id, parent_id, name, kind, path, root, version, nbytes, nlines, updated_at, updated_by, deleted_at";

    pub fn node_by_path(&self, path: &str) -> Result<Option<NodeRow>> {
        self.conn
            .prepare_cached(&format!(
                "SELECT {} FROM {}node WHERE path = ?1 AND deleted_at IS NULL",
                Self::NODE_COLS,
                self.p
            ))
            .map_err(sql_err)?
            .query_row(params![path], Self::row_from)
            .optional()
            .map_err(sql_err)
    }

    /// Live node if present, otherwise the most recently deleted node at that path.
    pub fn node_by_path_any(&self, path: &str) -> Result<Option<NodeRow>> {
        if let Some(n) = self.node_by_path(path)? {
            return Ok(Some(n));
        }
        self.conn
            .prepare_cached(&format!(
                "SELECT {} FROM {}node WHERE path = ?1 ORDER BY deleted_at DESC LIMIT 1",
                Self::NODE_COLS,
                self.p
            ))
            .map_err(sql_err)?
            .query_row(params![path], Self::row_from)
            .optional()
            .map_err(sql_err)
    }

    pub fn node_by_id(&self, id: i64) -> Result<Option<NodeRow>> {
        self.conn
            .prepare_cached(&format!("SELECT {} FROM {}node WHERE id = ?1", Self::NODE_COLS, self.p))
            .map_err(sql_err)?
            .query_row(params![id], Self::row_from)
            .optional()
            .map_err(sql_err)
    }

    fn file_by_path(&self, path: &str) -> Result<NodeRow> {
        match self.node_by_path(path)? {
            Some(n) if n.kind == 1 => Ok(n),
            Some(_) => Err(TextdbError::InvalidEdit(format!("{} is a folder", path))),
            None => Err(TextdbError::NotFound(path.to_string())),
        }
    }

    /// `mkdir -p`; returns the folder id.
    pub fn ensure_folder(&self, path: &str) -> Result<i64> {
        let path = normalize_path(path)?;
        if path == "/" {
            return self.ensure_root();
        }
        if let Some(n) = self.node_by_path(&path)? {
            if n.kind != 0 {
                return Err(TextdbError::InvalidEdit(format!("{} is a file", path)));
            }
            return Ok(n.id);
        }
        let parent = self.ensure_folder(parent_of(&path))?;
        let now = Self::now();
        self.conn
            .prepare_cached(&format!(
                "INSERT INTO {}node(parent_id, name, kind, path, created_at, updated_at) VALUES (?1, ?2, 0, ?3, ?4, ?4)",
                self.p
            ))
            .map_err(sql_err)?
            .execute(params![parent, name_of(&path), path, now])
            .map_err(sql_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Create a file (parents created), commit version 1.
    pub fn create(&self, path: &str, content: &[u8], author: Option<&str>, message: Option<&str>) -> Result<u64> {
        let path = normalize_path(path)?;
        self.tx(|db| {
            if db.node_by_path(&path)?.is_some() {
                return Err(TextdbError::InvalidEdit(format!("{} already exists", path)));
            }
            let parent = db.ensure_folder(parent_of(&path))?;
            let now = Self::now();
            db.conn
                .prepare_cached(&format!(
                    "INSERT INTO {}node(parent_id, name, kind, path, created_at, updated_at, updated_by) VALUES (?1, ?2, 1, ?3, ?4, ?4, ?5)",
                    db.p
                ))
                .map_err(sql_err)?
                .execute(params![parent, name_of(&path), path, now, author])
                .map_err(sql_err)?;
            let id = db.conn.last_insert_rowid();
            let mut st = db.storage();
            let (root, chunks) = textdb_core::build_with_chunks(&mut st, &db.params, content)?;
            if !st.cas_root(id as u64, None, &root)? {
                return Err(TextdbError::Storage("initial CAS failed".into()));
            }
            let c = Committed {
                version: 1,
                root,
                kind: CommitKind::Direct,
                new_chunks: chunks,
                retries: 0,
            };
            db.record_commit(id, &path, &c, None, author, message)?;
            Ok(1)
        })
    }

    /// `INSERT … ON CONFLICT (path) DO UPDATE SET content = EXCLUDED.content` semantics:
    /// identical content produces no new version.
    pub fn upsert(&self, path: &str, content: &[u8], author: Option<&str>) -> Result<WriteResult> {
        let path = normalize_path(path)?;
        self.tx(|db| match db.node_by_path(&path)? {
            None => Ok(WriteResult {
                version: db.create(&path, content, author, Some("import"))?,
                kind: CommitKind::Direct,
            }),
            Some(_) => db.update_content(&path, content, None, author, Some("import")),
        })
    }

    fn record_commit(
        &self,
        file_id: i64,
        path: &str,
        c: &Committed,
        parent_root: Option<&Hash>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<()> {
        let st = self.storage();
        let (nbytes, nlines) = totals(&st, &c.root)?;
        let now = Self::now();
        self.conn
            .prepare_cached(&format!(
                "INSERT INTO {}commit(file_id, version, root, parent_root, author, ts, message, nbytes, nlines) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                self.p
            ))
            .map_err(sql_err)?
            .execute(params![
                file_id,
                c.version as i64,
                &c.root[..],
                parent_root.map(|h| h.to_vec()),
                author,
                now,
                message,
                nbytes as i64,
                nlines as i64
            ])
            .map_err(sql_err)?;
        self.conn
            .prepare_cached(&format!(
                "UPDATE {}node SET nbytes = ?1, nlines = ?2, updated_by = ?3 WHERE id = ?4",
                self.p
            ))
            .map_err(sql_err)?
            .execute(params![nbytes as i64, nlines as i64, author, file_id])
            .map_err(sql_err)?;
        // Reverse index for search: chunk → file, recorded once per (chunk, file).
        let mut seen = std::collections::HashSet::new();
        for h in &c.new_chunks {
            if !seen.insert(*h) {
                continue;
            }
            self.conn
                .prepare_cached(&format!(
                    "INSERT OR IGNORE INTO {p}chunk_ref(chunk_id, file_id, version) SELECT id, ?2, ?3 FROM {p}chunk WHERE hash = ?1",
                    p = self.p
                ))
                .map_err(sql_err)?
                .execute(params![&h[..], file_id, c.version as i64])
                .map_err(sql_err)?;
        }
        // Structure rows (markdown only).
        if let Some(ex) = &self.extractor {
            let lower = path.to_ascii_lowercase();
            if lower.ends_with(".md") || lower.ends_with(".markdown") {
                let bytes = materialize(&st, &c.root)?;
                let s = ex.extract(&bytes);
                for sec in &s.sections {
                    self.conn
                        .prepare_cached(&format!(
                            "INSERT INTO {}section(file_id, version, heading_path, level, line_from, line_to) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            self.p
                        ))
                        .map_err(sql_err)?
                        .execute(params![
                            file_id,
                            c.version as i64,
                            sec.heading_path,
                            sec.level as i64,
                            sec.line_from as i64,
                            sec.line_to as i64
                        ])
                        .map_err(sql_err)?;
                }
                for l in &s.links {
                    self.conn
                        .prepare_cached(&format!(
                            "INSERT INTO {}link(file_id, version, target_path, line) VALUES (?1, ?2, ?3, ?4)",
                            self.p
                        ))
                        .map_err(sql_err)?
                        .execute(params![file_id, c.version as i64, l.target_path, l.line as i64])
                        .map_err(sql_err)?;
                }
                if let Some(fm) = &s.frontmatter {
                    self.conn
                        .prepare_cached(&format!(
                            "INSERT OR REPLACE INTO {}frontmatter(file_id, version, data) VALUES (?1, ?2, ?3)",
                            self.p
                        ))
                        .map_err(sql_err)?
                        .execute(params![file_id, c.version as i64, fm.to_string()])
                        .map_err(sql_err)?;
                }
            }
        }
        Ok(())
    }

    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        let path = normalize_path(path)?;
        let n = self.file_by_path(&path)?;
        let root = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        materialize(&self.storage(), &root)
    }

    pub fn root_of_version(&self, file_id: i64, version: u64) -> Result<Hash> {
        let root: Option<Vec<u8>> = self
            .conn
            .prepare_cached(&format!("SELECT root FROM {}commit WHERE file_id = ?1 AND version = ?2", self.p))
            .map_err(sql_err)?
            .query_row(params![file_id, version as i64], |r| r.get(0))
            .optional()
            .map_err(sql_err)?;
        to_hash(&root.ok_or_else(|| TextdbError::NotFound(format!("version {} of file {}", version, file_id)))?)
    }

    /// Content at a historical version; works for tombstoned files too.
    pub fn read_version(&self, path: &str, version: u64) -> Result<Vec<u8>> {
        let path = normalize_path(path)?;
        let n = self.node_by_path_any(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let root = self.root_of_version(n.id, version)?;
        materialize(&self.storage(), &root)
    }

    /// Replace the whole content (`UPDATE kb SET content = …`). The diff OLD→NEW is
    /// computed against `base_version` (or HEAD) and committed with rebase.
    pub fn update_content(
        &self,
        path: &str,
        new_content: &[u8],
        base_version: Option<u64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<WriteResult> {
        let path = normalize_path(path)?;
        self.tx(|db| {
            let n = db.file_by_path(&path)?;
            let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
            let base = match base_version {
                Some(v) if v as i64 != n.version => db.root_of_version(n.id, v)?,
                _ => cur,
            };
            let mut st = db.storage();
            let old = materialize(&st, &base)?;
            let edits = byte_edits(&old, new_content);
            if edits.is_empty() && base == cur {
                return Ok(WriteResult {
                    version: n.version as u64,
                    kind: CommitKind::NoOp,
                });
            }
            let c = commit(&mut st, &db.params, n.id as u64, &path, &base, &edits, db.retries)?;
            if c.kind != CommitKind::NoOp {
                db.record_commit(n.id, &path, &c, Some(&cur), author, message)?;
            }
            Ok(WriteResult {
                version: c.version,
                kind: c.kind,
            })
        })
    }

    /// Commit a byte-range edit set expressed against `base_version` (or HEAD).
    pub fn commit_edits(
        &self,
        path: &str,
        edits: &[Edit],
        base_version: Option<u64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<WriteResult> {
        let path = normalize_path(path)?;
        self.tx(|db| {
            let n = db.file_by_path(&path)?;
            let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
            let base = match base_version {
                Some(v) if v as i64 != n.version => db.root_of_version(n.id, v)?,
                _ => cur,
            };
            let mut st = db.storage();
            let c = commit(&mut st, &db.params, n.id as u64, &path, &base, edits, db.retries)?;
            if c.kind != CommitKind::NoOp {
                db.record_commit(n.id, &path, &c, Some(&cur), author, message)?;
            }
            Ok(WriteResult {
                version: c.version,
                kind: c.kind,
            })
        })
    }

    /// Strict replace: `old` must occur exactly once in the current content.
    pub fn edit(&self, path: &str, old: &[u8], new: &[u8], author: Option<&str>) -> Result<WriteResult> {
        let path = normalize_path(path)?;
        self.tx(|db| {
            let n = db.file_by_path(&path)?;
            let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
            let mut st = db.storage();
            let content = materialize(&st, &cur)?;
            let pos = find_unique(&content, old)?;
            let edits = [Edit::new(pos as u64, (pos + old.len()) as u64, new.to_vec())];
            let c = commit(&mut st, &db.params, n.id as u64, &path, &cur, &edits, db.retries)?;
            if c.kind != CommitKind::NoOp {
                db.record_commit(n.id, &path, &c, Some(&cur), author, Some("edit"))?;
            }
            Ok(WriteResult {
                version: c.version,
                kind: c.kind,
            })
        })
    }

    pub fn append(&self, path: &str, tail: &[u8], author: Option<&str>) -> Result<WriteResult> {
        let path = normalize_path(path)?;
        self.tx(|db| {
            let n = db.file_by_path(&path)?;
            let cur = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
            let mut st = db.storage();
            let c = commit_append(&mut st, &db.params, n.id as u64, &path, tail, db.retries)?;
            if c.kind != CommitKind::NoOp {
                db.record_commit(n.id, &path, &c, Some(&cur), author, Some("append"))?;
            }
            Ok(WriteResult {
                version: c.version,
                kind: c.kind,
            })
        })
    }

    /// Rename/move a file or folder (subtree path rewrite, ids stable).
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let from = normalize_path(from)?;
        let to = normalize_path(to)?;
        self.tx(|db| {
            if from == "/" || to == "/" {
                return Err(TextdbError::InvalidEdit("cannot move the root".into()));
            }
            let src = db.node_by_path(&from)?.ok_or_else(|| TextdbError::NotFound(from.clone()))?;
            if to == from || to.starts_with(&format!("{}/", from)) {
                return Err(TextdbError::InvalidEdit(format!("cannot move {} into itself", from)));
            }
            if db.node_by_path(&to)?.is_some() {
                return Err(TextdbError::InvalidEdit(format!("{} already exists", to)));
            }
            let parent = db.ensure_folder(parent_of(&to))?;
            let now = Self::now();
            db.conn
                .prepare_cached(&format!(
                    "UPDATE {}node SET path = ?2 || substr(path, length(?1) + 1), updated_at = ?3 WHERE substr(path, 1, length(?1) + 1) = ?1 || '/' AND deleted_at IS NULL",
                    db.p
                ))
                .map_err(sql_err)?
                .execute(params![from, to, now])
                .map_err(sql_err)?;
            db.conn
                .prepare_cached(&format!(
                    "UPDATE {}node SET path = ?1, name = ?2, parent_id = ?3, updated_at = ?4 WHERE id = ?5",
                    db.p
                ))
                .map_err(sql_err)?
                .execute(params![to, name_of(&to), parent, now, src.id])
                .map_err(sql_err)?;
            Ok(())
        })
    }

    /// Tombstone a file or folder subtree. Content and history are retained.
    pub fn delete(&self, path: &str) -> Result<()> {
        let path = normalize_path(path)?;
        self.tx(|db| {
            if path == "/" {
                return Err(TextdbError::InvalidEdit("cannot delete the root".into()));
            }
            db.node_by_path(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
            let now = Self::now();
            db.conn
                .prepare_cached(&format!(
                    "UPDATE {}node SET deleted_at = ?2 WHERE (path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/') AND deleted_at IS NULL",
                    db.p
                ))
                .map_err(sql_err)?
                .execute(params![path, now])
                .map_err(sql_err)?;
            Ok(())
        })
    }

    pub fn ls(&self, path: &str) -> Result<Vec<Entry>> {
        let path = normalize_path(path)?;
        let dir = self.node_by_path(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT name, path, kind, nbytes, nlines, updated_at FROM {}node WHERE parent_id = ?1 AND deleted_at IS NULL ORDER BY name",
                self.p
            ))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![dir.id], |r| {
                Ok(Entry {
                    name: r.get(0)?,
                    path: r.get(1)?,
                    kind: r.get(2)?,
                    nbytes: r.get(3)?,
                    nlines: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
    }

    /// All live files under `prefix` (a folder path), sorted by path.
    pub fn list_files(&self, prefix: &str) -> Result<Vec<NodeRow>> {
        let prefix = normalize_path(prefix)?;
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT {} FROM {}node WHERE kind = 1 AND deleted_at IS NULL AND (?1 = '/' OR substr(path, 1, length(?1) + 1) = ?1 || '/') ORDER BY path",
                Self::NODE_COLS,
                self.p
            ))
            .map_err(sql_err)?;
        let rows = stmt.query_map(params![prefix], Self::row_from).map_err(sql_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
    }

    pub fn history(&self, path: &str) -> Result<Vec<CommitRow>> {
        let path = normalize_path(path)?;
        let n = self.node_by_path_any(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT version, author, ts, message, nbytes, root FROM {}commit WHERE file_id = ?1 ORDER BY version",
                self.p
            ))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![n.id], |r| {
                let root: Vec<u8> = r.get(5)?;
                Ok(CommitRow {
                    version: r.get(0)?,
                    author: r.get(1)?,
                    ts: r.get(2)?,
                    message: r.get(3)?,
                    nbytes: r.get(4)?,
                    root: to_hash(&root).unwrap_or([0u8; 32]),
                })
            })
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
    }

    /// Lines `[from, to]`, 1-based inclusive.
    pub fn lines(&self, path: &str, from: u64, to: u64) -> Result<Vec<u8>> {
        let path = normalize_path(path)?;
        let n = self.file_by_path(&path)?;
        let root = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        if from == 0 || to < from {
            return Ok(Vec::new());
        }
        textdb_core::lines(&self.storage(), &root, from - 1, to - 1)
    }

    /// Text of the section whose heading matches `heading` (HEAD version).
    pub fn section(&self, path: &str, heading: &str) -> Result<Option<Vec<u8>>> {
        let path = normalize_path(path)?;
        let n = self.file_by_path(&path)?;
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT line_from, line_to FROM {}section WHERE file_id = ?1 AND version = ?2 AND (heading_path = ?3 OR lower(heading_path) = lower(?3) OR lower(heading_path) LIKE '%/ ' || lower(?3)) ORDER BY CASE WHEN heading_path = ?3 THEN 0 ELSE 1 END LIMIT 1",
                self.p
            ))
            .map_err(sql_err)?;
        let span: Option<(i64, i64)> = stmt
            .query_row(params![n.id, n.version, heading.trim()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(sql_err)?;
        match span {
            None => Ok(None),
            Some((a, b)) => Ok(Some(self.lines(&path, a as u64, b as u64)?)),
        }
    }

    pub fn diff(&self, path: &str, v1: u64, v2: u64) -> Result<String> {
        let path = normalize_path(path)?;
        let n = self.node_by_path_any(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let a = self.root_of_version(n.id, v1)?;
        let b = self.root_of_version(n.id, v2)?;
        let body = unified_diff(&self.storage(), &a, &b, 3)?;
        if body.is_empty() {
            return Ok(String::new());
        }
        Ok(format!("--- {p}@{v1}\n+++ {p}@{v2}\n{body}", p = path, v1 = v1, v2 = v2, body = body))
    }

    /// Full-text search over chunks, mapped to (path, line, snippet) at HEAD.
    /// Query syntax: whitespace-separated terms are ANDed at *document* level; `"a b"` is a
    /// phrase; `foo*` is a prefix. Each term is looked up in the chunk FTS index, chunk hits
    /// are mapped to files through `chunk_ref`, and the per-term file sets are intersected.
    pub fn search(&self, query: &str, prefix: &str, limit: usize) -> Result<Vec<Hit>> {
        let prefix = normalize_path(prefix)?;
        let terms = query_terms(query);
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let st = self.storage();
        // Per term: file_id → (chunk_id, rank) of the best chunk hit.
        let mut per_term: Vec<std::collections::HashMap<i64, (i64, f64)>> = Vec::new();
        let mut fts_stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT rowid, rank FROM {p}fts WHERE {p}fts MATCH ?1 ORDER BY rank LIMIT ?2",
                p = self.p
            ))
            .map_err(sql_err)?;
        let mut ref_stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT r.file_id FROM {p}chunk_ref r JOIN {p}node n ON n.id = r.file_id WHERE r.chunk_id = ?1 AND n.deleted_at IS NULL AND n.kind = 1 AND (?2 = '/' OR substr(n.path, 1, length(?2) + 1) = ?2 || '/')",
                p = self.p
            ))
            .map_err(sql_err)?;
        for t in &terms {
            let q = fts5_term(t);
            let chunk_hits: Vec<(i64, f64)> = fts_stmt
                .query_map(params![q, (limit.max(1) * 50) as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            let mut files: std::collections::HashMap<i64, (i64, f64)> = Default::default();
            for (chunk_id, rank) in chunk_hits {
                let ids: Vec<i64> = ref_stmt
                    .query_map(params![chunk_id, prefix], |r| r.get(0))
                    .map_err(sql_err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sql_err)?;
                for id in ids {
                    files.entry(id).or_insert((chunk_id, rank));
                }
            }
            per_term.push(files);
        }
        // Intersect file sets; order by the first term's rank.
        let mut candidates: Vec<(i64, i64, f64)> = per_term[0]
            .iter()
            .filter(|(id, _)| per_term[1..].iter().all(|m| m.contains_key(id)))
            .map(|(id, (chunk, rank))| (*id, *chunk, *rank))
            .collect();
        candidates.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        let mut hits = Vec::new();
        let mut node_stmt = self
            .conn
            .prepare_cached(&format!("SELECT path, root FROM {}node WHERE id = ?1 AND deleted_at IS NULL", self.p))
            .map_err(sql_err)?;
        let mut chunk_stmt = self
            .conn
            .prepare_cached(&format!("SELECT hash, bytes FROM {}chunk WHERE id = ?1", self.p))
            .map_err(sql_err)?;
        for (file_id, chunk_id, rank) in candidates {
            let (path, root): (String, Vec<u8>) = match node_stmt
                .query_row(params![file_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
                .map_err(sql_err)?
            {
                Some(x) => x,
                None => continue,
            };
            let (hash, bytes): (Vec<u8>, Vec<u8>) = chunk_stmt
                .query_row(params![chunk_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sql_err)?;
            let root = to_hash(&root)?;
            let hash = to_hash(&hash)?;
            // Verify the chunk is still part of HEAD (chunk_ref is append-only) and get its line.
            let leaf = match leaves(&st, &root)?.into_iter().find(|l| l.hash == hash) {
                Some(l) => l,
                None => {
                    // The chunk left this file; fall back to a HEAD scan for the first term.
                    let body = materialize(&st, &root)?;
                    let (line, snippet) = locate_terms(&body, &terms[..1]);
                    if snippet.is_empty() {
                        continue;
                    }
                    hits.push(Hit {
                        path,
                        line: line as i64 + 1,
                        snippet,
                        rank,
                    });
                    if hits.len() >= limit {
                        break;
                    }
                    continue;
                }
            };
            let (line_in_chunk, snippet) = locate_terms(&bytes, &terms[..1]);
            hits.push(Hit {
                path,
                line: leaf.line_off as i64 + line_in_chunk as i64 + 1,
                snippet,
                rank,
            });
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    pub fn export(&self, prefix: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let st = self.storage();
        let mut out = Vec::new();
        for n in self.list_files(prefix)? {
            if let Some(root) = n.root {
                out.push((n.path, materialize(&st, &root)?));
            }
        }
        Ok(out)
    }

    /// Record all current roots under a name (spec §8.6).
    pub fn checkpoint(&self, name: &str) -> Result<usize> {
        self.tx(|db| {
            let n = db
                .conn
                .execute(
                    &format!(
                        "INSERT OR REPLACE INTO {p}checkpoint(name, file_id, path, root, version) SELECT ?1, id, path, root, version FROM {p}node WHERE kind = 1 AND deleted_at IS NULL AND root IS NOT NULL",
                        p = db.p
                    ),
                    params![name],
                )
                .map_err(sql_err)?;
            Ok(n)
        })
    }

    /// (chunks, tree nodes, commits, live files, chunk bytes)
    pub fn stats(&self) -> Result<(i64, i64, i64, i64, i64)> {
        let q = |sql: &str| -> Result<i64> { self.conn.query_row(sql, [], |r| r.get(0)).map_err(sql_err) };
        Ok((
            q(&format!("SELECT count(*) FROM {}chunk", self.p))?,
            q(&format!("SELECT count(*) FROM {}tree_node", self.p))?,
            q(&format!("SELECT count(*) FROM {}commit", self.p))?,
            q(&format!("SELECT count(*) FROM {}node WHERE kind = 1 AND deleted_at IS NULL", self.p))?,
            q(&format!("SELECT coalesce(sum(length(bytes)), 0) FROM {}chunk", self.p))?,
        ))
    }
}

fn find_unique(content: &[u8], old: &[u8]) -> Result<usize> {
    if old.is_empty() {
        return Err(TextdbError::InvalidEdit("old text is empty".into()));
    }
    let mut found = None;
    let mut i = 0;
    while i + old.len() <= content.len() {
        if &content[i..i + old.len()] == old {
            if found.is_some() {
                return Err(TextdbError::InvalidEdit("old text is not unique".into()));
            }
            found = Some(i);
            i += old.len();
        } else {
            i += 1;
        }
    }
    found.ok_or_else(|| TextdbError::InvalidEdit("old text not found".into()))
}

/// Tokenise the harness query syntax: terms, `"phrases"`, `prefix*`.
pub fn query_terms(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for ch in q.chars() {
        match ch {
            '"' => {
                in_q = !in_q;
                if !in_q && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c if c.is_whitespace() && !in_q => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter()
        .filter(|t| !t.eq_ignore_ascii_case("and"))
        .collect()
}

/// FTS5 syntax for one term (phrase or prefix aware).
pub fn fts5_term(t: &str) -> String {
    if let Some(stem) = t.strip_suffix('*') {
        format!("\"{}\" *", stem.replace('"', "\"\""))
    } else {
        format!("\"{}\"", t.replace('"', "\"\""))
    }
}

/// FTS5 syntax for a whole query (terms ANDed within one document row).
pub fn fts5_query(q: &str) -> String {
    query_terms(q).iter().map(|t| fts5_term(t)).collect::<Vec<_>>().join(" AND ")
}

/// Line (0-based, within the chunk) of the first term occurrence and that line as snippet.
fn locate_terms(bytes: &[u8], terms: &[String]) -> (usize, String) {
    let text = String::from_utf8_lossy(bytes).to_lowercase();
    let mut best: Option<usize> = None;
    for t in terms {
        let t = t.trim_end_matches('*').to_lowercase();
        if t.is_empty() {
            continue;
        }
        if let Some(p) = text.find(&t) {
            best = Some(best.map_or(p, |b| b.min(p)));
        }
    }
    let pos = best.unwrap_or(0);
    let line = text[..pos].matches('\n').count();
    let raw = String::from_utf8_lossy(bytes);
    let snippet = raw.lines().nth(line).unwrap_or("").chars().take(200).collect();
    (line, snippet)
}
