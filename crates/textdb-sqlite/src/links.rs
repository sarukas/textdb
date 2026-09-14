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
        resolve(self, source_id, source, kind, target, anchor, external)
    }

    /// Replace the link rows of a file (resolution follows with [`TextDb::relink_where`]).
    pub(crate) fn write_link_rows(&self, file_id: i64, version: i64, links: &[Link]) -> Result<()> {
        self.conn
            .prepare_cached(&format!("DELETE FROM {}link WHERE file_id = ?1", self.p))
            .map_err(sql_err)?
            .execute(params![file_id])
            .map_err(sql_err)?;
        // Nine parameters a row, under SQLite's default limit of 32766 per statement.
        for batch in links.chunks(3000) {
            let rows = vec!["(?,?,?,?,?,?,?,?,?)"; batch.len()].join(",");
            let sql = format!(
                "INSERT INTO {}link(file_id, version, target_path, line, kind, anchor, alias, external, target_name) VALUES {rows}",
                self.p
            );
            let mut vals: Vec<Value> = Vec::with_capacity(batch.len() * 9);
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
        type Row = (i64, i64, String, String, String, Option<String>, bool);
        let rows: Vec<Row> = {
            let mut st = self
                .conn
                .prepare(&format!(
                    "SELECT l.rowid, l.file_id, n.path, coalesce(l.kind, ''), l.target_path, l.anchor, l.external \
                     FROM {p}link l JOIN {p}node n ON n.id = l.file_id AND n.deleted_at IS NULL WHERE ({cond})",
                    p = self.p
                ))
                .map_err(sql_err)?;
            let it = st
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get::<_, i64>(6)? != 0))
                })
                .map_err(sql_err)?;
            it.collect::<rusqlite::Result<_>>().map_err(sql_err)?
        };
        for (rowid, file_id, path, kind, target, anchor, external) in rows {
            let (id, status) = self.resolve_link(file_id, &path, &kind, &target, anchor.as_deref(), external)?;
            self.conn
                .prepare_cached(&format!("UPDATE {}link SET resolved_id = ?1, status = ?2 WHERE rowid = ?3", self.p))
                .map_err(sql_err)?
                .execute(params![id, status, rowid])
                .map_err(sql_err)?;
        }
        Ok(())
    }

    /// Resolve again every link that could point to a file named one of `names`, or points to
    /// or is written in one of the files `ids`.
    pub(crate) fn relink(&self, names: &[String], ids: &[i64]) -> Result<()> {
        for chunk in names.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            self.relink_where(&format!("l.target_name IN ({marks})"), chunk.iter().map(|n| Value::Text(n.clone())).collect())?;
        }
        for chunk in ids.chunks(500) {
            let marks = (1..=chunk.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
            self.relink_where(
                &format!("l.resolved_id IN ({marks}) OR l.file_id IN ({marks})"),
                chunk.iter().map(|i| Value::Integer(*i)).collect(),
            )?;
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
                    version = Some(self.commit_edits(&source, &edits, None, author, Some(&message))?.version);
                }
            }
            for p in &links {
                let now_at = self.live_path(p.resolved_id)?.map(|(path, _)| path).unwrap_or_default();
                changes.push(LinkChange {
                    path: source.clone(),
                    line: p.line,
                    kind: p.kind.clone(),
                    target: p.target.clone(),
                    now_at,
                    version: if rewritten.contains(&p.rowid) { version } else { None },
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
