//! Postgres: a database with the `textdb_pg` extension, reached over the network.
//!
//! Every operation is a call into the extension's SQL surface (`kb.*`); this module holds no
//! algorithm, only the mapping between rows, SQLSTATEs and the CLI's types.

use std::time::Duration;

use postgres::fallible_iterator::FallibleIterator;
use postgres::types::{ToSql, Type};
use postgres::{Client, NoTls, Row};
use textdb_sqlite::normalize_path;

use super::{
    BaseFile, BatchChange, Change, Chunk, Commit, Entry, FileHead, GitState, Hit, Hunk, ImportStats, LineRange, LinkRow, MovedBack, MovedLink,
    PathEvent, RestoredFile, Result, RevertOutcome, SqlResult, Stat, Store, StoreError, SyncBase, Written,
};

/// The views `textdb sql` offers, as in SQLite: the live store by path. Temporary, so they live
/// in this session only.
const SQL_VIEWS: &str = "\
CREATE OR REPLACE TEMP VIEW files AS
  SELECT id, path, name,
         CASE WHEN length(path) = length(name) + 1 THEN '/' ELSE left(path, length(path) - length(name) - 1) END AS dir,
         length(path) - length(replace(path, '/', '')) AS depth,
         CASE WHEN name ~ '^.+\\.[^.]*$' THEN lower(substring(name from '\\.([^.]*)$')) ELSE '' END AS ext,
         version, nbytes, nlines, nwords, nauthors, created_at, updated_at, updated_by
  FROM kb.node WHERE kind = 1 AND deleted_at IS NULL;
CREATE OR REPLACE TEMP VIEW folders AS
  SELECT id, path, name,
         CASE WHEN path = '/' THEN NULL WHEN length(path) = length(name) + 1 THEN '/'
              ELSE left(path, length(path) - length(name) - 1) END AS parent,
         CASE WHEN path = '/' THEN 0 ELSE length(path) - length(replace(path, '/', '')) END AS depth,
         files, folders, nbytes, nlines, nwords, versions, updated_at FROM kb.entry WHERE kind = 'folder';
CREATE OR REPLACE TEMP VIEW frontmatter AS
  SELECT n.path, f.data FROM kb.frontmatter f JOIN kb.node n ON n.id = f.file_id AND n.deleted_at IS NULL;
CREATE OR REPLACE TEMP VIEW sections AS
  SELECT n.path, s.heading_path AS heading, s.level, s.line_from, s.line_to
  FROM kb.section s JOIN kb.node n ON n.id = s.file_id AND n.deleted_at IS NULL;
CREATE OR REPLACE TEMP VIEW links AS
  SELECT n.path, l.target_path AS target, l.line, l.kind, l.anchor, l.alias, l.status, r.path AS resolved
  FROM kb.link l JOIN kb.node n ON n.id = l.file_id AND n.deleted_at IS NULL
  LEFT JOIN kb.node r ON r.id = l.resolved_id AND r.deleted_at IS NULL;
CREATE OR REPLACE TEMP VIEW commits AS
  SELECT n.path, c.version, c.author, c.ts, c.message, c.kind, c.base_version, c.nbytes, c.nlines, c.batch
  FROM kb.commit c JOIN kb.node n ON n.id = c.file_id AND n.deleted_at IS NULL;
CREATE OR REPLACE TEMP VIEW authors AS
  SELECT n.path, nullif(a.author, '') AS author, a.commits, a.first_ts, a.last_ts
  FROM kb.file_author a JOIN kb.node n ON n.id = a.file_id AND n.deleted_at IS NULL;";

/// The sync base tables, as the extension defines them, for stores installed before they were.
const SYNC_TABLES: &str = "\
CREATE TABLE IF NOT EXISTS kb.sync (
  id bigserial PRIMARY KEY, prefix text NOT NULL, dir text NOT NULL, seq bigint NOT NULL,
  synced_at timestamptz NOT NULL DEFAULT now(), author text,
  git_commit text, git_branch text, git_remote text, git_clean boolean,
  UNIQUE (prefix, dir)
);
CREATE TABLE IF NOT EXISTS kb.sync_file (
  sync_id bigint NOT NULL REFERENCES kb.sync(id) ON DELETE CASCADE, rel text NOT NULL,
  version bigint, blob text NOT NULL, disk_size bigint, disk_mtime bigint,
  conflict boolean NOT NULL DEFAULT false,
  PRIMARY KEY (sync_id, rel)
);
ALTER TABLE kb.sync ADD COLUMN IF NOT EXISTS rules text;";

