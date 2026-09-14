//! Namespace layer over the shadow tables: folders, files, commits, structure rows,
//! search. Every algorithm comes from `textdb-core`; this module only persists.

use rusqlite::{params, Connection, OptionalExtension};
use textdb_core::commit::{commit, commit_append, CommitKind, Committed};
use textdb_core::myers::byte_edits;
use textdb_core::storage::Result;
use textdb_core::tree::totals;
use textdb_core::{
    count_words, unified_diff, word_delta, ChunkParams, Edit, Hash, LeafRef, LineHunk, PathOp, Storage, StructureExtractor, TextdbError,
};
use textdb_md::MarkdownExtractor;

use crate::stats::Totals;
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

/// A file or folder in a listing. For a folder the size, line, word and version figures are
/// totals over every live file below it.
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub kind: i64,
    pub nbytes: Option<i64>,
    pub nlines: Option<i64>,
    pub nwords: Option<i64>,
    /// A file's version; a folder's total of versions below it.
    pub versions: i64,
    /// A file's last commit or move; for a folder the latest change to it or anywhere below.
    pub updated_at: String,
    pub updated_by: Option<String>,
    pub created_at: String,
    /// Folder only: live files and folders anywhere below it.
    pub files: Option<i64>,
    pub folders: Option<i64>,
    /// File only: everyone who committed to it, most commits first.
    pub authors: Vec<AuthorCount>,
}

/// One author's commits to a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorCount {
    /// `None` for commits made without an author.
    pub author: Option<String>,
    pub commits: i64,
    pub first_ts: String,
    pub last_ts: String,
}

impl AuthorCount {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"author": self.author, "commits": self.commits, "first_ts": self.first_ts, "last_ts": self.last_ts})
    }
}

