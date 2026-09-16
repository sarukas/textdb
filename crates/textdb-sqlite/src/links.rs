//! Links kept current in `{p}link`, resolved by the shared rules in [`textdb_md::resolve`].
//!
//! Each link row records the file it resolves to (`resolved_id`) and a `status`. Rows are
//! resolved when their file is committed, and again when a file they could point to is created,
//! moved or deleted: `target_name` (the last segment of the target, lower case, without `.md`)
//! finds those rows without resolving every link in the store.

use std::collections::{BTreeMap, HashSet};

use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension};
use textdb_core::storage::Result;
use textdb_core::{Edit, Link, StructureExtractor, TextdbError};
pub use textdb_md::resolve::{join, name_key, relative, LinkUpdates, LINK_UPDATES_SETTING};
use textdb_md::resolve::{line_of_offset, resolve, rewritten_target, Lookup};

use crate::db::{subtree_bounds, to_hash, TextDb};
use crate::storage::sql_err;

/// Does this status mean the link does not reach a document?
///
/// `external` is not broken — a URL is not this store's to resolve — and `not-in-store` means
/// the target is deliberately outside it. What counts is a link that meant to reach something
/// here and does not.
pub(crate) fn is_broken(status: Option<&str>) -> bool {
    matches!(status, Some("broken") | Some("anchor-missing") | Some("ambiguous"))
}

/// Ids or names per `IN (...)` statement.
///
/// Fixed so the statement text repeats and the cache can hold it: a chunk sized to whatever
/// happened to be left over would compile a new statement every time.
const CHUNK: usize = 500;

/// A link that pointed at a file a move took elsewhere, and no longer reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkChange {
    /// The file the link is written in, after the move.
    pub path: String,
    pub line: i64,
    pub kind: String,
    /// The target as it was written.
    pub target: String,
    /// Where the file it pointed at is now.
    pub now_at: String,
    /// The version of `path` the link was rewritten in; `None` when it was only reported.
    pub version: Option<u64>,
    /// The linking file is outside the caller's shares, so it was left alone and `path` is empty.
    ///
    /// A move rewrites links store-wide, and an account may not write every file that points into
    /// what it moved. Those are counted and reported as a number — never as paths, which would
    /// hand out the layout the alias exists to hide (#12 D14).
    pub outside: bool,
}

/// A resolved link captured before a move.
pub(crate) struct Pointing {
    rowid: i64,
    file_id: i64,
    line: i64,
    kind: String,
    target: String,
    resolved_id: i64,
}

impl<'c> TextDb<'c> {
    fn live_files(&self, cond: &str, arg: &str) -> Result<Vec<(i64, String)>> {
        let mut st = self
            .conn
            .prepare_cached(&format!("SELECT id, path FROM {}node WHERE kind = 1 AND deleted_at IS NULL AND {cond}", self.p))
            .map_err(sql_err)?;
        let rows = st.query_map(params![arg], |r| Ok((r.get(0)?, r.get(1)?))).map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)
    }
}

impl Lookup for TextDb<'_> {
    fn files_by_path(&self, path: &str) -> Result<Vec<(i64, String)>> {
        self.live_files("lower(path) = lower(?1)", path)
    }

    fn files_by_name(&self, name: &str) -> Result<Vec<(i64, String)>> {
        self.live_files("lower(name) = lower(?1)", name)
    }

    fn files_by_suffix(&self, suffix: &str) -> Result<Vec<(i64, String)>> {
        self.live_files("substr(lower(path), -length(?1)) = ?1", suffix)
    }

    fn is_folder(&self, path: &str) -> Result<bool> {
        self.conn
            .prepare_cached(&format!(
                "SELECT count(*) > 0 FROM {}node WHERE kind = 0 AND deleted_at IS NULL AND lower(path) = lower(?1)",
                self.p
            ))
            .map_err(sql_err)?
            .query_row(params![path], |r| r.get(0))
            .map_err(sql_err)
    }

    fn headings(&self, file_id: i64) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare_cached(&format!("SELECT heading_path FROM {}section WHERE file_id = ?1", self.p))
            .map_err(sql_err)?;
        let rows = st.query_map(params![file_id], |r| r.get(0)).map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)
    }
}

