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
    AccountRow, BaseFile, BatchChange, Change, Chunk, Commit, Entry, FileHead, GitState, Hit, Hunk, ImportStats, LineRange, LinkRow,
    MovedBack, MovedLink, PathEvent, RestoredFile, Result, RevertOutcome, ShareRow, SqlResult, Store, StoreError, SyncBase, TokenRow,
    Whoami, Written,
};

/// The views `textdb sql` offers, as in SQLite: the live store by path. Temporary, so they live
/// in this session only.
const SQL_VIEWS: &str = "\
-- `files` and `folders` are the canonical listing record filtered by kind, the same columns
-- in the same order as kb.entry and SQLite's. `folders.parent` is `dir` now.
CREATE OR REPLACE TEMP VIEW files AS
  SELECT * FROM kb.entry WHERE kind = 'file';
CREATE OR REPLACE TEMP VIEW folders AS
  SELECT * FROM kb.entry WHERE kind = 'folder';
-- Every one of these joins `kb.entry`, never `kb.node`: the view speaks the caller's paths and
-- holds only what it can see, so each of these inherits both. Joining the table gave an account
-- the store's paths on every surface but the two above (#12 K1, K5).
CREATE OR REPLACE TEMP VIEW frontmatter AS
  SELECT n.path, f.data FROM kb.frontmatter f JOIN kb.entry n ON n.id = f.file_id;
CREATE OR REPLACE TEMP VIEW properties AS
  SELECT n.path, r.key, r.val_txt AS value, r.val_num AS number, r.ord
  FROM kb.property r JOIN kb.entry n ON n.id = r.file_id;
CREATE OR REPLACE TEMP VIEW sections AS
  SELECT n.path, s.heading_path AS heading, s.level, s.line_from, s.line_to,
         s.heading AS title, s.nwords, s.nwords_total,
         n.nbytes, n.nlines, n.nwords AS file_nwords, n.version, n.updated_at, n.updated_by
  FROM kb.section s JOIN kb.entry n ON n.id = s.file_id;
CREATE OR REPLACE TEMP VIEW links AS
  SELECT n.path, n.version, l.line, coalesce(l.kind, '') AS kind,
         -- A target this account cannot see is the id reference its content reads as, never the
         -- path the link was written with (#12 E).
         CASE WHEN l.resolved_id IS NOT NULL AND r.id IS NULL THEN 'textdb:' || l.resolved_id
              ELSE l.target_path END AS target,
         l.anchor, l.alias, l.status,
         CASE WHEN lower(r.path) LIKE '%.tdbasset' THEN left(r.path, -9) ELSE r.path END AS resolved,
         coalesce(lower(r.path) LIKE '%.tdbasset', false) AS asset
  FROM kb.link l JOIN kb.entry n ON n.id = l.file_id
  LEFT JOIN kb.entry r ON r.id = l.resolved_id;
CREATE OR REPLACE TEMP VIEW commits AS
  SELECT n.path, c.version, c.author, c.ts, c.message, c.kind, c.base_version, c.nbytes, c.nlines, c.nwords, c.batch
  FROM kb.commit c JOIN kb.entry n ON n.id = c.file_id;
CREATE OR REPLACE TEMP VIEW authors AS
  SELECT n.path, nullif(a.author, '') AS author, a.commits, a.first_ts, a.last_ts
  FROM kb.file_author a JOIN kb.entry n ON n.id = a.file_id;";

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
ALTER TABLE kb.sync ADD COLUMN IF NOT EXISTS rules text;
ALTER TABLE kb.sync ADD COLUMN IF NOT EXISTS generation bigint NOT NULL DEFAULT 0;
ALTER TABLE kb.sync ADD COLUMN IF NOT EXISTS dir_id text;";

const SYNC_COLS: &str = "id, prefix, dir, seq, synced_at::text, author, git_commit, git_branch, git_remote, git_clean, rules, generation, dir_id";

/// The asset store table, as the extension defines it, for stores installed before it was.
const ASSET_TABLES: &str = "\
CREATE TABLE IF NOT EXISTS kb.asset_store (
  name text PRIMARY KEY, driver text NOT NULL, root text NOT NULL, options text,
  created_at timestamptz NOT NULL DEFAULT now()
);";

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
            generation: r.get(11),
            dir_id: r.get(12),
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
    /// The asset store table is known to exist.
    assets_ready: bool,
}

/// Keep the extension's `TX00n` SQLSTATEs, and the conflict payload it puts in `DETAIL`.
fn pg(e: postgres::Error) -> StoreError {
    let of = |code: String, db: &postgres::error::DbError| StoreError {
        conflict: (code == "TX001").then(|| db.detail().and_then(|d| serde_json::from_str(d).ok())).flatten(),
        message: db.message().trim_start_matches(&code).trim_start().to_string(),
        code,
    };
    match e.as_db_error() {
        Some(db) if db.code().code().starts_with("TX") => of(db.code().code().to_string(), db),
        // An error the extension raised inside SPI arrives as `XX000`: the re-raise on the way out
        // of a `#[pg_extern]` keeps the message and drops the SQLSTATE. `raise` puts the code in
        // front of the message for exactly this case, so a refusal is still a refusal here and not
        // an internal error (#12 C, H).
        Some(db) if db.message().len() > 5 && db.message().starts_with("TX") && db.message()[2..5].bytes().all(|c| c.is_ascii_digit()) => {
            of(db.message()[..5].to_string(), db)
        }
        Some(db) => StoreError::other(format!("{} (SQLSTATE {})", db.message(), db.code().code())),
        None => StoreError::other(e),
    }
}

