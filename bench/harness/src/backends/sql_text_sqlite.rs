//! `sql-text-sqlite`: `doc(id, path, body, version)` + `doc_rev` full-copy history,
//! OCC on `version`, FTS5 external content on `body`. The embedded naive answer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{params, Connection, OptionalExtension};

use crate::backend::*;
use crate::backends::fs::slice_lines;
use crate::backends::{col_bytes, first_hit_line, query_terms};

static INSTANCE: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CONNS: RefCell<HashMap<u64, Connection>> = RefCell::new(HashMap::new());
}

pub struct SqlTextSqlite {
    id: u64,
    file: PathBuf,
    mode: Mode,
    base_wchar: AtomicU64,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS doc (
  id INTEGER PRIMARY KEY, path TEXT NOT NULL, body TEXT NOT NULL,
  version INTEGER NOT NULL DEFAULT 1, deleted INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS doc_path ON doc(path) WHERE deleted = 0;
CREATE TABLE IF NOT EXISTS doc_rev (
  doc_id INTEGER NOT NULL, version INTEGER NOT NULL, body TEXT NOT NULL,
  PRIMARY KEY (doc_id, version)
);
CREATE VIRTUAL TABLE IF NOT EXISTS doc_fts USING fts5(body, content='doc', content_rowid='id', tokenize='unicode61');
CREATE TRIGGER IF NOT EXISTS doc_ai AFTER INSERT ON doc BEGIN
  INSERT INTO doc_fts(rowid, body) VALUES (new.id, new.body);
  INSERT INTO doc_rev(doc_id, version, body) VALUES (new.id, new.version, new.body);
END;
CREATE TRIGGER IF NOT EXISTS doc_au AFTER UPDATE OF body ON doc BEGIN
  INSERT INTO doc_fts(doc_fts, rowid, body) VALUES ('delete', old.id, old.body);
  INSERT INTO doc_fts(rowid, body) VALUES (new.id, new.body);
  INSERT INTO doc_rev(doc_id, version, body) VALUES (new.id, new.version, new.body);
END;
CREATE TRIGGER IF NOT EXISTS doc_ad AFTER UPDATE OF deleted ON doc WHEN new.deleted = 1 BEGIN
  INSERT INTO doc_fts(doc_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;
"#;

impl SqlTextSqlite {
    pub fn new(dir: &Path, mode: Mode) -> anyhow::Result<Self> {
        let file = dir.join("sqltext.db");
        let b = SqlTextSqlite {
            id: INSTANCE.fetch_add(1, Ordering::Relaxed),
            file,
            mode,
            base_wchar: AtomicU64::new(io_counters::self_wchar()),
        };
        b.with(|c| {
            c.execute_batch(SCHEMA)?;
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

    fn get(&self, c: &Connection, path: &str) -> R<(i64, Vec<u8>, i64)> {
        c.query_row(
            "SELECT id, body, version FROM doc WHERE path = ?1 AND deleted = 0",
            params![path],
            |r| Ok((r.get(0)?, col_bytes(r, 1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| BackendError::NotFound(path.to_string()))
    }

    fn tsq(query: &str) -> String {
        textdb_sqlite::db::fts5_query(query)
    }
}

fn body_param(b: &[u8]) -> rusqlite::types::Value {
    match std::str::from_utf8(b) {
        Ok(s) => rusqlite::types::Value::Text(s.to_string()),
        Err(_) => rusqlite::types::Value::Blob(b.to_vec()),
    }
}

impl Backend for SqlTextSqlite {
    fn id(&self) -> &'static str {
        "sql-text-sqlite"
    }
    fn capabilities(&self) -> Caps {
        Caps {
            replace: Cap::Emulated,
            read_lines: Cap::Emulated,
            read_version: Cap::Native,
            history: Cap::Native,
            search: Cap::Native,
            rename_folder: Cap::Emulated,
            concurrency_guard: "OCC on version; single writer",
            invalid_utf8: Cap::Emulated,
        }
    }
    fn create(&self, path: &str, body: &[u8]) -> R<Version> {
        self.with(|c| {
            c.execute("INSERT INTO doc(path, body) VALUES (?1, ?2)", params![path, body_param(body)])?;
            Ok(1)
        })
    }
    fn delete(&self, path: &str) -> R<()> {
        self.with(|c| {
            c.execute(
                "UPDATE doc SET deleted = 1 WHERE deleted = 0 AND (path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')",
                params![path],
            )?;
            Ok(())
        })
    }
    fn rename(&self, from: &str, to: &str) -> R<()> {
        self.with(|c| {
            c.execute(
                "UPDATE doc SET path = ?2 || substr(path, length(?1) + 1) WHERE deleted = 0 AND (path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')",
                params![from, to],
            )?;
            Ok(())
        })
    }
    fn list(&self, prefix: &str) -> R<Vec<Entry>> {
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT path, length(body) FROM doc WHERE deleted = 0 AND (?1 = '/' OR substr(path, 1, length(?1) + 1) = ?1 || '/') ORDER BY path",
            )?;
            let rows = st
                .query_map(params![prefix], |r| {
                    Ok(Entry {
                        path: r.get(0)?,
                        is_dir: false,
                        nbytes: Some(r.get::<_, i64>(1)? as u64),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    fn read(&self, path: &str) -> R<Vec<u8>> {
        self.with(|c| Ok(self.get(c, path)?.1))
    }
    fn read_versioned(&self, path: &str) -> R<(Vec<u8>, Version)> {
        self.with(|c| {
            let (_, b, v) = self.get(c, path)?;
            Ok((b, v as u64))
        })
    }
    fn read_lines(&self, path: &str, from: u64, to: u64) -> R<Vec<u8>> {
        Ok(slice_lines(&self.read(path)?, from, to))
    }
    fn read_version(&self, path: &str, v: Version) -> R<Vec<u8>> {
        self.with(|c| {
            c.query_row(
                "SELECT r.body FROM doc_rev r JOIN doc d ON d.id = r.doc_id WHERE d.path = ?1 AND r.version = ?2 ORDER BY d.deleted LIMIT 1",
                params![path, v as i64],
                |r| col_bytes(r, 0),
            )
            .optional()?
            .ok_or_else(|| BackendError::NotFound(format!("{} v{}", path, v)))
        })
    }
    fn overwrite(&self, path: &str, body: &[u8]) -> R<Version> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE doc SET body = ?1, version = version + 1 WHERE path = ?2 AND deleted = 0",
                params![body_param(body), path],
            )?;
            if n == 0 {
                return Err(BackendError::NotFound(path.to_string()));
            }
            Ok(c.query_row("SELECT version FROM doc WHERE path = ?1 AND deleted = 0", params![path], |r| r.get::<_, i64>(0))? as u64)
        })
    }
    fn replace(&self, path: &str, old: &[u8], new: &[u8], base: Option<Version>) -> R<WriteOutcome> {
        self.with(|c| {
            let n = match base {
                Some(v) => c.execute(
                    "UPDATE doc SET body = replace(body, ?1, ?2), version = version + 1 WHERE path = ?3 AND version = ?4 AND deleted = 0 AND instr(body, ?1) > 0",
                    params![body_param(old), body_param(new), path, v as i64],
                )?,
                None => c.execute(
                    "UPDATE doc SET body = replace(body, ?1, ?2), version = version + 1 WHERE path = ?3 AND deleted = 0 AND instr(body, ?1) > 0",
                    params![body_param(old), body_param(new), path],
                )?,
            };
            if n == 1 {
                let v: i64 = c.query_row("SELECT version FROM doc WHERE path = ?1 AND deleted = 0", params![path], |r| r.get(0))?;
                return Ok(WriteOutcome::Committed {
                    version: v as u64,
                    direct: base.map_or(true, |b| b + 1 == v as u64),
                });
            }
            let (_, body, _) = self.get(c, path)?;
            Ok(WriteOutcome::Conflict { current_region: body })
        })
    }
    fn append(&self, path: &str, tail: &[u8]) -> R<Version> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE doc SET body = body || ?1, version = version + 1 WHERE path = ?2 AND deleted = 0",
                params![body_param(tail), path],
            )?;
            if n == 0 {
                return Err(BackendError::NotFound(path.to_string()));
            }
            Ok(c.query_row("SELECT version FROM doc WHERE path = ?1 AND deleted = 0", params![path], |r| r.get::<_, i64>(0))? as u64)
        })
    }
    fn search(&self, query: &str, prefix: &str) -> R<Vec<Hit>> {
        let terms = query_terms(query);
        let q = Self::tsq(query);
        if q.is_empty() {
            return Ok(vec![]);
        }
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT d.path, d.body FROM doc_fts f JOIN doc d ON d.id = f.rowid WHERE doc_fts MATCH ?1 AND d.deleted = 0 AND (?2 = '/' OR substr(d.path, 1, length(?2) + 1) = ?2 || '/')",
            )?;
            let rows = st
                .query_map(params![q, prefix], |r| Ok((r.get::<_, String>(0)?, col_bytes(r, 1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .map(|(p, b)| Hit {
                    line: first_hit_line(&b, &terms),
                    path: p,
                })
                .collect())
        })
    }
    fn history(&self, path: &str) -> R<Vec<Version>> {
        self.with(|c| {
            let mut st = c.prepare_cached(
                "SELECT r.version FROM doc_rev r JOIN doc d ON d.id = r.doc_id WHERE d.path = ?1 ORDER BY d.deleted, r.version",
            )?;
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
            c.execute_batch("INSERT INTO doc_fts(doc_fts) VALUES('optimize'); VACUUM;")?;
            Ok("FTS5 optimize + VACUUM")
        })
    }
}