impl<'c> TextDb<'c> {
    /// The file a link written in `source` points to, and its status.
    pub fn resolve_link(
        &self,
        source_id: i64,
        source: &str,
        kind: &str,
        target: &str,
        anchor: Option<&str>,
        external: bool,
    ) -> Result<(Option<i64>, &'static str)> {
        if !external && self.names_no_share(kind, target) {
            return Ok((None, "broken"));
        }
        resolve(self, source_id, source, kind, target, anchor, external)
    }

    /// Is this a root path written in the account's own namespace that names no share it holds?
    ///
    /// Such a link is nonsense: the account meant a folder it does not have. Its bytes are kept
    /// as written (#12 §3.4) — the text is the author's — but it must not then be resolved
    /// against the store's root, which would let `[[hr/salaries]]` reach a document the account
    /// cannot see and put a link into it in everyone else's backlinks. It is broken, for
    /// everyone, which is what it means.
    ///
    /// A target that *did* un-project is a store path by the time it is indexed, so the account
    /// can see it and this says no. The check is therefore on the text as stored, and needs no
    /// record of what the writer meant.
    ///
    /// Re-resolution later, by someone whose view does contain the path, resolves it: `relink`
    /// runs under whoever triggered it. That only happens when the target itself moves, and the
    /// answer then is the one that account would get for text written now.
    fn names_no_share(&self, kind: &str, target: &str) -> bool {
        use textdb_core::access::Resolved;
        if self.view.is_admin() || target.is_empty() {
            return false;
        }
        let md = kind == "md" || kind == "image";
        let looks_root = if md { target.starts_with('/') } else { target.contains('/') };
        if !looks_root {
            return false;
        }
        let local = format!("/{}", target.trim_start_matches('/'));
        // Visible as written: it un-projected, and it is the store's path for a share.
        if self.view.to_view(&local).is_some() {
            return false;
        }
        // Addressable but not there: an ordinary broken link inside a share, which the resolver
        // should answer for itself.
        !matches!(self.view.to_store(&local), Resolved::In { .. } | Resolved::Root)
    }

    /// Replace the link rows of a file (resolution follows with [`TextDb::relink_where`]).
    pub(crate) fn write_link_rows(&self, file_id: i64, version: i64, links: &[Link]) -> Result<()> {
        self.conn
            .prepare_cached(&format!("DELETE FROM {}link WHERE file_id = ?1", self.p))
            .map_err(sql_err)?
            .execute(params![file_id])
            .map_err(sql_err)?;
        // Eleven parameters a row, under SQLite's default limit of 32766 per statement.
        for batch in links.chunks(2900) {
            let rows = vec!["(?,?,?,?,?,?,?,?,?,?,?)"; batch.len()].join(",");
            let sql = format!(
                "INSERT INTO {}link(file_id, version, target_path, line, kind, anchor, alias, external, target_name, span_from, span_to) \
                 VALUES {rows}",
                self.p
            );
            let mut vals: Vec<Value> = Vec::with_capacity(batch.len() * 11);
            for l in batch {
                let key = (!l.external && !l.target_path.is_empty()).then(|| name_key(&l.target_path));
                vals.extend([
                    file_id.into(),
                    version.into(),
                    l.target_path.clone().into(),
                    (l.line as i64).into(),
                    l.kind.clone().into(),
                    l.anchor.clone().map_or(Value::Null, Value::Text),
                    l.alias.clone().map_or(Value::Null, Value::Text),
                    (l.external as i64).into(),
                    key.map_or(Value::Null, Value::Text),
                    l.span.map_or(Value::Null, |(a, _)| Value::Integer(a as i64)),
                    l.span.map_or(Value::Null, |(_, b)| Value::Integer(b as i64)),
                ]);
            }
            self.conn
                .prepare_cached(&sql)
                .map_err(sql_err)?
                .execute(rusqlite::params_from_iter(vals))
                .map_err(sql_err)?;
        }
        Ok(())
    }

