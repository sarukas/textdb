//! Links in Postgres, as in the SQLite binding (`textdb-sqlite/src/links.rs`): rows written with
//! their parts, resolved by the rules both stores share (`textdb_md::resolve`), and resolved again
//! when a file of that name is created, moved or deleted. A move can report or rewrite the links
//! that pointed at what moved.

use std::collections::{BTreeMap, HashSet};

use pgrx::prelude::*;
use textdb_core::tree::materialize;
use textdb_core::{Edit, Link, TextdbError};
use textdb_md::resolve::{line_of_offset, name_key, resolve, rewritten_target, LinkUpdates, Lookup, LINK_UPDATES_SETTING};

use crate::store::{to_hash, SpiStorage};

type Result<T> = std::result::Result<T, TextdbError>;

fn err(e: pgrx::spi::Error) -> TextdbError {
    TextdbError::Storage(e.to_string())
}

/// `(id, path)` rows of a query taking one text argument.
fn id_paths(sql: &str, arg: &str) -> Result<Vec<(i64, String)>> {
    Spi::connect(|client| {
        let rows = client.select(sql, None, &[arg.into()]).map_err(err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push((r.get::<i64>(1).map_err(err)?.unwrap_or(0), r.get::<String>(2).map_err(err)?.unwrap_or_default()));
        }
        Ok(out)
    })
}

/// The store's answers for [`resolve`].
pub struct PgLookup;

impl Lookup for PgLookup {
    fn files_by_path(&self, path: &str) -> Result<Vec<(i64, String)>> {
        id_paths("SELECT id, path FROM kb.node WHERE kind = 1 AND deleted_at IS NULL AND lower(path) = lower($1)", path)
    }

    fn files_by_name(&self, name: &str) -> Result<Vec<(i64, String)>> {
        id_paths("SELECT id, path FROM kb.node WHERE kind = 1 AND deleted_at IS NULL AND lower(name) = lower($1)", name)
    }

    fn files_by_suffix(&self, suffix: &str) -> Result<Vec<(i64, String)>> {
        id_paths("SELECT id, path FROM kb.node WHERE kind = 1 AND deleted_at IS NULL AND right(lower(path), length($1)) = $1", suffix)
    }

    fn is_folder(&self, path: &str) -> Result<bool> {
        Ok(Spi::get_one_with_args::<bool>(
            "SELECT count(*) > 0 FROM kb.node WHERE kind = 0 AND deleted_at IS NULL AND lower(path) = lower($1)",
            &[path.into()],
        )
        .map_err(err)?
        .unwrap_or(false))
    }

    fn headings(&self, file_id: i64) -> Result<Vec<String>> {
        Spi::connect(|client| {
            let rows = client.select("SELECT heading_path FROM kb.section WHERE file_id = $1", None, &[file_id.into()]).map_err(err)?;
            let mut out = Vec::new();
            for r in rows {
                if let Some(h) = r.get::<String>(1).map_err(err)? {
                    out.push(h);
                }
            }
            Ok(out)
        })
    }
}

/// Replace the link rows of a file; resolve them with [`relink_file`].
pub fn write_rows(file_id: i64, version: i64, links: &[Link]) -> Result<()> {
    Spi::run_with_args("DELETE FROM kb.link WHERE file_id = $1", &[file_id.into()]).map_err(err)?;
    for l in links {
        let key = (!l.external && !l.target_path.is_empty()).then(|| name_key(&l.target_path));
        Spi::run_with_args(
            "INSERT INTO kb.link(file_id, version, target_path, line, kind, anchor, alias, external, target_name) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                file_id.into(),
                version.into(),
                l.target_path.as_str().into(),
                (l.line as i64).into(),
                l.kind.as_str().into(),
                l.anchor.as_deref().into(),
                l.alias.as_deref().into(),
                l.external.into(),
                key.as_deref().into(),
            ],
        )
        .map_err(err)?;
    }
    Ok(())
}

