//! Sections on Postgres: the backfill that gives an existing store the columns a newer build
//! expects. The rows themselves are written by `write_structure` at commit, and the reading
//! side is `kb.outline()` and `kb.headings()` in `lib.rs`.

use pgrx::prelude::*;
use textdb_core::storage::Result;

use crate::store::map as storage_err;

/// Fill `heading` and `heading_lc` from the stored `heading_path`, returning the row count.
///
/// `heading_path` is a lossy join — a heading may itself contain `" / "`, and then taking the
/// text after the last separator is simply wrong (`Top / Child / With Slash` is a *two*-level
/// path whose leaf is `Child / With Slash`). The components are still recoverable exactly
/// without reading a document: a section's parent comes before it in document order and its
/// `heading_path` is this one's prefix, so the heading is what remains after the longest
/// earlier path in the same file that this one continues. Done in Rust for that reason — no
/// single SQL expression over one row can see the rows it needs.
///
/// Word counts need the document's bytes and are left `NULL` until it is next written.
pub fn backfill() -> Result<i64> {
    let pending: Vec<(i64, i64, String)> = Spi::connect(|client| {
        let rows = client
            .select(
                "SELECT ctid::text::bigint, file_id, heading_path FROM kb.section WHERE heading = '' ORDER BY file_id, line_from",
                None,
                &[],
            )
            .map_err(storage_err)?;
        let mut out = Vec::new();
        for r in rows {
            let file: Option<i64> = r.get(2).map_err(storage_err)?;
            let path: Option<String> = r.get(3).map_err(storage_err)?;
            if let (Some(file), Some(path)) = (file, path) {
                out.push((0i64, file, path));
            }
        }
        Ok::<_, textdb_core::TextdbError>(out)
    })?;
    if pending.is_empty() {
        return Ok(0);
    }
    // Updated by (file_id, heading_path), which identifies a section within its document: two
    // sections of one file cannot share a heading path, since a repeated heading under the
    // same parent would still differ by what the stack put above it.
    let mut file = i64::MIN;
    let mut seen: Vec<String> = Vec::new();
    let mut n = 0i64;
    for (_, file_id, path) in &pending {
        if *file_id != file {
            file = *file_id;
            seen.clear();
        }
        let heading = leaf_of(path, &seen);
        Spi::run_with_args(
            "UPDATE kb.section SET heading = $1, heading_lc = lower($1) WHERE file_id = $2 AND heading_path = $3 AND heading = ''",
            &[heading.into(), (*file_id).into(), path.as_str().into()],
        )
        .map_err(storage_err)?;
        seen.push(path.clone());
        n += 1;
    }
    Ok(n)
}

/// The last component of `path` given the paths of the sections before it in the same file.
///
/// The longest `ancestors` entry that `path` continues is its parent, so whatever follows it
/// is the heading — separator characters inside the heading included.
fn leaf_of<'a>(path: &'a str, ancestors: &[String]) -> &'a str {
    ancestors
        .iter()
        .filter(|a| path.len() > a.len() + 3 && path.starts_with(a.as_str()) && path[a.len()..].starts_with(" / "))
        .max_by_key(|a| a.len())
        .map_or(path, |a| &path[a.len() + 3..])
}
