//! `textdb-sqlite`: the algorithm under test, embedded, driven through its SQL surface
//! (virtual table, table-valued functions and scalar functions) — not the Rust API.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{params, Connection, OptionalExtension};

use crate::backend::*;
use crate::backends::col_bytes;
use crate::reference::splice;

static INSTANCE: AtomicU64 = AtomicU64::new(1);
static WRITE_TAG: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CONNS: RefCell<HashMap<u64, Connection>> = RefCell::new(HashMap::new());
}

pub struct TextdbSqlite {
    id: u64,
    file: PathBuf,
    mode: Mode,
    base_wchar: AtomicU64,
}

impl TextdbSqlite {
    pub fn new(dir: &Path, mode: Mode) -> anyhow::Result<Self> {
        let file = dir.join("textdb.db");
        let b = TextdbSqlite {
            id: INSTANCE.fetch_add(1, Ordering::Relaxed),
            file,
            mode,
            base_wchar: AtomicU64::new(io_counters::self_wchar()),
        };
        b.with(|c| {
            c.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='kb_');")?;
            Ok(())
        })?;
        Ok(b)
    }

    fn open(&self) -> rusqlite::Result<Connection> {
        let c = Connection::open(&self.file)?;
        let sync = if self.mode == Mode::Durable { "FULL" } else { "NORMAL" };
        c.execute_batch(&format!(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = {}; PRAGMA busy_timeout = 60000; PRAGMA cache_size = -262144; PRAGMA mmap_size = 1073741824;",
            sync
        ))?;
        textdb_sqlite::register(&c, "kb_")?;
        Ok(c)
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> R<T>) -> R<T> {
        CONNS.with(|m| {
            let mut m = m.borrow_mut();
            if !m.contains_key(&self.id) {
                m.insert(self.id, self.open()?);
            }
            f(m.get(&self.id).unwrap())
        })
    }

    fn map_write_err(e: rusqlite::Error) -> R<WriteOutcome> {
        let msg = e.to_string();
        if let Some(idx) = msg.find("TX001") {
            let json = &msg[idx..];
            let json = &json[json.find('{').unwrap_or(0)..];
            let theirs = serde_json::from_str::<serde_json::Value>(json)
                .ok()
                .and_then(|v| v.get("theirs").and_then(|t| t.as_str()).map(|s| s.as_bytes().to_vec()))
                .unwrap_or_default();
            return Ok(WriteOutcome::Conflict { current_region: theirs });
        }
        if msg.contains("TX002") {
            return Ok(WriteOutcome::Contention);
        }
        Err(BackendError::Other(msg))
    }
}

fn content_param(b: &[u8]) -> rusqlite::types::Value {
    match std::str::from_utf8(b) {
        Ok(s) => rusqlite::types::Value::Text(s.to_string()),
        Err(_) => rusqlite::types::Value::Blob(b.to_vec()),
    }
}

/// Close and forget this instance's connection on the calling thread. Without this the
/// thread-local cache keeps the `Connection` — and its file handle — alive for the whole
/// process, so the next rep's `remove_dir_all` cannot delete the database on Windows.
fn close_conn(id: u64) {
    let _ = CONNS.try_with(|m| {
        drop(m.borrow_mut().remove(&id));
    });
}

impl Drop for TextdbSqlite {
    fn drop(&mut self) {
        close_conn(self.id);
    }
}

impl Backend for TextdbSqlite {
    fn thread_done(&self) {
        close_conn(self.id);
    }

