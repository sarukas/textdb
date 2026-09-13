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
