//! `textdb-pg`: the algorithm under test on Postgres, driven through the extension's
//! view/trigger/function surface (spec §7.2) — never direct table access.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use postgres::{Client, NoTls};

use crate::backend::*;
use crate::backends::sql_text_pg::pg_written_bytes;
use crate::reference::splice;

static INSTANCE: AtomicU64 = AtomicU64::new(1);
static WRITE_TAG: AtomicU64 = AtomicU64::new(1);

thread_local! {
    // `ManuallyDrop`: a `postgres::Client` must not be dropped during thread-local teardown
    // (its tokio runtime is already gone); connections are closed via `thread_done`/`Drop`.
    static CONNS: RefCell<HashMap<u64, std::mem::ManuallyDrop<Client>>> = RefCell::new(HashMap::new());
}

fn close_conn(id: u64) {
    let _ = CONNS.try_with(|m| {
        if let Some(mut c) = m.borrow_mut().remove(&id) {
            unsafe { std::mem::ManuallyDrop::drop(&mut c) };
        }
    });
}

pub struct TextdbPg {
    id: u64,
    url: String,
    mode: Mode,
    base_written: Mutex<u64>,
}

impl TextdbPg {
    pub fn new(url: &str, mode: Mode) -> anyhow::Result<Self> {
        let b = TextdbPg {
            id: INSTANCE.fetch_add(1, Ordering::Relaxed),
            url: url.to_string(),
            mode,
            base_written: Mutex::new(0),
        };
        b.with(|c| {
            c.batch_execute("DROP EXTENSION IF EXISTS textdb_pg CASCADE; DROP SCHEMA IF EXISTS kb CASCADE; CREATE EXTENSION textdb_pg;")?;
            Ok(())
        })?;
        Ok(b)
    }

    fn open(&self) -> Result<Client, postgres::Error> {
        let mut c = Client::connect(&self.url, NoTls)?;
        let sc = if self.mode == Mode::Durable { "on" } else { "off" };
        c.batch_execute(&format!("SET synchronous_commit = {};", sc))?;
        Ok(c)
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut Client) -> R<T>) -> R<T> {
        CONNS.with(|m| {
            let mut m = m.borrow_mut();
            if !m.contains_key(&self.id) {
                m.insert(self.id, std::mem::ManuallyDrop::new(self.open()?));
            }
            f(m.get_mut(&self.id).unwrap())
        })
    }

    fn text(b: &[u8]) -> R<&str> {
        std::str::from_utf8(b).map_err(|_| BackendError::NotSupported("kb.file.content is text; use bytea API for binary"))
    }

    fn map_write_err(e: postgres::Error) -> R<WriteOutcome> {
        if let Some(db) = e.as_db_error() {
            match db.code().code() {
                "TX001" => {
                    let theirs = db
                        .detail()
                        .and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok())
                        .and_then(|v| v.get("theirs").and_then(|t| t.as_str()).map(|s| s.as_bytes().to_vec()))
                        .unwrap_or_default();
                    return Ok(WriteOutcome::Conflict { current_region: theirs });
                }
                "TX002" => return Ok(WriteOutcome::Contention),
                _ => {}
            }
        }
        Err(e.into())
    }
}

impl Drop for TextdbPg {
    fn drop(&mut self) {
        close_conn(self.id);
    }
}

