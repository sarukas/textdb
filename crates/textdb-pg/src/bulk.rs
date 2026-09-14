//! Bulk edits and batches in Postgres, as in the SQLite binding (`textdb-sqlite/src/bulk.rs`):
//! several text replacements in one version, the changes a batch (session setting
//! `textdb.batch`) or a statement made, and reverting a batch.

use std::collections::{HashMap, HashSet};

use pgrx::prelude::*;
use serde_json::{json, Value};
use textdb_core::tree::materialize;
use textdb_core::{unified_diff, TextdbError};

use crate::kb;
use crate::store::{to_hash, SpiStorage};

type Result<T> = std::result::Result<T, TextdbError>;

fn err(e: pgrx::spi::Error) -> TextdbError {
    TextdbError::Storage(e.to_string())
}

/// Replace every occurrence of `old` with `new`. With `expected`, the file must hold exactly that
/// many occurrences; without, at least one.
pub struct Replacement {
    pub old: Vec<u8>,
    pub new: Vec<u8>,
    pub expected: Option<usize>,
}

/// `[[old, new], [old, new, expected_count], {"old", "new", "count"}, …]`.
pub fn parse_replacements(v: &Value) -> Result<Vec<Replacement>> {
    let usage = || {
        TextdbError::InvalidEdit(
            "replacements must be a JSON array of [old, new], [old, new, expected_count] or {\"old\", \"new\", \"count\"}".into(),
        )
    };
    let items = v.as_array().ok_or_else(usage)?;
    items
        .iter()
        .map(|item| {
            let (old, new, count) = match item {
                Value::Array(a) if a.len() == 2 || a.len() == 3 => (a[0].as_str(), a[1].as_str(), a.get(2)),
                Value::Object(o) => (o.get("old").and_then(Value::as_str), o.get("new").and_then(Value::as_str), o.get("count")),
                _ => return Err(usage()),
            };
            let (Some(old), Some(new)) = (old, new) else { return Err(usage()) };
            let expected = match count {
                None | Some(Value::Null) => None,
                Some(c) => Some(c.as_u64().ok_or_else(usage)? as usize),
            };
            Ok(Replacement { old: old.as_bytes().to_vec(), new: new.as_bytes().to_vec(), expected })
        })
        .collect()
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

/// `text` with `replacements` applied in order; a count that does not match fails.
pub fn apply_replacements(path: &str, mut text: Vec<u8>, replacements: &[Replacement]) -> Result<Vec<u8>> {
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
            None if at.is_empty() => return Err(TextdbError::InvalidEdit(format!("{path}: {:?} not found", shown(&r.old)))),
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
    }
    Ok(text)
}

struct ChangeRec {
    op: String,
    node_id: i64,
    path: String,
    old_path: Option<String>,
    version: Option<i64>,
}

fn change_recs(seq: Option<i64>, batch: Option<&str>) -> Result<Vec<ChangeRec>> {
    let (filter, arg) = match (seq, batch) {
        (_, Some(b)) => ("batch = $1", b.to_string()),
        (Some(s), None) => ("seq > $1::bigint", s.to_string()),
        (None, None) => return Err(TextdbError::InvalidEdit("give a change number or a batch".into())),
    };
    Spi::connect(|client| {
        let rows = client
            .select(
                &format!("SELECT op, node_id, path, old_path, version FROM kb.change WHERE {filter} ORDER BY seq"),
                None,
                &[arg.as_str().into()],
            )
            .map_err(err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(ChangeRec {
                op: r.get::<String>(1).map_err(err)?.unwrap_or_default(),
                node_id: r.get::<i64>(2).map_err(err)?.unwrap_or(0),
                path: r.get::<String>(3).map_err(err)?.unwrap_or_default(),
                old_path: r.get::<String>(4).map_err(err)?,
                version: r.get::<i64>(5).map_err(err)?,
            });
        }
        Ok(out)
    })
}

/// A node by id, deleted or not: kind, path, version, root, and whether it is deleted.
struct AnyNode {
    kind: i16,
    path: String,
    version: i64,
    root: Option<Vec<u8>>,
    deleted: bool,
}

