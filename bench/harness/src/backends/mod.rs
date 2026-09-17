pub mod delegate;
pub mod fs;
pub mod fs_git;
pub mod sql_text_pg;
pub mod sql_text_sqlite;
pub mod textdb_pg;
pub mod textdb_sqlite;

use crate::backend::{Backend, Mode};
use std::path::Path;

/// Construct a fresh, empty backend instance under `work` for the given id.
pub fn make(id: &str, work: &Path, mode: Mode, pg_url: Option<&str>) -> anyhow::Result<Option<Box<dyn Backend>>> {
    let dir = work.join(id);
    // A failed reset must never be silent: it would leave the previous rep's data in place
    // and every measurement after it would be against the wrong state.
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            anyhow::bail!("could not reset {}: {}", dir.display(), e);
        }
    }
    std::fs::create_dir_all(&dir)?;
    if std::fs::read_dir(&dir)?.next().is_some() {
        anyhow::bail!("{} is not empty after reset", dir.display());
    }
    // `textdb-pg@account` is the same engine holding a bearer (#14 item 2, `delegate`). Only the
    // two textdb backends have accounts at all; asking a baseline for one is a mistake worth
    // naming rather than a silent fall back to the owner, which would publish the owner's
    // numbers in the account's column.
    let (engine, delegated) = delegate::split(id);
    Ok(Some(match (engine, delegated) {
        ("fs", false) => Box::new(fs::FsBackend::new(&dir, mode)?),
        ("fs-git", false) => Box::new(fs_git::FsGitBackend::new(&dir, mode)?),
        ("sql-text-sqlite", false) => Box::new(sql_text_sqlite::SqlTextSqlite::new(&dir, mode)?),
        ("textdb-sqlite", false) => Box::new(textdb_sqlite::TextdbSqlite::new(&dir, mode)?),
        ("textdb-sqlite", true) => Box::new(textdb_sqlite::TextdbSqlite::as_account(&dir, mode)?),
        ("sql-text-pg", false) => match pg_url {
            Some(u) => Box::new(sql_text_pg::SqlTextPg::new(u, mode)?),
            None => return Ok(None),
        },
        ("textdb-pg", false) => match pg_url {
            Some(u) => Box::new(textdb_pg::TextdbPg::new(u, mode)?),
            None => return Ok(None),
        },
        ("textdb-pg", true) => match pg_url {
            Some(u) => Box::new(textdb_pg::TextdbPg::as_account(u, mode)?),
            None => return Ok(None),
        },
        (other, true) => anyhow::bail!("{} has no accounts; only the textdb backends can run delegated", other),
        (other, false) => anyhow::bail!("unknown backend {}", other),
    }))
}

pub const ALL: &[&str] = &["fs", "fs-git", "sql-text-sqlite", "sql-text-pg", "textdb-sqlite", "textdb-pg"];

/// The backends that can run as a delegated account, in the order `ALL` has them.
pub const DELEGABLE: &[&str] = &["textdb-sqlite", "textdb-pg"];

/// Add a delegated twin next to every backend in `list` that has one.
///
/// The twin sits **immediately after** its owner so the two run back to back on the same host
/// within each test, which is the whole point: a delegated run captured as a separate invocation
/// would be comparing two moments of this container, and the reference backends put that drift at
/// up to 10x on a single cell. A twin already named explicitly is not doubled.
pub fn with_accounts(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(list.len() * 2);
    for b in list {
        out.push(b.clone());
        let twin = format!("{b}{}", delegate::SUFFIX);
        if DELEGABLE.contains(&b.as_str()) && !list.contains(&twin) && !out.contains(&twin) {
            out.push(twin);
        }
    }
    out
}

/// Query syntax helpers shared by the baselines.
pub fn query_terms(q: &str) -> Vec<String> {
    ::textdb_sqlite::db::query_terms(q)
}

/// Line (1-based) of the first occurrence of any query term in `body`, for rg-parity hits.
pub fn first_hit_line(body: &[u8], terms: &[String]) -> u64 {
    // Folded, not merely lowercased, for the same reason the store folds: the corpus is full
    // of diacritics and the index strips them, so a raw comparison would put the baselines'
    // hits on the wrong line and make them look wrong against a store that is right.
    let text = textdb_core::fold::fold(&String::from_utf8_lossy(body));
    let mut best: Option<usize> = None;
    // A quoted phrase arrives as one term with its spaces intact, so it is matched word by
    // word: the words are adjacent in the query but the text between them in a document may
    // be punctuation, and looking for the phrase literally would find nothing.
    for t in terms.iter().flat_map(|t| t.split_whitespace()) {
        let t = textdb_core::fold::fold(t.trim_end_matches('*'));
        if let Some(p) = text.find(&t) {
            best = Some(best.map_or(p, |b| b.min(p)));
        }
    }
    text[..best.unwrap_or(0)].matches('\n').count() as u64 + 1
}

/// Line (1-based) and the whole matching line, for the backends that have no snippet of
/// their own and would otherwise be compared against nothing.
///
/// This is what `fs` shows a user: the line ripgrep printed. It is the fair baseline for a
/// store that builds a snippet — the oracle only asks that a snippet contain a term that was
/// searched for, which the matching line trivially does.
pub fn first_hit_line_and_text(body: &[u8], terms: &[String]) -> (u64, Option<String>) {
    let line = first_hit_line(body, terms);
    let text = String::from_utf8_lossy(body);
    let snippet = text.lines().nth(line.saturating_sub(1) as usize).map(|l| l.to_string());
    (line, snippet)
}

