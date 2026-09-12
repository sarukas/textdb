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
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
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
pub fn du(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            let md = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() {
                total += du(&e.path());
            } else {
                total += md.len();
            }
        }
    }
    total
}
