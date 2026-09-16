//! Sections: listing a document's headings, and asking across a folder or a whole vault.
//!
//! The rows themselves are written by `db::write_structure` at commit. This module is the
//! reading side, plus the backfill that gives an existing store the columns a newer build
//! expects — `heading`, `heading_lc` and the two word counts — without waiting for every
//! document to be rewritten.

use crate::storage::sql_err;
use rusqlite::{params, Connection};
use textdb_core::storage::Result;

/// One heading, with enough of its document alongside to be useful on its own.
///
/// A heading list is almost always read next to the file it came from — an outline pane shows
/// when the note was last touched, a vault-wide search for "Next steps" wants to know which
/// notes are stale. Carrying the file columns here is what saves every caller a second query
/// per row.
#[derive(Clone, Debug)]
pub struct OutlineRow {
    pub path: String,
    pub heading: String,
    pub heading_path: String,
    pub level: i64,
    pub line_from: i64,
    pub line_to: i64,
    pub nwords: Option<i64>,
    pub nwords_total: Option<i64>,
    /// The document's own figures, repeated on each of its rows.
    pub file_nbytes: Option<i64>,
    pub file_nlines: Option<i64>,
    pub file_nwords: Option<i64>,
    pub file_version: i64,
    pub updated_at: String,
    pub updated_by: Option<String>,
}

/// How `outline` matches the `heading` argument it is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    /// The whole heading, folded. `Next Steps` finds `next steps`.
    Exact,
    /// A folded prefix — what an autosuggest box wants.
    Prefix,
    /// A folded substring. The one shape that cannot use the index.
    Contains,
}

impl Match {
    pub fn parse(s: &str) -> Match {
        match s {
            "prefix" => Match::Prefix,
            "contains" => Match::Contains,
            _ => Match::Exact,
        }
    }
}

