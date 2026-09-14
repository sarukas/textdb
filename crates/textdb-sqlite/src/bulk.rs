//! Bulk edits for SQL clients: several text replacements in one version of a file, and batches —
//! the commits, moves and deletes one run (a `textdb sql --write` statement, say) made, recorded
//! under one id so they can be previewed, listed and reverted together.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use rusqlite::{params, Connection};
use textdb_core::storage::Result;
use textdb_core::TextdbError;

use crate::db::{normalize_path, subtree_bounds, to_hash, TextDb, WriteResult};
use crate::storage::sql_err;

/// The batch each connection's writes are recorded under, keyed by SQLite handle: scalar
/// functions build a new `Connection` per call, but its handle is the same.
static BATCHES: Mutex<Option<HashMap<usize, String>>> = Mutex::new(None);

/// Record the commits, moves and deletes made on the connection with SQLite handle `handle` under
/// `batch` from now on; `None` stops.
pub fn set_batch_for_handle(handle: usize, batch: Option<&str>) {
    let mut guard = BATCHES.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    match batch {
        Some(b) => {
            map.insert(handle, b.to_string());
        }
        None => {
            map.remove(&handle);
        }
    }
}

fn handle_of(conn: &Connection) -> usize {
    unsafe { conn.handle() as usize }
}

/// As [`set_batch_for_handle`], for `conn`.
pub fn set_batch(conn: &Connection, batch: Option<&str>) {
    set_batch_for_handle(handle_of(conn), batch);
}

/// The batch writes on `conn` are recorded under, if any.
pub fn current_batch(conn: &Connection) -> Option<String> {
    current_batch_for_handle(handle_of(conn))
}

pub fn current_batch_for_handle(handle: usize) -> Option<String> {
    let guard = BATCHES.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().and_then(|m| m.get(&handle).cloned())
}

/// Replace every occurrence of `old` with `new`. With `expected`, the file must hold exactly that
/// many occurrences; without, at least one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replacement {
    pub old: Vec<u8>,
    pub new: Vec<u8>,
    pub expected: Option<usize>,
}

/// One change in a preview or a batch: a file's content (`create` or `edit`, with the versions
/// before and after and a unified diff), a `move`, a `delete` or a `mkdir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchItem {
    pub op: String,
    pub path: String,
    pub old_path: Option<String>,
    pub from_version: Option<i64>,
    pub to_version: Option<i64>,
    pub diff: Option<String>,
}

/// What reverting a batch did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RevertReport {
    /// Files whose content went back to what it was before the batch, with the new version.
    pub restored: Vec<(String, i64)>,
    /// Files the batch created, deleted again.
    pub removed: Vec<String>,
    /// Moves undone: from, back to.
    pub moved_back: Vec<(String, String)>,
    /// Files and folders the batch deleted, created again at their paths with their content from
    /// before the batch. They are new files: the old history stays with the deleted ones.
    pub recreated: Vec<String>,
    /// What changed since the batch, or could not go back, and was left alone.
    pub skipped: Vec<String>,
}

struct ChangeRec {
    op: String,
    node_id: i64,
    path: String,
    old_path: Option<String>,
    version: Option<i64>,
}

fn occurrences(text: &[u8], pat: &[u8]) -> Vec<usize> {
    let mut at = Vec::new();
    let mut i = 0;
    while i + pat.len() <= text.len() {
        if &text[i..i + pat.len()] == pat {
            at.push(i);
            i += pat.len();
        } else {
            i += 1;
        }
    }
    at
}

fn shown(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(59).collect::<String>())
    } else {
        s.into_owned()
    }
}

/// A new file as a unified diff against nothing.
fn created_diff(path: &str, version: i64, content: &[u8]) -> String {
    let text = String::from_utf8_lossy(content);
    let lines: Vec<&str> = text.lines().collect();
    let mut s = format!("--- /dev/null\n+++ {path}@{version}\n@@ -0,0 +1,{} @@\n", lines.len());
    for l in lines {
        s.push('+');
        s.push_str(l);
        s.push('\n');
    }
    s
}

