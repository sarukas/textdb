//! SQLite: the store is a file, opened in-process with the binding compiled in.

use std::time::{Duration, Instant};

use rusqlite::Connection;
use textdb_core::CommitKind;
use textdb_sqlite::db::subtree_bounds;
use textdb_sqlite::{normalize_path, NodeRow, TextDb, DEFAULT_PREFIX};

use super::{Author, Change, Chunk, Commit, Entry, Hit, Hunk, ImportStats, PathEvent, Result, Stat, Store, StoreError, Written};

pub struct SqliteStore {
    conn: Connection,
    /// `--path-history`: this process's choice, or `None` to follow the store's setting.
    path_history: Option<bool>,
    /// `PRAGMA data_version` as of the last `wait`. It moves only when another connection
    /// commits, which is exactly the event a watcher is waiting for.
    data_version: Option<i64>,
}

fn sql(e: rusqlite::Error) -> StoreError {
    StoreError::other(e)
}

fn kind_name(kind: i64) -> String {
    if kind == 1 { "file" } else { "folder" }.to_string()
}

fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn written(r: textdb_sqlite::WriteResult) -> Written {
    Written {
        version: r.version as i64,
        kind: r.kind.as_str().to_string(),
    }
}

fn entry(n: NodeRow) -> Entry {
    Entry {
        path: n.path,
        name: n.name,
        kind: kind_name(n.kind),
        nbytes: n.nbytes,
        nlines: n.nlines,
        updated_at: Some(n.updated_at),
        ..Entry::default()
    }
}

fn not_found(path: &str) -> StoreError {
    StoreError::not_found(format!("not found: {path}"))
}

/// A version argument as the binding wants it. Versions start at 1, so a negative one can
/// only be a mistake, and 0 (the empty document before a file existed) is the floor.
fn version(v: i64) -> u64 {
    v.max(0) as u64
}

impl SqliteStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = textdb_sqlite::open(path).map_err(sql)?;
        // The virtual table is what the Python and Node clients query, and creating it lays
        // down a new store's tables; `migrate` then upgrades a store an older build wrote.
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS kb USING textdb(store='{DEFAULT_PREFIX}');"
        ))
        .map_err(sql)?;
        textdb_sqlite::schema::migrate(&conn, DEFAULT_PREFIX).map_err(sql)?;
        Ok(SqliteStore {
            conn,
            path_history: None,
            data_version: None,
        })
    }

    fn db(&self) -> TextDb<'_> {
        TextDb::attach(&self.conn, DEFAULT_PREFIX, true).with_path_history(self.path_history)
    }

    /// Run reads that must agree with each other — content and the version it is — in one
    /// read transaction, so a commit from another process cannot land between them.
    fn consistent<T>(&self, f: impl FnOnce(&TextDb) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN").map_err(sql)?;
        let r = f(&self.db());
        self.conn
            .execute_batch(if r.is_ok() { "COMMIT" } else { "ROLLBACK" })
            .map_err(sql)?;
        r
    }
}

impl Store for SqliteStore {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    fn init(&mut self) -> Result<()> {
        // `open` already created or upgraded everything.
        Ok(())
    }

    fn nodes(&mut self, prefix: &str) -> Result<Vec<Entry>> {
        let prefix = normalize_path(prefix)?;
        if prefix != "/" {
            let n = self.db().node_by_path(&prefix)?.ok_or_else(|| not_found(&prefix))?;
            if n.kind == 1 {
                return Ok(vec![entry(n)]);
            }
        }
        let cols = "path, name, kind, nbytes, nlines, updated_at";
        let map = |r: &rusqlite::Row| -> rusqlite::Result<Entry> {
            Ok(Entry {
                path: r.get(0)?,
                name: r.get(1)?,
                kind: kind_name(r.get(2)?),
                nbytes: r.get(3)?,
                nlines: r.get(4)?,
                updated_at: r.get(5)?,
                ..Entry::default()
            })
        };
        let rows = match subtree_bounds(&prefix) {
            None => self
                .conn
                .prepare_cached(&format!(
                    "SELECT {cols} FROM {DEFAULT_PREFIX}node WHERE deleted_at IS NULL AND path <> '/'"
                ))
                .map_err(sql)?
                .query_map([], map)
                .map_err(sql)?
                .collect::<rusqlite::Result<Vec<_>>>(),
            Some((lo, hi)) => self
                .conn
                .prepare_cached(&format!(
                    "SELECT {cols} FROM {DEFAULT_PREFIX}node WHERE deleted_at IS NULL AND path >= ?1 AND path < ?2"
                ))
                .map_err(sql)?
                .query_map([lo, hi], map)
                .map_err(sql)?
                .collect::<rusqlite::Result<Vec<_>>>(),
        };
        rows.map_err(sql)
    }