/// Resolve again the links written in live files that `cond` selects (over `l`, the link row;
/// `$1` is `arg` as text when given).
fn relink_where(cond: &str, arg: Option<&str>) -> Result<()> {
    type Row = (i64, i64, String, String, String, Option<String>, bool);
    let sql = format!(
        "SELECT l.id, l.file_id, n.path, coalesce(l.kind, ''), l.target_path, l.anchor, l.external \
         FROM kb.link l JOIN kb.node n ON n.id = l.file_id AND n.deleted_at IS NULL WHERE ({cond})"
    );
    let rows: Vec<Row> = Spi::connect(|client| {
        let t = match arg {
            Some(a) => client.select(&sql, None, &[a.into()]),
            None => client.select(&sql, None, &[]),
        }
        .map_err(err)?;
        let mut out = Vec::new();
        for r in t {
            out.push((
                r.get::<i64>(1).map_err(err)?.unwrap_or(0),
                r.get::<i64>(2).map_err(err)?.unwrap_or(0),
                r.get::<String>(3).map_err(err)?.unwrap_or_default(),
                r.get::<String>(4).map_err(err)?.unwrap_or_default(),
                r.get::<String>(5).map_err(err)?.unwrap_or_default(),
                r.get::<String>(6).map_err(err)?,
                r.get::<bool>(7).map_err(err)?.unwrap_or(false),
            ));
        }
        Ok::<_, TextdbError>(out)
    })?;
    // A link breaking is the one structure count that moves without a commit, so the files it
    // touched are collected and their totals refreshed once at the end.
    let mut touched: std::collections::BTreeMap<i64, String> = Default::default();
    for (id, file_id, path, kind, target, anchor, external) in rows {
        let (to, status) = resolve(&PgLookup, file_id, &path, &kind, &target, anchor.as_deref(), external)?;
        // Only rows whose outcome changed are written: a commit then locks no other file's rows
        // it leaves as they were, so writers to files that link to each other do not deadlock.
        let changed = Spi::get_one_with_args::<i64>(
            "WITH u AS (UPDATE kb.link SET resolved_id = $1, status = $2 \
                         WHERE id = $3 AND (resolved_id IS DISTINCT FROM $1 OR status IS DISTINCT FROM $2) \
                         RETURNING 1) \
             SELECT count(*) FROM u",
            &[to.into(), status.into(), id.into()],
        )
        .map_err(err)?
        .unwrap_or(0);
        if changed > 0 {
            touched.insert(file_id, path);
        }
    }
    for (file_id, path) in touched {
        crate::kb::refresh_broken_links(file_id, &path)?;
    }
    Ok(())
}

/// `col` is one of the ids in the JSON array `$1`, in a form the planner serves from an index.
fn in_ids(col: &str) -> String {
    format!("{col} = ANY(ARRAY(SELECT jsonb_array_elements_text($1::jsonb)::bigint))")
}

/// After a file's structure rows changed: its own links, and links to its headings.
pub fn relink_file(file_id: i64) -> Result<()> {
    relink_where(
        "l.file_id = $1::bigint OR (l.resolved_id = $1::bigint AND l.anchor IS NOT NULL)",
        Some(&file_id.to_string()),
    )
}

/// Every link that could point to a file named one of `names`, or points to or is written in one
/// of the files `ids`.
pub fn relink(names: &[String], ids: &[i64]) -> Result<()> {
    if !names.is_empty() {
        let list = serde_json::to_string(names).expect("names serialize");
        relink_where("l.target_name = ANY(ARRAY(SELECT jsonb_array_elements_text($1::jsonb)))", Some(&list))?;
    }
    if !ids.is_empty() {
        let list = serde_json::to_string(ids).expect("ids serialize");
        relink_where(&format!("{} OR {}", in_ids("l.resolved_id"), in_ids("l.file_id")), Some(&list))?;
    }
    Ok(())
}

/// The name keys of `files`' paths, without repeats.
pub fn names_of(files: &[(i64, String)]) -> Vec<String> {
    let mut names: Vec<String> = files.iter().map(|(_, p)| name_key(p)).collect();
    names.sort();
    names.dedup();
    names
}

/// The live files at or below `path`, as `(id, path)`.
pub fn files_at(path: &str) -> Result<Vec<(i64, String)>> {
    id_paths(
        "SELECT id, path FROM kb.node WHERE kind = 1 AND deleted_at IS NULL AND (path = $1 OR path LIKE kb._subtree_like($1))",
        path,
    )
}

/// What a move does to links: `choice` (`off`, `report`, `rewrite`) when given, else the store's
/// `link_updates` setting, else report.
pub fn mode(choice: Option<&str>) -> Result<LinkUpdates> {
    if let Some(c) = choice {
        return LinkUpdates::parse(c).ok_or_else(|| TextdbError::InvalidEdit(format!("link updates are off, report or rewrite, not '{c}'")));
    }
    let stored = Spi::get_one_with_args::<String>("SELECT kb.setting($1)", &[LINK_UPDATES_SETTING.into()]).map_err(err)?;
    Ok(stored.as_deref().and_then(LinkUpdates::parse).unwrap_or(LinkUpdates::DEFAULT))
}

/// A resolved link captured before a move.
pub struct Pointing {
    id: i64,
    file_id: i64,
    line: i64,
    kind: String,
    target: String,
    resolved_id: i64,
}