fn node_by_id(id: i64) -> Result<Option<AnyNode>> {
    Spi::connect(|client| {
        let rows = client
            .select("SELECT kind, path, version, root, deleted_at IS NOT NULL FROM kb.node WHERE id = $1", Some(1), &[id.into()])
            .map_err(err)?;
        let mut out = None;
        for r in rows {
            out = Some(AnyNode {
                kind: r.get::<i16>(1).map_err(err)?.unwrap_or(0),
                path: r.get::<String>(2).map_err(err)?.unwrap_or_default(),
                version: r.get::<i64>(3).map_err(err)?.unwrap_or(0),
                root: r.get::<Vec<u8>>(4).map_err(err)?,
                deleted: r.get::<bool>(5).map_err(err)?.unwrap_or(false),
            });
        }
        Ok(out)
    })
}

fn content_at(file_id: i64, version: i64) -> Result<Vec<u8>> {
    materialize(&SpiStorage::new(), &kb::root_of_version_r(file_id, version as u64)?)
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

/// The changes after change number `seq`, or under `batch`, a file's commits folded into one item
/// with a diff: `[{op, path, old_path, from_version, to_version, diff}]`.
pub fn changes(seq: Option<i64>, batch: Option<&str>) -> Result<Value> {
    let recs = change_recs(seq, batch)?;
    if let (Some(b), true) = (batch, recs.is_empty()) {
        return Err(TextdbError::NotFound(format!("no changes recorded under batch {b}")));
    }
    let mut items: Vec<Value> = Vec::new();
    let mut files: Vec<(i64, usize)> = Vec::new();
    for r in &recs {
        match r.op.as_str() {
            "create" | "commit" => {
                let v = r.version.unwrap_or(0);
                match files.iter().find(|(id, _)| *id == r.node_id) {
                    Some(&(_, i)) => items[i]["to_version"] = json!(v),
                    None => {
                        files.push((r.node_id, items.len()));
                        items.push(json!({
                            "op": if v <= 1 { "create" } else { "edit" },
                            "path": r.path,
                            "old_path": null,
                            "from_version": v - 1,
                            "to_version": v,
                            "diff": null,
                        }));
                    }
                }
            }
            "move" | "delete" | "mkdir" => items.push(json!({
                "op": r.op,
                "path": r.path,
                "old_path": r.old_path,
                "from_version": null,
                "to_version": null,
                "diff": null,
            })),
            _ => {}
        }
    }
    let st = SpiStorage::new();
    for (node, i) in files {
        let Some(n) = node_by_id(node)? else { continue };
        let from = items[i]["from_version"].as_i64().unwrap_or(0);
        let to = items[i]["to_version"].as_i64().unwrap_or(0);
        let diff = if from <= 0 {
            created_diff(&n.path, to, &content_at(node, to)?)
        } else {
            let body = unified_diff(&st, &kb::root_of_version_r(node, from as u64)?, &kb::root_of_version_r(node, to as u64)?, 3)?;
            if body.is_empty() {
                String::new()
            } else {
                format!("--- {p}@{from}\n+++ {p}@{to}\n{body}", p = n.path)
            }
        };
        items[i]["path"] = json!(n.path);
        items[i]["diff"] = json!(diff);
    }
    Ok(Value::Array(items))
}

/// Undo the batch: files it changed go back to their content from before it, files it created are
/// deleted, moves are undone and what it deleted is created again. Anything changed since is left
/// alone and reported; unless `skip_changed`, that fails the whole revert.
pub fn revert(batch: &str, author: Option<&str>, skip_changed: bool) -> Result<Value> {
    let recs = change_recs(None, Some(batch))?;
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
    let (mut restored, mut removed, mut moved_back, mut recreated, mut skipped) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut done: HashSet<i64> = HashSet::new();
    // Newest first, so each change is undone on top of the state it left.
    for r in recs.iter().rev() {
        match r.op.as_str() {
            "move" => {
                let Some(old) = &r.old_path else { continue };
                match node_by_id(r.node_id)? {
                    Some(n) if !n.deleted && n.path == r.path => {
                        if kb::node_by_path(old).is_some() {
                            skipped.push(format!("{} not moved back: {old} exists", r.path));
                            continue;
                        }
                        kb::rename_impl(&r.path, old, author, Some(&message), None, false)?;
                        moved_back.push(json!({ "from": r.path, "to": old }));
                    }
                    _ => skipped.push(format!("{} not moved back to {old}: moved or deleted since", r.path)),
                }
            }
            "delete" => {
                let Some(d) = node_by_id(r.node_id)? else { continue };
                if !d.deleted {
                    skipped.push(format!("{} not restored: no longer deleted", r.path));
                    continue;
                }
                // What this delete took: the files below the path deleted at the same moment.
                let files: Vec<(i64, String, Option<Vec<u8>>)> = Spi::connect(|client| {
                    let rows = client
                        .select(
                            "SELECT id, path, root FROM kb.node WHERE kind = 1 \
                             AND deleted_at = (SELECT deleted_at FROM kb.node WHERE id = $1) \
                             AND (path = $2 OR path LIKE kb._subtree_like($2)) ORDER BY path COLLATE \"C\"",
                            None,
                            &[r.node_id.into(), r.path.as_str().into()],
                        )
                        .map_err(err)?;
                    let mut out = Vec::new();
                    for row in rows {
                        out.push((
                            row.get::<i64>(1).map_err(err)?.unwrap_or(0),
                            row.get::<String>(2).map_err(err)?.unwrap_or_default(),
                            row.get::<Vec<u8>>(3).map_err(err)?,
                        ));
                    }
                    Ok::<_, TextdbError>(out)
                })?;
                if d.kind == 0 && files.is_empty() {
                    if kb::node_by_path(&r.path).is_none() {
                        kb::ensure_folder(&r.path)?;
                        recreated.push(r.path.clone());
                    }
                    continue;
                }
                for (id, path, root) in files {
                    if kb::node_by_path(&path).is_some() {
                        skipped.push(format!("{path} not restored: the path is taken"));
                        continue;
                    }
                    let content = match (span.get(&id), root) {
                        // Created by the batch and deleted again: nothing to bring back.
                        (Some(&(first, _)), _) if first <= 1 => continue,
                        (Some(&(first, _)), _) => content_at(id, first - 1)?,
                        (None, Some(root)) => materialize(&SpiStorage::new(), &to_hash(&root)?)?,
                        (None, None) => continue,
                    };
                    kb::create_impl(&path, &String::from_utf8_lossy(&content), author, Some(&message))?;
                    recreated.push(path);
                }
            }
            "create" | "commit" => {
                if !done.insert(r.node_id) {
                    continue;
                }
                let (first, last) = span[&r.node_id];
                let Some(n) = node_by_id(r.node_id)? else { continue };
                if n.deleted {
                    if !deleted_by_batch(&n.path) {
                        skipped.push(format!("{} not restored: deleted since the batch", n.path));
                    }
                    continue;
                }
                if n.version != last {
                    skipped.push(format!("{} not restored: changed since the batch (v{last}, now v{})", n.path, n.version));
                    continue;
                }
                if first <= 1 {
                    kb::delete_impl(&n.path, author, Some(&message))?;
                    removed.push(n.path.clone());
                    continue;
                }
                let content = content_at(r.node_id, first - 1)?;
                let (version, _) = kb::update_content_impl(&n.path, &String::from_utf8_lossy(&content), Some(n.version), author, Some(&message))?;
                restored.push(json!({ "path": n.path, "version": version }));
            }
            _ => {}
        }
    }
    if !skip_changed && !skipped.is_empty() {
        return Err(TextdbError::InvalidEdit(format!(
            "batch {batch} cannot be reverted cleanly, so nothing was changed: {}. Revert the rest with --skip-changed",
            skipped.join("; ")
        )));
    }
    Ok(json!({ "restored": restored, "removed": removed, "moved_back": moved_back, "recreated": recreated, "skipped": skipped }))
}
