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
    Ok(Some(match id {
        "fs" => Box::new(fs::FsBackend::new(&dir, mode)?),
        "fs-git" => Box::new(fs_git::FsGitBackend::new(&dir, mode)?),
        "sql-text-sqlite" => Box::new(sql_text_sqlite::SqlTextSqlite::new(&dir, mode)?),
        "textdb-sqlite" => Box::new(textdb_sqlite::TextdbSqlite::new(&dir, mode)?),
        "sql-text-pg" => match pg_url {
            Some(u) => Box::new(sql_text_pg::SqlTextPg::new(u, mode)?),
            None => return Ok(None),
        },
        "textdb-pg" => match pg_url {
            Some(u) => Box::new(textdb_pg::TextdbPg::new(u, mode)?),
            None => return Ok(None),
        },
        other => anyhow::bail!("unknown backend {}", other),
    }))
}

pub const ALL: &[&str] = &["fs", "fs-git", "sql-text-sqlite", "sql-text-pg", "textdb-sqlite", "textdb-pg"];

/// Query syntax helpers shared by the baselines.
pub fn query_terms(q: &str) -> Vec<String> {
    ::textdb_sqlite::db::query_terms(q)
}

/// Line (1-based) of the first occurrence of any query term in `body`, for rg-parity hits.
pub fn first_hit_line(body: &[u8], terms: &[String]) -> u64 {
    let text = String::from_utf8_lossy(body).to_lowercase();
    let mut best: Option<usize> = None;
    for t in terms {
        let t = t.trim_end_matches('*').to_lowercase();
        if let Some(p) = text.find(&t) {
            best = Some(best.map_or(p, |b| b.min(p)));
        }
    }
    text[..best.unwrap_or(0)].matches('\n').count() as u64 + 1
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
pub fn run_sync(store: &str, prefix: &str, dir: &std::path::Path) -> crate::backend::R<crate::backend::SyncStats> {
    use crate::backend::{BackendError, SyncStats};
    let bin = std::env::var("TEXTDB_BIN").unwrap_or_else(|_| "target/release/textdb".to_string());
    if !std::path::Path::new(&bin).exists() {
        return Err(BackendError::NotSupported("textdb binary not built; set TEXTDB_BIN"));
    }
    let out = std::process::Command::new(&bin)
        .args(["-s", store, "sync", prefix])
        .arg(dir)
        .args(["--json", "--author", "bench"])
        .output()?;
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
