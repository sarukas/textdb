//! SQLite: the store is a file, opened in-process with the binding compiled in.

use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};
use textdb_core::CommitKind;
use textdb_sqlite::db::subtree_bounds;
use textdb_sqlite::{normalize_path, NodeRow, TextDb, DEFAULT_PREFIX};

use super::{
    Author, BaseFile, BatchChange, Change, Chunk, Commit, Entry, FileHead, GitState, Hit, Hunk, ImportStats, MovedBack, PathEvent,
    RestoredFile, Result, RevertOutcome, SqlResult, Store, StoreError, SyncBase, Written,
};

pub struct SqliteStore {
    conn: Connection,
    /// `--path-history`: this process's choice, or `None` to follow the store's setting.
    path_history: Option<bool>,
    /// `PRAGMA data_version` as of the last `wait`. It moves only when another connection
    /// commits, which is exactly the event a watcher is waiting for.
    data_version: Option<i64>,
}

impl SqliteStore {
    fn link_rows(&self, cond: &str, args: [String; 3]) -> Result<Vec<super::LinkRow>> {
        let p = DEFAULT_PREFIX;
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT n.path, l.line, coalesce(l.kind, ''), l.target_path, l.anchor, l.alias, l.status, r.path \
                 FROM {p}link l JOIN {p}node n ON n.id = l.file_id AND n.deleted_at IS NULL \
                 LEFT JOIN {p}node r ON r.id = l.resolved_id AND r.deleted_at IS NULL \
                 WHERE {cond} ORDER BY n.path, l.line, l.rowid"
            ))
            .map_err(sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok(super::LinkRow {
                    path: r.get(0)?,
                    line: r.get(1)?,
                    kind: r.get(2)?,
                    target: r.get(3)?,
                    anchor: r.get(4)?,
                    alias: r.get(5)?,
                    status: r.get(6)?,
                    resolved: r.get(7)?,
                    asset: false,
                })
            })
            .map_err(sql)?;
        rows.map(|r| r.map(super::asset_link)).collect::<rusqlite::Result<_>>().map_err(sql)
    }
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