/// Refuse a token session the store's own tables (#12 K2).
///
/// `kb.entry`, `kb.ls`, the `textdb sql` views and the `kb.*` functions are the surface; `kb.node`,
/// `kb.commit`, `kb.grant` and the rest are the owner's, and reading them walks straight past every
/// filter this feature adds.
///
/// This reads the statement, not the plan. The plan cannot answer the question: Postgres inlines a
/// view into the plan of the query that reads it, so `SELECT * FROM files` and `SELECT * FROM
/// kb.node` both come out as a scan of `kb.node`, and a check on the plan would refuse the
/// sanctioned surface along with the raw one. What the statement *names* is therefore what decides,
/// and the limit of that is written down rather than implied: a caller that reaches a table under
/// another name — through `search_path`, a quoted spelling, or a view of its own — is not caught
/// here. The mechanism that does catch it is Postgres's own, a role with no privilege on the tables
/// and `SECURITY DEFINER` on the functions that need them, and that is the follow-up this guard
/// stands in for. Nothing about it is a substitute for the extension's filtering, which is where a
/// row is actually kept back; this is about the raw tables having no business being read at all.
fn refuse_raw_tables(tx: &mut postgres::Transaction<'_>, query: &str) -> Result<()> {
    let account: Option<String> = tx
        .query_one("SELECT (SELECT a.name FROM kb.account a WHERE a.id = kb.current_account())", &[])
        .map_err(pg)?
        .get(0);
    if account.is_none() {
        return Ok(());
    }
    // Every table of the extension's schema; its views are not in this list, and are the surface.
    let rows = tx
        .query(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'kb' AND c.relkind IN ('r', 'p')",
            &[],
        )
        .map_err(pg)?;
    let words: Vec<&str> = query.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')).collect();
    for t in rows.iter().map(|r| r.get::<_, String>(0)) {
        if words.iter().any(|w| w.eq_ignore_ascii_case(&format!("kb.{t}"))) {
            return Err(StoreError::forbidden(format!(
                "kb.{t} is the store's own table and is not yours to read; the views and kb.* are \
                 (files, folders, commits, links, properties, sections, authors, frontmatter)"
            )));
        }
    }
    Ok(())
}

/// The statuses a `links` call asked for, when it asked for more than the one `kb.links` takes.
fn keep_statuses(rows: Vec<LinkRow>, statuses: &[&str]) -> Vec<LinkRow> {
    if statuses.len() < 2 {
        return rows;
    }
    rows.into_iter().filter(|l| l.status.as_deref().is_some_and(|s| statuses.contains(&s))).collect()
}

/// Postgres stores content as `text`.
fn utf8<'a>(path: &str, bytes: &'a [u8]) -> Result<&'a str> {
    std::str::from_utf8(bytes)
        .map_err(|_| StoreError::invalid(format!("{path}: Postgres stores text, and this content is not valid UTF-8")))
}

fn written(json: &str) -> Result<Written> {
    serde_json::from_str(json).map_err(|e| StoreError::other(format!("unexpected write result {json}: {e}")))
}

/// One `kb.entry` row selected with [`ENTRY_COLS`], as the CLI's `Entry`.
///
/// One conversion for every listing path, matching the SQLite store's, so the twenty-four
/// keys mean the same thing on either backend.
fn entry(r: &Row) -> Entry {
    let authors: Option<String> = r.get(23);
    Entry {
        path: r.get(0),
        name: r.get(1),
        kind: r.get(2),
        version: r.get(3),
        nbytes: r.get(4),
        nlines: r.get(5),
        updated_at: r.get(6),
        updated_by: r.get(7),
        id: r.get(8),
        dir: r.get(9),
        depth: r.get(10),
        ext: r.get(11),
        title: r.get(12),
        nwords: r.get(13),
        nsections: r.get(14),
        nprops: r.get(15),
        nlinks: r.get(16),
        nlinks_broken: r.get(17),
        versions: r.get(18),
        created_at: r.get(19),
        files: r.get(20),
        folders: r.get(21),
        nauthors: r.get(22),
        // The share a row was reached through, when the connection has one. `try_get`, because
        // the same `entry()` reads rows from queries written before these columns existed.
        share: r.try_get("share").ok().flatten(),
        rights: r.try_get("rights").ok().flatten(),
        shares: r
            .try_get::<_, Option<String>>("shares")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok()),
        authors: authors.and_then(|a| serde_json::from_str(&a).ok()).unwrap_or_default(),
    }
}

/// Timestamps are rendered to ISO-8601 UTC in SQL rather than left to `::text`, which would
/// give `2026-09-16 05:08:32.033+00` in the session's time zone — a different spelling of the
/// same field from the one the SQLite backend returns.
pub(crate) fn utc(col: &str) -> String {
    format!("to_char({col} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')")
}

/// The canonical `Entry` columns of `kb.entry`, in order.
fn entry_cols() -> String {
    format!(
        "e.path, e.name, e.kind, e.version, e.nbytes, e.nlines, {upd}, e.updated_by, e.id, e.dir, e.depth, e.ext, \
         e.title, e.nwords, e.nsections, e.nprops, e.nlinks, e.nlinks_broken, e.versions, {cre}, e.files, e.folders, \
         e.nauthors, e.authors::text, e.share, e.rights, e.shares::text",
        upd = utc("e.updated_at"),
        cre = utc("e.created_at"),
    )
}

impl PgStore {
    pub fn connect(url: &str) -> Result<Self> {
        Ok(PgStore {
            client: Client::connect(url, NoTls).map_err(pg)?,
            listening: false,
            sync_ready: false,
            sql_views_ready: false,
            assets_ready: false,
        })
    }

    /// Why a `kb.entry` lookup found nothing: forbidden, or simply not there.
    ///
    /// `kb.entry` holds only what the caller can see, so a path under a share whose folder is in
    /// the trash — or whose grant was taken away — is missing from it exactly as a path that never
    /// existed is. The difference is the whole of TX005, and `kb.resolve` is what knows it: sync
    /// deletes what the store no longer has and leaves alone what it merely may not have, so a
    /// revocation reported as absence is what would empty a checkout (#12 D11, H6).
    fn why_missing(&mut self, path: &str) -> StoreError {
        match self.client.query_one("SELECT kb.resolve($1)", &[&path]) {
            Err(e) => pg(e),
            // It resolves, so there is no node at it.
            Ok(_) => StoreError::not_found(format!("not found: {path}")),
        }
    }