/// Resolved links to the files `moved` or written in them, before they move.
pub fn links_into(moved: &[(i64, String)]) -> Result<Vec<Pointing>> {
    if moved.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(&moved.iter().map(|(id, _)| *id).collect::<Vec<_>>()).expect("ids serialize");
    Spi::connect(|client| {
        let t = client
            .select(
                &format!(
                    "SELECT l.id, l.file_id, l.line, coalesce(l.kind, ''), l.target_path, l.resolved_id \
                     FROM kb.link l JOIN kb.node n ON n.id = l.file_id AND n.deleted_at IS NULL \
                     WHERE l.resolved_id IS NOT NULL AND l.target_path <> '' AND ({} OR {}) \
                     ORDER BY l.id",
                    in_ids("l.resolved_id"),
                    in_ids("l.file_id")
                ),
                None,
                &[ids.as_str().into()],
            )
            .map_err(err)?;
        let mut out = Vec::new();
        for r in t {
            out.push(Pointing {
                id: r.get::<i64>(1).map_err(err)?.unwrap_or(0),
                file_id: r.get::<i64>(2).map_err(err)?.unwrap_or(0),
                line: r.get::<i64>(3).map_err(err)?.unwrap_or(0),
                kind: r.get::<String>(4).map_err(err)?.unwrap_or_default(),
                target: r.get::<String>(5).map_err(err)?.unwrap_or_default(),
                resolved_id: r.get::<i64>(6).map_err(err)?.unwrap_or(0),
            });
        }
        Ok(out)
    })
}

/// A live node's path and root.
fn live_path(id: i64) -> Result<Option<(String, Option<Vec<u8>>)>> {
    Spi::connect(|client| {
        let t = client.select("SELECT path, root FROM kb.node WHERE id = $1 AND deleted_at IS NULL", Some(1), &[id.into()]).map_err(err)?;
        let mut out = None;
        for r in t {
            out = Some((r.get::<String>(1).map_err(err)?.unwrap_or_default(), r.get::<Vec<u8>>(2).map_err(err)?));
        }
        Ok(out)
    })
}

/// After a move from `from` to `to`: the links in `pointing` that no longer reach the file they
/// did, as JSON `{path, line, kind, target, now_at, version}`, rewritten when `mode` says so.
/// `commit(path, edits, message)` commits edits to a file and returns its new version.
pub fn follow_move(
    pointing: Vec<Pointing>,
    from: &str,
    to: &str,
    mode: LinkUpdates,
    mut commit: impl FnMut(&str, &[Edit], &str) -> Result<i64>,
) -> Result<Vec<serde_json::Value>> {
    let mut by_file: BTreeMap<i64, Vec<Pointing>> = BTreeMap::new();
    for p in pointing {
        let now = Spi::get_one_with_args::<i64>("SELECT (SELECT resolved_id FROM kb.link WHERE id = $1)", &[p.id.into()]).map_err(err)?;
        if now != Some(p.resolved_id) {
            by_file.entry(p.file_id).or_default().push(p);
        }
    }
    let message = format!("links: {from} -> {to}");
    let mut changes = Vec::new();
    for (file_id, links) in by_file {
        let Some((source, root)) = live_path(file_id)? else {
            continue;
        };
        let mut rewritten = HashSet::new();
        let mut version = None;
        if let (LinkUpdates::Rewrite, Some(root)) = (mode, root) {
            let doc = materialize(&SpiStorage::new(), &to_hash(&root)?)?;
            let line_of = line_of_offset(&doc);
            let mut edits = Vec::new();
            let mut paired = HashSet::new();
            for span in textdb_md::links::scan(&doc) {
                let line = line_of(span.offset);
                let Some(range) = span.range.clone() else { continue };
                // Rows are in document order, as the spans are: pair each span with the first row
                // of its line, kind and target not already paired.
                let Some(p) = links.iter().find(|p| p.line == line && p.kind == span.kind && p.target == span.target && !paired.contains(&p.id)) else {
                    continue;
                };
                paired.insert(p.id);
                let Some((target, _)) = live_path(p.resolved_id)? else { continue };
                let raw = String::from_utf8_lossy(&doc[range.clone()]).to_string();
                let new = rewritten_target(&PgLookup, span.kind, &raw, span.angle, &source, &target)?;
                if new != raw {
                    edits.push(Edit::new(range.start as u64, range.end as u64, new.into_bytes()));
                    rewritten.insert(p.id);
                }
            }
            if !edits.is_empty() {
                version = Some(commit(&source, &edits, &message)?);
            }
        }
        for p in &links {
            let now_at = live_path(p.resolved_id)?.map(|(path, _)| path).unwrap_or_default();
            changes.push(serde_json::json!({
                "path": source,
                "line": p.line,
                "kind": p.kind,
                "target": p.target,
                "now_at": now_at,
                "version": if rewritten.contains(&p.id) { version } else { None },
            }));
        }
    }
    Ok(changes)
}