    fn ls(&mut self, path: &str, recursive: bool) -> Result<Vec<Entry>> {
        Ok(self
            .db()
            .list(path, recursive)?
            .into_iter()
            .map(|e| Entry {
                path: e.path,
                name: e.name,
                kind: kind_name(e.kind),
                nbytes: e.nbytes,
                nlines: e.nlines,
                updated_at: Some(e.updated_at),
                nwords: e.nwords,
                versions: Some(e.versions),
                created_at: Some(e.created_at),
                updated_by: e.updated_by,
                files: e.files,
                folders: e.folders,
                authors: e
                    .authors
                    .into_iter()
                    .map(|a| Author {
                        author: a.author,
                        commits: a.commits,
                        last_ts: Some(a.last_ts),
                    })
                    .collect(),
            })
            .collect())
    }

    fn stat(&mut self, path: &str) -> Result<Stat> {
        let path = normalize_path(path)?;
        let n = self.db().node_by_path(&path)?.ok_or_else(|| not_found(&path))?;
        Ok(Stat {
            path: n.path,
            kind: kind_name(n.kind),
            version: n.version,
            nbytes: n.nbytes,
            nlines: n.nlines,
            updated_at: Some(n.updated_at),
            updated_by: n.updated_by,
        })
    }

    fn read(&mut self, path: &str, v: Option<i64>) -> Result<(Vec<u8>, i64)> {
        let path = normalize_path(path)?;
        self.consistent(|db| match v {
            Some(v) => Ok((db.read_version(&path, version(v))?, v)),
            None => {
                let n = db.node_by_path(&path)?.ok_or_else(|| not_found(&path))?;
                Ok((db.read(&path)?, n.version))
            }
        })
    }