    fn id(&self) -> &'static str {
        "textdb-sqlite"
    }
    fn capabilities(&self) -> Caps {
        Caps {
            replace: Cap::Native,
            read_lines: Cap::Native,
            read_version: Cap::Native,
            history: Cap::Native,
            search: Cap::Native,
            rename_folder: Cap::Native,
            concurrency_guard: "CAS + rebase; single writer",
            invalid_utf8: Cap::Native,
        }
    }
    fn create(&self, path: &str, body: &[u8]) -> R<Version> {
        self.with(|c| {
            c.execute(
                "INSERT INTO kb(path, content, author) VALUES (?1, ?2, 'bench')",
                params![path, content_param(body)],
            )?;
            Ok(1)
        })
    }
    fn delete(&self, path: &str) -> R<()> {
        self.with(|c| {
            let n = c.execute("DELETE FROM kb WHERE path = ?1", params![path])?;
            if n == 0 {
                return Err(BackendError::NotFound(path.to_string()));
            }
            Ok(())
        })
    }
    fn rename(&self, from: &str, to: &str) -> R<()> {
        self.with(|c| {
            let n = c.execute("UPDATE kb SET path = ?2 WHERE path = ?1", params![from, to])?;
            if n == 0 {
                return Err(BackendError::NotFound(from.to_string()));
            }
            Ok(())
        })
    }
    fn list(&self, prefix: &str) -> R<Vec<Entry>> {
        self.with(|c| {
            // A bound on `path` is pushed into the shadow table's index by the virtual
            // table; the root has no bounds and gets the plain scan.
            let bounds = crate::backends::subtree_bounds(prefix);
            let sql = match bounds {
                Some(_) => "SELECT path, kind, nbytes FROM kb WHERE path >= ?1 AND path < ?2 ORDER BY path",
                None => "SELECT path, kind, nbytes FROM kb ORDER BY path",
            };
            let args: Vec<String> = bounds.map(|(lo, hi)| vec![lo, hi]).unwrap_or_default();
            let mut st = c.prepare_cached(sql)?;
            let rows = st
                .query_map(rusqlite::params_from_iter(args), |r| {
                    Ok(Entry {
                        path: r.get(0)?,
                        is_dir: r.get::<_, String>(1)? == "folder",
                        nbytes: r.get::<_, Option<i64>>(2)?.map(|x| x as u64),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn read(&self, path: &str) -> R<Vec<u8>> {
        self.with(|c| {
            c.query_row("SELECT content FROM kb WHERE path = ?1", params![path], |r| col_bytes(r, 0))
                .optional()?
                .ok_or_else(|| BackendError::NotFound(path.to_string()))
        })
    }
    fn read_versioned(&self, path: &str) -> R<(Vec<u8>, Version)> {
        self.with(|c| {
            c.query_row("SELECT content, version FROM kb WHERE path = ?1", params![path], |r| {
                Ok((col_bytes(r, 0)?, r.get::<_, i64>(1)? as u64))
            })
            .optional()?
            .ok_or_else(|| BackendError::NotFound(path.to_string()))
        })
    }
    fn read_lines(&self, path: &str, from: u64, to: u64) -> R<Vec<u8>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT textdb_lines(?1, ?2, ?3)",
                params![path, from as i64, to as i64],
                |r| col_bytes(r, 0),
            )?)
        })
    }
    fn read_version(&self, path: &str, v: Version) -> R<Vec<u8>> {
        self.with(|c| {
            c.query_row("SELECT textdb_content(?1, ?2)", params![path, v as i64], |r| col_bytes(r, 0))
                .map_err(|e| BackendError::Other(e.to_string()))
        })
    }
    fn overwrite(&self, path: &str, body: &[u8]) -> R<Version> {
        self.with(|c| {
            let n = c.execute("UPDATE kb SET content = ?1 WHERE path = ?2", params![content_param(body), path])?;
            if n == 0 {
                return Err(BackendError::NotFound(path.to_string()));
            }
            Ok(c.query_row("SELECT version FROM kb WHERE path = ?1", params![path], |r| r.get::<_, i64>(0))? as u64)
        })
    }
    fn replace(&self, path: &str, old: &[u8], new: &[u8], base: Option<Version>) -> R<WriteOutcome> {
        self.with(|c| match base {
            // Client holds a stale (content, version): rewrite the body as it saw it and let
            // the trigger diff + rebase (spec §7.2 `UPDATE kb.file SET content = …`).
            Some(v) => {
                let seen: Vec<u8> = c
                    .query_row("SELECT textdb_content(?1, ?2)", params![path, v as i64], |r| col_bytes(r, 0))
                    .map_err(|e| BackendError::Other(e.to_string()))?;
                let next = match splice(&seen, old, new) {
                    Some(n) => n,
                    None => return Ok(WriteOutcome::Conflict { current_region: seen }),
                };
                // A unique author tag lets the outcome be classified through the SQL surface:
                // a commit row carrying the tag exists iff this write created a version.
                let tag = format!("bench-{}", WRITE_TAG.fetch_add(1, Ordering::Relaxed));
                match c.execute(
                    "UPDATE kb SET content = ?1, base_version = ?2, author = ?4 WHERE path = ?3",
                    params![content_param(&next), v as i64, path, tag],
                ) {
                    Ok(_) => {
                        // The path may be mid-rename (CW-06): retry the lookup briefly.
                        let mut mine: Option<i64> = None;
                        for attempt in 0..10 {
                            match c.query_row(
                                "SELECT max(version) FROM textdb_history(?1) WHERE author = ?2",
                                params![path, tag],
                                |r| r.get::<_, Option<i64>>(0),
                            ) {
                                Ok(v) => {
                                    mine = v;
                                    break;
                                }
                                Err(e) if attempt < 9 && e.to_string().contains("TX003") => {
                                    std::thread::sleep(std::time::Duration::from_millis(20));
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                        match mine {
                            Some(nv) => Ok(WriteOutcome::Committed {
                                version: nv as u64,
                                direct: nv as u64 == v + 1,
                            }),
                            None => {
                                let nv: i64 = c.query_row("SELECT version FROM kb WHERE path = ?1", params![path], |r| r.get(0))?;
                                Ok(WriteOutcome::Absorbed { version: nv as u64 })
                            }
                        }
                    }
                    Err(e) => Self::map_write_err(e),
                }
            }
            None => match c.query_row(
                "SELECT textdb_edit(?1, ?2, ?3, 'bench')",
                params![path, content_param(old), content_param(new)],
                |r| r.get::<_, i64>(0),
            ) {
                Ok(v) => Ok(WriteOutcome::Committed {
                    version: v as u64,
                    direct: true,
                }),
                Err(e) => {
                    if e.to_string().contains("TX004") {
                        // On the connection already borrowed here, not through `self.read`:
                        // that re-enters `with`, and the thread-local `RefCell` is held for
                        // the whole closure, so the recovery path panicked instead of
                        // reporting the conflict it exists to report.
                        let cur: Vec<u8> = c
                            .query_row("SELECT content FROM kb WHERE path = ?1", params![path], |r| col_bytes(r, 0))
                            .optional()?
                            .unwrap_or_default();
                        return Ok(WriteOutcome::Conflict { current_region: cur });
                    }
                    Self::map_write_err(e)
                }
            },
        })
    }
    fn append(&self, path: &str, tail: &[u8]) -> R<Version> {
        self.with(|c| {
            Ok(c.query_row("SELECT textdb_append(?1, ?2, 'bench')", params![path, content_param(tail)], |r| {
                r.get::<_, i64>(0)
            })? as u64)
        })
    }
    fn search(&self, query: &str, prefix: &str) -> R<Vec<Hit>> {
        self.with(|c| {
            let mut st = c.prepare_cached("SELECT path, line, text FROM textdb_search(?1, ?2, 100000, 1000000)")?;
            let rows = st
                .query_map(params![query, prefix], |r| {
                    Ok(Hit {
                        path: r.get(0)?,
                        line: r.get::<_, i64>(1)? as u64,
                        snippet: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn history(&self, path: &str) -> R<Vec<Version>> {
        self.with(|c| {
            let mut st = c.prepare_cached("SELECT version FROM textdb_history(?1)")?;
            let v = st.query_map(params![path], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v.into_iter().map(|x| x as u64).collect())
        })
    }
    // structure sidecar -------------------------------------------------------------
    //
    // Read through the sidecar tables the write path maintains (`kb_link`, `kb_section`,
    // `kb_frontmatter`), joined to `kb_node` for paths, which is what the CLI's `links`,
    // `sections` and `frontmatter` views do. Rows are kept for HEAD only (ADR 0007), so
    // every query is at the document's current version.

    fn links(&self, prefix: &str) -> R<Vec<LinkRow>> {
        self.with(|c| {
            let (lo, hi) = subtree_range(prefix);
            let mut st = c.prepare_cached(
                "SELECT n.path, l.target_path, l.line, l.status, r.path
                   FROM kb_link l
                   JOIN kb_node n ON n.id = l.file_id AND n.deleted_at IS NULL
                   LEFT JOIN kb_node r ON r.id = l.resolved_id AND r.deleted_at IS NULL
                  WHERE n.path = ?3 OR (n.path >= ?1 AND n.path < ?2)
                  ORDER BY n.path, l.line",
            )?;
            let rows = st.query_map(params![lo, hi, prefix], link_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn backlinks(&self, path: &str) -> R<Vec<LinkRow>> {
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT n.path, l.target_path, l.line, l.status, r.path
                   FROM kb_link l
                   JOIN kb_node r ON r.id = l.resolved_id AND r.deleted_at IS NULL AND r.path = ?1
                   JOIN kb_node n ON n.id = l.file_id AND n.deleted_at IS NULL
                  ORDER BY n.path, l.line",
            )?;
            let rows = st.query_map(params![path], link_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn frontmatter(&self, path: &str) -> R<Option<String>> {
        self.with(|c| {
            let v: Option<Option<String>> = c
                .prepare_cached(
                    "SELECT f.data FROM kb_frontmatter f
                       JOIN kb_node n ON n.id = f.file_id AND n.deleted_at IS NULL AND n.path = ?1",
                )?
                .query_row(params![path], |r| r.get(0))
                .optional()?;
            Ok(v.flatten())
        })
    }
    fn set_meta(&self, path: &str, key: &str, value: &str) -> R<Version> {
        // The store has no "set one key" primitive: front matter is part of the document,
        // so this reads, edits the YAML block and writes back. That is what the CLI's
        // `meta set` does, and the cost the suite reports is that whole round trip.
        let (body, _) = self.read_versioned(path)?;
        let next = set_frontmatter_key(&body, key, value);
        self.overwrite(path, &next)
    }
    fn outline(&self, prefix: &str, heading: Option<&str>, mode: &str, max_level: Option<u32>) -> R<Vec<OutlineRow>> {
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT path, heading, level, line_from, nwords, nwords_total, nbytes
                   FROM textdb_outline(?1, ?2, ?3, ?4, 1000000)",
            )?;
            let rows = st
                .query_map(params![prefix, heading, mode, max_level.map(|l| l as i64)], |r| {
                    Ok(OutlineRow {
                        path: r.get(0)?,
                        heading: r.get(1)?,
                        level: r.get::<_, i64>(2)? as u32,
                        line_from: r.get::<_, i64>(3)? as u64,
                        nwords: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                        nwords_total: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                        file_nbytes: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn heading_names(&self, prefix: &str, starts: &str) -> R<Vec<(String, u64, u64)>> {
        self.with(|c| {
            let mut st = c.prepare_cached("SELECT heading, sections, docs FROM textdb_headings(?1, ?2, 100000)")?;
            let rows = st
                .query_map(params![prefix, starts], |r| {
                    Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn sections(&self, path: &str) -> R<Vec<SectionRow>> {
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT s.heading_path, s.level, s.line_from, s.line_to
                   FROM kb_section s
                   JOIN kb_node n ON n.id = s.file_id AND n.deleted_at IS NULL AND n.path = ?1
                  ORDER BY s.line_from",
            )?;
            let rows = st
                .query_map(params![path], |r| {
                    Ok(SectionRow {
                        heading: r.get(0)?,
                        level: r.get::<_, i64>(1)? as u64,
                        line_from: r.get::<_, i64>(2)? as u64,
                        line_to: r.get::<_, i64>(3)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn section(&self, path: &str, heading: &str) -> R<Option<Vec<u8>>> {
        self.with(|c| {
            let v: Option<Vec<u8>> = c
                .prepare_cached("SELECT textdb_section(?1, ?2)")?
                .query_row(params![path, heading], |r| col_bytes(r, 0))
                .optional()?;
            // `textdb_section` returns NULL for a heading the document does not have, which
            // `col_bytes` flattens to empty: an empty answer is "no such section" here.
            Ok(v.filter(|b| !b.is_empty()))
        })
    }
    fn set_link_mode(&self, mode: &str) -> R<()> {
        self.with(|c| {
            c.prepare_cached("SELECT textdb_setting('link_updates', ?1)")?
                .query_row(params![mode], |_| Ok(()))?;
            Ok(())
        })
    }
    fn property_keys(&self, prefix: &str) -> R<Vec<(String, u64)>> {
        self.with(|c| {
            let mut st = c.prepare_cached("SELECT key, docs FROM textdb_prop_keys(?1, 10000)")?;
            let rows = st
                .query_map(params![prefix], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn property_values(&self, key: &str, prefix: &str) -> R<Vec<(String, u64)>> {
        self.with(|c| {
            let mut st = c.prepare_cached("SELECT value, docs FROM textdb_prop_values(?1, ?2, 10000)")?;
            let rows = st
                .query_map(params![key, prefix], |r| {
                    Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, i64>(1)? as u64))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn property_find(&self, query: &str) -> R<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare_cached("SELECT path FROM textdb_prop_find(?1, '/', 1000000)")?;
            let rows = st.query_map(params![query], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn sync_dir(&self, prefix: &str, dir: &std::path::Path) -> R<crate::backend::SyncStats> {
        super::run_sync(&self.file.display().to_string(), prefix, dir)
    }
    fn changes_since(&self, seq: u64) -> R<(u64, u64)> {
        self.with(|c| {
            let mut st = c.prepare_cached("SELECT seq FROM textdb_feed(?1)")?;
            let mut last = seq;
            let mut n = 0u64;
            for row in st.query_map(params![seq as i64], |r| r.get::<_, i64>(0))? {
                last = last.max(row? as u64);
                n += 1;
            }
            Ok((last, n))
        })
    }
    fn storage_bytes(&self) -> R<u64> {
        let mut total = 0;
        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{}", self.file.display(), suffix));
            total += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        }
        Ok(total)
    }
    fn bytes_written_since_reset(&self) -> R<u64> {
        Ok(io_counters::self_wchar().saturating_sub(self.base_wchar.load(Ordering::Relaxed)))
    }
    fn reset_counters(&self) -> R<()> {
        self.base_wchar.store(io_counters::self_wchar(), Ordering::Relaxed);
        Ok(())
    }
    fn maintenance(&self) -> R<&'static str> {
        self.with(|c| {
            c.execute_batch("INSERT INTO kb_fts(kb_fts) VALUES('optimize'); VACUUM;")?;
            Ok("FTS5 optimize + VACUUM (GC stub: none)")
        })
    }
    fn extra_stats(&self, path: &str) -> R<Vec<(&'static str, f64)>> {
        self.with(|c| {
            let chunks: i64 = c.query_row("SELECT count(*) FROM kb_chunk", [], |r| r.get(0))?;
            let chunk_bytes: i64 = c.query_row("SELECT coalesce(sum(length(bytes)),0) FROM kb_chunk", [], |r| r.get(0))?;
            let nodes: i64 = c.query_row("SELECT count(*) FROM kb_tree_node", [], |r| r.get(0))?;
            let mut v = vec![
                ("chunks", chunks as f64),
                ("chunk_bytes", chunk_bytes as f64),
                ("tree_nodes", nodes as f64),
            ];
            if !path.is_empty() {
                let db = textdb_sqlite::TextDb::attach(c, "kb_", true);
                if let Ok(Some(n)) = db.node_by_path(path) {
                    if let Some(root) = n.root {
                        let st = textdb_sqlite::SqliteStorage::new(c, "kb_");
                        if let Ok(d) = textdb_core::tree::depth(&st, &root) {
                            v.push(("tree_depth", d as f64));
                        }
                        if let Ok(l) = textdb_core::leaves(&st, &root) {
                            v.push(("leaves", l.len() as f64));
                        }
                    }
                }
            }
            Ok(v)
        })
    }
}

/// Leaf hash set of a file at HEAD (for LL-04 / ME-04 leaf-stability metrics).
pub fn leaf_hashes(b: &TextdbSqlite, path: &str) -> R<Vec<textdb_core::Hash>> {
    b.with(|c| {
        let db = textdb_sqlite::TextDb::attach(c, "kb_", true);
        let n = db
            .node_by_path(path)
            .map_err(|e| BackendError::Other(e.to_string()))?
            .ok_or_else(|| BackendError::NotFound(path.to_string()))?;
        let root = n.root.ok_or_else(|| BackendError::NotFound(path.to_string()))?;
        let st = textdb_sqlite::SqliteStorage::new(c, "kb_");
        Ok(textdb_core::leaves(&st, &root)
            .map_err(|e| BackendError::Other(e.to_string()))?
            .into_iter()
            .map(|l| l.hash)
            .collect())
    })
}

/// `[lo, hi)` over `kb_node.path` covering a subtree, as an index range rather than a
/// `substr` predicate (the same trick the vtab uses: "0" is the byte after "/").
///
/// The range covers what is *below* the path, so callers pair it with an equality on the
/// path itself: `links("/a/b.md")` means that document, `links("/a")` means the folder.
fn subtree_range(prefix: &str) -> (String, String) {
    if prefix == "/" || prefix.is_empty() {
        ("/".to_string(), "0".to_string())
    } else {
        let p = prefix.trim_end_matches('/');
        (format!("{}/", p), format!("{}0", p))
    }
}

fn link_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LinkRow> {
    Ok(LinkRow {
        path: r.get(0)?,
        target: r.get(1)?,
        line: r.get::<_, i64>(2)? as u64,
        status: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        resolved: r.get(4)?,
    })
}

/// Set one top-level key in a document's YAML front matter, leaving every other line
/// exactly as it was, and creating the block when the document has none.
///
/// Deliberately the same shape as the CLI's `meta set`: a line-wise edit of the block, not
/// a YAML round trip, because re-serialising would rewrite lines the user did not touch and
/// the suite's oracle checks that the body comes back byte for byte.
pub fn set_frontmatter_key(body: &[u8], key: &str, value: &str) -> Vec<u8> {
    let line = format!("{}: {}\n", key, value);
    let Some((fm, end)) = textdb_md::split_frontmatter(body) else {
        let mut out = format!("---\n{}---\n", line).into_bytes();
        out.extend_from_slice(body);
        return out;
    };
    let mut out = Vec::with_capacity(body.len() + line.len());
    out.extend_from_slice(b"---\n");
    let mut replaced = false;
    for l in fm.split(|&b| b == b'\n') {
        if l.is_empty() {
            continue;
        }
        let is_key = l
            .iter()
            .position(|&b| b == b':')
            .is_some_and(|i| std::str::from_utf8(&l[..i]).is_ok_and(|k| k.trim() == key));
        if is_key {
            if !replaced {
                out.extend_from_slice(line.as_bytes());
                replaced = true;
            }
        } else {
            out.extend_from_slice(l);
            out.push(b'\n');
        }
    }
    if !replaced {
        out.extend_from_slice(line.as_bytes());
    }
    out.extend_from_slice(b"---\n");
    out.extend_from_slice(&body[end..]);
    out
}