/// Headings under `prefix`, optionally only those matching `heading`, at or above `max_level`.
///
/// `prefix` is a folder (or a single file's path); `/` means the whole vault. The rows come
/// back in document order within a document, and documents in path order, so an outline pane
/// can render them without sorting.
#[allow(clippy::too_many_arguments)]
pub fn outline(
    conn: &Connection,
    p: &str,
    prefix: &str,
    heading: Option<&str>,
    mode: Match,
    max_level: Option<i64>,
    limit: usize,
) -> Result<Vec<OutlineRow>> {
    let prefix = crate::normalize_path(prefix)?;
    // The same indexed range the rest of the binding uses: `path >= '/a/' AND path < '/a0'`
    // seeks `node_path`, where `like` or `substr` would scan it. The root has no bound, so
    // it takes the range that holds every path.
    let (lo, hi) = crate::db::subtree_bounds(&prefix).unwrap_or_else(|| ("/".into(), "0".into()));
    let mut sql = format!(
        "SELECT n.path, s.heading, s.heading_path, s.level, s.line_from, s.line_to, \
                s.nwords, s.nwords_total, n.nbytes, n.nlines, n.nwords, n.version, \
                n.updated_at, n.updated_by \
           FROM {p}section s JOIN {p}node n ON n.id = s.file_id \
          WHERE n.deleted_at IS NULL AND (n.path = ?1 OR (n.path >= ?2 AND n.path < ?3))"
    );
    let mut args: Vec<rusqlite::types::Value> = vec![prefix.clone().into(), lo.into(), hi.into()];
    if let Some(h) = heading {
        let folded = h.to_lowercase();
        // Only `contains` has to scan; the other two seek `section_heading`.
        match mode {
            Match::Exact => {
                sql.push_str(" AND s.heading_lc = ?4");
                args.push(folded.into());
            }
            Match::Prefix => {
                sql.push_str(" AND s.heading_lc >= ?4 AND s.heading_lc < ?5");
                let (hlo, hhi) = fold_range(h);
                args.push(hlo.into());
                args.push(hhi.into());
            }
            Match::Contains => {
                sql.push_str(" AND s.heading_lc LIKE ?4 ESCAPE '\\'");
                args.push(format!("%{}%", escape_like(&folded)).into());
            }
        }
    }
    if let Some(l) = max_level {
        sql.push_str(&format!(" AND s.level <= ?{}", args.len() + 1));
        args.push(l.into());
    }
    sql.push_str(&format!(" ORDER BY n.path, s.line_from LIMIT ?{}", args.len() + 1));
    args.push((limit as i64).into());

    let mut st = conn.prepare_cached(&sql).map_err(sql_err)?;
    let rows = st
        .query_map(rusqlite::params_from_iter(args), |r| {
            Ok(OutlineRow {
                path: r.get(0)?,
                heading: r.get(1)?,
                heading_path: r.get(2)?,
                level: r.get(3)?,
                line_from: r.get(4)?,
                line_to: r.get(5)?,
                nwords: r.get(6)?,
                nwords_total: r.get(7)?,
                file_nbytes: r.get(8)?,
                file_nlines: r.get(9)?,
                file_nwords: r.get(10)?,
                file_version: r.get(11)?,
                updated_at: r.get(12)?,
                updated_by: r.get(13)?,
            })
        })
        .map_err(sql_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
}

/// Every distinct heading under `prefix`, most-used first, for autosuggest.
pub fn heading_names(conn: &Connection, p: &str, prefix: &str, starts: &str, limit: usize) -> Result<Vec<(String, i64, i64)>> {
    let prefix = crate::normalize_path(prefix)?;
    let (lo, hi) = crate::db::subtree_bounds(&prefix).unwrap_or_else(|| ("/".into(), "0".into()));
    let (hlo, hhi) = fold_range(starts);
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT min(s.heading), count(*), count(DISTINCT s.file_id) \
               FROM {p}section s JOIN {p}node n ON n.id = s.file_id \
              WHERE n.deleted_at IS NULL AND (n.path = ?1 OR (n.path >= ?2 AND n.path < ?3)) \
                AND s.heading_lc >= ?4 AND s.heading_lc < ?5 \
              GROUP BY s.heading_lc ORDER BY count(*) DESC, s.heading_lc LIMIT ?6"
        ))
        .map_err(sql_err)?;
    let rows = st
        .query_map(params![prefix, lo, hi, hlo, hhi, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(sql_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// The folded half-open range `[s, successor(s))`, so a prefix match is an index seek.
///
/// An empty prefix means "every heading", which is the range that holds all of them.
fn fold_range(s: &str) -> (String, String) {
    let lo = s.to_lowercase();
    if lo.is_empty() {
        return (String::new(), "\u{10FFFF}".to_string());
    }
    let mut hi = lo.clone();
    while let Some(c) = hi.pop() {
        if let Some(next) = char::from_u32(c as u32 + 1) {
            hi.push(next);
            return (lo, hi);
        }
    }
    (lo, "\u{10FFFF}".to_string())
}

/// Fill `heading` and `heading_lc` for rows written by a build that had no such columns.
///
/// `heading_path` is a lossy join — a heading may itself contain `" / "`, and then splitting
/// on the last separator is simply wrong (`Top / Child / With Slash` is a *two*-level path
/// whose leaf is `Child / With Slash`, not `With Slash`). The components are still recoverable
/// exactly, though, without reading a single document: a section's parent always appears
/// before it in document order and its `heading_path` is exactly this one's prefix, so the
/// heading is what remains after the longest earlier path in the same file that this one
/// continues.
///
/// Word counts cannot be recovered this way — they need the bytes — and stay `NULL` until a
/// document is next written.
pub fn backfill(conn: &Connection, p: &str) -> Result<usize> {
    let pending: Vec<(i64, i64, String)> = {
        let mut st = conn
            .prepare(&format!(
                "SELECT rowid, file_id, heading_path FROM {p}section WHERE heading = '' ORDER BY file_id, line_from"
            ))
            .map_err(sql_err)?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?)))
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)?
    };
    if pending.is_empty() {
        return Ok(0);
    }
    let mut set = conn
        .prepare(&format!("UPDATE {p}section SET heading = ?1, heading_lc = ?2 WHERE rowid = ?3"))
        .map_err(sql_err)?;
    let mut file = i64::MIN;
    let mut seen: Vec<String> = Vec::new();
    for (rowid, file_id, path) in &pending {
        if *file_id != file {
            file = *file_id;
            seen.clear();
        }
        let heading = leaf_of(path, &seen);
        set.execute(params![heading, heading.to_lowercase(), rowid]).map_err(sql_err)?;
        seen.push(path.clone());
    }
    Ok(pending.len())
}

/// The last component of `path` given the paths of the sections before it in the same file.
///
/// The longest `ancestors` entry that `path` continues is its parent, so whatever follows it
/// is the heading — separator characters in the heading included.
fn leaf_of<'a>(path: &'a str, ancestors: &[String]) -> &'a str {
    ancestors
        .iter()
        .filter(|a| path.len() > a.len() + 3 && path.starts_with(a.as_str()) && path[a.len()..].starts_with(" / "))
        .max_by_key(|a| a.len())
        .map_or(path, |a| &path[a.len() + 3..])
}

#[cfg(test)]
mod tests {
    use super::leaf_of;

    #[test]
    fn a_heading_containing_the_separator_survives_the_round_trip() {
        let mut seen: Vec<String> = Vec::new();
        let cases = [
            ("Top", "Top"),
            // The leaf here is `Child / With Slash`: a two-level path, not a three-level one.
            ("Top / Child / With Slash", "Child / With Slash"),
            ("Top / Child / With Slash / Deep", "Deep"),
            // A sibling at the top level starts a new branch.
            ("Second", "Second"),
            ("Second / Leaf", "Leaf"),
        ];
        for (path, want) in cases {
            assert_eq!(leaf_of(path, &seen), want, "{path}");
            seen.push(path.to_string());
        }
    }

    #[test]
    fn a_path_that_merely_starts_like_another_is_not_its_child() {
        let seen = vec!["Top".to_string()];
        // `Topic` starts with `Top` but does not continue it.
        assert_eq!(leaf_of("Topic", &seen), "Topic");
        assert_eq!(leaf_of("Top / A", &seen), "A");
    }
}