/// Read a TEXT or BLOB column as bytes (rusqlite refuses `Vec<u8>` for TEXT).
pub fn col_bytes(r: &rusqlite::Row, i: usize) -> rusqlite::Result<Vec<u8>> {
    Ok(match r.get_ref(i)? {
        rusqlite::types::ValueRef::Text(t) => t.to_vec(),
        rusqlite::types::ValueRef::Blob(b) => b.to_vec(),
        rusqlite::types::ValueRef::Null => Vec::new(),
        rusqlite::types::ValueRef::Integer(n) => n.to_string().into_bytes(),
        rusqlite::types::ValueRef::Real(f) => f.to_string().into_bytes(),
    })
}

/// Directory size in bytes (like `du -sb`).
/// Iterative: NS-02 nests folders 1000 deep, and one frame per level overflows the stack
/// — sooner on Windows, whose 1 MiB main stack is a fraction of Linux's 8 MiB.
pub fn du(path: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let md = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() {
                pending.push(e.path());
            } else {
                total += md.len();
            }
        }
    }
    total
}

/// Half-open `[lo, hi)` bounds selecting every path strictly under the folder `prefix`;
/// `None` for the root, which bounds nothing.
///
/// Both SQL backends used to spell a subtree test `substr(path, 1, length(?1) + 1) = ?1 || '/'`,
/// which is a function of the column and so defeats the unique index on `path` on either
/// side: every subtree listing, folder rename and folder delete scanned the whole table.
/// Measured at 58x a range over 2000 files. Giving both backends the range form keeps the
/// comparison fair — it is the query a competent implementation of either would write —
/// and stops the suite reporting a self-inflicted full scan as the cost of the operation.
/// `'0'` (0x30) is the byte after `'/'` (0x2F), so under SQLite's default BINARY collation
/// `prefix || '0'` is the exclusive end of the subtree and the range is exact.
pub fn subtree_bounds(prefix: &str) -> Option<(String, String)> {
    if prefix == "/" {
        return None;
    }
    Some((format!("{}/", prefix), format!("{}0", prefix)))
}

/// Run each statement on its own, outside any transaction block.
///
/// `batch_execute` sends everything as one simple-query batch, which PostgreSQL wraps in an
/// implicit transaction — and `VACUUM` refuses to run inside one ("25001: VACUUM cannot run
/// inside a transaction block"). Both Postgres backends hit this, so the maintenance step and
/// every footprint-after-maintenance figure for either of them was an error rather than a
/// measurement.
pub fn run_each(c: &mut postgres::Client, stmts: &[&str]) -> Result<(), postgres::Error> {
    for s in stmts {
        c.simple_query(s)?;
    }
    Ok(())
}

/// Run `textdb sync PREFIX DIR --json` against `store` and read its report.
///
/// Sync lives in the CLI, not in the SQL surface the backends otherwise drive, so this
/// shells out — the same way `fs-git` shells out to `git`. The binary is
/// `$TEXTDB_BIN`, else `target/release/textdb`; when it is not there the cell records N/A
/// with that reason rather than reporting a zero.
///
/// `bearer` is the delegated twin's token (`delegate`). It goes in the environment, which is
/// where the CLI's `--token` flag reads it from and where a deployment would put it, and it also
/// **drops `--author`**: a token session writes as its own account and refuses to be told
/// otherwise, which is the point of having one. Passing both got `TX005: --author says 'bench',
/// but a token session writes as its own account` and every delegated sync cell reported zero
/// files moved — a refusal the JSON report carries, so it read as a slow backend rather than a
/// rejected command.
pub fn run_sync(store: &str, prefix: &str, dir: &std::path::Path, bearer: Option<&str>) -> crate::backend::R<crate::backend::SyncStats> {
    use crate::backend::{BackendError, SyncStats};
    let bin = std::env::var("TEXTDB_BIN").unwrap_or_else(|_| "target/release/textdb".to_string());
    if !std::path::Path::new(&bin).exists() {
        return Err(BackendError::NotSupported("textdb binary not built; set TEXTDB_BIN"));
    }
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["-s", store, "sync", prefix]).arg(dir).arg("--json");
    match bearer {
        Some(b) => {
            cmd.env("TEXTDB_TOKEN", b);
        }
        None => {
            cmd.args(["--author", "bench"]);
        }
    }
    let out = cmd.output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|e| BackendError::Other(format!("sync report not JSON ({e}): {} {}", text, String::from_utf8_lossy(&out.stderr))))?;
    // `--json` reports failures in the document too, so a non-zero exit with a parsable
    // report is still read; only an unparsable one is an error.
    let n = |side: &str, key: &str| v.get(side).and_then(|s| s.get(key)).and_then(|a| a.as_array()).map(|a| a.len() as u64).unwrap_or(0);
    let len = |key: &str| v.get(key).and_then(|a| a.as_array()).map(|a| a.len() as u64).unwrap_or(0);
    Ok(SyncStats {
        to_store: n("to_textdb", "new") + n("to_textdb", "changed") + n("to_textdb", "deleted"),
        to_disk: n("to_disk", "new") + n("to_disk", "changed") + n("to_disk", "deleted"),
        merged: len("merged"),
        conflicted: len("conflicts"),
    })
}
