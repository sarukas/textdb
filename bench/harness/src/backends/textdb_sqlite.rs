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
            let mut st = c.prepare_cached(
                "SELECT path, kind, nbytes FROM kb WHERE ?1 = '/' OR substr(path, 1, length(?1) + 1) = ?1 || '/' ORDER BY path",
            )?;
            let rows = st
                .query_map(params![prefix], |r| {
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
                        let cur = self.read(path)?;
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
            let mut st = c.prepare_cached("SELECT path, line FROM textdb_search(?1, ?2, 100000)")?;
            let rows = st
                .query_map(params![query, prefix], |r| {
                    Ok(Hit {
                        path: r.get(0)?,
                        line: r.get::<_, i64>(1)? as u64,
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