    /// Resolve again the links of live files matching `cond` (over `l`, the link row).
    pub(crate) fn relink_where(&self, cond: &str, args: Vec<Value>) -> Result<()> {
        type Row = (i64, i64, String, String, String, Option<String>, bool, Option<i64>, Option<String>);
        let rows: Vec<Row> = {
            let mut st = self
                .conn
                .prepare_cached(&format!(
                    "SELECT l.rowid, l.file_id, n.path, coalesce(l.kind, ''), l.target_path, l.anchor, l.external, \
                            l.resolved_id, l.status \
                     FROM {p}link l JOIN {p}node n ON n.id = l.file_id AND n.deleted_at IS NULL WHERE ({cond})",
                    p = self.p
                ))
                .map_err(sql_err)?;
            let it = st
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get::<_, i64>(6)? != 0, r.get(7)?, r.get(8)?))
                })
                .map_err(sql_err)?;
            it.collect::<rusqlite::Result<_>>().map_err(sql_err)?
        };
        // A link breaking is the one structure count that moves without a commit, so the files
        // whose broken total changed are collected here and rolled up once at the end rather
        // than recomputed per row.
        let mut touched: std::collections::BTreeMap<i64, String> = Default::default();
        for (rowid, file_id, path, kind, target, anchor, external, had_id, had_status) in rows {
            let (id, status) = self.resolve_link(file_id, &path, &kind, &target, anchor.as_deref(), external)?;
            // Most re-resolutions confirm what the row already said — a folder rename moves a
            // file and the siblings its relative links point at together, so the answer does
            // not change — and writing that back cost an UPDATE and a WAL record per link.
            if had_id == id && had_status.as_deref() == Some(status) {
                continue;
            }
            if is_broken(had_status.as_deref()) != is_broken(Some(status)) {
                touched.insert(file_id, path.clone());
            }
            self.conn
                .prepare_cached(&format!("UPDATE {}link SET resolved_id = ?1, status = ?2 WHERE rowid = ?3", self.p))
                .map_err(sql_err)?
                .execute(params![id, status, rowid])
                .map_err(sql_err)?;
        }
        for (file_id, path) in touched {
            self.refresh_broken_links(file_id, &path)?;
        }
        Ok(())
    }

    /// Recount `file_id`'s broken links, store the number and move the folders above it.
    fn refresh_broken_links(&self, file_id: i64, path: &str) -> Result<()> {
        let now = self.broken_links_of(file_id)?;
        let before: i64 = self
            .conn
            .prepare_cached(&format!("SELECT nlinks_broken FROM {}node WHERE id = ?1", self.p))
            .map_err(sql_err)?
            .query_row(params![file_id], |r| r.get(0))
            .map_err(sql_err)?;
        if now == before {
            return Ok(());
        }
        self.conn
            .prepare_cached(&format!("UPDATE {}node SET nlinks_broken = ?1 WHERE id = ?2", self.p))
            .map_err(sql_err)?
            .execute(params![now, file_id])
            .map_err(sql_err)?;
        let change = crate::stats::Totals {
            links_broken: now - before,
            ..Default::default()
        };
        self.add_to_ancestors(path, &change, &Self::now(), None)
    }

    /// Resolve again every link that could point to a file named one of `names`, or points to
    /// or is written in one of the files `ids`.
    /// Does this store record any links at all?
    ///
    /// A move or a delete has link bookkeeping to do only if something could point at what it
    /// touches. Without this, a store of plain text paid to list the whole moved subtree and
    /// query the empty link table just to find out there was nothing to do.
    pub(crate) fn has_links(&self) -> Result<bool> {
        let any: Option<i64> = self
            .conn
            .prepare_cached(&format!("SELECT 1 FROM {}link LIMIT 1", self.p))
            .map_err(sql_err)?
            .query_row([], |r| r.get(0))
            .optional()
            .map_err(sql_err)?;
        Ok(any.is_some())
    }

    pub(crate) fn relink(&self, names: &[String], ids: &[i64]) -> Result<()> {
        if (names.is_empty() && ids.is_empty()) || !self.has_links()? {
            return Ok(());
        }
        for chunk in names.chunks(CHUNK) {
            let marks = vec!["?"; chunk.len()].join(",");
            self.relink_where(&format!("l.target_name IN ({marks})"), chunk.iter().map(|n| Value::Text(n.clone())).collect())?;
        }
        // Two statements rather than one with `OR`: `resolved_id` and `file_id` have an index
        // each, and an `OR` across both columns lets SQLite use neither, so what should be two
        // index seeks became a scan of the whole link table on every move.
        for chunk in ids.chunks(CHUNK) {
            let marks = vec!["?"; chunk.len()].join(",");
            let args: Vec<Value> = chunk.iter().map(|i| Value::Integer(*i)).collect();
            self.relink_where(&format!("l.resolved_id IN ({marks})"), args.clone())?;
            self.relink_where(&format!("l.file_id IN ({marks})"), args)?;
        }
        Ok(())
    }

    /// What a move does to links: this handle's choice, else the store's `link_updates`
    /// setting, else [`LinkUpdates::DEFAULT`].
    pub fn link_updates_mode(&self) -> Result<LinkUpdates> {
        if let Some(mode) = self.link_updates {
            return Ok(mode);
        }
        Ok(self.setting(LINK_UPDATES_SETTING)?.as_deref().and_then(LinkUpdates::parse).unwrap_or(LinkUpdates::DEFAULT))
    }

    /// Resolved links to the files `moved` or written in them, before they move.
    pub(crate) fn links_into(&self, moved: &[(i64, String)]) -> Result<Vec<Pointing>> {
        let mut out = Vec::new();
        for chunk in moved.chunks(500) {
            let marks = (1..=chunk.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
            let mut st = self
                .conn
                .prepare(&format!(
                    "SELECT l.rowid, l.file_id, l.line, coalesce(l.kind, ''), l.target_path, l.resolved_id \
                     FROM {p}link l JOIN {p}node n ON n.id = l.file_id AND n.deleted_at IS NULL \
                     WHERE l.resolved_id IS NOT NULL AND l.target_path <> '' AND (l.resolved_id IN ({marks}) OR l.file_id IN ({marks}))",
                    p = self.p
                ))
                .map_err(sql_err)?;
            let rows = st
                .query_map(rusqlite::params_from_iter(chunk.iter().map(|(id, _)| *id)), |r| {
                    Ok(Pointing { rowid: r.get(0)?, file_id: r.get(1)?, line: r.get(2)?, kind: r.get(3)?, target: r.get(4)?, resolved_id: r.get(5)? })
                })
                .map_err(sql_err)?;
            for row in rows {
                out.push(row.map_err(sql_err)?);
            }
        }
        out.sort_by_key(|p| p.rowid);
        out.dedup_by_key(|p| p.rowid);
        Ok(out)
    }

    fn live_path(&self, id: i64) -> Result<Option<(String, Option<Vec<u8>>)>> {
        self.conn
            .prepare_cached(&format!("SELECT path, root FROM {}node WHERE id = ?1 AND deleted_at IS NULL", self.p))
            .map_err(sql_err)?
            .query_row(params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(sql_err)
    }

    /// After a move from `from` to `to`: the links in `pointing` that no longer reach the file
    /// they did, rewritten (one commit per linking file) when `mode` says so.
    pub(crate) fn follow_move(&self, pointing: Vec<Pointing>, from: &str, to: &str, mode: LinkUpdates, author: Option<&str>) -> Result<Vec<LinkChange>> {
        let mut by_file: BTreeMap<i64, Vec<Pointing>> = BTreeMap::new();
        for p in pointing {
            let now: Option<i64> = self
                .conn
                .prepare_cached(&format!("SELECT resolved_id FROM {}link WHERE rowid = ?1", self.p))
                .map_err(sql_err)?
                .query_row(params![p.rowid], |r| r.get(0))
                .optional()
                .map_err(sql_err)?
                .flatten();
            if now != Some(p.resolved_id) {
                by_file.entry(p.file_id).or_default().push(p);
            }
        }
        let message = format!("links: {from} -> {to}");
        let mut changes = Vec::new();
        for (file_id, links) in by_file {
            let Some((source, root)) = self.live_path(file_id)? else {
                continue;
            };
            // A linking file the caller may not write is left exactly as it is. It is still a
            // fact about the move — the link now points somewhere else — so it is reported, as a
            // count with no path: naming it would disclose a layout the account cannot see.
            if !self.view.is_admin() && !self.can_write_store_path(&source) {
                changes.push(LinkChange {
                    path: String::new(),
                    line: 0,
                    kind: String::new(),
                    target: String::new(),
                    now_at: String::new(),
                    version: None,
                    outside: true,
                });
                continue;
            }
            let mut rewritten = HashSet::new();
            let mut version = None;
            if mode == LinkUpdates::Rewrite {
                let root = root.ok_or_else(|| TextdbError::NotFound(source.clone()))?;
                let doc = self.storage().document(&to_hash(&root)?)?.0;
                let line_of = line_of_offset(&doc);
                let mut edits = Vec::new();
                let mut paired = HashSet::new();
                for span in textdb_md::links::scan(&doc) {
                    let line = line_of(span.offset);
                    let Some(range) = span.range.clone() else { continue };
                    // Rows are in document order, as the spans are: pair each span with the first
                    // row of its line, kind and target not already paired.
                    let Some(p) = links
                        .iter()
                        .find(|p| p.line == line && p.kind == span.kind && p.target == span.target && !paired.contains(&p.rowid))
                    else {
                        continue;
                    };
                    paired.insert(p.rowid);
                    let Some((target, _)) = self.live_path(p.resolved_id)? else { continue };
                    let raw = String::from_utf8_lossy(&doc[range.clone()]).to_string();
                    let new = rewritten_target(self, span.kind, &raw, span.angle, &source, &target)?;
                    if new != raw {
                        edits.push(Edit::new(range.start as u64, range.end as u64, new.into_bytes()));
                        rewritten.insert(p.rowid);
                    }
                }
                if !edits.is_empty() {
                    version = Some(self.commit_edits_at(&source, &edits, None, author, Some(&message))?.version);
                }
            }
            for p in &links {
                let now_at = self.live_path(p.resolved_id)?.map(|(path, _)| path).unwrap_or_default();
                // Both paths in the caller's namespace; the file is one it can write, so it has
                // one, and the target may not be — then it is named as nothing rather than as a
                // store path.
                let (path, now_at) = match self.view.is_admin() {
                    true => (source.clone(), now_at),
                    false => (
                        self.view_path(&source).unwrap_or_default(),
                        self.view_path(&now_at).unwrap_or_default(),
                    ),
                };
                changes.push(LinkChange {
                    path,
                    line: p.line,
                    kind: p.kind.clone(),
                    target: p.target.clone(),
                    now_at,
                    version: if rewritten.contains(&p.rowid) { version } else { None },
                    outside: false,
                });
            }
        }
        Ok(changes)
    }

    /// The live files at or below `path`, as `(id, path)`.
    pub(crate) fn files_at(&self, path: &str) -> Result<Vec<(i64, String)>> {
        let (lo, hi) = subtree_bounds(path).unwrap_or_else(|| ("/".to_string(), "0".to_string()));
        let mut st = self
            .conn
            .prepare_cached(&format!(
                "SELECT id, path FROM {}node WHERE kind = 1 AND deleted_at IS NULL AND (path = ?1 OR (path >= ?2 AND path < ?3))",
                self.p
            ))
            .map_err(sql_err)?;
        let rows = st.query_map(params![path, lo, hi], |r| Ok((r.get(0)?, r.get(1)?))).map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)
    }
}