    fn section(&mut self, path: &str, heading: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.db().section(path, heading)?)
    }

    fn search(&mut self, query: &str, prefix: &str, limit: i64) -> Result<Vec<Hit>> {
        Ok(self
            .db()
            .search(query, prefix, limit.max(1) as usize)?
            .into_iter()
            .map(|h| Hit {
                path: h.path,
                line: h.line,
                snippet: h.snippet,
                rank: h.rank,
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
        Ok(written(self.db().write(path, content, base_version.map(version), author, message)?))
    }

    fn edit(&mut self, path: &str, old: &[u8], new: &[u8], author: Option<&str>) -> Result<Written> {
        Ok(written(self.db().edit(path, old, new, author)?))
    }

    fn append(&mut self, path: &str, tail: &[u8], author: Option<&str>) -> Result<Written> {
        Ok(written(self.db().append(path, tail, author)?))
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
        Ok(written(self.db().replace_lines(
            path,
            version(from),
            version(to),
            text,
            base_version.map(version),
            author,
        )?))
    }

    fn history(&mut self, path: &str) -> Result<Vec<Commit>> {
        Ok(self
            .db()
            .history(path)?
            .into_iter()
            .map(|c| Commit {
                version: c.version,
                author: c.author,
                ts: c.ts,
                message: c.message,
                nbytes: c.nbytes,
                kind: c.kind,
                base_version: c.base_version,
            })
            .collect())
    }

    fn diff(&mut self, path: &str, v1: i64, v2: i64) -> Result<String> {
        Ok(self.db().diff(path, version(v1), version(v2))?)
    }

    fn hunks(&mut self, path: &str, v1: i64, v2: i64) -> Result<Vec<Hunk>> {
        Ok(self
            .db()
            .hunks(path, version(v1), version(v2))?
            .into_iter()
            .map(|h| Hunk {
                old_from: h.old_from as i64 + 1,
                old_count: h.old_count as i64,
                new_from: h.new_from as i64 + 1,
                new_count: h.new_count as i64,
                old_text: lossy(h.old_text),
                new_text: lossy(h.new_text),
            })
            .collect())
    }

    fn chunks(&mut self, path: &str, v: Option<i64>) -> Result<Vec<Chunk>> {
        Ok(self
            .db()
            .chunks(path, v.map(version))?
            .into_iter()
            .enumerate()
            .map(|(i, l)| Chunk {
                ord: i as i64,
                hash: textdb_core::hash::hex(&l.hash),
                byte_from: l.byte_off as i64,
                nbytes: l.nbytes as i64,
                line_from: l.line_off as i64 + 1,
                nlines: l.nlines as i64,
            })
            .collect())
    }

    fn mv(&mut self, from: &str, to: &str, author: Option<&str>) -> Result<()> {
        Ok(self.db().rename_by(from, to, author)?)
    }

    fn rm(&mut self, path: &str, author: Option<&str>) -> Result<()> {
        Ok(self.db().delete_by(path, author)?)
    }

    fn path_history(&mut self, path: &str) -> Result<Vec<PathEvent>> {
        Ok(self
            .db()
            .path_history(path)?
            .into_iter()
            .map(|e| PathEvent {
                id: e.id,
                ts: e.ts,
                op: e.op,
                old_path: e.old_path,
                new_path: e.new_path,
                via: e.via,
                version: e.version,
                author: e.author,
            })
            .collect())
    }

    fn set_session_path_history(&mut self, on: Option<bool>) -> Result<()> {
        self.path_history = on;
        Ok(())
    }

    fn path_history_enabled(&mut self) -> Result<bool> {
        Ok(self.db().path_history_enabled()?)
    }

    fn setting(&mut self, key: &str) -> Result<Option<String>> {
        Ok(self.db().setting(key)?)
    }

    fn set_setting(&mut self, key: &str, value: Option<&str>) -> Result<Option<String>> {
        let db = self.db();
        db.set_setting(key, value)?;
        Ok(db.setting(key)?)
    }

    fn last_seq(&mut self) -> Result<i64> {
        Ok(self.db().last_seq()?)
    }

    fn feed(&mut self, since: i64, limit: i64) -> Result<Vec<Change>> {
        Ok(self
            .db()
            .feed(since, limit.max(0) as usize)?
            .into_iter()
            .map(|c| Change {
                seq: c.seq,
                ts: c.ts,
                op: c.op,
                path: c.path,
                old_path: c.old_path,
                node_kind: kind_name(c.node_kind),
                version: c.version,
                base_version: c.base_version,
                commit_kind: c.commit_kind,
                author: c.author,
                message: c.message,
            })
            .collect())
    }

    fn wait(&mut self, timeout: Duration) -> Result<()> {
        // SQLite cannot notify another process, so poll the one counter that moves when some
        // other connection commits. It is connection-local and costs no I/O.
        let deadline = Instant::now() + timeout;
        loop {
            let now: i64 = self.conn.query_row("PRAGMA data_version", [], |r| r.get(0)).map_err(sql)?;
            let changed = self.data_version.is_some_and(|seen| seen != now);
            self.data_version = Some(now);
            if changed || Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn import(
        &mut self,
        files: &mut dyn Iterator<Item = (String, Vec<u8>)>,
        author: Option<&str>,
        batch: usize,
        progress: &mut dyn FnMut(&ImportStats),
        on_error: &mut dyn FnMut(&str, &StoreError),
    ) -> Result<ImportStats> {
        let db = TextDb::attach(&self.conn, DEFAULT_PREFIX, true);
        let mut stats = ImportStats::default();
        let mut pending = 0;
        self.conn.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
        for (path, body) in files {
            // Inside the batch transaction each upsert runs under its own savepoint, so a file
            // that fails is rolled back alone and the batch carries on.
            match db.upsert(&path, &body, author) {
                Ok(r) if r.kind == CommitKind::NoOp => stats.unchanged += 1,
                Ok(r) if r.version == 1 => stats.created += 1,
                Ok(_) => stats.updated += 1,
                Err(e) => {
                    stats.failed += 1;
                    on_error(&path, &StoreError::from(e));
                }
            }
            stats.files += 1;
            stats.bytes += body.len() as u64;
            pending += 1;
            if pending >= batch {
                self.conn.execute_batch("COMMIT; BEGIN IMMEDIATE").map_err(sql)?;
                pending = 0;
                progress(&stats);
            }
        }
        self.conn.execute_batch("COMMIT").map_err(sql)?;
        progress(&stats);
        Ok(stats)
    }
}
