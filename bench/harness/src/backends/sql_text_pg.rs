//! `sql-text-pg`: Postgres `doc(id, path, body text, version)` + `doc_rev` full copies,
//! OCC on `version`, GIN tsvector recomputed per edit. The naive database answer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use postgres::{Client, NoTls};

use crate::backend::*;
use crate::backends::fs::slice_lines;
use crate::backends::{first_hit_line, query_terms};

static INSTANCE: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CONNS: RefCell<HashMap<u64, Client>> = RefCell::new(HashMap::new());
}

pub struct SqlTextPg {
    id: u64,
    url: String,
    mode: Mode,
    base_written: Mutex<u64>,
}

const SCHEMA: &str = r#"
DROP SCHEMA IF EXISTS sqltext CASCADE;
CREATE SCHEMA sqltext;
CREATE TABLE sqltext.doc (
  id bigserial PRIMARY KEY, path text NOT NULL, body text NOT NULL,
  version int NOT NULL DEFAULT 1, deleted boolean NOT NULL DEFAULT false,
  tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', body)) STORED
);
CREATE UNIQUE INDEX doc_path ON sqltext.doc(path text_pattern_ops) WHERE NOT deleted;
CREATE INDEX doc_tsv ON sqltext.doc USING gin(tsv);
CREATE TABLE sqltext.doc_rev (doc_id bigint NOT NULL, version int NOT NULL, body text NOT NULL, PRIMARY KEY (doc_id, version));
CREATE FUNCTION sqltext.rev() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN INSERT INTO sqltext.doc_rev(doc_id, version, body) VALUES (NEW.id, NEW.version, NEW.body); RETURN NEW; END $$;
CREATE TRIGGER doc_rev AFTER INSERT OR UPDATE OF body ON sqltext.doc FOR EACH ROW EXECUTE FUNCTION sqltext.rev();
"#;

impl SqlTextPg {
    pub fn new(url: &str, mode: Mode) -> anyhow::Result<Self> {
        let b = SqlTextPg {
            id: INSTANCE.fetch_add(1, Ordering::Relaxed),
            url: url.to_string(),
            mode,
            base_written: Mutex::new(0),
        };
        b.with(|c| {
            c.batch_execute(SCHEMA)?;
            Ok(())
        })?;
        Ok(b)
    }

    fn open(&self) -> Result<Client, postgres::Error> {
        let mut c = Client::connect(&self.url, NoTls)?;
        let sc = if self.mode == Mode::Durable { "on" } else { "off" };
        c.batch_execute(&format!("SET synchronous_commit = {}; SET search_path = sqltext, public;", sc))?;
        Ok(c)
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut Client) -> R<T>) -> R<T> {
        CONNS.with(|m| {
            let mut m = m.borrow_mut();
            if !m.contains_key(&self.id) {
                m.insert(self.id, self.open()?);
            }
            f(m.get_mut(&self.id).unwrap())
        })
    }

    fn text(b: &[u8]) -> R<&str> {
        std::str::from_utf8(b).map_err(|_| BackendError::NotSupported("text column rejects invalid UTF-8"))
    }
}

/// `to_tsquery('simple', …)` text for the harness query syntax.
pub fn tsquery(query: &str) -> String {
    let mut parts = Vec::new();
    for t in query_terms(query) {
        if let Some(stem) = t.strip_suffix('*') {
            parts.push(format!("{}:*", quote_lexeme(stem)));
        } else if t.contains(' ') {
            parts.push(
                t.split_whitespace()
                    .map(quote_lexeme)
                    .collect::<Vec<_>>()
                    .join(" <-> "),
            );
        } else {
            parts.push(quote_lexeme(&t));
        }
    }
    parts.join(" & ")
}