impl Backend for TextdbPg {
    fn thread_done(&self) {
        close_conn(self.id);
    }
    fn id(&self) -> &'static str {
        "textdb-pg"
    }
    fn capabilities(&self) -> Caps {
        Caps {
            replace: Cap::Native,
            read_lines: Cap::Native,
            read_version: Cap::Native,
            history: Cap::Native,
            search: Cap::Native,
            rename_folder: Cap::Native,
            concurrency_guard: "MVCC + CAS + rebase",
            invalid_utf8: Cap::NA,
        }
    }
    fn create(&self, path: &str, body: &[u8]) -> R<Version> {
        let body = Self::text(body)?;
        self.with(|c| {
            c.execute("INSERT INTO kb.file(path, content, updated_by) VALUES ($1, $2, 'bench')", &[&path, &body])?;
            Ok(1)
        })
    }
    fn delete(&self, path: &str) -> R<()> {
        self.with(|c| {
            let n = c.execute("DELETE FROM kb.file WHERE path = $1", &[&path])?;
            if n == 0 {
                let n2 = c.execute("DELETE FROM kb.folder WHERE path = $1", &[&path])?;
                if n2 == 0 {
                    return Err(BackendError::NotFound(path.to_string()));
                }
            }
            Ok(())
        })
    }
    fn rename(&self, from: &str, to: &str) -> R<()> {
        self.with(|c| {
            let n = c.execute("UPDATE kb.file SET path = $2 WHERE path = $1", &[&from, &to])?;
            if n == 0 {
                let n2 = c.execute("UPDATE kb.folder SET path = $2 WHERE path = $1", &[&from, &to])?;
                if n2 == 0 {
                    return Err(BackendError::NotFound(from.to_string()));
                }
            }
            Ok(())
        })
    }
    fn list(&self, prefix: &str) -> R<Vec<Entry>> {
        self.with(|c| {
            let rows = c.query(
                "SELECT path, nbytes FROM kb.file WHERE $1 = '/' OR path LIKE $1 || '/%' ORDER BY path",
                &[&prefix],
            )?;
            Ok(rows
                .iter()
                .map(|r| Entry {
                    path: r.get(0),
                    is_dir: false,
                    nbytes: r.get::<_, Option<i64>>(1).map(|x| x as u64),
                })
                .collect())
        })
    }
    fn read(&self, path: &str) -> R<Vec<u8>> {
        Ok(self.read_versioned(path)?.0)
    }
    fn read_versioned(&self, path: &str) -> R<(Vec<u8>, Version)> {
        self.with(|c| {
            let row = c
                .query_opt("SELECT content, version FROM kb.file WHERE path = $1", &[&path])?
                .ok_or_else(|| BackendError::NotFound(path.to_string()))?;
            Ok((row.get::<_, String>(0).into_bytes(), row.get::<_, i64>(1) as u64))
        })
    }
    fn read_lines(&self, path: &str, from: u64, to: u64) -> R<Vec<u8>> {
        self.with(|c| {
            let row = c.query_one("SELECT kb.lines($1, $2, $3)", &[&path, &(from as i64), &(to as i64)])?;
            Ok(row.get::<_, String>(0).into_bytes())
        })
    }
    fn read_version(&self, path: &str, v: Version) -> R<Vec<u8>> {
        self.with(|c| {
            let row = c.query_one("SELECT kb.content($1, $2)", &[&path, &(v as i64)])?;
            Ok(row.get::<_, String>(0).into_bytes())
        })
    }
    fn overwrite(&self, path: &str, body: &[u8]) -> R<Version> {
        let body = Self::text(body)?;
        self.with(|c| {
            let n = c.execute("UPDATE kb.file SET content = $1, updated_by = 'bench' WHERE path = $2", &[&body, &path])?;
            if n == 0 {
                return Err(BackendError::NotFound(path.to_string()));
            }
            let row = c.query_one("SELECT version FROM kb.file WHERE path = $1", &[&path])?;
            Ok(row.get::<_, i64>(0) as u64)
        })
    }
    fn replace(&self, path: &str, old: &[u8], new: &[u8], base: Option<Version>) -> R<WriteOutcome> {
        let old = Self::text(old)?;
        let new = Self::text(new)?;
        self.with(|c| match base {
            Some(v) => {
                let seen = c.query_one("SELECT kb.content($1, $2)", &[&path, &(v as i64)])?.get::<_, String>(0);
                let next = match splice(seen.as_bytes(), old.as_bytes(), new.as_bytes()) {
                    Some(n) => String::from_utf8(n).unwrap(),
                    None => return Ok(WriteOutcome::Conflict { current_region: seen.into_bytes() }),
                };
                // Unique author tag: a kb.file_version row with it exists iff a version was created.
                let tag = format!("bench-{}", WRITE_TAG.fetch_add(1, Ordering::Relaxed));
                match c.execute(
                    "UPDATE kb.file SET content = $1, base_version = $2, updated_by = $4 WHERE path = $3",
                    &[&next, &(v as i64), &path, &tag],
                ) {
                    Ok(_) => {
                        let mine = c
                            .query_one("SELECT max(version) FROM kb.history($1) WHERE author = $2", &[&path, &tag])?
                            .get::<_, Option<i64>>(0);
                        match mine {
                            Some(nv) => Ok(WriteOutcome::Committed {
                                version: nv as u64,
                                direct: nv as u64 == v + 1,
                            }),
                            None => {
                                let nv = c.query_one("SELECT version FROM kb.file WHERE path = $1", &[&path])?.get::<_, i64>(0) as u64;
                                Ok(WriteOutcome::Absorbed { version: nv })
                            }
                        }
                    }
                    Err(e) => Self::map_write_err(e),
                }
            }
            None => match c.query_one("SELECT kb.edit($1, $2, $3, 'bench')", &[&path, &old, &new]) {
                Ok(row) => Ok(WriteOutcome::Committed {
                    version: row.get::<_, i64>(0) as u64,
                    direct: true,
                }),
                Err(e) => {
                    if e.as_db_error().map(|d| d.code().code()) == Some("TX004") {
                        let cur = self.read(path)?;
                        return Ok(WriteOutcome::Conflict { current_region: cur });
                    }
                    Self::map_write_err(e)
                }
            },
        })
    }
    fn append(&self, path: &str, tail: &[u8]) -> R<Version> {
        let tail = Self::text(tail)?;
        self.with(|c| {
            let row = c.query_one("SELECT kb.append($1, $2, 'bench')", &[&path, &tail])?;
            Ok(row.get::<_, i64>(0) as u64)
        })
    }
    fn search(&self, query: &str, prefix: &str) -> R<Vec<Hit>> {
        self.with(|c| {
            let rows = c.query("SELECT path, line FROM kb.search($1, $2)", &[&query, &prefix])?;
            Ok(rows
                .iter()
                .map(|r| Hit {
                    path: r.get(0),
                    line: r.get::<_, i64>(1) as u64,
                })
                .collect())
        })
    }
    fn history(&self, path: &str) -> R<Vec<Version>> {
        self.with(|c| {
            let rows = c.query("SELECT version FROM kb.history($1)", &[&path])?;
            Ok(rows.iter().map(|r| r.get::<_, i64>(0) as u64).collect())
        })
    }
    fn storage_bytes(&self) -> R<u64> {
        self.with(|c| {
            let row = c.query_one(
                "SELECT coalesce(sum(pg_total_relation_size(format('%I.%I', schemaname, tablename)::regclass)), 0)::bigint FROM pg_tables WHERE schemaname = 'kb'",
                &[],
            )?;
            Ok(row.get::<_, i64>(0) as u64)
        })
    }
    fn bytes_written_since_reset(&self) -> R<u64> {
        let base = *self.base_written.lock().unwrap();
        self.with(|c| Ok(pg_written_bytes(c)?.saturating_sub(base)))
    }
    fn reset_counters(&self) -> R<()> {
        let now = self.with(|c| pg_written_bytes(c))?;
        *self.base_written.lock().unwrap() = now;
        Ok(())
    }
    fn maintenance(&self) -> R<&'static str> {
        self.with(|c| {
            c.batch_execute("VACUUM FULL kb.chunk; VACUUM FULL kb.tree_node; VACUUM FULL kb.node; SELECT gin_clean_pending_list('kb.chunk_tsv');")?;
            Ok("VACUUM FULL + gin_clean_pending_list (GC stub: none)")
        })
    }
    fn extra_stats(&self, path: &str) -> R<Vec<(&'static str, f64)>> {
        self.with(|c| {
            let chunks = c.query_one("SELECT count(*) FROM kb.chunk", &[])?.get::<_, i64>(0);
            let chunk_bytes = c
                .query_one("SELECT coalesce(sum(length(bytes)), 0)::bigint FROM kb.chunk", &[])?
                .get::<_, i64>(0);
            let nodes = c.query_one("SELECT count(*) FROM kb.tree_node", &[])?.get::<_, i64>(0);
            let mut v = vec![
                ("chunks", chunks as f64),
                ("chunk_bytes", chunk_bytes as f64),
                ("tree_nodes", nodes as f64),
            ];
            if !path.is_empty() {
                if let Some(row) = c.query_opt("SELECT depth, leaves FROM kb.tree_stats($1)", &[&path])? {
                    v.push(("tree_depth", row.get::<_, i64>(0) as f64));
                    v.push(("leaves", row.get::<_, i64>(1) as f64));
                }
            }
            Ok(v)
        })
    }
}
