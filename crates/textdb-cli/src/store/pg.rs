//! Postgres: a database with the `textdb_pg` extension, reached over the network.
//!
//! Every operation is a call into the extension's SQL surface (`kb.*`); this module holds no
//! algorithm, only the mapping between rows, SQLSTATEs and the CLI's types.

use std::time::Duration;

use postgres::fallible_iterator::FallibleIterator;
use postgres::{Client, NoTls, Row};
use textdb_sqlite::normalize_path;

use super::{Change, Chunk, Commit, Entry, Hit, Hunk, ImportStats, Result, Stat, Store, StoreError, Written};

pub struct PgStore {
    client: Client,
    listening: bool,
}

/// Keep the extension's `TX00n` SQLSTATEs, and the conflict payload it puts in `DETAIL`.
fn pg(e: postgres::Error) -> StoreError {
    match e.as_db_error() {
        Some(db) if db.code().code().starts_with("TX") => {
            let code = db.code().code().to_string();
            let conflict = if code == "TX001" {
                db.detail().and_then(|d| serde_json::from_str(d).ok())
            } else {
                None
            };
            StoreError {
                code,
                message: db.message().to_string(),
                conflict,
            }
        }
        Some(db) => StoreError::other(format!("{} (SQLSTATE {})", db.message(), db.code().code())),
        None => StoreError::other(e),
    }
}

/// Postgres stores content as `text`.
fn utf8<'a>(path: &str, bytes: &'a [u8]) -> Result<&'a str> {
    std::str::from_utf8(bytes)
        .map_err(|_| StoreError::invalid(format!("{path}: Postgres stores text, and this content is not valid UTF-8")))
}

fn written(json: &str) -> Result<Written> {
    serde_json::from_str(json).map_err(|e| StoreError::other(format!("unexpected write result {json}: {e}")))
}

fn entry(r: &Row) -> Entry {
    Entry {
        path: r.get(0),
        name: r.get(1),
        kind: r.get(2),
        nbytes: r.get(3),
        nlines: r.get(4),
        updated_at: r.get(5),
    }
}

const ENTRY_COLS: &str =
    "n.path, n.name, CASE n.kind WHEN 1 THEN 'file' ELSE 'folder' END, n.nbytes, n.nlines, n.updated_at::text";

impl PgStore {
    pub fn connect(url: &str) -> Result<Self> {
        Ok(PgStore {
            client: Client::connect(url, NoTls).map_err(pg)?,
            listening: false,
        })
    }

    /// `kb.edit` and `kb.append` return only the version; the commit row says how it landed.
    fn written_at(&mut self, path: &str, version: i64) -> Result<Written> {
        let kind: Option<String> = self
            .client
            .query_opt(
                "SELECT c.kind FROM kb.commit c JOIN kb.node n ON n.id = c.file_id \
                 WHERE n.path = $1 AND n.deleted_at IS NULL AND c.version = $2",
                &[&path, &version],
            )
            .map_err(pg)?
            .and_then(|r| r.get(0));
        Ok(Written {
            version,
            kind: kind.unwrap_or_else(|| "direct".to_string()),
        })
    }
}

