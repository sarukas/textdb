//! Link resolution, by Obsidian's rules, kept current in `{p}link`.
//!
//! Each link row records the file it resolves to (`resolved_id`) and a `status`: `ok`,
//! `ambiguous` (several files match; the nearest is taken), `anchor-missing` (the file has no
//! such heading), `broken`, `not-in-store` (a PDF, image or other file a text store does not
//! hold) or `external` (URLs, email addresses, queries, numbered references). Rows are resolved
//! when their file is committed, and again when a file they could point to is created, moved or
//! deleted: `target_name` (the last segment of the target, lower case, without `.md`) finds
//! those rows without resolving every link in the store.

use rusqlite::types::Value;
use rusqlite::{params, Connection};
use textdb_core::storage::Result;
use textdb_core::{Link, StructureExtractor};

use crate::db::{parent_of, subtree_bounds, to_hash, TextDb};
use crate::storage::sql_err;

/// The store setting deciding what a move does to links pointing at what moved.
pub const LINK_UPDATES_SETTING: &str = "link_updates";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkUpdates {
    /// Nothing.
    Off,
    /// List the links the move leaves pointing elsewhere.
    Report,
    /// Rewrite them, one commit per linking file, in the same transaction as the move.
    Rewrite,
}

impl LinkUpdates {
    pub const DEFAULT: LinkUpdates = LinkUpdates::Report;

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "report" => Some(Self::Report),
            "rewrite" => Some(Self::Rewrite),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Report => "report",
            Self::Rewrite => "rewrite",
        }
    }
}

/// Extensions of files a store of text documents does not hold.
const NOT_TEXT: &[&str] = &[
    "pdf", "png", "jpg", "jpeg", "gif", "svg", "webp", "bmp", "heic", "tif", "tiff", "ico", "xlsx", "xls", "docx", "doc", "pptx", "ppt",
    "odt", "ods", "csv", "tsv", "zip", "mp4", "mov", "mp3", "wav", "m4a", "canvas", "base", "json", "html", "htm", "drawio", "excalidraw",
];

/// A file name as links find it: lower case, without `.md`.
pub fn name_key(path_or_name: &str) -> String {
    let last = path_or_name.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_lowercase();
    last.strip_suffix(".md").map(str::to_string).unwrap_or(last)
}

/// `rel` against the folder `base` (`/a/b`), resolving `.` and `..`; `None` above the root.
pub fn join(base: &str, rel: &str) -> Option<String> {
    let mut segs: Vec<&str> = if rel.starts_with('/') { Vec::new() } else { base.split('/').filter(|s| !s.is_empty()).collect() };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop()?;
            }
            s => segs.push(s),
        }
    }
    Some(format!("/{}", segs.join("/")))
}

fn variants(path: &str) -> Vec<String> {
    if path.to_ascii_lowercase().ends_with(".md") {
        vec![path.to_string()]
    } else {
        vec![path.to_string(), format!("{path}.md")]
    }
}

/// Lower-case letters and digits only, so `Next steps` and `next-steps` compare equal.
fn heading_key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
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

    fn heading_exists(&self, file_id: i64, anchor: &str) -> Result<bool> {
        let last = anchor.rsplit('#').next().unwrap_or(anchor);
        if last.starts_with('^') {
            return Ok(true); // block ids are not checked
        }
        let want = heading_key(last);
        let mut st = self
            .conn
            .prepare_cached(&format!("SELECT heading_path FROM {}section WHERE file_id = ?1", self.p))
            .map_err(sql_err)?;
        let headings = st.query_map(params![file_id], |r| r.get::<_, String>(0)).map_err(sql_err)?;
        for h in headings {
            let h = h.map_err(sql_err)?;
            if heading_key(h.rsplit(" / ").next().unwrap_or(&h)) == want {
                return Ok(true);
            }
        }
        Ok(false)
    }

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
        if external {
            return Ok((None, "external"));
        }
        if target.is_empty() {
            return Ok(match anchor {
                None => (None, "broken"),
                Some(a) if self.heading_exists(source_id, a)? => (Some(source_id), "ok"),
                Some(_) => (Some(source_id), "anchor-missing"),
            });
        }
        let folder = parent_of(source);
        let mut found: Vec<(i64, String)> = Vec::new();
        let mut by_path = |found: &mut Vec<(i64, String)>, p: Option<String>| -> Result<()> {
            for v in p.map(|p| variants(&p)).unwrap_or_default() {
                found.extend(self.live_files("lower(path) = lower(?1)", &v)?);
            }
            Ok(())
        };
        if kind == "md" || kind == "image" {
            by_path(&mut found, join(folder, target))?;
            if found.is_empty() && !target.starts_with('/') {
                by_path(&mut found, join("/", target))?;
            }
        } else if target.contains('/') {
            by_path(&mut found, join("/", target))?;
            if found.is_empty() {
                by_path(&mut found, join(folder, target))?;
            }
            if found.is_empty() {
                for v in variants(target.trim_start_matches('/')) {
                    let suffix = format!("/{}", v.to_lowercase());
                    found.extend(self.live_files("substr(lower(path), -length(?1)) = ?1", &suffix)?);
                }
            }
        } else {
            for v in variants(target) {
                found.extend(self.live_files("lower(name) = lower(?1)", &v)?);
            }
        }
        found.sort();
        found.dedup();
        if found.is_empty() {
            let ext = target.rsplit('/').next().and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase());
            let status = if ext.is_some_and(|e| NOT_TEXT.contains(&e.as_str())) { "not-in-store" } else { "broken" };
            return Ok((None, status));
        }
        let several = found.len() > 1;
        // The exact spelling first, then the linking note's folder, then the shortest path.
        found.sort_by_key(|(_, p)| {
            let exact = p.ends_with(target) || p.ends_with(&format!("{target}.md"));
            (!exact, parent_of(p) != folder, p.len(), p.clone())
        });
        let id = found[0].0;
        if let Some(a) = anchor {
            if !self.heading_exists(id, a)? {
                return Ok((Some(id), "anchor-missing"));
            }
        }
        Ok((Some(id), if several { "ambiguous" } else { "ok" }))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_join_and_names_key() {
        assert_eq!(join("/a/b", "../c/d.md").as_deref(), Some("/a/c/d.md"));
        assert_eq!(join("/a", "./x/./y").as_deref(), Some("/a/x/y"));
        assert_eq!(join("/a", "/z.md").as_deref(), Some("/z.md"));
        assert_eq!(join("/a", "../../x"), None);
        assert_eq!(name_key("/Acc/Jazz Pakistan.MD"), "jazz pakistan.md".strip_suffix(".md").unwrap());
        assert_eq!(name_key("Notes/Plan.md"), "plan");
        assert_eq!(heading_key("Next steps"), heading_key("next-steps"));
    }
}