/// The binding's listing row as the CLI's, field for field.
///
/// One conversion for every listing path — `ls`, `tree`, `stat`, `export` — so the twenty-four
/// keys cannot drift between commands the way they used to.
fn entry(e: textdb_sqlite::db::Entry) -> Entry {
    Entry {
        path: e.path,
        name: e.name,
        kind: kind_name(e.kind),
        version: e.version,
        nbytes: e.nbytes,
        nlines: e.nlines,
        updated_at: e.updated_at,
        updated_by: e.updated_by,
        id: e.id,
        dir: e.dir,
        depth: e.depth,
        ext: e.ext,
        title: e.title,
        nwords: e.nwords,
        nsections: e.nsections,
        nprops: e.nprops,
        nlinks: e.nlinks,
        nlinks_broken: e.nlinks_broken,
        versions: e.versions,
        created_at: e.created_at,
        files: e.files,
        folders: e.folders,
        nauthors: e.nauthors,
        authors: e
            .authors
            .into_iter()
            .map(|a| Author {
                author: a.author,
                commits: a.commits,
                last_ts: Some(a.last_ts),
            })
            .collect(),
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
        // The same query `ls -R` runs, so every listing command returns the same row. This
        // used to be a six-column projection, which is why `tree --json` returned less than
        // `tree` printed.
        Ok(self.db().list(prefix, true)?.into_iter().map(entry).collect())
    }

    fn ls(&mut self, path: &str, recursive: bool) -> Result<Vec<Entry>> {
        Ok(self.db().list(path, recursive)?.into_iter().map(entry).collect())
    }

    fn stat(&mut self, path: &str) -> Result<Entry> {
        Ok(entry(self.db().entry(path)?))
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

    fn property_keys(&mut self, prefix: &str, limit: i64) -> Result<Vec<crate::store::PropKey>> {
        Ok(self
            .db()
            .property_keys(prefix, limit.max(1) as usize)?
            .into_iter()
            .map(|k| crate::store::PropKey {
                key: k.key,
                docs: k.docs,
                values: k.values,
                kind: k.kind,
            })
            .collect())
    }
    fn property_values(&mut self, key: &str, prefix: &str, limit: i64) -> Result<Vec<crate::store::PropValue>> {
        Ok(self
            .db()
            .property_values(key, prefix, limit.max(1) as usize)?
            .into_iter()
            .map(|v| crate::store::PropValue { value: v.value, docs: v.docs })
            .collect())
    }
    fn outline(
        &mut self,
        prefix: &str,
        heading: Option<&str>,
        mode: &str,
        max_level: Option<i64>,
        limit: i64,
    ) -> Result<Vec<crate::store::OutlineRow>> {
        let m = textdb_sqlite::sections::Match::parse(mode);
        Ok(textdb_sqlite::sections::outline(&self.conn, DEFAULT_PREFIX, prefix, heading, m, max_level, limit.max(1) as usize)?
            .into_iter()
            .map(|r| crate::store::OutlineRow {
                path: r.path,
                heading: r.heading,
                heading_path: r.heading_path,
                level: r.level,
                line_from: r.line_from,
                line_to: r.line_to,
                nwords: r.nwords,
                nwords_total: r.nwords_total,
                nbytes: r.file_nbytes,
                nlines: r.file_nlines,
                file_nwords: r.file_nwords,
                version: r.file_version,
                updated_at: r.updated_at,
                updated_by: r.updated_by,
            })
            .collect())
    }
    fn heading_names(&mut self, prefix: &str, starts: &str, limit: i64) -> Result<Vec<crate::store::HeadingName>> {
        Ok(textdb_sqlite::sections::heading_names(&self.conn, DEFAULT_PREFIX, prefix, starts, limit.max(1) as usize)?
            .into_iter()
            .map(|(heading, sections, docs)| crate::store::HeadingName { heading, sections, docs })
            .collect())
    }
    fn property_find(&mut self, query: &str, folder: &str, limit: i64) -> Result<Vec<crate::store::PropHit>> {
        Ok(self
            .db()
            .property_find(query, folder, limit.max(1) as usize)?
            .into_iter()
            .map(|h| crate::store::PropHit {
                path: h.path,
                nbytes: h.nbytes,
                updated_at: h.updated_at,
                frontmatter: h.frontmatter.and_then(|t| serde_json::from_str(&t).ok()),
            })
            .collect())
    }
    fn search(&mut self, query: &str, prefix: &str, limit: i64, per_file: i64) -> Result<Vec<Hit>> {
        Ok(self
            .db()
            .search_lines(query, prefix, limit.max(1) as usize, per_file.max(1) as usize)?
            .into_iter()
            .map(|h| Hit {
                path: h.path,
                version: h.version,
                line: h.line,
                text: h.text,
                section: h.section,
                score: Some(h.score),
                more: h.more,
            })
            .collect())
    }

    fn sections_of(&mut self, path: &str) -> Result<Vec<(i64, i64, String)>> {
        Ok(textdb_sqlite::sections::outline(&self.conn, DEFAULT_PREFIX, path, None, textdb_sqlite::sections::Match::Exact, None, 10_000)?
            .into_iter()
            .map(|r| (r.line_from, r.line_to, r.heading_path))
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

    fn edit(&mut self, path: &str, old: &[u8], new: &[u8], author: Option<&str>, message: Option<&str>) -> Result<Written> {
        Ok(written(self.db().with_message(message).edit(path, old, new, author)?))
    }

    fn append(&mut self, path: &str, tail: &[u8], author: Option<&str>, message: Option<&str>) -> Result<Written> {
        Ok(written(self.db().with_message(message).append(path, tail, author)?))
    }

    fn replace_lines(
        &mut self,
        path: &str,
        from: i64,
        to: i64,
        text: &[u8],
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<Written> {
        Ok(written(self.db().with_message(message).replace_lines(
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
                nlines: c.nlines,
                nwords: c.nwords,
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

    fn replace_ranges(
        &mut self,
        path: &str,
        ranges: &[super::LineRange],
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<Written> {
        let ranges: Vec<(u64, u64, Vec<u8>)> = super::sorted_ranges(ranges)?
            .into_iter()
            .map(|r| (r.from as u64, r.to as u64, r.text.clone().into_bytes()))
            .collect();
        Ok(written(self.db().with_message(message).replace_line_ranges(path, &ranges, base_version.map(version), author)?))
    }

    fn links(&mut self, path: &str, statuses: &[&str]) -> Result<Vec<super::LinkRow>> {
        let path = normalize_path(path)?;
        let (lo, hi) = subtree_bounds(&path).unwrap_or_else(|| ("/".into(), "0".into()));
        let only = if statuses.is_empty() {
            String::new()
        } else {
            format!(" AND l.status IN ({})", statuses.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(","))
        };
        self.link_rows(&format!("(n.path = ?1 OR (n.path >= ?2 AND n.path < ?3)){only}"), [path, lo, hi])
    }

    fn backlinks(&mut self, path: &str) -> Result<Vec<super::LinkRow>> {
        let path = normalize_path(path)?;
        let (lo, hi) = subtree_bounds(&path).unwrap_or_else(|| ("/".into(), "0".into()));
        self.link_rows(
            &format!(
                "l.target_path <> '' AND l.resolved_id IN (SELECT id FROM {DEFAULT_PREFIX}node WHERE kind = 1 AND deleted_at IS NULL \
                 AND (path = ?1 OR path = ?1 || '.tdbasset' OR (path >= ?2 AND path < ?3)))"
            ),
            [path, lo, hi],
        )
    }

    fn mv(&mut self, from: &str, to: &str, author: Option<&str>, message: Option<&str>) -> Result<()> {
        use textdb_sqlite::links::LinkUpdates;
        Ok(self.db().with_message(message).with_link_updates(Some(LinkUpdates::Off)).rename_by(from, to, author)?)
    }

    fn mv_links(&mut self, from: &str, to: &str, author: Option<&str>, message: Option<&str>, update: Option<bool>) -> Result<Vec<super::MovedLink>> {
        use textdb_sqlite::links::LinkUpdates;
        let mode = update.map(|rewrite| if rewrite { LinkUpdates::Rewrite } else { LinkUpdates::Off });
        Ok(self
            .db()
            .with_message(message)
            .with_link_updates(mode)
            .rename_links(from, to, author)?
            .into_iter()
            .map(|c| super::MovedLink {
                path: c.path,
                line: c.line,
                kind: c.kind,
                target: c.target,
                now_at: c.now_at,
                version: c.version.map(|v| v as i64),
            })
            .collect())
    }

    fn rm(&mut self, path: &str, author: Option<&str>, message: Option<&str>) -> Result<()> {
        Ok(self.db().with_message(message).delete_by(path, author)?)
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

    fn file_heads(&mut self, prefix: &str) -> Result<Vec<FileHead>> {
        let prefix = normalize_path(prefix)?;
        let cols = "path, version, updated_by";
        let map = |r: &rusqlite::Row| -> rusqlite::Result<FileHead> {
            Ok(FileHead {
                path: r.get(0)?,
                version: r.get(1)?,
                updated_by: r.get(2)?,
            })
        };
        let rows = match subtree_bounds(&prefix) {
            None => self
                .conn
                .prepare_cached(&format!("SELECT {cols} FROM {DEFAULT_PREFIX}node WHERE deleted_at IS NULL AND kind = 1"))
                .map_err(sql)?
                .query_map([], map)
                .map_err(sql)?
                .collect::<rusqlite::Result<Vec<_>>>(),
            Some((lo, hi)) => self
                .conn
                .prepare_cached(&format!(
                    "SELECT {cols} FROM {DEFAULT_PREFIX}node WHERE deleted_at IS NULL AND kind = 1 AND path >= ?1 AND path < ?2"
                ))
                .map_err(sql)?
                .query_map([lo, hi], map)
                .map_err(sql)?
                .collect::<rusqlite::Result<Vec<_>>>(),
        };
        rows.map_err(sql)
    }

    fn sync_bases(&mut self, prefix: &str) -> Result<Vec<SyncBase>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT {SYNC_COLS} FROM {DEFAULT_PREFIX}sync WHERE prefix = ?1 ORDER BY synced_at DESC"
            ))
            .map_err(sql)?;
        let rows = stmt
            .query_map([prefix], sync_row)
            .map_err(sql)?
            .map(|r| r.map(|(_, base)| base))
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql)?;
        Ok(rows)
    }

    fn sync_base(&mut self, prefix: &str, dir: &str) -> Result<Option<SyncBase>> {
        let found = self
            .conn
            .query_row(
                &format!("SELECT {SYNC_COLS} FROM {DEFAULT_PREFIX}sync WHERE prefix = ?1 AND dir = ?2"),
                [prefix, dir],
                sync_row,
            )
            .optional()
            .map_err(sql)?;
        let Some((id, mut base)) = found else {
            return Ok(None);
        };
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT rel, version, blob, disk_size, disk_mtime, conflict FROM {DEFAULT_PREFIX}sync_file WHERE sync_id = ?1"
            ))
            .map_err(sql)?;
        base.files = stmt
            .query_map([id], |r| {
                Ok(BaseFile {
                    rel: r.get(0)?,
                    version: r.get(1)?,
                    blob: r.get(2)?,
                    disk_size: r.get(3)?,
                    disk_mtime: r.get(4)?,
                    conflict: r.get(5)?,
                })
            })
            .map_err(sql)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql)?;
        Ok(Some(base))
    }

    fn sql(&mut self, query: &str, params: &[String], author: Option<&str>, write: bool, dry_run: bool) -> Result<SqlResult> {
        self.conn.execute_batch(&sql_views(DEFAULT_PREFIX)).map_err(sql)?;
        let before = self.db().last_seq()?;
        let batch = if write { Some(new_batch_id(&self.conn)?) } else { None };
        // Read-only covers everything the statement runs, the textdb functions' own writes
        // included. A writing statement runs in one transaction: a row that fails undoes the
        // rows changed before it, and a dry run undoes them all.
        self.conn
            .execute_batch(if write { "BEGIN IMMEDIATE" } else { "PRAGMA query_only = ON" })
            .map_err(sql)?;
        textdb_sqlite::bulk::set_batch(&self.conn, batch.as_deref());
        let outcome = (|| -> Result<SqlResult> {
            let mut result = run_sql(&self.conn, query, params, author, write)?;
            if write {
                let db = self.db();
                result.store_changes = Some(db.last_seq()? - before);
                if dry_run {
                    result.dry_run = true;
                    result.changes = db.changes_after(before)?.into_iter().map(batch_change).collect();
                }
            }
            Ok(result)
        })();
        textdb_sqlite::bulk::set_batch(&self.conn, None);
        let end = match (write, outcome.is_ok() && !dry_run) {
            (true, true) => "COMMIT",
            (true, false) => "ROLLBACK",
            (false, _) => "PRAGMA query_only = OFF",
        };
        self.conn.execute_batch(end).map_err(sql)?;
        let mut result = outcome?;
        if !dry_run && result.store_changes.unwrap_or(0) > 0 {
            result.batch = batch;
        }
        Ok(result)
    }

    fn all_sync_bases(&mut self) -> Result<Vec<SyncBase>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("SELECT {SYNC_COLS} FROM {DEFAULT_PREFIX}sync ORDER BY synced_at DESC"))
            .map_err(sql)?;
        let rows = stmt
            .query_map([], sync_row)
            .map_err(sql)?
            .map(|r| r.map(|(_, base)| base))
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql)?;
        Ok(rows)
    }

    fn asset_stores(&mut self) -> Result<Vec<super::AssetStore>> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("SELECT name, driver, root, options, created_at FROM {DEFAULT_PREFIX}asset_store ORDER BY name"))
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(super::AssetStore {
                    name: r.get(0)?,
                    driver: r.get(1)?,
                    root: r.get(2)?,
                    options: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })
            .map_err(sql)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)
    }

    fn put_asset_store(&mut self, s: &super::AssetStore) -> Result<()> {
        self.conn
            .execute(
                &format!(
                    "INSERT INTO {DEFAULT_PREFIX}asset_store(name, driver, root, options, created_at) \
                     VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
                     ON CONFLICT(name) DO UPDATE SET driver = excluded.driver, root = excluded.root, options = excluded.options"
                ),
                rusqlite::params![s.name, s.driver, s.root, s.options],
            )
            .map_err(sql)?;
        Ok(())
    }

    fn remove_asset_store(&mut self, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute(&format!("DELETE FROM {DEFAULT_PREFIX}asset_store WHERE name = ?1"), [name])
            .map_err(sql)?
            > 0)
    }

    fn revert_batch(&mut self, batch: &str, author: Option<&str>, skip_changed: bool, dry_run: bool) -> Result<RevertOutcome> {
        let id = new_batch_id(&self.conn)?;
        self.conn.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
        textdb_sqlite::bulk::set_batch(&self.conn, Some(&id));
        let outcome = self.db().revert_batch(batch, author, skip_changed);
        textdb_sqlite::bulk::set_batch(&self.conn, None);
        self.conn
            .execute_batch(if outcome.is_ok() && !dry_run { "COMMIT" } else { "ROLLBACK" })
            .map_err(sql)?;
        let r = outcome?;
        Ok(RevertOutcome {
            batch: batch.to_string(),
            dry_run,
            revert_batch: (!dry_run).then_some(id),
            restored: r.restored.into_iter().map(|(path, version)| RestoredFile { path, version }).collect(),
            removed: r.removed,
            moved_back: r.moved_back.into_iter().map(|(from, to)| MovedBack { from, to }).collect(),
            recreated: r.recreated,
            skipped: r.skipped,
        })
    }

    fn save_sync_base(&mut self, base: &SyncBase) -> Result<()> {
        let p = DEFAULT_PREFIX;
        let git = base.git.as_ref();
        let tx = self.conn.transaction().map_err(sql)?;
        let id: i64 = tx
            .query_row(
                &format!(
                    "INSERT INTO {p}sync(prefix, dir, seq, synced_at, author, git_commit, git_branch, git_remote, git_clean, rules) \
                     VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?4, ?5, ?6, ?7, ?8, ?9) \
                     ON CONFLICT(prefix, dir) DO UPDATE SET seq = excluded.seq, synced_at = excluded.synced_at, \
                     author = excluded.author, git_commit = excluded.git_commit, git_branch = excluded.git_branch, \
                     git_remote = excluded.git_remote, git_clean = excluded.git_clean, rules = excluded.rules RETURNING id"
                ),
                rusqlite::params![
                    base.prefix,
                    base.dir,
                    base.seq,
                    base.author,
                    git.and_then(|g| g.commit.as_deref()),
                    git.and_then(|g| g.branch.as_deref()),
                    git.and_then(|g| g.remote.as_deref()),
                    git.map(|g| g.clean),
                    base.rules,
                ],
                |r| r.get(0),
            )
            .map_err(sql)?;
        // A sync that changed nothing still arrives here with the whole base, and rewriting it
        // meant a DELETE and an INSERT per file — hundreds of statements and their WAL records
        // for a run whose answer was "nothing to do". Read the rows first and skip the rewrite
        // when they already say this; one indexed scan against two statements per file.
        if same_sync_files(&tx, p, id, &base.files)? {
            return tx.commit().map_err(sql);
        }
        tx.execute(&format!("DELETE FROM {p}sync_file WHERE sync_id = ?1"), [id]).map_err(sql)?;
        {
            let mut insert = tx
                .prepare(&format!(
                    "INSERT INTO {p}sync_file(sync_id, rel, version, blob, disk_size, disk_mtime, conflict) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                ))
                .map_err(sql)?;
            for f in &base.files {
                insert
                    .execute(rusqlite::params![id, f.rel, f.version, f.blob, f.disk_size, f.disk_mtime, f.conflict])
                    .map_err(sql)?;
            }
        }
        tx.commit().map_err(sql)
    }

    fn put_sync_files(&mut self, prefix: &str, dir: &str, files: &[BaseFile]) -> Result<bool> {
        let p = DEFAULT_PREFIX;
        let tx = self.conn.transaction().map_err(sql)?;
        let Some(id) = tx
            .query_row(&format!("SELECT id FROM {p}sync WHERE prefix = ?1 AND dir = ?2"), [prefix, dir], |r| r.get::<_, i64>(0))
            .optional()
            .map_err(sql)?
        else {
            return Ok(false);
        };
        for f in files {
            tx.execute(&format!("DELETE FROM {p}sync_file WHERE sync_id = ?1 AND rel = ?2"), rusqlite::params![id, f.rel])
                .map_err(sql)?;
            tx.execute(
                &format!(
                    "INSERT INTO {p}sync_file(sync_id, rel, version, blob, disk_size, disk_mtime, conflict) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                ),
                rusqlite::params![id, f.rel, f.version, f.blob, f.disk_size, f.disk_mtime, f.conflict],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        Ok(true)
    }

    fn rename_sync_dir(&mut self, prefix: &str, from: &str, to: &str) -> Result<()> {
        self.conn
            .execute(&format!("UPDATE {DEFAULT_PREFIX}sync SET dir = ?3 WHERE prefix = ?1 AND dir = ?2"), [prefix, from, to])
            .map(|_| ())
            .map_err(sql)
    }
}

/// The views `textdb sql` offers: the live store by path, without internal ids or deleted files.
/// Temporary, so they exist on this connection only and never change the store's schema.
/// A batch id: when, and four random hex digits (`20260914-211500-3f9a`).
fn new_batch_id(conn: &Connection) -> Result<String> {
    conn.query_row("SELECT strftime('%Y%m%d-%H%M%S', 'now') || '-' || lower(hex(randomblob(2)))", [], |r| r.get(0))
        .map_err(sql)
}

fn batch_change(i: textdb_sqlite::bulk::BatchItem) -> BatchChange {
    BatchChange {
        op: i.op,
        path: i.path,
        old_path: i.old_path,
        from_version: i.from_version,
        to_version: i.to_version,
        diff: i.diff,
    }
}

fn sql_views(p: &str) -> String {
    format!(
        "-- `files` and `folders` are the canonical listing record filtered by kind: the same
         -- columns in the same order as textdb_ls, textdb_entry and kb.entry. `folders.parent`
         -- is `dir` now, the one name for it on every surface.
         CREATE TEMP VIEW IF NOT EXISTS files AS
           SELECT
                  path, name,
                  CASE kind WHEN 1 THEN 'file' ELSE 'folder' END AS kind,
                  CASE kind WHEN 1 THEN version END AS version,
                  CASE kind WHEN 1 THEN coalesce(nbytes, 0) ELSE t_bytes END AS nbytes,
                  CASE kind WHEN 1 THEN coalesce(nlines, 0) ELSE t_lines END AS nlines,
                  CASE WHEN kind = 0 AND t_updated_at > updated_at THEN t_updated_at ELSE updated_at END AS updated_at,
                  updated_by, id,
                  CASE WHEN path = '/' THEN NULL WHEN length(path) = length(name) + 1 THEN '/'
                       ELSE substr(path, 1, length(path) - length(name) - 1) END AS dir,
                  CASE WHEN path = '/' THEN 0 ELSE length(path) - length(replace(path, '/', '')) END AS depth,
                  CASE WHEN kind = 1 AND name GLOB '?*.*'
                       THEN lower(substr(name, length(rtrim(name, replace(name, '.', ''))) + 1)) END AS ext,
                  title,
                  CASE kind WHEN 1 THEN coalesce(nwords, 0) ELSE t_words END AS nwords,
                  CASE kind WHEN 1 THEN nsections ELSE t_sections END AS nsections,
                  CASE kind WHEN 1 THEN nprops ELSE t_props END AS nprops,
                  CASE kind WHEN 1 THEN nlinks ELSE t_links END AS nlinks,
                  CASE kind WHEN 1 THEN nlinks_broken ELSE t_links_broken END AS nlinks_broken,
                  CASE kind WHEN 1 THEN version ELSE t_versions END AS versions,
                  created_at,
                  CASE kind WHEN 0 THEN t_files END AS files,
                  CASE kind WHEN 0 THEN t_folders END AS folders,
                  coalesce(nauthors, 0) AS nauthors
           FROM {p}node WHERE kind = 1 AND deleted_at IS NULL
           -- No limit really, but a view with one is not flattened into a query with a WHERE: without
           -- it SQLite may call `textdb_content(path)` in that WHERE on folder rows, which fails.
           LIMIT -1;
         CREATE TEMP VIEW IF NOT EXISTS folders AS
           SELECT
                  path, name,
                  CASE kind WHEN 1 THEN 'file' ELSE 'folder' END AS kind,
                  CASE kind WHEN 1 THEN version END AS version,
                  CASE kind WHEN 1 THEN coalesce(nbytes, 0) ELSE t_bytes END AS nbytes,
                  CASE kind WHEN 1 THEN coalesce(nlines, 0) ELSE t_lines END AS nlines,
                  CASE WHEN kind = 0 AND t_updated_at > updated_at THEN t_updated_at ELSE updated_at END AS updated_at,
                  updated_by, id,
                  CASE WHEN path = '/' THEN NULL WHEN length(path) = length(name) + 1 THEN '/'
                       ELSE substr(path, 1, length(path) - length(name) - 1) END AS dir,
                  CASE WHEN path = '/' THEN 0 ELSE length(path) - length(replace(path, '/', '')) END AS depth,
                  CASE WHEN kind = 1 AND name GLOB '?*.*'
                       THEN lower(substr(name, length(rtrim(name, replace(name, '.', ''))) + 1)) END AS ext,
                  title,
                  CASE kind WHEN 1 THEN coalesce(nwords, 0) ELSE t_words END AS nwords,
                  CASE kind WHEN 1 THEN nsections ELSE t_sections END AS nsections,
                  CASE kind WHEN 1 THEN nprops ELSE t_props END AS nprops,
                  CASE kind WHEN 1 THEN nlinks ELSE t_links END AS nlinks,
                  CASE kind WHEN 1 THEN nlinks_broken ELSE t_links_broken END AS nlinks_broken,
                  CASE kind WHEN 1 THEN version ELSE t_versions END AS versions,
                  created_at,
                  CASE kind WHEN 0 THEN t_files END AS files,
                  CASE kind WHEN 0 THEN t_folders END AS folders,
                  coalesce(nauthors, 0) AS nauthors
           FROM {p}node WHERE kind = 0 AND deleted_at IS NULL;
         CREATE TEMP VIEW IF NOT EXISTS frontmatter AS
           SELECT n.path, f.data FROM {p}frontmatter f JOIN {p}node n ON n.id = f.file_id AND n.deleted_at IS NULL;
         CREATE TEMP VIEW IF NOT EXISTS sections AS
           SELECT n.path, s.heading_path AS heading, s.level, s.line_from, s.line_to,
                  s.heading AS title, s.nwords, s.nwords_total,
                  n.nbytes, n.nlines, n.nwords AS file_nwords, n.version, n.updated_at, n.updated_by
           FROM {p}section s JOIN {p}node n ON n.id = s.file_id AND n.deleted_at IS NULL;
         CREATE TEMP VIEW IF NOT EXISTS links AS
           SELECT n.path, l.target_path AS target, l.line, l.kind, l.anchor, l.alias, l.status,
                  CASE WHEN r.path LIKE '%.tdbasset' THEN substr(r.path, 1, length(r.path) - 9) ELSE r.path END AS resolved,
                  r.path LIKE '%.tdbasset' AS asset
           FROM {p}link l JOIN {p}node n ON n.id = l.file_id AND n.deleted_at IS NULL
           LEFT JOIN {p}node r ON r.id = l.resolved_id AND r.deleted_at IS NULL;
         CREATE TEMP VIEW IF NOT EXISTS properties AS
           SELECT n.path, r.key, r.val_txt AS value, r.val_num AS number, r.ord
           FROM {p}property r JOIN {p}node n ON n.id = r.file_id AND n.deleted_at IS NULL;
         CREATE TEMP VIEW IF NOT EXISTS commits AS
           SELECT n.path, c.version, c.author, c.ts, c.message, c.kind, c.base_version, c.nbytes, c.nlines, c.batch
           FROM {p}commit c JOIN {p}node n ON n.id = c.file_id AND n.deleted_at IS NULL;
         CREATE TEMP VIEW IF NOT EXISTS authors AS
           SELECT n.path, nullif(a.author, '') AS author, a.commits, a.first_ts, a.last_ts
           FROM {p}file_author a JOIN {p}node n ON n.id = a.file_id AND n.deleted_at IS NULL;"
    )
}

fn sql_value(v: rusqlite::types::ValueRef) -> serde_json::Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => i.into(),
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map_or(serde_json::Value::Null, serde_json::Value::Number),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
        ValueRef::Blob(b) => match std::str::from_utf8(b) {
            Ok(s) => s.into(),
            Err(_) => serde_json::json!({ "blob_bytes": b.len() }),
        },
    }
}

/// A statement's own error: the SQL is the caller's, so it is reported as invalid input.
fn statement_error(e: rusqlite::Error, write: bool) -> StoreError {
    let message = e.to_string();
    if !write && message.contains("readonly") {
        StoreError::invalid(format!("{message}: this statement changes the store; run it with --write"))
    } else {
        StoreError::invalid(message)
    }
}

fn run_sql(conn: &Connection, query: &str, params: &[String], author: Option<&str>, write: bool) -> Result<SqlResult> {
    let fail = |e: rusqlite::Error| statement_error(e, write);
    let mut stmt = conn.prepare(query).map_err(fail)?;
    if !write && !stmt.readonly() {
        return Err(StoreError::invalid("this statement changes the store; run it with --write"));
    }
    let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_string).collect();
    let names: Vec<Option<String>> = (1..=stmt.parameter_count()).map(|i| stmt.parameter_name(i).map(str::to_string)).collect();
    let mut values = params.iter();
    for (i, name) in names.iter().enumerate() {
        if matches!(name.as_deref(), Some(":author" | "@author" | "$author")) {
            stmt.raw_bind_parameter(i + 1, author).map_err(fail)?;
        } else {
            let value = values.next().ok_or_else(|| {
                StoreError::invalid(format!("the statement has more placeholders than the {} --param values given", params.len()))
            })?;
            stmt.raw_bind_parameter(i + 1, value.as_str()).map_err(fail)?;
        }
    }
    if values.next().is_some() {
        return Err(StoreError::invalid("more --param values were given than the statement has placeholders"));
    }
    let mut rows = stmt.raw_query();
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(fail)? {
        let values = (0..columns.len()).map(|c| row.get_ref(c).map(sql_value)).collect::<rusqlite::Result<Vec<_>>>();
        out.push(values.map_err(fail)?);
    }
    Ok(SqlResult {
        columns,
        rows: out,
        ..SqlResult::default()
    })
}

const SYNC_COLS: &str = "id, prefix, dir, seq, synced_at, author, git_commit, git_branch, git_remote, git_clean, rules";

fn sync_row(r: &rusqlite::Row) -> rusqlite::Result<(i64, SyncBase)> {
    let clean: Option<bool> = r.get(9)?;
    let (commit, branch, remote): (Option<String>, Option<String>, Option<String>) = (r.get(6)?, r.get(7)?, r.get(8)?);
    Ok((
        r.get(0)?,
        SyncBase {
            prefix: r.get(1)?,
            dir: r.get(2)?,
            seq: r.get(3)?,
            synced_at: r.get(4)?,
            author: r.get(5)?,
            git: clean.map(|clean| GitState { commit, branch, remote, clean }),
            rules: r.get(10)?,
            files: Vec::new(),
        },
    ))
}

/// Do the stored `sync_file` rows for `id` already say exactly what `want` says?
///
/// Compared as a map rather than in order: the rewrite this avoids did not preserve any
/// order either, and `rel` is unique per sync, so the map is the meaning of the table.
fn same_sync_files(tx: &rusqlite::Transaction<'_>, p: &str, id: i64, want: &[BaseFile]) -> Result<bool> {
    type Row = (Option<i64>, String, Option<i64>, Option<i64>, bool);
    let mut have: std::collections::HashMap<String, Row> = std::collections::HashMap::with_capacity(want.len());
    {
        let mut st = tx
            .prepare_cached(&format!(
                "SELECT rel, version, blob, disk_size, disk_mtime, conflict FROM {p}sync_file WHERE sync_id = ?1"
            ))
            .map_err(sql)?;
        let rows = st
            .query_map([id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, i64>(5)? != 0),
                ))
            })
            .map_err(sql)?;
        for row in rows {
            let (rel, rest) = row.map_err(sql)?;
            have.insert(rel, rest);
        }
    }
    if have.len() != want.len() {
        return Ok(false);
    }
    Ok(want
        .iter()
        .all(|f| have.get(&f.rel) == Some(&(f.version, f.blob.clone(), f.disk_size, f.disk_mtime, f.conflict))))
}