impl Store for PgStore {
    fn backend(&self) -> &'static str {
        "postgres"
    }

    fn init(&mut self) -> Result<()> {
        self.client.batch_execute("CREATE EXTENSION IF NOT EXISTS textdb_pg").map_err(pg)?;
        let current: bool = self
            .client
            .query_one("SELECT to_regprocedure('kb.last_seq()') IS NOT NULL", &[])
            .map_err(pg)?
            .get(0);
        if !current {
            return Err(StoreError::other(
                "the textdb_pg extension in this database predates the change feed; install the build from this \
                 repository and recreate the extension",
            ));
        }
        Ok(())
    }

    fn nodes(&mut self, prefix: &str) -> Result<Vec<Entry>> {
        let prefix = normalize_path(prefix)?;
        if prefix != "/" {
            self.stat(&prefix)?;
        }
        let rows = self
            .client
            .query(
                &format!(
                    "SELECT {ENTRY_COLS} FROM kb.node n WHERE n.deleted_at IS NULL AND n.path <> '/' \
                     AND ($1 = '/' OR n.path LIKE kb._subtree_like($1) OR (n.path = $1 AND n.kind = 1))"
                ),
                &[&prefix],
            )
            .map_err(pg)?;
        Ok(rows.iter().map(entry).collect())
    }

    fn ls(&mut self, path: &str) -> Result<Vec<Entry>> {
        let path = normalize_path(path)?;
        self.stat(&path)?;
        let rows = self
            .client
            .query(
                &format!(
                    "SELECT {ENTRY_COLS} FROM kb.node n JOIN kb.node d ON n.parent_id = d.id \
                     WHERE d.path = $1 AND d.deleted_at IS NULL AND n.deleted_at IS NULL ORDER BY n.name"
                ),
                &[&path],
            )
            .map_err(pg)?;
        Ok(rows.iter().map(entry).collect())
    }

    fn stat(&mut self, path: &str) -> Result<Stat> {
        let path = normalize_path(path)?;
        let row = self
            .client
            .query_opt(
                "SELECT path, CASE kind WHEN 1 THEN 'file' ELSE 'folder' END, version, nbytes, nlines, \
                 updated_at::text, updated_by FROM kb.node WHERE path = $1 AND deleted_at IS NULL",
                &[&path],
            )
            .map_err(pg)?
            .ok_or_else(|| StoreError::not_found(format!("not found: {path}")))?;
        Ok(Stat {
            path: row.get(0),
            kind: row.get(1),
            version: row.get(2),
            nbytes: row.get(3),
            nlines: row.get(4),
            updated_at: row.get(5),
            updated_by: row.get(6),
        })
    }

    fn read(&mut self, path: &str, version: Option<i64>) -> Result<(Vec<u8>, i64)> {
        let path = normalize_path(path)?;
        match version {
            Some(v) => {
                let text: String = self.client.query_one("SELECT kb.content($1, $2)", &[&path, &v]).map_err(pg)?.get(0);
                Ok((text.into_bytes(), v))
            }
            None => {
                // One statement, so the content and its version come from the same snapshot.
                let row = self
                    .client
                    .query_opt(
                        "SELECT kb.content(path, NULL::bigint), version FROM kb.node \
                         WHERE path = $1 AND kind = 1 AND deleted_at IS NULL",
                        &[&path],
                    )
                    .map_err(pg)?
                    .ok_or_else(|| StoreError::not_found(format!("not found: {path}")))?;
                let text: String = row.get(0);
                Ok((text.into_bytes(), row.get(1)))
            }
        }
    }

    fn section(&mut self, path: &str, heading: &str) -> Result<Option<Vec<u8>>> {
        let text: Option<String> = self
            .client
            .query_one("SELECT kb.section($1, $2)", &[&path, &heading])
            .map_err(pg)?
            .get(0);
        Ok(text.map(String::into_bytes))
    }

    fn search(&mut self, query: &str, prefix: &str, limit: i64) -> Result<Vec<Hit>> {
        let rows = self
            .client
            .query(
                "SELECT path, line, snippet, rank::float8 FROM kb.search($1, $2, $3)",
                &[&query, &prefix, &limit.max(1)],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| Hit {
                path: r.get(0),
                line: r.get(1),
                snippet: r.get(2),
                rank: r.get(3),
            })
            .collect())
    }

    fn write(
        &mut self,
        path: &str,
        content: &[u8],
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<Written> {
        let content = utf8(path, content)?;
        let row = self
            .client
            .query_one(
                "SELECT kb.write($1, $2, $3, $4, $5)::text",
                &[&path, &content, &base_version, &author, &message],
            )
            .map_err(pg)?;
        written(row.get(0))
    }

    fn edit(&mut self, path: &str, old: &[u8], new: &[u8], author: Option<&str>) -> Result<Written> {
        let (old, new) = (utf8(path, old)?, utf8(path, new)?);
        let version: i64 = self
            .client
            .query_one("SELECT kb.edit($1, $2, $3, $4)", &[&path, &old, &new, &author])
            .map_err(pg)?
            .get(0);
        self.written_at(&normalize_path(path)?, version)
    }

    fn append(&mut self, path: &str, tail: &[u8], author: Option<&str>) -> Result<Written> {
        let tail = utf8(path, tail)?;
        let version: i64 = self
            .client
            .query_one("SELECT kb.append($1, $2, $3)", &[&path, &tail, &author])
            .map_err(pg)?
            .get(0);
        self.written_at(&normalize_path(path)?, version)
    }

    fn replace_lines(
        &mut self,
        path: &str,
        from: i64,
        to: i64,
        text: &[u8],
        base_version: Option<i64>,
        author: Option<&str>,
    ) -> Result<Written> {
        let text = utf8(path, text)?;
        let row = self
            .client
            .query_one(
                "SELECT kb.replace_lines($1, $2, $3, $4, $5, $6)::text",
                &[&path, &from, &to, &text, &base_version, &author],
            )
            .map_err(pg)?;
        written(row.get(0))
    }

    fn history(&mut self, path: &str) -> Result<Vec<Commit>> {
        let rows = self
            .client
            .query("SELECT version, author, ts::text, message, kind, base_version FROM kb.history($1)", &[&path])
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| Commit {
                version: r.get(0),
                author: r.get(1),
                ts: r.get(2),
                message: r.get(3),
                nbytes: None,
                kind: r.get(4),
                base_version: r.get(5),
            })
            .collect())
    }

    fn diff(&mut self, path: &str, v1: i64, v2: i64) -> Result<String> {
        Ok(self.client.query_one("SELECT kb.diff($1, $2, $3)", &[&path, &v1, &v2]).map_err(pg)?.get(0))
    }

    fn hunks(&mut self, path: &str, v1: i64, v2: i64) -> Result<Vec<Hunk>> {
        let rows = self
            .client
            .query(
                "SELECT old_from, old_count, new_from, new_count, old_text, new_text FROM kb.hunks($1, $2, $3)",
                &[&path, &v1, &v2],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| Hunk {
                old_from: r.get(0),
                old_count: r.get(1),
                new_from: r.get(2),
                new_count: r.get(3),
                old_text: r.get(4),
                new_text: r.get(5),
            })
            .collect())
    }

    fn chunks(&mut self, path: &str, version: Option<i64>) -> Result<Vec<Chunk>> {
        let rows = self
            .client
            .query(
                "SELECT ord, hash, byte_from, nbytes, line_from, nlines FROM kb.chunks($1, $2)",
                &[&path, &version],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| Chunk {
                ord: r.get(0),
                hash: r.get(1),
                byte_from: r.get(2),
                nbytes: r.get(3),
                line_from: r.get(4),
                nlines: r.get(5),
            })
            .collect())
    }

    fn mv(&mut self, from: &str, to: &str, author: Option<&str>) -> Result<()> {
        self.client.execute("SELECT kb.move($1, $2, $3)", &[&from, &to, &author]).map_err(pg)?;
        Ok(())
    }

    fn rm(&mut self, path: &str, author: Option<&str>) -> Result<()> {
        self.client.execute("SELECT kb.remove($1, $2)", &[&path, &author]).map_err(pg)?;
        Ok(())
    }

    fn last_seq(&mut self) -> Result<i64> {
        Ok(self.client.query_one("SELECT kb.last_seq()", &[]).map_err(pg)?.get(0))
    }

    fn feed(&mut self, since: i64, limit: i64) -> Result<Vec<Change>> {
        let rows = self
            .client
            .query(
                "SELECT seq, ts::text, op, path, old_path, node_kind, version, base_version, commit_kind, author, message \
                 FROM kb.feed($1, $2)",
                &[&since, &limit],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| Change {
                seq: r.get(0),
                ts: r.get(1),
                op: r.get(2),
                path: r.get(3),
                old_path: r.get(4),
                node_kind: r.get(5),
                version: r.get(6),
                base_version: r.get(7),
                commit_kind: r.get(8),
                author: r.get(9),
                message: r.get(10),
            })
            .collect())
    }

    fn wait(&mut self, timeout: Duration) -> Result<()> {
        // The extension notifies `textdb_change` in the same transaction as each change, and
        // Postgres delivers a notification only once that transaction commits.
        if !self.listening {
            self.client.batch_execute("LISTEN textdb_change").map_err(pg)?;
            self.listening = true;
            return Ok(());
        }
        let mut notes = self.client.notifications();
        if notes.timeout_iter(timeout).next().map_err(pg)?.is_some() {
            // Commits arrive in bursts; one read of the feed covers all that are queued.
            let mut queued = notes.iter();
            while queued.next().map_err(pg)?.is_some() {}
        }
        Ok(())
    }

    fn import(
        &mut self,
        files: &mut dyn Iterator<Item = (String, Vec<u8>)>,
        author: Option<&str>,
        batch: usize,
        progress: &mut dyn FnMut(&ImportStats),
        on_error: &mut dyn FnMut(&str, &StoreError),
    ) -> Result<ImportStats> {
        let mut stats = ImportStats::default();
        let mut pending = 0;
        let mut tx = self.client.transaction().map_err(pg)?;
        for (path, body) in files {
            stats.files += 1;
            stats.bytes += body.len() as u64;
            let outcome = match utf8(&path, &body) {
                Err(e) => Err(e),
                Ok(content) => {
                    // An error aborts a whole Postgres transaction; a savepoint per file keeps
                    // one bad file from costing the batch. Dropping it uncommitted rolls back.
                    let mut sp = tx.savepoint("import_file").map_err(pg)?;
                    match sp.query_one("SELECT kb.write($1, $2, NULL::bigint, $3, 'import')::text", &[&path, &content, &author]) {
                        Ok(row) => {
                            let w = written(row.get(0));
                            sp.commit().map_err(pg)?;
                            w
                        }
                        Err(e) => Err(pg(e)),
                    }
                }
            };
            match outcome {
                Ok(w) if w.kind == "noop" => stats.unchanged += 1,
                Ok(w) if w.version == 1 => stats.created += 1,
                Ok(_) => stats.updated += 1,
                Err(e) => {
                    stats.failed += 1;
                    on_error(&path, &e);
                }
            }
            pending += 1;
            if pending >= batch {
                tx.commit().map_err(pg)?;
                pending = 0;
                progress(&stats);
                tx = self.client.transaction().map_err(pg)?;
            }
        }
        tx.commit().map_err(pg)?;
        progress(&stats);
        Ok(stats)
    }

    fn export(&mut self, prefix: &str, sink: &mut dyn FnMut(&str, &[u8]) -> std::io::Result<()>) -> Result<usize> {
        let prefix = normalize_path(prefix)?;
        let paths: Vec<String> = self
            .client
            .query(
                "SELECT path FROM kb.node WHERE kind = 1 AND deleted_at IS NULL \
                 AND ($1 = '/' OR path LIKE kb._subtree_like($1)) ORDER BY path",
                &[&prefix],
            )
            .map_err(pg)?
            .iter()
            .map(|r| r.get(0))
            .collect();
        for path in &paths {
            let (body, _) = self.read(path, None)?;
            sink(path, &body)?;
        }
        Ok(paths.len())
    }
}