fn quote_lexeme(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn pg_written_bytes(c: &mut Client) -> R<u64> {
    let row = c.query_one(
        "SELECT (SELECT coalesce(sum(writes * op_bytes), 0)::bigint FROM pg_stat_io) + (SELECT wal_bytes FROM pg_stat_wal)::bigint",
        &[],
    )?;
    Ok(row.get::<_, i64>(0) as u64)
}

impl Backend for SqlTextPg {
    fn id(&self) -> &'static str {
        "sql-text-pg"
    }
    fn capabilities(&self) -> Caps {
        Caps {
            replace: Cap::Emulated,
            read_lines: Cap::Emulated,
            read_version: Cap::Native,
            history: Cap::Native,
            search: Cap::Native,
            rename_folder: Cap::Emulated,
            concurrency_guard: "OCC on version",
            invalid_utf8: Cap::NA,
        }
    }
    fn create(&self, path: &str, body: &[u8]) -> R<Version> {
        let body = Self::text(body)?;
        self.with(|c| {
            c.execute("INSERT INTO sqltext.doc(path, body) VALUES ($1, $2)", &[&path, &body])?;
            Ok(1)
        })
    }
    fn delete(&self, path: &str) -> R<()> {
        self.with(|c| {
            c.execute(
                "UPDATE sqltext.doc SET deleted = true WHERE NOT deleted AND (path = $1 OR path LIKE $1 || '/%')",
                &[&path],
            )?;
            Ok(())
        })
    }
    fn rename(&self, from: &str, to: &str) -> R<()> {
        self.with(|c| {
            c.execute(
                "UPDATE sqltext.doc SET path = $2 || substr(path, length($1) + 1) WHERE NOT deleted AND (path = $1 OR path LIKE $1 || '/%')",
                &[&from, &to],
            )?;
            Ok(())
        })
    }
    fn list(&self, prefix: &str) -> R<Vec<Entry>> {
        self.with(|c| {
            let rows = c.query(
                "SELECT path, length(body) FROM sqltext.doc WHERE NOT deleted AND ($1 = '/' OR path LIKE $1 || '/%') ORDER BY path",
                &[&prefix],
            )?;
            Ok(rows
                .iter()
                .map(|r| Entry {
                    path: r.get(0),
                    is_dir: false,
                    nbytes: Some(r.get::<_, i32>(1) as u64),
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
                .query_opt("SELECT body, version FROM sqltext.doc WHERE path = $1 AND NOT deleted", &[&path])?
                .ok_or_else(|| BackendError::NotFound(path.to_string()))?;
            Ok((row.get::<_, String>(0).into_bytes(), row.get::<_, i32>(1) as u64))
        })
    }
    fn read_lines(&self, path: &str, from: u64, to: u64) -> R<Vec<u8>> {
        Ok(slice_lines(&self.read(path)?, from, to))
    }
    fn read_version(&self, path: &str, v: Version) -> R<Vec<u8>> {
        self.with(|c| {
            let row = c
                .query_opt(
                    "SELECT r.body FROM sqltext.doc_rev r JOIN sqltext.doc d ON d.id = r.doc_id WHERE d.path = $1 AND r.version = $2 ORDER BY d.deleted LIMIT 1",
                    &[&path, &(v as i32)],
                )?
                .ok_or_else(|| BackendError::NotFound(format!("{} v{}", path, v)))?;
            Ok(row.get::<_, String>(0).into_bytes())
        })
    }
    fn overwrite(&self, path: &str, body: &[u8]) -> R<Version> {
        let body = Self::text(body)?;
        self.with(|c| {
            let row = c
                .query_opt(
                    "UPDATE sqltext.doc SET body = $1, version = version + 1 WHERE path = $2 AND NOT deleted RETURNING version",
                    &[&body, &path],
                )?
                .ok_or_else(|| BackendError::NotFound(path.to_string()))?;
            Ok(row.get::<_, i32>(0) as u64)
        })
    }
    fn replace(&self, path: &str, old: &[u8], new: &[u8], base: Option<Version>) -> R<WriteOutcome> {
        let old = Self::text(old)?;
        let new = Self::text(new)?;
        self.with(|c| {
            let row = match base {
                Some(v) => c.query_opt(
                    "UPDATE sqltext.doc SET body = replace(body, $1, $2), version = version + 1 WHERE path = $3 AND version = $4 AND NOT deleted AND position($1 in body) > 0 RETURNING version",
                    &[&old, &new, &path, &(v as i32)],
                )?,
                None => c.query_opt(
                    "UPDATE sqltext.doc SET body = replace(body, $1, $2), version = version + 1 WHERE path = $3 AND NOT deleted AND position($1 in body) > 0 RETURNING version",
                    &[&old, &new, &path],
                )?,
            };
            match row {
                Some(r) => {
                    let v = r.get::<_, i32>(0) as u64;
                    Ok(WriteOutcome::Committed {
                        version: v,
                        direct: base.map_or(true, |b| b + 1 == v),
                    })
                }
                None => {
                    let cur = c
                        .query_opt("SELECT body FROM sqltext.doc WHERE path = $1 AND NOT deleted", &[&path])?
                        .ok_or_else(|| BackendError::NotFound(path.to_string()))?;
                    Ok(WriteOutcome::Conflict {
                        current_region: cur.get::<_, String>(0).into_bytes(),
                    })
                }
            }
        })
    }
    fn append(&self, path: &str, tail: &[u8]) -> R<Version> {
        let tail = Self::text(tail)?;
        self.with(|c| {
            let row = c
                .query_opt(
                    "UPDATE sqltext.doc SET body = body || $1, version = version + 1 WHERE path = $2 AND NOT deleted RETURNING version",
                    &[&tail, &path],
                )?
                .ok_or_else(|| BackendError::NotFound(path.to_string()))?;
            Ok(row.get::<_, i32>(0) as u64)
        })
    }
    fn search(&self, query: &str, prefix: &str) -> R<Vec<Hit>> {
        let terms = query_terms(query);
        let q = tsquery(query);
        if q.is_empty() {
            return Ok(vec![]);
        }
        self.with(|c| {
            let rows = c.query(
                "SELECT path, body FROM sqltext.doc WHERE tsv @@ to_tsquery('simple', $1) AND NOT deleted AND ($2 = '/' OR path LIKE $2 || '/%')",
                &[&q, &prefix],
            )?;
            Ok(rows
                .iter()
                .map(|r| Hit {
                    path: r.get(0),
                    line: first_hit_line(r.get::<_, String>(1).as_bytes(), &terms),
                })
                .collect())
        })
    }
    fn history(&self, path: &str) -> R<Vec<Version>> {
        self.with(|c| {
            let rows = c.query(
                "SELECT r.version FROM sqltext.doc_rev r JOIN sqltext.doc d ON d.id = r.doc_id WHERE d.path = $1 ORDER BY d.deleted, r.version",
                &[&path],
            )?;
            Ok(rows.iter().map(|r| r.get::<_, i32>(0) as u64).collect())
        })
    }
    fn storage_bytes(&self) -> R<u64> {
        self.with(|c| {
            let row = c.query_one(
                "SELECT coalesce(sum(pg_total_relation_size(format('%I.%I', schemaname, tablename)::regclass)), 0)::bigint FROM pg_tables WHERE schemaname = 'sqltext'",
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
            c.batch_execute("VACUUM FULL sqltext.doc; VACUUM FULL sqltext.doc_rev; SELECT gin_clean_pending_list('sqltext.doc_tsv');")?;
            Ok("VACUUM FULL + gin_clean_pending_list")
        })
    }
}