    /// A path as this connection's own, resolving an `id:1234` reference to the path that names
    /// the same document here.
    ///
    /// Every query below addresses `kb.entry`, whose `path` is the caller's, and an id reference
    /// is not a path in any view — so it has to become one before it can be compared. Only an id
    /// costs the round trip; an ordinary path is already what it will be matched against.
    fn own_path(&mut self, path: &str) -> Result<String> {
        if !path.trim_start_matches('/').starts_with("id:") {
            return Ok(path.to_string());
        }
        let row = self.client.query_one("SELECT kb.to_view(kb.resolve($1))", &[&path]).map_err(pg)?;
        row.try_get::<_, Option<String>>(0)
            .ok()
            .flatten()
            .ok_or_else(|| StoreError::not_found(format!("not found: {path}")))
    }

    fn ensure_asset_tables(&mut self) -> Result<()> {
        if !self.assets_ready {
            self.client.batch_execute(ASSET_TABLES).map_err(pg)?;
            self.assets_ready = true;
        }
        Ok(())
    }

    fn ensure_sync_tables(&mut self) -> Result<()> {
        if !self.sync_ready {
            self.client.batch_execute(SYNC_TABLES).map_err(pg)?;
            self.sync_ready = true;
        }
        Ok(())
    }

    /// Links as `kb.links` or `kb.backlinks` reports them, in path and line order.
    ///
    /// Through the extension, never over `kb.link` and `kb.node`: those hold the store's own paths
    /// and no view of them, so a hand-written join here answered an account in the store's paths
    /// and reported a hidden target by naming it. The header's rule again — every operation is a
    /// call into `kb.*`.
    fn link_rows(&mut self, incoming: bool, path: &str, statuses: &[&str]) -> Result<Vec<LinkRow>> {
        let which = if incoming { "kb.backlinks" } else { "kb.links" };
        // `kb.links` narrows to one status. Asked for several — `--broken` is broken,
        // anchor-missing and not-in-store — it is asked for all of them and `keep_statuses` picks,
        // because narrowing to the first would drop the other two.
        let one = if statuses.len() == 1 { statuses[0] } else { "" };
        let rows = self
            .client
            .query(
                &format!(
                    "SELECT path, version, line, kind, target, anchor, alias, status, resolved, asset \
                     FROM {which}($1, $2) ORDER BY path COLLATE \"C\", line"
                ),
                &[&path, &one],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| LinkRow {
                path: r.get(0),
                version: r.get(1),
                line: r.get(2),
                kind: r.get(3),
                target: r.get(4),
                anchor: r.get(5),
                alias: r.get(6),
                status: r.get(7),
                resolved: r.get(8),
                asset: r.get(9),
                resolved_id: None,
            })
            .map(super::asset_link)
            .collect())
    }
}

/// A batch id: when, and four random hex digits (`20260914-211500-3f9a`), as SQLite makes them.
const NEW_BATCH_ID: &str = "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYYMMDD-HH24MISS') || '-' || substr(md5(random()::text), 1, 4)";

/// The placeholders of a statement, found outside strings, quoted names, comments and `::` casts.
#[derive(Debug, PartialEq)]
struct Placeholders {
    /// The statement with each `:author` replaced by `$n` (`n` as given), when it has one.
    rewritten: Option<String>,
    /// The highest `$n` written in the statement itself.
    positional: usize,
}

fn placeholders(query: &str, author_index: usize) -> Placeholders {
    let b = query.as_bytes();
    let (mut out, mut i, mut last) = (String::with_capacity(query.len()), 0, 0);
    let mut found = (false, 0usize);
    // Part of a name: ASCII letters, digits, `_`, `$`, and any byte of a non-ASCII character.
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                // E'…' strings escape with backslashes; others only double the quote.
                let escapes = i > 0 && matches!(b[i - 1], b'e' | b'E') && (i < 2 || !ident(b[i - 2]));
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\\' if escapes => i += 2,
                        b'\'' if b.get(i + 1) == Some(&b'\'') => i += 2,
                        b'\'' => break,
                        _ => i += 1,
                    }
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
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
                // Block comments nest.
                let mut depth = 0;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' if i == 0 || !ident(b[i - 1]) => {
                let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
                if digits > 0 {
                    found.1 = found.1.max(query[i + 1..i + 1 + digits].parse().unwrap_or(0));
                    i += 1 + digits;
                    continue;
                }
                // A dollar-quoted string: $tag$ … $tag$.
                let tag_len = b[i + 1..].iter().take_while(|c| **c != b'$' && ident(**c)).count();
                if b.get(i + 1 + tag_len) == Some(&b'$') {
                    let tag = &query[i..i + 2 + tag_len];
                    let body = i + tag.len();
                    i = query[body..].find(tag).map_or(b.len(), |p| body + p + tag.len());
                } else {
                    i += 1;
                }
            }
            b':' if b.get(i + 1) == Some(&b':') => i += 2,
            b':' if query[i + 1..].starts_with("author")
                && (i == 0 || !ident(b[i - 1]))
                && !query[i + 7..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$') =>
            {
                out.push_str(&query[last..i]);
                out.push_str(&format!("${author_index}"));
                i += 7;
                last = i;
                found.0 = true;
            }
            _ => i += 1,
        }
    }
    Placeholders {
        rewritten: found.0.then(|| {
            out.push_str(&query[last.min(query.len())..]);
            out
        }),
        positional: found.1,
    }
}

/// A statement's own error: the SQL is the caller's, so it is reported as invalid input, as in
/// SQLite; a lost connection stays an error of its own.
fn statement_error(e: postgres::Error, write: bool) -> StoreError {
    let Some(db) = e.as_db_error() else { return pg(e) };
    let read_only = db.code().code() == "25006" || db.message().contains("read-only transaction");
    let mut err = pg(e);
    // The SQL is the caller's, so a *SQL* error is invalid input — but a refusal the store raised
    // from inside the statement keeps its own kind, as it does on SQLite: a row refused for want of
    // rights exits 7, and a path the caller cannot address exits 5 (#12 C18, K4).
    if !err.code.starts_with("TX") || err.code == "TX000" {
        err.code = "TX004".to_string();
        err.conflict = None;
    }
    if read_only && !write {
        err.message = format!("{}: this statement changes the store; run it with --write", err.message);
    }
    err
}