impl Entry {
    /// The entry as `textdb_ls` columns name it, `authors` as an array.
    pub fn to_json(&self) -> serde_json::Value {
        let file = self.kind == 1;
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "path": self.path,
            "kind": if file { "file" } else { "folder" },
            "nbytes": self.nbytes,
            "nlines": self.nlines,
            "nwords": self.nwords,
            "versions": self.versions,
            "updated_at": self.updated_at,
            "updated_by": self.updated_by,
            "created_at": self.created_at,
            "files": self.files,
            "folders": self.folders,
            "nauthors": if file { Some(self.authors.len()) } else { None },
            "authors": self.authors.iter().map(AuthorCount::to_json).collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct CommitRow {
    pub version: i64,
    pub author: Option<String>,
    pub ts: String,
    pub message: Option<String>,
    pub nbytes: Option<i64>,
    pub root: Hash,
    /// How the commit landed: `direct`, `rebased` or `merged`. `None` for commits written
    /// before the column existed.
    pub kind: Option<String>,
    /// The version the writer started from; `None` for a file's first version.
    pub base_version: Option<i64>,
}

/// One entry of the change feed: a commit, folder creation, move or delete, in the order
/// they were made. `seq` only grows, so a reader that remembers the last one it saw can ask
/// for exactly what came after.
#[derive(Clone, Debug)]
pub struct ChangeRow {
    pub seq: i64,
    pub ts: String,
    /// `create`, `commit`, `mkdir`, `move`, `delete` or `purge` (removed from the trash).
    pub op: String,
    pub node_id: i64,
    /// 0 folder, 1 file.
    pub node_kind: i64,
    /// The path after the change.
    pub path: String,
    /// For a move, the path before it.
    pub old_path: Option<String>,
    pub version: Option<i64>,
    pub base_version: Option<i64>,
    pub commit_kind: Option<String>,
    pub author: Option<String>,
    pub message: Option<String>,
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
    /// Record renames, moves and deletes (`Some`), or leave it to the store's `path_history`
    /// setting (`None`). See [`TextDb::path_history_enabled`].
    pub path_history: Option<bool>,
    /// The message recorded for edits, appends, line replacements, moves and deletes made
    /// through this handle, instead of their defaults (`edit`, `append`, `replace-lines`, none).
    pub message: Option<String>,
}

pub(crate) fn to_hash(v: &[u8]) -> Result<Hash> {
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

/// Half-open `[lo, hi)` bounds selecting everything strictly under the folder `path`.
///
/// A prefix test written `substr(path, 1, length(?1) + 1) = ?1 || '/'` is a function of the
/// column, so no index can serve it and every such query scans the whole `node` table —
/// measured at 58x a range over 2000 files, and 147x at path depth 1000. The same set as a
/// range uses `{p}node_path` directly.
///
/// `'0'` is `0x30` and `'/'` is `0x2F`, so under the `BINARY` collation that `path` is
/// stored with, `prefix || '0'` is the immediate successor of `prefix || '/'` and the range
/// is exact: nothing sorts between them. `None` for the root, which has no bound — every
/// path is under it.
pub fn subtree_bounds(path: &str) -> Option<(String, String)> {
    if path == "/" {
        return None;
    }
    Some((format!("{}/", path), format!("{}0", path)))
}

impl<'c> TextDb<'c> {
    /// Create the shadow tables if missing and return a handle.
    pub fn open(conn: &'c Connection, prefix: &str) -> Result<Self> {
        crate::schema::migrate(conn, prefix).map_err(sql_err)?;
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
            path_history: None,
            message: None,
        }
    }

    /// Record `message` on the edits, appends, line replacements, moves and deletes made
    /// through this handle.
    pub fn with_message(mut self, message: Option<&str>) -> Self {
        self.message = message.map(str::to_string);
        self
    }

    /// This handle's own choice about recording renames, moves and deletes; `None` follows the
    /// store's setting.
    pub fn with_path_history(mut self, on: Option<bool>) -> Self {
        self.path_history = on;
        self
    }

    pub(crate) fn storage(&self) -> SqliteStorage<'c> {
        SqliteStorage::new(self.conn, &self.p)
    }

    pub(crate) fn now() -> String {
        SqliteStorage::now()
    }

    /// Run `f` atomically. A handle that manages transactions opens `BEGIN IMMEDIATE …
    /// COMMIT` in autocommit mode (this also works from inside a scalar SQL function and
    /// avoids one autocommit per nested statement) and a nested `SAVEPOINT` inside an
    /// explicit transaction. Inside a virtual-table update (`manage_tx == false`) the
    /// enclosing statement's transaction already makes the operation atomic, and SQLite
    /// forbids savepoints while a write statement is in progress, so `f` runs inline.
    pub fn tx<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        if !self.manage_tx {
            return f(self);
        }
        let (begin, commit, rollback) = if self.conn.is_autocommit() {
            ("BEGIN IMMEDIATE", "COMMIT", "ROLLBACK")
        } else {
            ("SAVEPOINT textdb_op", "RELEASE textdb_op", "ROLLBACK TO textdb_op; RELEASE textdb_op")
        };
        self.conn.execute_batch(begin).map_err(sql_err)?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch(commit).map_err(sql_err)?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch(rollback);
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

    pub(crate) fn row_from(r: &rusqlite::Row) -> rusqlite::Result<NodeRow> {
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

    pub(crate) const NODE_COLS: &'static str =
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
        // Walk up to the deepest folder that already exists, collecting what is missing,
        // then create those top down. Recursing per component instead costs one stack
        // frame per path segment, which overflows on a deeply nested path.
        let mut missing: Vec<String> = Vec::new();
        let mut cur = path.clone();
        let mut parent = loop {
            if cur == "/" {
                break self.ensure_root()?;
            }
            match self.node_by_path(&cur)? {
                Some(n) if n.kind != 0 => return Err(TextdbError::InvalidEdit(format!("{} is a file", cur))),
                Some(n) => break n.id,
                None => {
                    let up = parent_of(&cur).to_string();
                    missing.push(std::mem::replace(&mut cur, up));
                }
            }
        };
        let now = Self::now();
        // `missing` runs deepest first, so a folder's index is the number of new folders below it.
        for (below, p) in missing.iter().enumerate().rev() {
            self.conn
                .prepare_cached(&format!(
                    "INSERT INTO {}node(parent_id, name, kind, path, created_at, updated_at, t_folders, t_updated_at) \
                     VALUES (?1, ?2, 0, ?3, ?4, ?4, ?5, CASE WHEN ?5 > 0 THEN ?4 END)",
                    self.p
                ))
                .map_err(sql_err)?
                .execute(params![parent, name_of(p), p, now, below as i64])
                .map_err(sql_err)?;
            parent = self.conn.last_insert_rowid();
            self.record_change("mkdir", parent, 0, p, None, None, None, None, None, None)?;
        }
        if let Some(top) = missing.last() {
            let added = Totals {
                folders: missing.len() as i64,
                ..Totals::default()
            };
            self.add_to_ancestors(top, &added, &now)?;
        }
        Ok(parent)
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
            // The caller's buffer *is* this root's content; say so before anything reads it
            // back (the structure sidecar immediately, and usually the client straight
            // after) rather than reassembling it from the chunks just written.
            st.remember_document(&root, content);
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
            db.record_commit(id, &path, &c, None, None, author, message)?;
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

    #[allow(clippy::too_many_arguments)]
    fn record_commit(
        &self,
        file_id: i64,
        path: &str,
        c: &Committed,
        parent_root: Option<&Hash>,
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<()> {
        let st = self.storage();
        let (nbytes, nlines) = totals(&st, &c.root)?;
        let now = Self::now();
        let (old_bytes, old_lines, old_words): (Option<i64>, Option<i64>, Option<i64>) = self
            .conn
            .prepare_cached(&format!("SELECT nbytes, nlines, nwords FROM {}node WHERE id = ?1", self.p))
            .map_err(sql_err)?
            .query_row(params![file_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(sql_err)?;
        // Words over the lines this commit changed when the previous count is known; a first
        // version counts its whole content, which the caller has just put in the cache.
        let nwords = match (parent_root, old_words) {
            (Some(parent), Some(words)) => words + word_delta(&st, parent, &c.root)?,
            _ => count_words(&st.document(&c.root)?.0) as i64,
        };
        self.conn
            .prepare_cached(&format!(
                "INSERT INTO {}commit(file_id, version, root, parent_root, author, ts, message, nbytes, nlines, kind, base_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                nlines as i64,
                c.kind.as_str(),
                base_version
            ])
            .map_err(sql_err)?;
        let op = if c.version == 1 { "create" } else { "commit" };
        self.record_change(
            op,
            file_id,
            1,
            path,
            None,
            Some(c.version as i64),
            base_version,
            Some(c.kind.as_str()),
            author,
            message,
        )?;
        self.conn
            .prepare_cached(&format!(
                "INSERT INTO {}file_author(file_id, author, commits, first_ts, last_ts) VALUES (?1, coalesce(?2, ''), 1, ?3, ?3) \
                 ON CONFLICT(file_id, author) DO UPDATE SET commits = commits + 1, last_ts = excluded.last_ts",
                self.p
            ))
            .map_err(sql_err)?
            .execute(params![file_id, author, now])
            .map_err(sql_err)?;
        self.conn
            .prepare_cached(&format!(
                "UPDATE {p}node SET nbytes = ?1, nlines = ?2, updated_by = ?3, nwords = ?5, \
                 nauthors = (SELECT count(*) FROM {p}file_author WHERE file_id = ?4) WHERE id = ?4",
                p = self.p
            ))
            .map_err(sql_err)?
            .execute(params![nbytes as i64, nlines as i64, author, file_id, nwords])
            .map_err(sql_err)?;
        let change = Totals {
            files: (c.version == 1) as i64,
            folders: 0,
            bytes: nbytes as i64 - old_bytes.unwrap_or(0),
            lines: nlines as i64 - old_lines.unwrap_or(0),
            words: nwords - old_words.unwrap_or(0),
            versions: 1,
        };
        self.add_to_ancestors(path, &change, &now)?;
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
        // Structure rows (markdown only), kept for HEAD only: per-version rows cost more
        // than the chunk data itself (ADR 0007); historical structure is recomputable.
        if let Some(ex) = &self.extractor {
            let lower = path.to_ascii_lowercase();
            if lower.ends_with(".md") || lower.ends_with(".markdown") {
                // No `.clone()`: `extract` wants a slice, and the cache already holds the
                // bytes — seeded by the caller for a whole-content write, materialised once
                // here otherwise (which also warms the cache for whoever reads next).
                let doc = st.document(&c.root)?.0;
                let s = ex.extract(&doc);
                self.write_structure(file_id, c.version as i64, &s)?;
            }
        }
        Ok(())
    }

    /// Replace a file's structure rows, writing nothing when they already say the same thing.
    ///
    /// These rows are HEAD-only (ADR 0007) and derived, so every commit used to delete them
    /// and re-insert one statement per section and per link — 339 inserts for a 1 MiB
    /// document whose headings a one-line body edit had not touched. Structure changes far
    /// less often than content, so the rows are compared first and rewritten only when they
    /// differ; when they match, all that is left to do is carry the `version` column
    /// forward, since `section()` looks rows up at the file's current version.
    ///
    /// A rewrite batches its inserts into one multi-`VALUES` statement per table instead of
    /// one statement per row.
    fn write_structure(&self, file_id: i64, version: i64, s: &textdb_core::structure::Structure) -> Result<()> {
        if self.structure_matches(file_id, s)? {
            for t in ["section", "link"] {
                self.conn
                    .prepare_cached(&format!("UPDATE {}{} SET version = ?1 WHERE file_id = ?2 AND version <> ?1", self.p, t))
                    .map_err(sql_err)?
                    .execute(params![version, file_id])
                    .map_err(sql_err)?;
            }
            if s.frontmatter.is_some() {
                self.conn
                    .prepare_cached(&format!(
                        "UPDATE {}frontmatter SET version = ?1 WHERE file_id = ?2 AND version <> ?1",
                        self.p
                    ))
                    .map_err(sql_err)?
                    .execute(params![version, file_id])
                    .map_err(sql_err)?;
            }
            return Ok(());
        }
        for t in ["section", "link", "frontmatter"] {
            self.conn
                .prepare_cached(&format!("DELETE FROM {}{} WHERE file_id = ?1", self.p, t))
                .map_err(sql_err)?
                .execute(params![file_id])
                .map_err(sql_err)?;
        }
        // SQLite's default parameter limit is 32766, so a document with a very large number
        // of headings is written in several batches rather than one statement.
        const MAX_ROWS_PER_BATCH: usize = 4000;
        for batch in s.sections.chunks(MAX_ROWS_PER_BATCH) {
            let mut sql = format!(
                "INSERT INTO {}section(file_id, version, heading_path, level, line_from, line_to) VALUES ",
                self.p
            );
            for i in 0..batch.len() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str("(?,?,?,?,?,?)");
            }
            let mut vals: Vec<rusqlite::types::Value> = Vec::with_capacity(batch.len() * 6);
            for sec in batch {
                vals.push(file_id.into());
                vals.push(version.into());
                vals.push(sec.heading_path.clone().into());
                vals.push((sec.level as i64).into());
                vals.push((sec.line_from as i64).into());
                vals.push((sec.line_to as i64).into());
            }
            self.conn
                .prepare_cached(&sql)
                .map_err(sql_err)?
                .execute(rusqlite::params_from_iter(vals))
                .map_err(sql_err)?;
        }
        for batch in s.links.chunks(MAX_ROWS_PER_BATCH) {
            let mut sql = format!("INSERT INTO {}link(file_id, version, target_path, line) VALUES ", self.p);
            for i in 0..batch.len() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str("(?,?,?,?)");
            }
            let mut vals: Vec<rusqlite::types::Value> = Vec::with_capacity(batch.len() * 4);
            for l in batch {
                vals.push(file_id.into());
                vals.push(version.into());
                vals.push(l.target_path.clone().into());
                vals.push((l.line as i64).into());
            }
            self.conn
                .prepare_cached(&sql)
                .map_err(sql_err)?
                .execute(rusqlite::params_from_iter(vals))
                .map_err(sql_err)?;
        }
        if let Some(fm) = &s.frontmatter {
            self.conn
                .prepare_cached(&format!(
                    "INSERT OR REPLACE INTO {}frontmatter(file_id, version, data) VALUES (?1, ?2, ?3)",
                    self.p
                ))
                .map_err(sql_err)?
                .execute(params![file_id, version, fm.to_string()])
                .map_err(sql_err)?;
        }
        Ok(())
    }

    /// Do the stored rows for `file_id` already describe `s`, version aside?
    fn structure_matches(&self, file_id: i64, s: &textdb_core::structure::Structure) -> Result<bool> {
        let mut st = self
            .conn
            .prepare_cached(&format!(
                "SELECT heading_path, level, line_from, line_to FROM {}section WHERE file_id = ?1 ORDER BY rowid",
                self.p
            ))
            .map_err(sql_err)?;
        let mut rows = st.query(params![file_id]).map_err(sql_err)?;
        for sec in &s.sections {
            let Some(r) = rows.next().map_err(sql_err)? else {
                return Ok(false);
            };
            let stored: (String, i64, i64, i64) = (
                r.get(0).map_err(sql_err)?,
                r.get(1).map_err(sql_err)?,
                r.get(2).map_err(sql_err)?,
                r.get(3).map_err(sql_err)?,
            );
            if stored != (sec.heading_path.clone(), sec.level as i64, sec.line_from as i64, sec.line_to as i64) {
                return Ok(false);
            }
        }
        if rows.next().map_err(sql_err)?.is_some() {
            return Ok(false);
        }
        drop(rows);

        let mut st = self
            .conn
            .prepare_cached(&format!(
                "SELECT target_path, line FROM {}link WHERE file_id = ?1 ORDER BY rowid",
                self.p
            ))
            .map_err(sql_err)?;
        let mut rows = st.query(params![file_id]).map_err(sql_err)?;
        for l in &s.links {
            let Some(r) = rows.next().map_err(sql_err)? else {
                return Ok(false);
            };
            let stored: (String, i64) = (r.get(0).map_err(sql_err)?, r.get(1).map_err(sql_err)?);
            if stored != (l.target_path.clone(), l.line as i64) {
                return Ok(false);
            }
        }
        if rows.next().map_err(sql_err)?.is_some() {
            return Ok(false);
        }
        drop(rows);

        let stored_fm: Option<Option<String>> = self
            .conn
            .prepare_cached(&format!("SELECT data FROM {}frontmatter WHERE file_id = ?1", self.p))
            .map_err(sql_err)?
            .query_row(params![file_id], |r| r.get(0))
            .optional()
            .map_err(sql_err)?;
        let want = s.frontmatter.as_ref().map(|fm| fm.to_string());
        Ok(stored_fm.flatten() == want)
    }

    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        Ok((*self.read_shared(path)?.0).clone())
    }

    /// HEAD content as the cache holds it, with whether it is valid UTF-8.
    ///
    /// The `Vec`-returning `read` copies the whole document for its caller to copy again
    /// into SQLite. Anything handing bytes straight back — the scalar functions, the
    /// virtual table's content column — wants the shared buffer and the UTF-8 answer that
    /// came with it, since both are properties of content the cache is keyed by.
    pub fn read_shared(&self, path: &str) -> Result<(std::sync::Arc<Vec<u8>>, bool)> {
        let path = normalize_path(path)?;
        let n = self.file_by_path(&path)?;
        let root = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        self.storage().document(&root)
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
        Ok((*self.read_version_shared(path, version)?.0).clone())
    }

    /// As `read_version`, returning the cached buffer and its UTF-8 flag.
    ///
    /// Two statements, deliberately. Folding them into one join or one correlated subquery
    /// was tried and is much worse, because neither can use an index for the part that makes
    /// the lookup cheap. `{p}node_path` is a *partial* index (`WHERE deleted_at IS NULL`),
    /// and a historical read has to find tombstoned files too, so any formulation that
    /// expresses "live row first, else the most recently deleted one" as an `ORDER BY` loses
    /// the index and sorts. Measured over 300 files: two statements 3.6 us, subquery 17.4 us
    /// (scans `node`), join 39.4 us (scans `commit`, whose primary key starts at `file_id`
    /// and cannot serve a filter on `version` alone). `node_by_path_any` instead tries the
    /// indexed live lookup first and only falls back to the scan when the path is not live.
    pub fn read_version_shared(&self, path: &str, version: u64) -> Result<(std::sync::Arc<Vec<u8>>, bool)> {
        let path = normalize_path(path)?;
        let n = self.node_by_path_any(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let root = self.root_of_version(n.id, version)?;
        self.storage().document(&root)
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
            let old = (*st.document(&base)?.0).clone();
            let edits = byte_edits(&old, new_content);
            if edits.is_empty() && base == cur {
                return Ok(WriteResult {
                    version: n.version as u64,
                    kind: CommitKind::NoOp,
                });
            }
            let c = commit(&mut st, &db.params, n.id as u64, &path, &base, &edits, db.retries)?;
            if c.kind != CommitKind::NoOp {
                // A `Direct` commit landed exactly the caller's bytes, so the cache can be
                // told what they are. `Rebased` and `Merged` landed something else — the
                // merge of this write and a concurrent one — so those have to be read back.
                if c.kind == CommitKind::Direct {
                    st.remember_document(&c.root, new_content);
                }
                let base_v = base_version.map_or(n.version, |v| v as i64);
                db.record_commit(n.id, &path, &c, Some(&cur), Some(base_v), author, message)?;
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
                let base_v = base_version.map_or(n.version, |v| v as i64);
                db.record_commit(n.id, &path, &c, Some(&cur), Some(base_v), author, message)?;
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
            let content = (*st.document(&cur)?.0).clone();
            let pos = find_unique(&content, old)?;
            let edits = [Edit::new(pos as u64, (pos + old.len()) as u64, new.to_vec())];
            let c = commit(&mut st, &db.params, n.id as u64, &path, &cur, &edits, db.retries)?;
            if c.kind != CommitKind::NoOp {
                db.record_commit(n.id, &path, &c, Some(&cur), Some(n.version), author, db.message.as_deref().or(Some("edit")))?;
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
                db.record_commit(n.id, &path, &c, Some(&cur), Some(n.version), author, db.message.as_deref().or(Some("append")))?;
            }
            Ok(WriteResult {
                version: c.version,
                kind: c.kind,
            })
        })
    }

    /// Rename/move a file or folder (subtree path rewrite, ids stable).
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.rename_by(from, to, None)
    }

    /// As [`rename`](Self::rename), attributing the move in the change feed.
    pub fn rename_by(&self, from: &str, to: &str, author: Option<&str>) -> Result<()> {
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
            let moved = db.subtree_totals(src.id)?;
            db.add_to_ancestors(&from, &moved.neg(), &now)?;
            let seq = db.record_change("move", src.id, src.kind, &to, Some(&from), None, None, None, author, db.message.as_deref())?;
            if db.path_history_enabled()? {
                db.record_path_events(PathOp::classify(&from, &to), &src, Some(&to), author, seq, &now)?;
            }
            // `from` is never "/" here, so the subtree always has bounds.
            let (lo, hi) = subtree_bounds(&from).expect("rename rejects the root above");
            db.conn
                .prepare_cached(&format!(
                    "UPDATE {}node SET path = ?2 || substr(path, length(?1) + 1), updated_at = ?3 \
                     WHERE path >= ?4 AND path < ?5 AND deleted_at IS NULL",
                    db.p
                ))
                .map_err(sql_err)?
                .execute(params![from, to, now, lo, hi])
                .map_err(sql_err)?;
            db.conn
                .prepare_cached(&format!(
                    "UPDATE {}node SET path = ?1, name = ?2, parent_id = ?3, updated_at = ?4 WHERE id = ?5",
                    db.p
                ))
                .map_err(sql_err)?
                .execute(params![to, name_of(&to), parent, now, src.id])
                .map_err(sql_err)?;
            db.add_to_ancestors(&to, &moved, &now)?;
            Ok(())
        })
    }

    /// Tombstone a file or folder subtree. Content and history are retained.
    pub fn delete(&self, path: &str) -> Result<()> {
        self.delete_by(path, None)
    }

    /// As [`delete`](Self::delete), attributing the change in the feed.
    pub fn delete_by(&self, path: &str, author: Option<&str>) -> Result<()> {
        let path = normalize_path(path)?;
        self.tx(|db| {
            if path == "/" {
                return Err(TextdbError::InvalidEdit("cannot delete the root".into()));
            }
            let n = db.node_by_path(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
            let now = Self::now();
            let gone = db.subtree_totals(n.id)?;
            db.add_to_ancestors(&path, &gone.neg(), &now)?;
            let seq = db.record_change("delete", n.id, n.kind, &path, None, None, None, None, author, db.message.as_deref())?;
            if db.path_history_enabled()? {
                db.record_path_events(PathOp::Delete, &n, None, author, seq, &now)?;
            }
            let (lo, hi) = subtree_bounds(&path).expect("delete rejects the root above");
            db.conn
                .prepare_cached(&format!(
                    "UPDATE {}node SET deleted_at = ?2 \
                     WHERE (path = ?1 OR (path >= ?3 AND path < ?4)) AND deleted_at IS NULL",
                    db.p
                ))
                .map_err(sql_err)?
                .execute(params![path, now, lo, hi])
                .map_err(sql_err)?;
            Ok(())
        })
    }

    /// The live files and folders directly in the folder `path`, by name.
    pub fn ls(&self, path: &str) -> Result<Vec<Entry>> {
        self.list(path, false)
    }

    /// As [`ls`](Self::ls), or with `recursive` every live file and folder below `path`, by
    /// path.
    pub fn list(&self, path: &str, recursive: bool) -> Result<Vec<Entry>> {
        use rusqlite::types::Value;
        let path = normalize_path(path)?;
        let dir = self.node_by_path(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        match (recursive, subtree_bounds(&path)) {
            (false, _) => self.entries_where("parent_id = ?1", vec![Value::Integer(dir.id)], "name"),
            (true, None) => self.entries_where("path <> '/'", vec![], "path"),
            (true, Some((lo, hi))) => self.entries_where("path >= ?1 AND path < ?2", vec![Value::Text(lo), Value::Text(hi)], "path"),
        }
    }

    /// The live file or folder at `path` as a listing shows it; the root too.
    pub fn entry(&self, path: &str) -> Result<Entry> {
        let path = normalize_path(path)?;
        let n = self.node_by_path(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        self.entries_where("id = ?1", vec![rusqlite::types::Value::Integer(n.id)], "id")?
            .pop()
            .ok_or(TextdbError::NotFound(path))
    }

    /// Live nodes matching `scope`, a condition over unqualified node columns: the same
    /// condition selects the nodes and, joined, their authors.
    fn entries_where(&self, scope: &str, args: Vec<rusqlite::types::Value>, order: &str) -> Result<Vec<Entry>> {
        let mut authors: std::collections::HashMap<i64, Vec<AuthorCount>> = std::collections::HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare_cached(&format!(
                    "SELECT file_id, author, commits, first_ts, last_ts FROM {p}file_author JOIN {p}node ON id = file_id \
                     WHERE kind = 1 AND deleted_at IS NULL AND {scope} ORDER BY file_id, commits DESC, last_ts DESC",
                    p = self.p
                ))
                .map_err(sql_err)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(args.iter())).map_err(sql_err)?;
            while let Some(r) = rows.next().map_err(sql_err)? {
                let author: String = r.get(1).map_err(sql_err)?;
                authors.entry(r.get(0).map_err(sql_err)?).or_default().push(AuthorCount {
                    author: (!author.is_empty()).then_some(author),
                    commits: r.get(2).map_err(sql_err)?,
                    first_ts: r.get(3).map_err(sql_err)?,
                    last_ts: r.get(4).map_err(sql_err)?,
                });
            }
        }
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT id, name, path, kind, \
                 CASE kind WHEN 1 THEN nbytes ELSE t_bytes END, CASE kind WHEN 1 THEN nlines ELSE t_lines END, \
                 CASE kind WHEN 1 THEN nwords ELSE t_words END, CASE kind WHEN 1 THEN version ELSE t_versions END, \
                 CASE WHEN kind = 0 AND t_updated_at > updated_at THEN t_updated_at ELSE updated_at END, \
                 updated_by, created_at, CASE kind WHEN 0 THEN t_files END, CASE kind WHEN 0 THEN t_folders END \
                 FROM {}node WHERE deleted_at IS NULL AND {scope} ORDER BY {order}",
                self.p
            ))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                let id: i64 = r.get(0)?;
                Ok(Entry {
                    id,
                    name: r.get(1)?,
                    path: r.get(2)?,
                    kind: r.get(3)?,
                    nbytes: r.get(4)?,
                    nlines: r.get(5)?,
                    nwords: r.get(6)?,
                    versions: r.get(7)?,
                    updated_at: r.get(8)?,
                    updated_by: r.get(9)?,
                    created_at: r.get(10)?,
                    files: r.get(11)?,
                    folders: r.get(12)?,
                    authors: authors.remove(&id).unwrap_or_default(),
                })
            })
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
    }

    /// All live files under `prefix` (a folder path), sorted by path.
    pub fn list_files(&self, prefix: &str) -> Result<Vec<NodeRow>> {
        let prefix = normalize_path(prefix)?;
        // Two statements rather than one with `?1 = '/' OR …`: the root case has no bounds,
        // and a query that has to evaluate the alternative cannot use the range for a seek.
        match subtree_bounds(&prefix) {
            None => {
                let mut stmt = self
                    .conn
                    .prepare_cached(&format!(
                        "SELECT {} FROM {}node WHERE kind = 1 AND deleted_at IS NULL ORDER BY path",
                        Self::NODE_COLS,
                        self.p
                    ))
                    .map_err(sql_err)?;
                let rows = stmt.query_map([], Self::row_from).map_err(sql_err)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
            }
            Some((lo, hi)) => {
                let mut stmt = self
                    .conn
                    .prepare_cached(&format!(
                        "SELECT {} FROM {}node WHERE kind = 1 AND deleted_at IS NULL \
                         AND path >= ?1 AND path < ?2 ORDER BY path",
                        Self::NODE_COLS,
                        self.p
                    ))
                    .map_err(sql_err)?;
                let rows = stmt.query_map(params![lo, hi], Self::row_from).map_err(sql_err)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
            }
        }
    }

    pub fn history(&self, path: &str) -> Result<Vec<CommitRow>> {
        let path = normalize_path(path)?;
        let n = self.node_by_path_any(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        self.commits_of(n.id)
    }

    /// Every commit of the file with id `file_id`, oldest first.
    pub(crate) fn commits_of(&self, file_id: i64) -> Result<Vec<CommitRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT version, author, ts, message, nbytes, root, kind, base_version FROM {}commit WHERE file_id = ?1 ORDER BY version",
                self.p
            ))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![file_id], |r| {
                let root: Vec<u8> = r.get(5)?;
                Ok(CommitRow {
                    version: r.get(0)?,
                    author: r.get(1)?,
                    ts: r.get(2)?,
                    message: r.get(3)?,
                    nbytes: r.get(4)?,
                    root: to_hash(&root).unwrap_or([0u8; 32]),
                    kind: r.get(6)?,
                    base_version: r.get(7)?,
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

    /// Line hunks that turn version `v1` of a file into version `v2`, for a client patching
    /// what it already displays. Version 0 is the empty document before the file existed,
    /// so `hunks(path, 0, 1)` is the whole first version as one insertion.
    pub fn hunks(&self, path: &str, v1: u64, v2: u64) -> Result<Vec<LineHunk>> {
        let path = normalize_path(path)?;
        let n = self.node_by_path_any(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        let st = self.storage();
        if v1 == v2 {
            return Ok(Vec::new());
        }
        if v1 == 0 || v2 == 0 {
            // There is no stored root for "nothing" to diff against, and a read should not
            // write one. One side is empty, so the hunk is the other side, whole.
            let root = self.root_of_version(n.id, v1.max(v2))?;
            let text = (*st.document(&root)?.0).clone();
            if text.is_empty() {
                return Ok(Vec::new());
            }
            let count = textdb_core::myers::split_lines(&text).len() as u64;
            let (old_count, new_count, old_text, new_text) =
                if v1 == 0 { (0, count, Vec::new(), text) } else { (count, 0, text, Vec::new()) };
            return Ok(vec![LineHunk {
                old_from: 0,
                old_count,
                new_from: 0,
                new_count,
                old_text,
                new_text,
            }]);
        }
        let a = self.root_of_version(n.id, v1)?;
        let b = self.root_of_version(n.id, v2)?;
        textdb_core::line_hunks(&st, &a, &b)
    }

    /// The chunks of a file at `version` (HEAD when `None`), in document order, with where
    /// each one sits. Unchanged content keeps its hash from one version to the next, so a
    /// client comparing two listings knows which parts of a document it can leave alone.
    pub fn chunks(&self, path: &str, version: Option<u64>) -> Result<Vec<LeafRef>> {
        let path = normalize_path(path)?;
        let root = match version {
            None => self.file_by_path(&path)?.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?,
            Some(v) => {
                let n = self.node_by_path_any(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
                self.root_of_version(n.id, v)?
            }
        };
        textdb_core::leaves(&self.storage(), &root)
    }

    /// Create the file, or replace its content exactly as [`update_content`](Self::update_content) does.
    pub fn write(
        &self,
        path: &str,
        content: &[u8],
        base_version: Option<u64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<WriteResult> {
        let path = normalize_path(path)?;
        self.tx(|db| match db.node_by_path(&path)? {
            None => Ok(WriteResult {
                version: db.create(&path, content, author, message)?,
                kind: CommitKind::Direct,
            }),
            Some(_) => db.update_content(&path, content, base_version, author, message),
        })
    }

    /// Replace lines `[from, to]` (1-based, inclusive) with `text`, where the numbers refer
    /// to `base_version` (HEAD when `None`). `to = from - 1` inserts in front of line `from`
    /// without replacing anything, and `from` one past the last line appends.
    ///
    /// The range is resolved against the version the caller read and committed with that
    /// version as its base, so a commit that landed elsewhere in the file in the meantime
    /// does not shift it: the edit is rebased like any other write, or conflicts if the same
    /// lines changed.
    pub fn replace_lines(
        &self,
        path: &str,
        from: u64,
        to: u64,
        text: &[u8],
        base_version: Option<u64>,
        author: Option<&str>,
    ) -> Result<WriteResult> {
        self.replace_line_ranges(path, &[(from, to, text.to_vec())], base_version, author)
    }

    /// Replace several line ranges `(from, to, text)`, all numbered as in `base_version`, in one
    /// commit. Each is as in [`TextDb::replace_lines`]; they may come in any order, but must not
    /// overlap or start at the same line.
    pub fn replace_line_ranges(
        &self,
        path: &str,
        ranges: &[(u64, u64, Vec<u8>)],
        base_version: Option<u64>,
        author: Option<&str>,
    ) -> Result<WriteResult> {
        let path = normalize_path(path)?;
        let mut sorted: Vec<&(u64, u64, Vec<u8>)> = ranges.iter().collect();
        sorted.sort_by_key(|r| r.0);
        if sorted.is_empty() {
            return Err(TextdbError::InvalidEdit("no line ranges".into()));
        }
        for &&(from, to, _) in &sorted {
            if from == 0 || to + 1 < from {
                return Err(TextdbError::InvalidEdit(format!("invalid line range {}-{}", from, to)));
            }
        }
        for w in sorted.windows(2) {
            if w[1].0 <= w[0].1 || w[1].0 == w[0].0 {
                return Err(TextdbError::InvalidEdit(format!(
                    "line ranges {}-{} and {}-{} overlap",
                    w[0].0, w[0].1, w[1].0, w[1].1
                )));
            }
        }
        let n = self.file_by_path(&path)?;
        let base_v = base_version.unwrap_or(n.version as u64);
        let root = match n.root {
            Some(r) if base_v as i64 == n.version => r,
            _ => self.root_of_version(n.id, base_v)?,
        };
        let st = self.storage();
        let (len, newlines) = totals(&st, &root)?;
        let unterminated = len > 0 && textdb_core::materialize_range(&st, &root, len - 1, len)? != b"\n";
        let nlines = newlines + unterminated as u64;
        let mut edits = Vec::with_capacity(sorted.len());
        for &&(from, to, ref text) in &sorted {
            if from - 1 > nlines || to > nlines {
                return Err(TextdbError::InvalidEdit(format!(
                    "lines {}-{} are outside {}, which has {} lines at version {}",
                    from, to, path, nlines, base_v
                )));
            }
            let mut replacement = text.clone();
            let start = match textdb_core::locate_line(&st, &root, from - 1)? {
                Some(off) => off,
                // Appending after a last line with no newline: supply one, or the new text
                // would run on from that line instead of following it.
                None => {
                    replacement.insert(0, b'\n');
                    len
                }
            };
            let end = if to < from {
                start
            } else {
                textdb_core::locate_line(&st, &root, to)?.unwrap_or(len)
            };
            edits.push(Edit::new(start, end, replacement));
        }
        self.commit_edits(&path, &edits, Some(base_v), author, self.message.as_deref().or(Some("replace-lines")))
    }

    /// Append one row to the change feed. Callers run it inside the operation's own
    /// transaction, so a watcher can never see a change the store does not have, nor miss
    /// one it does.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_change(
        &self,
        op: &str,
        node_id: i64,
        node_kind: i64,
        path: &str,
        old_path: Option<&str>,
        version: Option<i64>,
        base_version: Option<i64>,
        commit_kind: Option<&str>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<i64> {
        self.conn
            .prepare_cached(&format!(
                "INSERT INTO {}change(ts, op, node_id, node_kind, path, old_path, version, base_version, commit_kind, author, message) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                self.p
            ))
            .map_err(sql_err)?
            .execute(params![
                Self::now(),
                op,
                node_id,
                node_kind,
                path,
                old_path,
                version,
                base_version,
                commit_kind,
                author,
                message
            ])
            .map_err(sql_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Change-feed rows with `seq > since`, oldest first, at most `limit` of them.
    pub fn feed(&self, since: i64, limit: usize) -> Result<Vec<ChangeRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT seq, ts, op, node_id, node_kind, path, old_path, version, base_version, commit_kind, author, message \
                 FROM {}change WHERE seq > ?1 ORDER BY seq LIMIT ?2",
                self.p
            ))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![since, limit.min(i64::MAX as usize) as i64], |r| {
                Ok(ChangeRow {
                    seq: r.get(0)?,
                    ts: r.get(1)?,
                    op: r.get(2)?,
                    node_id: r.get(3)?,
                    node_kind: r.get(4)?,
                    path: r.get(5)?,
                    old_path: r.get(6)?,
                    version: r.get(7)?,
                    base_version: r.get(8)?,
                    commit_kind: r.get(9)?,
                    author: r.get(10)?,
                    message: r.get(11)?,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
    }

    /// The newest sequence number in the change feed, 0 when it is empty. A watcher that
    /// starts from here reports only what happens after it attached.
    pub fn last_seq(&self) -> Result<i64> {
        self.conn
            .prepare_cached(&format!("SELECT coalesce(max(seq), 0) FROM {}change", self.p))
            .map_err(sql_err)?
            .query_row([], |r| r.get(0))
            .map_err(sql_err)
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
        // The index is over chunks, so every hit has to be resolved to the files that
        // contain it. Doing that one chunk at a time meant up to `limit * 50` extra
        // statements per term; the join does it in one, and `ORDER BY rank` is what makes
        // the first row seen for a file its best chunk, as before.
        let mut fts_stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT r.file_id, f.chunk_id, f.rank                  FROM (SELECT rowid AS chunk_id, rank AS rank FROM {p}fts WHERE {p}fts MATCH ?1 ORDER BY rank LIMIT ?2) f                  JOIN {p}chunk_ref r ON r.chunk_id = f.chunk_id                  JOIN {p}node n ON n.id = r.file_id                  WHERE n.deleted_at IS NULL AND n.kind = 1                    AND (?3 = '/' OR substr(n.path, 1, length(?3) + 1) = ?3 || '/')                  ORDER BY f.rank",
                p = self.p
            ))
            .map_err(sql_err)?;
        for t in &terms {
            let q = fts5_term(t);
            let mut files: std::collections::HashMap<i64, (i64, f64)> = Default::default();
            let rows = fts_stmt
                .query_map(params![q, (limit.max(1) * 50) as i64, prefix], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?))
                })
                .map_err(sql_err)?;
            for row in rows {
                let (file_id, chunk_id, rank) = row.map_err(sql_err)?;
                files.entry(file_id).or_insert((chunk_id, rank));
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
            let leaf = match textdb_core::tree::find_leaf(&st, &root, &hash)? {
                Some(l) => l,
                None => {
                    // The chunk left this file; fall back to a HEAD scan for the first term.
                    let body = (*st.document(&root)?.0).clone();
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
                out.push((n.path, (*st.document(&root)?.0).clone()));
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