/// Fill the link columns of a store that had none: extract every live markdown file's links
/// again and resolve them all.
pub fn backfill(conn: &Connection, p: &str) -> Result<()> {
    // No view, deliberately: migration-time, over every file in the store. See stats::backfill.
    let db = TextDb::attach(conn, p, false);
    let files: Vec<(i64, i64, Vec<u8>, String)> = {
        let mut stmt = conn
            .prepare(&format!("SELECT id, version, root, path FROM {p}node WHERE kind = 1 AND root IS NOT NULL AND deleted_at IS NULL"))
            .map_err(sql_err)?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)?
    };
    let st = db.storage();
    let extractor = textdb_md::MarkdownExtractor;
    for (id, version, root, path) in files {
        let lower = path.to_ascii_lowercase();
        if !(lower.ends_with(".md") || lower.ends_with(".markdown")) {
            continue;
        }
        let doc = st.document(&to_hash(&root)?)?.0;
        db.write_link_rows(id, version, &extractor.extract(&doc).links)?;
    }
    db.relink_where("1", Vec::new())
}

/// One link as every surface returns it — the canonical row of `docs/shapes.md`.
///
/// `version` is the file's, and it is here for the same reason it is on a search hit: a line
/// number belongs to a version, and an agent that reads a link and then edits by line needs
/// one to pass as `base_version`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRow {
    /// The file the link is written in.
    pub path: String,
    pub version: i64,
    pub line: i64,
    /// `wiki`, `embed`, `md` or `image`.
    pub kind: String,
    pub target: String,
    pub anchor: Option<String>,
    pub alias: Option<String>,
    /// `ok`, `ambiguous`, `anchor-missing`, `broken`, `not-in-store` or `external`.
    pub status: Option<String>,
    /// The file it points to; for an asset, the asset itself rather than its `.tdbasset` pointer.
    pub resolved: Option<String>,
    /// It resolves to an asset.
    pub asset: bool,
}