/// A column of a row read without a JSON wrapper, for statements that cannot be a subquery.
fn cell(row: &Row, i: usize) -> serde_json::Value {
    use serde_json::Value;
    if let Ok(v) = row.try_get::<_, Option<String>>(i) {
        return v.map_or(Value::Null, Value::from);
    }
    if let Ok(v) = row.try_get::<_, Option<i64>>(i) {
        return v.map_or(Value::Null, Value::from);
    }
    if let Ok(v) = row.try_get::<_, Option<i32>>(i) {
        return v.map_or(Value::Null, Value::from);
    }
    if let Ok(v) = row.try_get::<_, Option<f64>>(i) {
        return v.map_or(Value::Null, Value::from);
    }
    if let Ok(v) = row.try_get::<_, Option<bool>>(i) {
        return v.map_or(Value::Null, Value::from);
    }
    Value::from(format!("({})", row.columns()[i].type_().name()))
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
    // ------------------------------------------------------------ accounts, tokens and shares
    //
    // Every one of these is one call into `kb.*`, as with everything else in this module: the
    // rules and the refusals are the extension's, so a client that talks to the database without
    // going through this CLI gets exactly the same answers.

    fn authenticate(&mut self, bearer: &str) -> Result<()> {
        self.client.query_one("SELECT kb.auth($1)", &[&bearer]).map_err(pg)?;
        Ok(())
    }

    fn whoami(&mut self) -> Result<Whoami> {
        let rows = self.client.query("SELECT * FROM kb.whoami()", &[]).map_err(pg)?;
        let first = rows.first().ok_or_else(|| StoreError::other("kb.whoami() said nothing"))?;
        let account: Option<String> = first.get(0);
        let admin: bool = first.get(1);
        let kind: String = first.get(2);
        let namespace: String = first.get(3);
        let shares = rows
            .iter()
            .filter_map(|r| {
                let alias: Option<String> = r.get(4);
                alias.map(|alias| ShareRow {
                    account: account.clone().unwrap_or_default(),
                    alias,
                    rights: r.get::<_, Option<String>>(5).unwrap_or_default(),
                    // An account is never told where its shares live in the store.
                    store_path: None,
                    node_id: r.get::<_, Option<i64>>(6).unwrap_or_default(),
                    dormant: r.get::<_, Option<bool>>(7).unwrap_or(false),
                })
            })
            .collect();
        Ok(Whoami { account, admin, kind, namespace, shares })
    }

    fn account_create(&mut self, name: &str, kind: &str, root: Option<&str>) -> Result<()> {
        self.client
            .query_one("SELECT kb.account_create($1, $2, $3)", &[&name, &kind, &root])
            .map_err(pg)?;
        Ok(())
    }

    fn account_disable(&mut self, name: &str, disabled: bool) -> Result<()> {
        self.client
            .query_one("SELECT kb.account_disable($1, $2)", &[&name, &disabled])
            .map_err(pg)?;
        Ok(())
    }

    fn account_ls(&mut self) -> Result<Vec<AccountRow>> {
        let rows = self
            .client
            .query(
                &format!("SELECT name, kind, root, {}, disabled, shares FROM kb.account_ls()", utc("created_at")),
                &[],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| AccountRow {
                name: r.get(0),
                kind: r.get(1),
                root: r.get(2),
                created_at: r.get(3),
                disabled: r.get(4),
                shares: r.get::<_, i64>(5) as usize,
            })
            .collect())
    }

    fn account_convert(&mut self, name: &str, alias: Option<&str>) -> Result<String> {
        Ok(self
            .client
            .query_one("SELECT kb.account_convert($1, $2)", &[&name, &alias])
            .map_err(pg)?
            .get(0))
    }

    fn token_create(&mut self, account: &str, label: Option<&str>, expires_at: Option<&str>) -> Result<(String, i64)> {
        // `$3::text::timestamptz`, not `$3::timestamptz`. A bare parameter under a single cast
        // is inferred as the cast's *target*, so Postgres declares `$3` a timestamptz and
        // rust-postgres then refuses to send a string for it ("error serializing parameter 2").
        // The first cast pins the parameter to text, which is what the expiry actually is.
        let row = self
            .client
            .query_one(
                "SELECT bearer, id FROM kb.token_create($1, $2, $3::text::timestamptz)",
                &[&account, &label, &expires_at],
            )
            .map_err(pg)?;
        Ok((row.get(0), row.get(1)))
    }

    fn token_ls(&mut self, account: Option<&str>) -> Result<Vec<TokenRow>> {
        let rows = self
            .client
            .query(
                &format!(
                    "SELECT id, account, label, {c}, {e}, {r}, {u}, live FROM kb.token_ls($1)",
                    c = utc("created_at"),
                    e = utc("expires_at"),
                    r = utc("revoked_at"),
                    u = utc("last_used_at"),
                ),
                &[&account],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| TokenRow {
                id: r.get(0),
                account: r.get(1),
                label: r.get(2),
                created_at: r.get(3),
                expires_at: r.get(4),
                revoked_at: r.get(5),
                last_used_at: r.get(6),
                live: r.get(7),
            })
            .collect())
    }

    fn token_revoke(&mut self, id: i64) -> Result<()> {
        self.client.query_one("SELECT kb.token_revoke($1)", &[&id]).map_err(pg)?;
        Ok(())
    }

    fn access_grant(&mut self, account: &str, path: &str, rights: &str, alias: Option<&str>) -> Result<ShareRow> {
        let row = self
            .client
            .query_one(
                "SELECT alias, rights, store_path, node_id FROM kb.access_grant($1, $2, $3, $4)",
                &[&account, &path, &rights, &alias],
            )
            .map_err(pg)?;
        Ok(ShareRow {
            account: account.to_string(),
            alias: row.get(0),
            rights: row.get(1),
            store_path: Some(row.get(2)),
            node_id: row.get(3),
            dormant: false,
        })
    }

    fn access_rename(&mut self, account: &str, from: &str, to: &str) -> Result<()> {
        self.client
            .query_one("SELECT kb.access_rename($1, $2, $3)", &[&account, &from, &to])
            .map_err(pg)?;
        Ok(())
    }

    fn access_revoke(&mut self, account: &str, alias: &str) -> Result<()> {
        self.client
            .query_one("SELECT kb.access_revoke($1, $2)", &[&account, &alias])
            .map_err(pg)?;
        Ok(())
    }

    fn share_state(&mut self) -> Result<Vec<(String, String)>> {
        let rows = self
            .client
            .query(
                "SELECT alias, CASE WHEN dormant THEN 'denied' ELSE rights END FROM kb.my_grant ORDER BY alias",
                &[],
            )
            .map_err(pg)?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    fn access_ls(&mut self, who: Option<&str>) -> Result<Vec<ShareRow>> {
        let rows = self
            .client
            .query("SELECT account, alias, rights, store_path, node_id, dormant FROM kb.access_ls($1)", &[&who])
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| ShareRow {
                account: r.get(0),
                alias: r.get(1),
                rights: r.get(2),
                store_path: Some(r.get(3)),
                node_id: r.get(4),
                dormant: r.get(5),
            })
            .collect())
    }

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
        // The same query `ls -R` runs, so every listing command returns the same row.
        self.ls(&prefix, true)
    }

    fn ls(&mut self, path: &str, recursive: bool) -> Result<Vec<Entry>> {
        let path = self.own_path(&normalize_path(path)?)?;
        self.stat(&path)?;
        let rows = self
            .client
            .query(&format!("SELECT {} FROM kb.ls($1, $2) e", entry_cols()), &[&path, &recursive])
            .map_err(pg)?;
        Ok(rows.iter().map(entry).collect())
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.client.query_one("SELECT kb.mkdir($1)", &[&path]).map_err(pg)?;
        Ok(())
    }

    fn stat(&mut self, path: &str) -> Result<Entry> {
        let path = self.own_path(&normalize_path(path)?)?;
        let row = self
            .client
            .query_opt(
                &format!(
                    "SELECT {c} FROM (SELECT * FROM kb.entry UNION ALL SELECT * FROM kb.root_entry) e WHERE e.path = $1",
                    c = entry_cols()
                ),
                &[&path],
            )
            .map_err(pg)?;
        let row = match row {
            Some(row) => row,
            None => return Err(self.why_missing(&path)),
        };
        Ok(entry(&row))
    }

    fn read(&mut self, path: &str, version: Option<i64>) -> Result<(Vec<u8>, i64)> {
        let path = self.own_path(&normalize_path(path)?)?;
        match version {
            Some(v) => {
                let text: String = self.client.query_one("SELECT kb.content($1, $2)", &[&path, &v]).map_err(pg)?.get(0);
                Ok((text.into_bytes(), v))
            }
            None => {
                // One statement, so the content and its version come from the same snapshot.
                //
                // A view, not `kb.node`: the raw table holds store paths, and the caller's path
                // is its own. Everything this module sends is in the caller's namespace and the
                // extension translates — reaching past it to a table was a leak waiting to
                // happen and, once accounts existed, simply failed to find anything.
                //
                // `kb.file` rather than `kb.entry`, because `kb.file` already *has* the content
                // column and builds it from the row it found. `kb.content(path)` off `kb.entry`
                // resolved the caller's path a second time and looked the node up again, on the
                // hottest read there is, to reach the row the outer query was already standing
                // on.
                let row = self
                    .client
                    .query_opt("SELECT content, version FROM kb.file WHERE path = $1", &[&path])
                    .map_err(pg)?;
                let row = match row {
                    Some(row) => row,
                    None => return Err(self.why_missing(&path)),
                };
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

    fn property_keys(&mut self, prefix: &str, limit: i64) -> Result<Vec<crate::store::PropKey>> {
        let rows = self
            .client
            .query("SELECT key, docs, values_n, kind FROM kb.prop_keys($1, $2)", &[&prefix, &limit.max(1)])
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| crate::store::PropKey {
                key: r.get(0),
                docs: r.get(1),
                values: r.get(2),
                kind: r.get(3),
            })
            .collect())
    }
    fn property_values(&mut self, key: &str, prefix: &str, limit: i64) -> Result<Vec<crate::store::PropValue>> {
        let rows = self
            .client
            .query("SELECT value, docs FROM kb.prop_values($1, $2, $3)", &[&key, &prefix, &limit.max(1)])
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| crate::store::PropValue {
                value: r.get(0),
                docs: r.get(1),
            })
            .collect())
    }
    fn property_find(&mut self, query: &str, folder: &str, limit: i64) -> Result<Vec<crate::store::PropHit>> {
        let rows = self
            .client
            .query(
                "SELECT path, nbytes, updated_at, frontmatter FROM kb.prop_find($1, $2, $3)",
                &[&query, &folder, &limit.max(1)],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| crate::store::PropHit {
                path: r.get(0),
                nbytes: r.get(1),
                updated_at: r.get(2),
                frontmatter: r.get::<_, Option<String>>(3).and_then(|t| serde_json::from_str(&t).ok()),
            })
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
        // `updated_at` comes back as text so the two engines print the same thing: the SQLite
        // binding stores the ISO-8601 string, and Postgres would otherwise render its own.
        let rows = self
            .client
            .query(
                "SELECT path, heading, heading_path, level, line_from, line_to, nwords, nwords_total, \
                        nbytes, nlines, file_nwords, version, \
                        to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), updated_by \
                   FROM kb.outline($1, $2, $3, $4, $5)",
                &[&prefix, &heading, &mode, &max_level, &limit.max(1)],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| crate::store::OutlineRow {
                path: r.get(0),
                heading: r.get(1),
                heading_path: r.get(2),
                level: r.get(3),
                line_from: r.get(4),
                line_to: r.get(5),
                nwords: r.get(6),
                nwords_total: r.get(7),
                nbytes: r.get(8),
                nlines: r.get(9),
                file_nwords: r.get(10),
                version: r.get(11),
                updated_at: r.get(12),
                updated_by: r.get(13),
            })
            .collect())
    }
    fn settle(&mut self) -> Result<()> {
        self.client.execute("SELECT kb.analyze_store()", &[]).map_err(pg)?;
        Ok(())
    }
    fn heading_names(&mut self, prefix: &str, starts: &str, limit: i64) -> Result<Vec<crate::store::HeadingName>> {
        let rows = self
            .client
            .query("SELECT heading, sections, docs FROM kb.headings($1, $2, $3)", &[&prefix, &starts, &limit.max(1)])
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| crate::store::HeadingName {
                heading: r.get(0),
                sections: r.get(1),
                docs: r.get(2),
            })
            .collect())
    }
    fn search(&mut self, query: &str, prefix: &str, limit: i64, per_file: i64) -> Result<Vec<Hit>> {
        let rows = self
            .client
            .query(
                "SELECT path, version, line, text, section, score::float8, more FROM kb.search($1, $2, $3, $4)",
                &[&query, &prefix, &limit.max(1), &per_file.max(1)],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| Hit {
                path: r.get(0),
                version: r.get(1),
                line: r.get(2),
                text: r.get(3),
                section: r.get(4),
                score: r.get(5),
                more: r.get(6),
            })
            .collect())
    }

    fn sections_of(&mut self, path: &str) -> Result<Vec<(i64, i64, String)>> {
        let rows = self
            .client
            .query(
                "SELECT line_from, line_to, heading_path FROM kb.outline($1, NULL, 'exact', NULL, 10000)",
                &[&path],
            )
            .map_err(pg)?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect())
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
            .query(
                &format!(
                    "SELECT version, author, {ts}, message, kind, base_version, nbytes, nlines, nwords FROM kb.history($1)",
                    ts = utc("ts")
                ),
                &[&path],
            )
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| Commit {
                version: r.get(0),
                author: r.get(1),
                ts: r.get(2),
                message: r.get(3),
                kind: r.get(4),
                base_version: r.get(5),
                nbytes: r.get(6),
                nlines: r.get(7),
                nwords: r.get(8),
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
                        outside: l["outside"].as_bool().unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn links(&mut self, path: &str, statuses: &[&str]) -> Result<Vec<LinkRow>> {
        let path = normalize_path(path)?;
        let rows = self.link_rows(false, &path, statuses)?;
        Ok(keep_statuses(rows, statuses))
    }

    fn backlinks(&mut self, path: &str) -> Result<Vec<LinkRow>> {
        let path = normalize_path(path)?;
        self.link_rows(true, &path, &[])
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

    fn may_name(&mut self, location: &str) -> Result<bool> {
        // Not a path, so not this question's to answer: an id means a file, not a place.
        if !location.starts_with('/') {
            return Ok(true);
        }
        // `kb.to_view` is NULL for a path the caller cannot address, and the path itself for the
        // owner -- the same rule every other read of a path goes through.
        let row = self.client.query_one("SELECT kb.to_view($1) IS NOT NULL", &[&location]).map_err(pg)?;
        Ok(row.get(0))
    }

    fn owner_paths(&mut self, paths: &[String]) -> Result<Vec<String>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        // One statement for the whole vault rather than a round trip per asset. `kb.to_store` is
        // lexical -- an alias stands for a subtree -- so it answers for a path nothing is at yet,
        // which is where a push is when it asks.
        let rows = self
            .client
            .query("SELECT kb.to_store(p) FROM unnest($1::text[]) WITH ORDINALITY AS t(p, i) ORDER BY i", &[&paths])
            .map_err(pg)?;
        Ok(paths
            .iter()
            .zip(rows)
            .map(|(given, r)| r.get::<_, Option<String>>(0).unwrap_or_else(|| given.clone()))
            .collect())
    }

    fn asset_item_users(&mut self, store: &str, location: &str, own: &str) -> Result<Option<crate::store::ItemUsers>> {
        use crate::assets::pointer::{asset_path, SUFFIX};
        use crate::assets::{location_key, pointer_names};
        // Row-level security is the caller's view made the table's own rule. Where it is enforced
        // for this connection the raw table is filtered too, so the honest answer is that this
        // cannot be told from here -- and nothing is then taken out of anybody's drive. A
        // connection that owns the tables reads them whole, which is the deployment the CLI has.
        let enforced: bool = self.client.query_one("SELECT row_security_active('kb.node')", &[]).map_err(pg)?.get(0);
        if enforced {
            return Ok(None);
        }
        let want = (store.to_string(), location_key(location));
        // `own` is already the owner's path, as `kb.node` holds: the assets it is compared with
        // are the store's own, not a view's.
        let mine = location_key(own);
        let like = format!("%{SUFFIX}");
        // `kb.node` and not a view: the views answer in the caller's namespace, which is the very
        // thing this question has to see past. Counts are all that leaves this method.
        let rows = self
            .client
            .query(
                "SELECT n.path, kb._materialize(n.root) FROM kb.node n \
                 WHERE n.deleted_at IS NULL AND n.kind = 1 AND n.path LIKE $1",
                &[&like],
            )
            .map_err(pg)?;
        let (mut others, mut unreadable) = (0, 0);
        for r in &rows {
            let path: String = r.get(0);
            let text: Option<String> = r.get(1);
            match text.as_deref().and_then(|t| pointer_names(&path, t)) {
                None => unreadable += 1,
                Some(names) if names == want && location_key(asset_path(&path)) != mine => others += 1,
                Some(_) => {}
            }
        }
        Ok(Some(crate::store::ItemUsers { others, unreadable }))
    }

    fn file_heads(&mut self, prefix: &str) -> Result<Vec<FileHead>> {
        let prefix = normalize_path(prefix)?;
        // The prefix has to resolve before anything is read: a folder outside the caller's shares
        // is not an empty sync, it is not found, and `kb.entry` filtering it away would have made
        // a sync of someone else's folder look like a successful sync of nothing (#12 G17, L4).
        if prefix != "/" {
            self.client.query_one("SELECT kb.resolve($1)", &[&prefix]).map_err(pg)?;
        }
        // `kb.entry`, not `kb.node`: the raw table holds the store's paths, and a sync reconciles
        // the caller's. Reading the table here gave a checkout the store's layout — `legal/` as a
        // top-level directory — and then failed to read back what it had just written.
        let rows = self
            .client
            .query(
                "SELECT path, version, updated_by FROM kb.entry \
                 WHERE kind = 'file' AND ($1 = '/' OR path = $1 OR path LIKE kb._subtree_like($1))",
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

    fn all_sync_bases(&mut self) -> Result<Vec<SyncBase>> {
        self.ensure_sync_tables()?;
        let rows = self
            .client
            .query(&format!("SELECT {SYNC_COLS} FROM kb.sync ORDER BY synced_at DESC"), &[])
            .map_err(pg)?;
        Ok(rows.iter().map(|r| sync_row(r).1).collect())
    }

    fn asset_stores(&mut self) -> Result<Vec<super::AssetStore>> {
        self.ensure_asset_tables()?;
        let rows = self
            .client
            .query("SELECT name, driver, root, options, created_at::text FROM kb.asset_store ORDER BY name", &[])
            .map_err(pg)?;
        Ok(rows
            .iter()
            .map(|r| super::AssetStore {
                name: r.get(0),
                driver: r.get(1),
                root: r.get(2),
                options: r.get(3),
                created_at: r.get(4),
            })
            .collect())
    }

    fn put_asset_store(&mut self, s: &super::AssetStore) -> Result<()> {
        self.ensure_asset_tables()?;
        self.client
            .execute(
                "INSERT INTO kb.asset_store(name, driver, root, options) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (name) DO UPDATE SET driver = excluded.driver, root = excluded.root, options = excluded.options",
                &[&s.name, &s.driver, &s.root, &s.options],
            )
            .map_err(pg)?;
        Ok(())
    }

    fn remove_asset_store(&mut self, name: &str) -> Result<bool> {
        self.ensure_asset_tables()?;
        Ok(self.client.execute("DELETE FROM kb.asset_store WHERE name = $1", &[&name]).map_err(pg)? > 0)
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
        // `:author` is bound to the author, after the positional parameters.
        let found = placeholders(query, params.len() + 1);
        if found.positional > params.len() {
            return Err(StoreError::invalid(format!(
                "the statement has more placeholders than the {} --param values given",
                params.len()
            )));
        }
        if found.positional < params.len() {
            return Err(StoreError::invalid("more --param values were given than the statement has placeholders"));
        }
        let query = found.rewritten.as_deref().unwrap_or(query);
        let mut types = vec![Type::TEXT; params.len()];
        let mut values: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        if found.rewritten.is_some() {
            types.push(Type::TEXT);
            values.push(&author);
        }
        let fail = |e| statement_error(e, write);
        let mut tx = self.client.build_transaction().read_only(!write).start().map_err(pg)?;
        refuse_raw_tables(&mut tx, query)?;
        let batch: Option<String> = if write {
            let id: String = tx.query_one(NEW_BATCH_ID, &[]).map_err(pg)?.get(0);
            tx.execute("SELECT set_config('textdb.batch', $1, true)", &[&id]).map_err(pg)?;
            Some(id)
        } else {
            None
        };
        let mut result = SqlResult::default();
        let inner = tx.prepare_typed(query, &types).map_err(fail)?;
        if inner.columns().is_empty() {
            tx.execute(&inner, &values).map_err(fail)?;
        } else {
            result.columns = inner.columns().iter().map(|c| c.name().to_string()).collect();
            // Each row as a JSON array, so any column type comes back without a Rust mapping for
            // it; columns are renamed positionally, so repeated names keep their own values.
            let n = result.columns.len();
            let wrapped_sql = format!(
                "SELECT json_build_array({})::text FROM ({query}) AS q({})",
                (1..=n).map(|i| format!("q.c{i}")).collect::<Vec<_>>().join(", "),
                (1..=n).map(|i| format!("c{i}")).collect::<Vec<_>>().join(", ")
            );
            // A statement that cannot be a subquery (EXPLAIN, SHOW, RETURNING, a writing WITH)
            // fails to prepare wrapped; the savepoint keeps that from aborting the transaction.
            let wrapped = if n <= 100 {
                let mut sp = tx.savepoint("textdb_sql_wrap").map_err(pg)?;
                match sp.prepare_typed(&wrapped_sql, &types) {
                    Ok(stmt) => {
                        sp.commit().map_err(pg)?;
                        Some(stmt)
                    }
                    Err(_) => None,
                }
            } else {
                None
            };
            match wrapped {
                Some(stmt) => {
                    for row in tx.query(&stmt, &values).map_err(fail)? {
                        let text: String = row.get(0);
                        let cells: Vec<serde_json::Value> =
                            serde_json::from_str(&text).map_err(|e| StoreError::other(format!("a row could not be read ({e}): {text}")))?;
                        result.rows.push(cells);
                    }
                }
                None => {
                    for row in tx.query(&inner, &values).map_err(fail)? {
                        result.rows.push((0..n).map(|i| cell(&row, i)).collect());
                    }
                }
            }
        }
        if let Some(id) = &batch {
            let changes: i64 = tx.query_one("SELECT count(*) FROM kb.change WHERE batch = $1", &[id]).map_err(pg)?.get(0);
            result.store_changes = Some(changes);
            if dry_run && changes > 0 {
                let json: String = tx.query_one("SELECT kb.batch_changes($1)::text", &[id]).map_err(pg)?.get(0);
                let items: Vec<serde_json::Value> = serde_json::from_str(&json).map_err(|e| StoreError::other(format!("unexpected changes {json}: {e}")))?;
                result.changes = items.iter().map(batch_change).collect();
            }
            result.dry_run = dry_run;
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
        // Compare-and-swap, as on SQLite: the row must still be at the generation this sync read.
        // `WHERE` on the conflict clause makes a stale save return no row rather than overwrite.
        // As there, this runs after the writes: it protects the next sync's starting point, not
        // this one's output.
        let id: i64 = tx
            .query_opt(
                "INSERT INTO kb.sync(prefix, dir, seq, author, git_commit, git_branch, git_remote, git_clean, rules, generation, dir_id) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::bigint + 1, $11) \
                 ON CONFLICT (prefix, dir) DO UPDATE SET seq = excluded.seq, synced_at = now(), author = excluded.author, \
                 git_commit = excluded.git_commit, git_branch = excluded.git_branch, git_remote = excluded.git_remote, \
                 git_clean = excluded.git_clean, rules = excluded.rules, generation = kb.sync.generation + 1, \
                 dir_id = excluded.dir_id \
                 WHERE kb.sync.generation = $10::bigint RETURNING id",
                &[&base.prefix, &base.dir, &base.seq, &base.author, &commit, &branch, &remote, &clean, &base.rules, &base.generation, &base.dir_id],
            )
            .map_err(pg)?
            .ok_or_else(|| {
                StoreError::contention(format!(
                    "{} and {} were synced by another process while this one was running, so this run's base was \
                     not recorded. What it wrote is in the store and on disk; run sync again to reconcile against \
                     the base that process left",
                    base.prefix, base.dir
                ))
            })?
            .get(0);
        // As in the SQLite store: a sync that changed nothing still arrives with the whole
        // base, and rewriting it cost a DELETE and an INSERT of every row for a run whose
        // answer was "nothing to do". One indexed read to find that out instead.
        if same_sync_files(&mut tx, id, &base.files)? {
            return tx.commit().map_err(pg);
        }
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

    fn put_sync_files(&mut self, prefix: &str, dir: &str, files: &[BaseFile]) -> Result<bool> {
        self.ensure_sync_tables()?;
        let mut tx = self.client.transaction().map_err(pg)?;
        let Some(row) = tx
            .query_opt("SELECT id FROM kb.sync WHERE prefix = $1 AND dir = $2 FOR UPDATE", &[&prefix, &dir])
            .map_err(pg)?
        else {
            return Ok(false);
        };
        let id: i64 = row.get(0);
        for f in files {
            tx.execute("DELETE FROM kb.sync_file WHERE sync_id = $1 AND rel = $2", &[&id, &f.rel]).map_err(pg)?;
            tx.execute(
                "INSERT INTO kb.sync_file(sync_id, rel, version, blob, disk_size, disk_mtime, conflict) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[&id, &f.rel, &f.version, &f.blob, &f.disk_size, &f.disk_mtime, &f.conflict],
            )
            .map_err(pg)?;
        }
        tx.commit().map_err(pg)?;
        Ok(true)
    }

    fn rename_sync_dir(&mut self, prefix: &str, from: &str, to: &str) -> Result<()> {
        self.ensure_sync_tables()?;
        self.client
            .execute("UPDATE kb.sync SET dir = $3 WHERE prefix = $1 AND dir = $2", &[&prefix, &from, &to])
            .map(|_| ())
            .map_err(pg)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(q: &str) -> (String, usize) {
        let p = placeholders(q, 9);
        (p.rewritten.unwrap_or_else(|| q.to_string()), p.positional)
    }

    #[test]
    fn author_placeholders_outside_strings_comments_and_casts() {
        assert_eq!(rewrite("SELECT kb.edit($1, 'a', 'b', :author)"), ("SELECT kb.edit($1, 'a', 'b', $9)".into(), 1));
        assert_eq!(rewrite("SELECT ':author', \":author\", x::author, :authority, $2"), ("SELECT ':author', \":author\", x::author, :authority, $2".into(), 2));
        assert_eq!(rewrite("SELECT 'it''s :author', :author -- :author\n"), ("SELECT 'it''s :author', $9 -- :author\n".into(), 0));
        assert_eq!(rewrite("SELECT E'it\\'s :author', :author"), ("SELECT E'it\\'s :author', $9".into(), 0));
        assert_eq!(rewrite("SELECT $$ :author $$, $tag$ $1 :author $tag$, :author"), ("SELECT $$ :author $$, $tag$ $1 :author $tag$, $9".into(), 0));
        assert_eq!(rewrite("/* a /* nested */ :author */ SELECT :author"), ("/* a /* nested */ :author */ SELECT $9".into(), 0));
        assert_eq!(rewrite("SELECT arr[1:author], :authoré, 'ünï' || :author"), ("SELECT arr[1:author], :authoré, 'ünï' || $9".into(), 0));
        assert_eq!(rewrite("SELECT 'unterminated :author"), ("SELECT 'unterminated :author".into(), 0));
        assert_eq!(rewrite("SELECT $10 FROM t$1"), ("SELECT $10 FROM t$1".into(), 10));
    }
}

/// Do the stored `kb.sync_file` rows for `id` already say exactly what `want` says?
fn same_sync_files(tx: &mut postgres::Transaction<'_>, id: i64, want: &[BaseFile]) -> Result<bool> {
    type Row = (Option<i64>, String, Option<i64>, Option<i64>, bool);
    let rows = tx
        .query(
            "SELECT rel, version, blob, disk_size, disk_mtime, conflict FROM kb.sync_file WHERE sync_id = $1",
            &[&id],
        )
        .map_err(pg)?;
    if rows.len() != want.len() {
        return Ok(false);
    }
    let have: std::collections::HashMap<String, Row> = rows
        .iter()
        .map(|r| (r.get(0), (r.get(1), r.get(2), r.get(3), r.get(4), r.get(5))))
        .collect();
    Ok(want
        .iter()
        .all(|f| have.get(&f.rel) == Some(&(f.version, f.blob.clone(), f.disk_size, f.disk_mtime, f.conflict))))
}