const SYNC_COLS: &str = "id, prefix, dir, seq, synced_at::text, author, git_commit, git_branch, git_remote, git_clean, rules";

fn sync_row(r: &Row) -> (i64, SyncBase) {
    let clean: Option<bool> = r.get(9);
    (
        r.get(0),
        SyncBase {
            prefix: r.get(1),
            dir: r.get(2),
            seq: r.get(3),
            synced_at: r.get(4),
            author: r.get(5),
            git: clean.map(|clean| GitState {
                commit: r.get(6),
                branch: r.get(7),
                remote: r.get(8),
                clean,
            }),
            rules: r.get(10),
            files: Vec::new(),
        },
    )
}

pub struct PgStore {
    client: Client,
    listening: bool,
    /// The sync base tables are known to exist.
    sync_ready: bool,
    /// This session has the `textdb sql` views.
    sql_views_ready: bool,
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
        ..Entry::default()
    }
}

const ENTRY_COLS: &str =
    "n.path, n.name, CASE n.kind WHEN 1 THEN 'file' ELSE 'folder' END, n.nbytes, n.nlines, n.updated_at::text";

/// A row of `kb.entry` selected with [`LISTING_COLS`].
fn listed(r: &Row) -> Entry {
    let authors: Option<String> = r.get(12);
    Entry {
        nwords: r.get(6),
        versions: r.get(7),
        created_at: r.get(8),
        updated_by: r.get(9),
        files: r.get(10),
        folders: r.get(11),
        authors: authors.and_then(|a| serde_json::from_str(&a).ok()).unwrap_or_default(),
        ..entry(r)
    }
}

const LISTING_COLS: &str = "e.path, e.name, e.kind, e.nbytes, e.nlines, e.updated_at::text, e.nwords, e.versions, \
     e.created_at::text, e.updated_by, e.files, e.folders, e.authors::text";

impl PgStore {
    pub fn connect(url: &str) -> Result<Self> {
        Ok(PgStore {
            client: Client::connect(url, NoTls).map_err(pg)?,
            listening: false,
            sync_ready: false,
            sql_views_ready: false,
        })
    }

    fn ensure_sync_tables(&mut self) -> Result<()> {
        if !self.sync_ready {
            self.client.batch_execute(SYNC_TABLES).map_err(pg)?;
            self.sync_ready = true;
        }
        Ok(())
    }

    /// Links of live files matching `cond` (over `n`, the file written in, `l` and `r`, the file
    /// resolved to), in path and line order.
    fn link_rows(&mut self, cond: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<LinkRow>> {
        let rows = self
            .client
            .query(
                &format!(
                    "SELECT n.path, l.line, coalesce(l.kind, ''), l.target_path, l.anchor, l.alias, l.status, r.path \
                     FROM kb.link l JOIN kb.node n ON n.id = l.file_id AND n.deleted_at IS NULL \
                     LEFT JOIN kb.node r ON r.id = l.resolved_id AND r.deleted_at IS NULL \
                     WHERE {cond} ORDER BY n.path COLLATE \"C\", l.line, l.id"
                ),
                params,
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| LinkRow {
                path: r.get(0),
                line: r.get(1),
                kind: r.get(2),
                target: r.get(3),
                anchor: r.get(4),
                alias: r.get(5),
                status: r.get(6),
                resolved: r.get(7),
            })
            .collect())
    }
}

/// A batch id: when, and four random hex digits (`20260914-211500-3f9a`), as SQLite makes them.
const NEW_BATCH_ID: &str = "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYYMMDD-HH24MISS') || '-' || substr(md5(random()::text), 1, 4)";

/// `query` with each `:author` placeholder (outside strings, quoted names, comments and `::`
/// casts) replaced by `$n`; `None` when it has none.
fn author_placeholder(query: &str, n: usize) -> Option<String> {
    let b = query.as_bytes();
    let (mut out, mut i, mut last, mut found) = (String::with_capacity(query.len()), 0, 0, false);
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'$' if i == 0 || !ident(b[i - 1]) => {
                // A dollar-quoted string: $tag$ … $tag$.
                let tag_end = b[i + 1..].iter().position(|c| !ident(*c)).map(|p| i + 1 + p);
                match tag_end {
                    Some(e) if b[e] == b'$' && !b[i + 1..e].first().is_some_and(u8::is_ascii_digit) => {
                        let tag = &query[i..=e];
                        i = query[e + 1..].find(tag).map_or(b.len(), |p| e + 1 + p + tag.len());
                    }
                    _ => i += 1,
                }
            }
            b':' if b.get(i + 1) == Some(&b':') => i += 2,
            b':' if query[i + 1..].starts_with("author") && !b.get(i + 7).is_some_and(|c| ident(*c)) => {
                out.push_str(&query[last..i]);
                out.push_str(&format!("${n}"));
                i += 7;
                last = i;
                found = true;
            }
            _ => i += 1,
        }
    }
    found.then(|| {
        out.push_str(&query[last.min(query.len())..]);
        out
    })
}

