//! `textdb-pg`: the algorithm under test on Postgres, driven through the extension's
//! view/trigger/function surface (spec §7.2) — never direct table access.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use postgres::{Client, NoTls};

use crate::backend::*;
use crate::backends::delegate;
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
    /// The bearer every connection authenticates with, for the `@account` twin (#14 item 2).
    /// `None` is the owner, and then nothing on any path below this costs anything.
    bearer: Option<String>,
}

impl TextdbPg {
    pub fn new(url: &str, mode: Mode) -> anyhow::Result<Self> {
        Self::open_store(url, mode, false)
    }

    /// The same store, opened by an account that holds [`delegate::SHARE`] as its whole
    /// namespace. See `delegate` for why that shape and what it does not measure — in
    /// particular that the RLS policy on `kb.node` does not apply to the role the harness
    /// connects as, so this measures the `kb.*` surface and not the layer beneath it.
    pub fn as_account(url: &str, mode: Mode) -> anyhow::Result<Self> {
        Self::open_store(url, mode, true)
    }

    fn open_store(url: &str, mode: Mode, delegated: bool) -> anyhow::Result<Self> {
        let mut b = TextdbPg {
            id: INSTANCE.fetch_add(1, Ordering::Relaxed),
            url: url.to_string(),
            mode,
            base_written: Mutex::new(0),
            bearer: None,
        };
        b.with(|c| {
            c.batch_execute("DROP EXTENSION IF EXISTS textdb_pg CASCADE; DROP SCHEMA IF EXISTS kb CASCADE; CREATE EXTENSION textdb_pg;")?;
            Ok(())
        })?;
        if delegated {
            let bearer = b.with(grant_bench_account)?;
            b.bearer = Some(bearer);
            // The connection this thread cached during setup is the owner's, and `open` only
            // authenticates when it builds one. Dropping it here is what makes the very next
            // call — on this thread as much as any other — arrive as the account.
            close_conn(b.id);
        }
        Ok(b)
    }