/// Which way to follow a link: the ones written under `path`, or the ones pointing at it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
}

/// Links under `path` (a file or a whole folder), or the links pointing at it.
///
/// `statuses` filters on `status` and empty means all of them. A link to an asset's pointer is
/// reported as the asset, which is what the user wrote and what `backlinks` on an asset finds.
pub fn rows(
    conn: &Connection,
    p: &str,
    path: &str,
    dir: Direction,
    statuses: &[&str],
    lim: usize,
) -> Result<Vec<LinkRow>> {
    let (lo, hi) = subtree_bounds(path).unwrap_or_else(|| ("/".into(), "0".into()));
    // `status` is a closed set the caller does not choose freely, so the list is built into the
    // statement text: it keeps the statement cacheable and the values can only be our own.
    let only = if statuses.is_empty() {
        String::new()
    } else {
        let known = ["ok", "ambiguous", "anchor-missing", "broken", "not-in-store", "external"];
        let mut names: Vec<String> = Vec::new();
        for s in statuses {
            if !known.contains(s) {
                return Err(TextdbError::InvalidEdit(format!("unknown link status: {s}")));
            }
            names.push(format!("'{s}'"));
        }
        format!(" AND l.status IN ({})", names.join(","))
    };
    let cond = match dir {
        Direction::Out => format!("(n.path = ?1 OR (n.path >= ?2 AND n.path < ?3)){only}"),
        // An asset is linked by its own name; the node that exists is the `.tdbasset` pointer.
        Direction::In => format!(
            "l.target_path <> '' AND l.resolved_id IN (SELECT id FROM {p}node WHERE kind = 1 AND deleted_at IS NULL \
             AND (path = ?1 OR path = ?1 || '.tdbasset' OR (path >= ?2 AND path < ?3))){only}"
        ),
    };
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT n.path, n.version, l.line, coalesce(l.kind, ''), l.target_path, l.anchor, l.alias, l.status, \
             CASE WHEN r.path LIKE '%.tdbasset' THEN substr(r.path, 1, length(r.path) - 9) ELSE r.path END, \
             coalesce(r.path LIKE '%.tdbasset', 0) \
             FROM {p}link l JOIN {p}node n ON n.id = l.file_id AND n.deleted_at IS NULL \
             LEFT JOIN {p}node r ON r.id = l.resolved_id AND r.deleted_at IS NULL \
             WHERE {cond} ORDER BY n.path, l.line, l.rowid LIMIT ?4"
        ))
        .map_err(sql_err)?;
    let out = stmt
        .query_map(params![path, lo, hi, lim as i64], |r| {
            Ok(LinkRow {
                path: r.get(0)?,
                version: r.get(1)?,
                line: r.get(2)?,
                kind: r.get(3)?,
                target: r.get(4)?,
                anchor: r.get(5)?,
                alias: r.get(6)?,
                status: r.get(7)?,
                resolved: r.get(8)?,
                asset: r.get::<_, i64>(9)? != 0,
            })
        })
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err)?;
    Ok(out)
}