/// A statement's own error: the SQL is the caller's, so anything but the store's own `TX00n`
/// errors is reported as invalid input, as in SQLite.
fn statement_error(e: postgres::Error, write: bool) -> StoreError {
    let read_only = e.as_db_error().is_some_and(|db| db.code().code() == "25006");
    let mut err = pg(e);
    if err.code == "TX000" {
        err.code = "TX004".to_string();
    }
    if read_only && !write {
        err.message = format!("{}: this statement changes the store; run it with --write", err.message);
    }
    err
}

fn batch_change(v: &serde_json::Value) -> BatchChange {
    let text = |k: &str| v[k].as_str().map(str::to_string);
    BatchChange {
        op: text("op").unwrap_or_default(),
        path: text("path").unwrap_or_default(),
        old_path: text("old_path"),
        from_version: v["from_version"].as_i64(),
        to_version: v["to_version"].as_i64(),
        diff: text("diff"),
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
            .query_one("SELECT to_regprocedure('kb.revert_batch(text,text,boolean)') IS NOT NULL", &[])
            .map_err(pg)?
            .get(0);
        if !current {
            return Err(StoreError::other(
                "the textdb_pg extension in this database predates link resolution and batches; export the store, \
                 install the build from this repository, recreate the extension (DROP EXTENSION textdb_pg CASCADE; \
                 CREATE EXTENSION textdb_pg) and import again",
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

    fn ls(&mut self, path: &str, recursive: bool) -> Result<Vec<Entry>> {
        let path = normalize_path(path)?;
        self.stat(&path)?;
        let rows = self
            .client
            .query(&format!("SELECT {LISTING_COLS} FROM kb.ls($1, $2) e"), &[&path, &recursive])
            .map_err(pg)?;
        Ok(rows.iter().map(listed).collect())
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

    fn edit(&mut self, path: &str, old: &[u8], new: &[u8], author: Option<&str>, message: Option<&str>) -> Result<Written> {
        let (old, new) = (utf8(path, old)?, utf8(path, new)?);
        let row = self
            .client
            .query_one("SELECT kb._check_j(kb._edit_j($1, $2, $3, $4, $5))::text", &[&path, &old, &new, &author, &message])
            .map_err(pg)?;
        written(row.get(0))
    }

    fn append(&mut self, path: &str, tail: &[u8], author: Option<&str>, message: Option<&str>) -> Result<Written> {
        let tail = utf8(path, tail)?;
        let row = self
            .client
            .query_one("SELECT kb._check_j(kb._append_j($1, $2, $3, $4))::text", &[&path, &tail, &author, &message])
            .map_err(pg)?;
        written(row.get(0))
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
        let text = utf8(path, text)?;
        let row = self
            .client
            .query_one(
                "SELECT kb.replace_lines($1, $2, $3, $4, $5, $6, $7)::text",
                &[&path, &from, &to, &text, &base_version, &author, &message],
            )
            .map_err(pg)?;
        written(row.get(0))
    }

    fn history(&mut self, path: &str) -> Result<Vec<Commit>> {
        let rows = self
            .client
            .query("SELECT version, author, ts::text, message, kind, base_version, nbytes FROM kb.history($1)", &[&path])
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| Commit {
                version: r.get(0),
                author: r.get(1),
                ts: r.get(2),
                message: r.get(3),
                nbytes: r.get(6),
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

    fn replace_ranges(
        &mut self,
        path: &str,
        ranges: &[LineRange],
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<Written> {
        let sorted = super::sorted_ranges(ranges)?;
        let json = serde_json::Value::Array(
            sorted.iter().map(|r| serde_json::json!({ "from": r.from, "to": r.to, "text": r.text })).collect(),
        )
        .to_string();
        let row = self
            .client
            .query_one(
                "SELECT kb.replace_ranges($1, $2::text::jsonb, $3, $4, $5)::text",
                &[&path, &json, &base_version, &author, &message],
            )
            .map_err(pg)?;
        written(row.get(0))
    }

    fn mv_links(&mut self, from: &str, to: &str, author: Option<&str>, message: Option<&str>, update: Option<bool>) -> Result<Vec<MovedLink>> {
        let mode = update.map(|rewrite| if rewrite { "rewrite" } else { "off" });
        let json: String = self
            .client
            .query_one("SELECT kb.move_links($1, $2, $3, $4, $5)::text", &[&from, &to, &author, &message, &mode])
            .map_err(pg)?
            .get(0);
        let v: serde_json::Value = serde_json::from_str(&json).map_err(|e| StoreError::other(format!("unexpected move result {json}: {e}")))?;
        Ok(v["links"]
            .as_array()
            .map(|links| {
                links
                    .iter()
                    .map(|l| MovedLink {
                        path: l["path"].as_str().unwrap_or_default().to_string(),
                        line: l["line"].as_i64().unwrap_or(0),
                        kind: l["kind"].as_str().unwrap_or_default().to_string(),
                        target: l["target"].as_str().unwrap_or_default().to_string(),
                        now_at: l["now_at"].as_str().unwrap_or_default().to_string(),
                        version: l["version"].as_i64(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn links(&mut self, path: &str, statuses: &[&str]) -> Result<Vec<LinkRow>> {
        let path = normalize_path(path)?;
        let statuses: Vec<&str> = statuses.to_vec();
        self.link_rows(
            "($1 = '/' OR n.path = $1 OR n.path LIKE kb._subtree_like($1)) AND (cardinality($2::text[]) = 0 OR l.status = ANY($2::text[]))",
            &[&path, &statuses],
        )
    }

    fn backlinks(&mut self, path: &str) -> Result<Vec<LinkRow>> {
        let path = normalize_path(path)?;
        self.link_rows(
            "l.target_path <> '' AND r.kind = 1 AND ($1 = '/' OR r.path = $1 OR r.path LIKE kb._subtree_like($1))",
            &[&path],
        )
    }

    /// A move that leaves links as they are (a sync's move follows one made on disk).
    fn mv(&mut self, from: &str, to: &str, author: Option<&str>, message: Option<&str>) -> Result<()> {
        self.client
            .execute("SELECT kb.move_links($1, $2, $3, $4, 'off')", &[&from, &to, &author, &message])
            .map_err(pg)?;
        Ok(())
    }

    fn rm(&mut self, path: &str, author: Option<&str>, message: Option<&str>) -> Result<()> {
        self.client.execute("SELECT kb.remove($1, $2, $3)", &[&path, &author, &message]).map_err(pg)?;
        Ok(())
    }

    fn path_history(&mut self, path: &str) -> Result<Vec<PathEvent>> {
        let rows = self
            .client
            .query(
                "SELECT id, ts::text, op, old_path, new_path, via, version, author FROM kb.path_history($1)",
                &[&path],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| PathEvent {
                id: r.get(0),
                ts: r.get(1),
                op: r.get(2),
                old_path: r.get(3),
                new_path: r.get(4),
                via: r.get(5),
                version: r.get(6),
                author: r.get(7),
            })
            .collect())
    }

    /// A session setting (`textdb.path_history`), so it covers every call on this connection.
    fn set_session_path_history(&mut self, on: Option<bool>) -> Result<()> {
        match on {
            Some(true) => self.client.batch_execute("SET textdb.path_history = 'on'").map_err(pg),
            Some(false) => self.client.batch_execute("SET textdb.path_history = 'off'").map_err(pg),
            None => Ok(()),
        }
    }

    fn path_history_enabled(&mut self) -> Result<bool> {
        Ok(self.client.query_one("SELECT kb.path_history_enabled()", &[]).map_err(pg)?.get(0))
    }

    fn setting(&mut self, key: &str) -> Result<Option<String>> {
        Ok(self.client.query_one("SELECT kb.setting($1)", &[&key]).map_err(pg)?.get(0))
    }

    fn set_setting(&mut self, key: &str, value: Option<&str>) -> Result<Option<String>> {
        Ok(self
            .client
            .query_one("SELECT kb.set_setting($1, $2)", &[&key, &value])
            .map_err(pg)?
            .get(0))
    }

    fn last_seq(&mut self) -> Result<i64> {
        Ok(self.client.query_one("SELECT kb.last_seq()", &[]).map_err(pg)?.get(0))
    }

    fn file_heads(&mut self, prefix: &str) -> Result<Vec<FileHead>> {
        let prefix = normalize_path(prefix)?;
        let rows = self
            .client
            .query(
                "SELECT path, version, updated_by FROM kb.node \
                 WHERE deleted_at IS NULL AND kind = 1 AND ($1 = '/' OR path LIKE kb._subtree_like($1))",
                &[&prefix],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| FileHead {
                path: r.get(0),
                version: r.get(1),
                updated_by: r.get(2),
            })
            .collect())
    }

    fn sync_bases(&mut self, prefix: &str) -> Result<Vec<SyncBase>> {
        self.ensure_sync_tables()?;
        let rows = self
            .client
            .query(&format!("SELECT {SYNC_COLS} FROM kb.sync WHERE prefix = $1 ORDER BY synced_at DESC"), &[&prefix])
            .map_err(pg)?;
        Ok(rows.iter().map(|r| sync_row(r).1).collect())
    }

    fn sync_base(&mut self, prefix: &str, dir: &str) -> Result<Option<SyncBase>> {
        self.ensure_sync_tables()?;
        let Some(row) = self
            .client
            .query_opt(&format!("SELECT {SYNC_COLS} FROM kb.sync WHERE prefix = $1 AND dir = $2"), &[&prefix, &dir])
            .map_err(pg)?
        else {
            return Ok(None);
        };
        let (id, mut base) = sync_row(&row);
        base.files = self
            .client
            .query(
                "SELECT rel, version, blob, disk_size, disk_mtime, conflict FROM kb.sync_file WHERE sync_id = $1",
                &[&id],
            )
            .map_err(pg)?
            .iter()
            .map(|r| BaseFile {
                rel: r.get(0),
                version: r.get(1),
                blob: r.get(2),
                disk_size: r.get(3),
                disk_mtime: r.get(4),
                conflict: r.get(5),
            })
            .collect();
        Ok(Some(base))
    }

    fn revert_batch(&mut self, batch: &str, author: Option<&str>, skip_changed: bool, dry_run: bool) -> Result<RevertOutcome> {
        let mut tx = self.client.transaction().map_err(pg)?;
        let id: String = tx.query_one(NEW_BATCH_ID, &[]).map_err(pg)?.get(0);
        tx.execute("SELECT set_config('textdb.batch', $1, true)", &[&id]).map_err(pg)?;
        let json: String = tx
            .query_one("SELECT kb.revert_batch($1, $2, $3)::text", &[&batch, &author, &skip_changed])
            .map_err(pg)?
            .get(0);
        if dry_run {
            tx.rollback().map_err(pg)?;
        } else {
            tx.commit().map_err(pg)?;
        }
        let v: serde_json::Value = serde_json::from_str(&json).map_err(|e| StoreError::other(format!("unexpected revert result {json}: {e}")))?;
        let texts = |k: &str| -> Vec<String> {
            v[k].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
        };
        let empty = Vec::new();
        Ok(RevertOutcome {
            batch: batch.to_string(),
            dry_run,
            revert_batch: (!dry_run).then_some(id),
            restored: v["restored"]
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .map(|r| RestoredFile { path: r["path"].as_str().unwrap_or_default().to_string(), version: r["version"].as_i64().unwrap_or(0) })
                .collect(),
            removed: texts("removed"),
            moved_back: v["moved_back"]
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .map(|m| MovedBack { from: m["from"].as_str().unwrap_or_default().to_string(), to: m["to"].as_str().unwrap_or_default().to_string() })
                .collect(),
            recreated: texts("recreated"),
            skipped: texts("skipped"),
        })
    }

    fn sql(&mut self, query: &str, params: &[String], author: Option<&str>, write: bool, dry_run: bool) -> Result<SqlResult> {
        if !self.sql_views_ready {
            self.client.batch_execute(SQL_VIEWS).map_err(pg)?;
            self.sql_views_ready = true;
        }
        let before = self.last_seq()?;
        // `:author` is bound to the author, after the positional parameters.
        let rewritten = author_placeholder(query, params.len() + 1);
        let query = rewritten.as_deref().unwrap_or(query);
        let mut types = vec![Type::TEXT; params.len()];
        let mut values: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        if rewritten.is_some() {
            types.push(Type::TEXT);
            values.push(&author);
        }
        let first = query
            .trim_start()
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let fail = |e| statement_error(e, write);
        let mut tx = self.client.build_transaction().read_only(!write).start().map_err(pg)?;
        let batch: Option<String> = if write {
            let id: String = tx.query_one(NEW_BATCH_ID, &[]).map_err(pg)?.get(0);
            tx.execute("SELECT set_config('textdb.batch', $1, true)", &[&id]).map_err(pg)?;
            Some(id)
        } else {
            None
        };
        let mut result = SqlResult::default();
        if matches!(first.as_str(), "select" | "with" | "values" | "table") {
            // Each row as JSON, so any column type comes back without a Rust mapping for it.
            let inner = tx.prepare_typed(query, &types).map_err(fail)?;
            result.columns = inner.columns().iter().map(|c| c.name().to_string()).collect();
            let wrapped = tx
                .prepare_typed(&format!("SELECT row_to_json(q)::text FROM ({query}) q"), &types)
                .map_err(fail)?;
            for row in tx.query(&wrapped, &values).map_err(fail)? {
                let text: String = row.get(0);
                let object: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
                result
                    .rows
                    .push(result.columns.iter().map(|c| object.get(c).cloned().unwrap_or_default()).collect());
            }
        } else {
            let stmt = tx.prepare_typed(query, &types).map_err(fail)?;
            tx.execute(&stmt, &values).map_err(fail)?;
        }
        if write {
            let after: i64 = tx.query_one("SELECT coalesce(max(seq), 0) FROM kb.change", &[]).map_err(pg)?.get(0);
            result.store_changes = Some(after - before);
            if dry_run {
                result.dry_run = true;
                let json: String = tx.query_one("SELECT kb.changes_after($1)::text", &[&before]).map_err(pg)?.get(0);
                let items: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap_or_default();
                result.changes = items.iter().map(batch_change).collect();
            }
        }
        if dry_run {
            tx.rollback().map_err(pg)?;
        } else {
            tx.commit().map_err(pg)?;
        }
        if !dry_run && result.store_changes.unwrap_or(0) > 0 {
            result.batch = batch;
        }
        Ok(result)
    }

    fn save_sync_base(&mut self, base: &SyncBase) -> Result<()> {
        self.ensure_sync_tables()?;
        let git = base.git.as_ref();
        let (commit, branch, remote) = (
            git.and_then(|g| g.commit.clone()),
            git.and_then(|g| g.branch.clone()),
            git.and_then(|g| g.remote.clone()),
        );
        let clean = git.map(|g| g.clean);
        let mut tx = self.client.transaction().map_err(pg)?;
        let id: i64 = tx
            .query_one(
                "INSERT INTO kb.sync(prefix, dir, seq, author, git_commit, git_branch, git_remote, git_clean, rules) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (prefix, dir) DO UPDATE SET seq = excluded.seq, synced_at = now(), author = excluded.author, \
                 git_commit = excluded.git_commit, git_branch = excluded.git_branch, git_remote = excluded.git_remote, \
                 git_clean = excluded.git_clean, rules = excluded.rules RETURNING id",
                &[&base.prefix, &base.dir, &base.seq, &base.author, &commit, &branch, &remote, &clean, &base.rules],
            )
            .map_err(pg)?
            .get(0);
        tx.execute("DELETE FROM kb.sync_file WHERE sync_id = $1", &[&id]).map_err(pg)?;
        let rels: Vec<&str> = base.files.iter().map(|f| f.rel.as_str()).collect();
        let versions: Vec<Option<i64>> = base.files.iter().map(|f| f.version).collect();
        let blobs: Vec<&str> = base.files.iter().map(|f| f.blob.as_str()).collect();
        let sizes: Vec<Option<i64>> = base.files.iter().map(|f| f.disk_size).collect();
        let mtimes: Vec<Option<i64>> = base.files.iter().map(|f| f.disk_mtime).collect();
        let conflicts: Vec<bool> = base.files.iter().map(|f| f.conflict).collect();
        tx.execute(
            "INSERT INTO kb.sync_file(sync_id, rel, version, blob, disk_size, disk_mtime, conflict) \
             SELECT $1, * FROM unnest($2::text[], $3::bigint[], $4::text[], $5::bigint[], $6::bigint[], $7::bool[])",
            &[&id, &rels, &versions, &blobs, &sizes, &mtimes, &conflicts],
        )
        .map_err(pg)?;
        tx.commit().map_err(pg)
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
}