    fn open(&self) -> Result<Client, postgres::Error> {
        let mut c = Client::connect(&self.url, NoTls)?;
        let sc = if self.mode == Mode::Durable { "on" } else { "off" };
        c.batch_execute(&format!("SET synchronous_commit = {};", sc))?;
        if let Some(bearer) = &self.bearer {
            // One call, the way a SQL client does it: from here every `kb.*` view and function
            // answers in this account's paths and sees only its share.
            c.query_one("SELECT kb.auth($1)", &[&bearer.as_str()])?;
        }
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
        if self.bearer.is_some() {
            "textdb-pg@account"
        } else {
            "textdb-pg"
        }
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
                        // The path may be mid-rename (CW-06): retry the lookup briefly.
                        let mut mine: Option<i64> = None;
                        for attempt in 0..10 {
                            match c.query_one("SELECT max(version) FROM kb.history($1) WHERE author = $2", &[&path, &tag]) {
                                Ok(row) => {
                                    mine = row.get::<_, Option<i64>>(0);
                                    break;
                                }
                                Err(e) if attempt < 9 && e.as_db_error().map(|d| d.code().code()) == Some("TX003") => {
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
            let rows = c.query("SELECT path, line, text FROM kb.search($1, $2, $3, 1000000)", &[&query, &prefix, &100_000i64])?;
            Ok(rows
                .iter()
                .map(|r| Hit {
                    path: r.get(0),
                    line: r.get::<_, i64>(1) as u64,
                    snippet: r.get(2),
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
    // structure sidecar -------------------------------------------------------------
    // The same sidecar tables as the SQLite binding, in the `kb` schema. `kb._subtree_like`
    // escapes `%` and `_` in the prefix, so a folder named `100%_done` selects itself and
    // not `100XXXdone`.

    fn links(&self, prefix: &str) -> R<Vec<LinkRow>> {
        // Read from the sidecar tables, which speak store paths and carry unprojected link
        // targets: there is no account-side answer to give. See `delegate::NO_SIDECAR_VIEW`.
        if self.bearer.is_some() {
            return Err(BackendError::NotSupported(delegate::NO_SIDECAR_VIEW));
        }
        self.with(|c| {
            let rows = c.query(
                "SELECT n.path, l.target_path, l.line, l.status, r.path
                   FROM kb.link l
                   JOIN kb.node n ON n.id = l.file_id AND n.deleted_at IS NULL
                   LEFT JOIN kb.node r ON r.id = l.resolved_id AND r.deleted_at IS NULL
                  WHERE n.path = $1 OR n.path LIKE kb._subtree_like($1) ESCAPE '\'
                  ORDER BY n.path, l.line",
                &[&prefix],
            )?;
            Ok(rows.iter().map(pg_link_row).collect())
        })
    }
    fn backlinks(&self, path: &str) -> R<Vec<LinkRow>> {
        // Read from the sidecar tables, which speak store paths and carry unprojected link
        // targets: there is no account-side answer to give. See `delegate::NO_SIDECAR_VIEW`.
        if self.bearer.is_some() {
            return Err(BackendError::NotSupported(delegate::NO_SIDECAR_VIEW));
        }
        self.with(|c| {
            let rows = c.query(
                "SELECT n.path, l.target_path, l.line, l.status, r.path
                   FROM kb.link l
                   JOIN kb.node r ON r.id = l.resolved_id AND r.deleted_at IS NULL AND r.path = $1
                   JOIN kb.node n ON n.id = l.file_id AND n.deleted_at IS NULL
                  ORDER BY n.path, l.line",
                &[&path],
            )?;
            Ok(rows.iter().map(pg_link_row).collect())
        })
    }
    fn frontmatter(&self, path: &str) -> R<Option<String>> {
        // Read from the sidecar tables, which speak store paths and carry unprojected link
        // targets: there is no account-side answer to give. See `delegate::NO_SIDECAR_VIEW`.
        if self.bearer.is_some() {
            return Err(BackendError::NotSupported(delegate::NO_SIDECAR_VIEW));
        }
        self.with(|c| {
            let rows = c.query(
                // `data` is jsonb here and TEXT in the SQLite binding; ::text gives both
                // bindings the same shape for the suite to check.
                "SELECT f.data::text FROM kb.frontmatter f
                   JOIN kb.node n ON n.id = f.file_id AND n.deleted_at IS NULL AND n.path = $1",
                &[&path],
            )?;
            Ok(rows.first().and_then(|r| r.get::<_, Option<String>>(0)))
        })
    }
    fn set_meta(&self, path: &str, key: &str, value: &str) -> R<Version> {
        let (body, _) = self.read_versioned(path)?;
        let next = super::textdb_sqlite::set_frontmatter_key(&body, key, value);
        self.overwrite(path, &next)
    }
    fn settle(&self) -> R<()> {
        self.with(|c| {
            c.execute("SELECT kb.analyze_store()", &[])?;
            Ok(())
        })
    }
    fn outline(&self, prefix: &str, heading: Option<&str>, mode: &str, max_level: Option<u32>) -> R<Vec<OutlineRow>> {
        self.with(|c| {
            let lvl = max_level.map(|l| l as i64);
            let rows = c.query(
                "SELECT path, heading, level, line_from, nwords, nwords_total, nbytes
                   FROM kb.outline($1, $2, $3, $4, 1000000)",
                &[&prefix, &heading, &mode, &lvl],
            )?;
            Ok(rows
                .iter()
                .map(|r| OutlineRow {
                    path: r.get(0),
                    heading: r.get(1),
                    level: r.get::<_, i64>(2) as u32,
                    line_from: r.get::<_, i64>(3) as u64,
                    nwords: r.get::<_, Option<i64>>(4).map(|v| v as u64),
                    nwords_total: r.get::<_, Option<i64>>(5).map(|v| v as u64),
                    file_nbytes: r.get::<_, Option<i64>>(6).map(|v| v as u64),
                })
                .collect())
        })
    }
    fn heading_names(&self, prefix: &str, starts: &str) -> R<Vec<(String, u64, u64)>> {
        self.with(|c| {
            let rows = c.query("SELECT heading, sections, docs FROM kb.headings($1, $2, 100000)", &[&prefix, &starts])?;
            Ok(rows
                .iter()
                .map(|r| (r.get(0), r.get::<_, i64>(1) as u64, r.get::<_, i64>(2) as u64))
                .collect())
        })
    }
    fn sections(&self, path: &str) -> R<Vec<SectionRow>> {
        // Read from the sidecar tables, which speak store paths and carry unprojected link
        // targets: there is no account-side answer to give. See `delegate::NO_SIDECAR_VIEW`.
        if self.bearer.is_some() {
            return Err(BackendError::NotSupported(delegate::NO_SIDECAR_VIEW));
        }
        self.with(|c| {
            let rows = c.query(
                "SELECT s.heading_path, s.level, s.line_from, s.line_to
                   FROM kb.section s
                   JOIN kb.node n ON n.id = s.file_id AND n.deleted_at IS NULL AND n.path = $1
                  ORDER BY s.line_from",
                &[&path],
            )?;
            Ok(rows
                .iter()
                .map(|r| SectionRow {
                    heading: r.get(0),
                    level: r.get::<_, i32>(1) as u64,
                    line_from: r.get::<_, i64>(2) as u64,
                    line_to: r.get::<_, i64>(3) as u64,
                })
                .collect())
        })
    }
    fn section(&self, path: &str, heading: &str) -> R<Option<Vec<u8>>> {
        self.with(|c| {
            let rows = c.query("SELECT kb.section($1, $2)", &[&path, &heading])?;
            Ok(rows.first().and_then(|r| r.get::<_, Option<String>>(0)).map(|s| s.into_bytes()))
        })
    }
    fn set_link_mode(&self, mode: &str) -> R<()> {
        self.with(|c| {
            c.execute("SELECT kb.set_setting('link_updates', $1)", &[&mode])?;
            Ok(())
        })
    }
    fn property_keys(&self, prefix: &str) -> R<Vec<(String, u64)>> {
        self.with(|c| {
            let rows = c.query("SELECT key, docs FROM kb.prop_keys($1, 10000)", &[&prefix])?;
            Ok(rows.iter().map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as u64)).collect())
        })
    }
    fn property_values(&self, key: &str, prefix: &str) -> R<Vec<(String, u64)>> {
        self.with(|c| {
            let rows = c.query("SELECT value, docs FROM kb.prop_values($1, $2, 10000)", &[&key, &prefix])?;
            Ok(rows
                .iter()
                .map(|r| (r.get::<_, Option<String>>(0).unwrap_or_default(), r.get::<_, i64>(1) as u64))
                .collect())
        })
    }
    fn property_find(&self, query: &str) -> R<Vec<String>> {
        self.with(|c| {
            let rows = c.query("SELECT path FROM kb.prop_find($1, '/', 1000000)", &[&query])?;
            Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
        })
    }
    fn sync_dir(&self, prefix: &str, dir: &std::path::Path) -> R<crate::backend::SyncStats> {
        // `sync` is the one operation that lives in the CLI rather than the SQL surface, so the
        // bearer reaches it the way a deployment sends one: in the environment.
        super::run_sync(&self.url, prefix, dir, self.bearer.as_deref())
    }
    fn changes_since(&self, seq: u64) -> R<(u64, u64)> {
        self.with(|c| {
            let rows = c.query("SELECT seq FROM kb.feed($1)", &[&(seq as i64)])?;
            let mut last = seq;
            for r in &rows {
                last = last.max(r.get::<_, i64>(0) as u64);
            }
            Ok((last, rows.len() as u64))
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
            crate::backends::run_each(
                c,
                &[
                    "VACUUM FULL kb.chunk",
                    "VACUUM FULL kb.tree_node",
                    "VACUUM FULL kb.node",
                    "SELECT gin_clean_pending_list('kb.chunk_tsv')",
                    // Without this the planner keeps whatever autovacuum worked out while the
                    // store was still nearly empty, which is what a bulk-loaded store has.
                    "SELECT kb.analyze_store()",
                ],
            )?;
            Ok("VACUUM FULL + gin_clean_pending_list + ANALYZE (GC stub: none)")
        })
    }
    fn leaf_hashes(&self, path: &str) -> R<Option<std::collections::HashSet<textdb_core::Hash>>> {
        self.with(|c| {
            let mut out = std::collections::HashSet::new();
            for row in c.query("SELECT hash FROM kb.leaf_hashes($1)", &[&path])? {
                let h: Vec<u8> = row.get(0);
                let Ok(h) = <[u8; 32]>::try_from(h.as_slice()) else {
                    return Err(BackendError::Other("kb.leaf_hashes returned a non-32-byte hash".into()));
                };
                out.insert(h);
            }
            Ok(Some(out))
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

/// Create the share folder, the account whose root it is, and a bearer for it. Returns the
/// bearer.
///
/// Through `kb.*` rather than the Rust API, because Postgres has the admin surface in SQL and
/// that is the path a deployment of this engine takes. (SQLite has no SQL surface for accounts,
/// so its half calls the same functions the CLI does.) Untimed either way: this runs once,
/// before the suite, and nothing here is measured.
fn grant_bench_account(c: &mut Client) -> R<String> {
    c.execute("INSERT INTO kb.folder (path) VALUES ($1)", &[&delegate::SHARE])
        .map_err(|e| delegate::setup_err("share folder", e))?;
    // `--root` makes it single-root and writes the grant in the same call, under the empty
    // alias: the account's root *is* that folder, so its paths are the suite's paths.
    c.query_one("SELECT kb.account_create($1, 'agent', $2)", &[&delegate::ACCOUNT, &delegate::SHARE])
        .map_err(|e| delegate::setup_err("account", e))?;
    let row = c
        .query_one("SELECT bearer FROM kb.token_create($1, 'bench')", &[&delegate::ACCOUNT])
        .map_err(|e| delegate::setup_err("token", e))?;
    Ok(row.get::<_, String>(0))
}

fn pg_link_row(r: &postgres::Row) -> LinkRow {
    LinkRow {
        path: r.get(0),
        target: r.get(1),
        line: r.get::<_, i64>(2) as u64,
        status: r.get::<_, Option<String>>(3).unwrap_or_default(),
        resolved: r.get(4),
    }
}