impl<'c> TextDb<'c> {
    /// Apply `replacements` in order to the current content and commit the result as one
    /// version. Returns the write and how many occurrences each replacement found. A count that
    /// does not match (or none found, without an expected count) fails the whole call.
    pub fn replace_text(
        &self,
        path: &str,
        replacements: &[Replacement],
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<(WriteResult, Vec<usize>)> {
        let path = normalize_path(path)?;
        if replacements.is_empty() {
            return Err(TextdbError::InvalidEdit("no replacements given".into()));
        }
        self.tx(|db| {
            let n = match db.node_by_path(&path)? {
                Some(n) if n.kind == 1 => n,
                Some(_) => return Err(TextdbError::InvalidEdit(format!("{path} is a folder"))),
                None => return Err(TextdbError::NotFound(path.clone())),
            };
            let root = n.root.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
            let mut text = (*db.storage().document(&root)?.0).clone();
            let mut counts = Vec::with_capacity(replacements.len());
            for (i, r) in replacements.iter().enumerate() {
                if r.old.is_empty() {
                    return Err(TextdbError::InvalidEdit(format!("replacement {}: the old text is empty", i + 1)));
                }
                let at = occurrences(&text, &r.old);
                match r.expected {
                    Some(e) if e != at.len() => {
                        return Err(TextdbError::InvalidEdit(format!(
                            "{path}: expected {e} occurrence{} of {:?}, found {}",
                            if e == 1 { "" } else { "s" },
                            shown(&r.old),
                            at.len()
                        )))
                    }
                    None if at.is_empty() => {
                        return Err(TextdbError::InvalidEdit(format!("{path}: {:?} not found", shown(&r.old))))
                    }
                    _ => {}
                }
                if !at.is_empty() {
                    let mut out = Vec::with_capacity(text.len());
                    let mut pos = 0;
                    for a in &at {
                        out.extend_from_slice(&text[pos..*a]);
                        out.extend_from_slice(&r.new);
                        pos = a + r.old.len();
                    }
                    out.extend_from_slice(&text[pos..]);
                    text = out;
                }
                counts.push(at.len());
            }
            let w = db.update_content(&path, &text, Some(n.version as u64), author, message.or(Some("replace")))?;
            Ok((w, counts))
        })
    }

    fn change_recs(&self, filter: &str, arg: rusqlite::types::Value) -> Result<Vec<ChangeRec>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT op, node_id, path, old_path, version FROM {}change WHERE {filter} ORDER BY seq", self.p))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![arg], |r| {
                Ok(ChangeRec {
                    op: r.get(0)?,
                    node_id: r.get(1)?,
                    path: r.get(2)?,
                    old_path: r.get(3)?,
                    version: r.get(4)?,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
    }

    /// The changes recorded after change number `seq`, a file's commits folded into one item
    /// with a diff: what a statement run since then did, for a dry run to show.
    pub fn changes_after(&self, seq: i64) -> Result<Vec<BatchItem>> {
        let recs = self.change_recs("seq > ?1", seq.into())?;
        self.summarize(&recs)
    }

    /// The changes recorded under `batch`, as [`changes_after`](Self::changes_after) shows them.
    pub fn batch_changes(&self, batch: &str) -> Result<Vec<BatchItem>> {
        let recs = self.change_recs("batch = ?1", batch.to_string().into())?;
        if recs.is_empty() {
            return Err(TextdbError::NotFound(format!("no changes recorded under batch {batch}")));
        }
        self.summarize(&recs)
    }

    fn summarize(&self, recs: &[ChangeRec]) -> Result<Vec<BatchItem>> {
        let mut items: Vec<BatchItem> = Vec::new();
        let mut files: HashMap<i64, usize> = HashMap::new();
        for r in recs {
            match r.op.as_str() {
                "create" | "commit" => {
                    let v = r.version.unwrap_or(0);
                    match files.get(&r.node_id) {
                        Some(&i) => items[i].to_version = Some(v),
                        None => {
                            files.insert(r.node_id, items.len());
                            items.push(BatchItem {
                                op: if v <= 1 { "create" } else { "edit" }.into(),
                                path: r.path.clone(),
                                old_path: None,
                                from_version: Some(v - 1),
                                to_version: Some(v),
                                diff: None,
                            });
                        }
                    }
                }
                "move" | "delete" | "mkdir" => items.push(BatchItem {
                    op: r.op.clone(),
                    path: r.path.clone(),
                    old_path: r.old_path.clone(),
                    from_version: None,
                    to_version: None,
                    diff: None,
                }),
                _ => {}
            }
        }
        for (node, &i) in &files {
            let Some(n) = self.node_by_id(*node)? else { continue };
            let (from, to) = (items[i].from_version.unwrap_or(0), items[i].to_version.unwrap_or(0));
            let diff = if from <= 0 {
                let root = self.root_of_version(n.id, to as u64)?;
                created_diff(&n.path, to, &self.storage().document(&root)?.0)
            } else {
                self.diff(&n.path, from as u64, to as u64)?
            };
            items[i].path = n.path;
            items[i].diff = Some(diff);
        }
        Ok(items)
    }

    /// Undo the batch `batch`: files it changed go back to their content from before it, files it
    /// created are deleted, moves are undone and what it deleted is created again. Anything changed
    /// since the batch is left alone and reported; unless `skip_changed`, that fails the whole
    /// revert, so nothing changes. The revert is itself recorded like any other write.
    pub fn revert_batch(&self, batch: &str, author: Option<&str>, skip_changed: bool) -> Result<RevertReport> {
        self.tx(|db| {
            let recs = db.change_recs("batch = ?1", batch.to_string().into())?;
            if recs.is_empty() {
                return Err(TextdbError::NotFound(format!("no changes recorded under batch {batch}")));
            }
            let message = format!("revert batch {batch}");
            let mut span: HashMap<i64, (i64, i64)> = HashMap::new();
            for r in recs.iter().filter(|r| r.op == "create" || r.op == "commit") {
                let v = r.version.unwrap_or(0);
                let e = span.entry(r.node_id).or_insert((v, v));
                e.0 = e.0.min(v);
                e.1 = e.1.max(v);
            }
            let deleted_paths: Vec<String> = recs.iter().filter(|r| r.op == "delete").map(|r| r.path.clone()).collect();
            let deleted_by_batch = |path: &str| deleted_paths.iter().any(|d| path == d || path.starts_with(&format!("{d}/")));
            let mut report = RevertReport::default();
            let mut done: HashSet<i64> = HashSet::new();
            // Newest first, so each change is undone on top of the state it left.
            for r in recs.iter().rev() {
                match r.op.as_str() {
                    "move" => {
                        let Some(old) = &r.old_path else { continue };
                        match db.node_by_id(r.node_id)? {
                            Some(n) if n.deleted_at.is_none() && n.path == r.path => {
                                if db.node_by_path(old)?.is_some() {
                                    report.skipped.push(format!("{} not moved back: {old} exists", r.path));
                                    continue;
                                }
                                db.rename_by(&r.path, old, author)?;
                                report.moved_back.push((r.path.clone(), old.clone()));
                            }
                            _ => report.skipped.push(format!("{} not moved back to {old}: moved or deleted since", r.path)),
                        }
                    }
                    "delete" => {
                        let Some(d) = db.node_by_id(r.node_id)? else { continue };
                        let Some(deleted_at) = d.deleted_at.clone() else {
                            report.skipped.push(format!("{} not restored: no longer deleted", r.path));
                            continue;
                        };
                        let files: Vec<(i64, String, Option<Vec<u8>>)> = {
                            let (lo, hi) = subtree_bounds(&r.path).unwrap_or_default();
                            let mut stmt = db
                                .conn
                                .prepare(&format!(
                                    "SELECT id, path, root FROM {}node WHERE kind = 1 AND deleted_at = ?1 \
                                     AND (path = ?2 OR (path >= ?3 AND path < ?4)) ORDER BY path",
                                    db.p
                                ))
                                .map_err(sql_err)?;
                            let rows = stmt
                                .query_map(params![deleted_at, r.path, lo, hi], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                                .map_err(sql_err)?;
                            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)?
                        };
                        if d.kind == 0 && files.is_empty() {
                            if db.node_by_path(&r.path)?.is_none() {
                                db.ensure_folder(&r.path)?;
                                report.recreated.push(r.path.clone());
                            }
                            continue;
                        }
                        for (id, path, root) in files {
                            if db.node_by_path(&path)?.is_some() {
                                report.skipped.push(format!("{path} not restored: the path is taken"));
                                continue;
                            }
                            let content_root = match (span.get(&id), root) {
                                // Created by the batch and deleted again: nothing to bring back.
                                (Some(&(first, _)), _) if first <= 1 => continue,
                                (Some(&(first, _)), _) => db.root_of_version(id, (first - 1) as u64)?,
                                (None, Some(root)) => to_hash(&root)?,
                                (None, None) => continue,
                            };
                            let content = (*db.storage().document(&content_root)?.0).clone();
                            db.create(&path, &content, author, Some(&message))?;
                            report.recreated.push(path);
                        }
                    }
                    "create" | "commit" => {
                        if !done.insert(r.node_id) {
                            continue;
                        }
                        let (first, last) = span[&r.node_id];
                        let Some(n) = db.node_by_id(r.node_id)? else { continue };
                        if n.deleted_at.is_some() {
                            if !deleted_by_batch(&n.path) {
                                report.skipped.push(format!("{} not restored: deleted since the batch", n.path));
                            }
                            continue;
                        }
                        if n.version != last {
                            report.skipped.push(format!("{} not restored: changed since the batch (v{last}, now v{})", n.path, n.version));
                            continue;
                        }
                        if first <= 1 {
                            db.delete_by(&n.path, author)?;
                            report.removed.push(n.path.clone());
                            continue;
                        }
                        let root = db.root_of_version(n.id, (first - 1) as u64)?;
                        let content = (*db.storage().document(&root)?.0).clone();
                        let w = db.update_content(&n.path, &content, Some(n.version as u64), author, Some(&message))?;
                        report.restored.push((n.path.clone(), w.version as i64));
                    }
                    _ => {}
                }
            }
            if !skip_changed && !report.skipped.is_empty() {
                return Err(TextdbError::InvalidEdit(format!(
                    "batch {batch} cannot be reverted cleanly, so nothing was changed: {}. Revert the rest with --skip-changed",
                    report.skipped.join("; ")
                )));
            }
            Ok(report)
        })
    }
}
