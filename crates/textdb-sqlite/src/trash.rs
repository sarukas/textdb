//! Trash: files and folders that were deleted and not yet purged.
//!
//! A delete tombstones a node and everything below it (`deleted_at`, one timestamp for the
//! whole operation) and keeps content and history. Each delete is one trash item: the node it
//! named, found as a tombstone whose parent is live, gone, or deleted at another moment. The
//! entries inside a trashed folder are its children deleted at the same moment.
//!
//! Purging removes the rows for good, then the chunks and tree nodes that no remaining
//! version, HEAD or checkpoint still reaches. Content is shared by hash across files and
//! versions, so what a purge frees is found by walking every remaining root, not by what the
//! purged files happened to reference.

use std::collections::HashSet;
use std::sync::Arc;

use rusqlite::{params, OptionalExtension};
use textdb_core::storage::Result;
use textdb_core::{Hash, Storage, TextdbError};

use crate::db::{CommitRow, NodeRow, TextDb};
use crate::storage::{sql_err, SqliteStorage};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashEntry {
    pub id: i64,
    pub name: String,
    /// 0 folder, 1 file.
    pub kind: i64,
    /// Where it was when it was deleted.
    pub path: String,
    pub version: i64,
    /// A file's size; for a folder, the total of the files deleted with it.
    pub nbytes: i64,
    pub nlines: Option<i64>,
    /// 1 for a file; for a folder, the files deleted with it.
    pub files: i64,
    pub updated_at: String,
    pub updated_by: Option<String>,
    pub deleted_at: String,
    /// Who made the delete this entry went to the trash with.
    pub deleted_by: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PurgeStats {
    /// Trash items removed whole.
    pub items: i64,
    pub files: i64,
    pub folders: i64,
    pub versions: i64,
    pub chunks: i64,
    pub tree_nodes: i64,
    /// Chunk bytes freed: content that nothing remaining shares.
    pub bytes: i64,
}

/// Walk the trees under `roots`, adding every tree node and leaf chunk to the sets. A node
/// already seen is not walked again. `strict` fails on a missing node rather than skipping it.
fn reach(
    st: &SqliteStorage,
    roots: impl IntoIterator<Item = Hash>,
    nodes: &mut HashSet<Hash>,
    leaves: &mut HashSet<Hash>,
    strict: bool,
) -> Result<()> {
    let mut stack: Vec<Hash> = roots.into_iter().collect();
    while let Some(h) = stack.pop() {
        if !nodes.insert(h) {
            continue;
        }
        match st.get_node(&h)? {
            Some(n) => {
                for c in &n.children {
                    if c.is_leaf {
                        leaves.insert(c.hash);
                    } else {
                        stack.push(c.hash);
                    }
                }
            }
            None if strict => return Err(TextdbError::MissingNode(h)),
            None => {}
        }
    }
    Ok(())
}

fn hash_of(v: Vec<u8>) -> Option<Hash> {
    v.try_into().ok()
}

impl<'c> TextDb<'c> {
    /// The trash items, newest delete first; with `parent`, the entries deleted together
    /// inside that trashed folder, folders first.
    pub fn trash(&self, parent: Option<i64>) -> Result<Vec<TrashEntry>> {
        // An account's trash is its own: a delete inside a share it holds. What was deleted
        // elsewhere is not its business, and its `path` is the account's own.
        let mine = |rows: Vec<TrashEntry>| -> Vec<TrashEntry> {
            if self.view.is_admin() {
                return rows;
            }
            rows.into_iter()
                .filter_map(|mut e| {
                    e.path = self.view_path(&e.path)?;
                    Some(e)
                })
                .collect()
        };
        Ok(mine(self.trash_rows(parent)?))
    }

    fn trash_rows(&self, parent: Option<i64>) -> Result<Vec<TrashEntry>> {
        match parent {
            None => self
                .trash_items()?
                .into_iter()
                .map(|n| {
                    let by = self.deleted_by(n.id)?;
                    self.trash_entry_from(n, by)
                })
                .collect(),
            Some(id) => {
                let dir = self.trashed(id)?;
                if dir.kind != 0 {
                    return Err(TextdbError::InvalidEdit(format!("{} is a file", dir.path)));
                }
                let by = self.deleted_by(self.trash_item_of(&dir)?.id)?;
                let rows = self
                    .conn
                    .prepare_cached(&format!(
                        "SELECT {} FROM {}node WHERE parent_id = ?1 AND deleted_at = ?2 ORDER BY kind, name",
                        Self::NODE_COLS,
                        self.p
                    ))
                    .map_err(sql_err)?
                    .query_map(params![dir.id, dir.deleted_at], Self::row_from)
                    .map_err(sql_err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sql_err)?;
                rows.into_iter().map(|n| self.trash_entry_from(n, by.clone())).collect()
            }
        }
    }

    /// One entry of the trash, at any depth.
    pub fn trash_entry(&self, id: i64) -> Result<TrashEntry> {
        let n = self.trashed(id)?;
        let by = self.deleted_by(self.trash_item_of(&n)?.id)?;
        self.trash_entry_from(n, by)
    }

    /// A trashed file's content at `version`, or at the version it was deleted with.
    pub fn trash_read(&self, id: i64, version: Option<u64>) -> Result<(Arc<Vec<u8>>, bool)> {
        let n = self.trashed_file(id)?;
        let root = match version {
            Some(v) => self.root_of_version(n.id, v)?,
            None => match n.root {
                Some(root) => root,
                None => return Ok((Arc::new(Vec::new()), true)),
            },
        };
        self.storage().document(&root)
    }

    pub fn trash_history(&self, id: i64) -> Result<Vec<CommitRow>> {
        let n = self.trashed_file(id)?;
        self.commits_of(n.id)
    }

    /// Remove a trash entry and everything deleted with it inside, for good.
    /// Bring a trash item back where it was, with everything that went to the trash with it.
    ///
    /// The inverse of a delete, and it needs no record of its own: one delete writes one
    /// `deleted_at` across the subtree it named, so what came with this node is exactly the
    /// tombstones below it carrying that same timestamp. A later delete inside the same folder has
    /// its own timestamp and its own trash entry, and stays in the trash.
    ///
    /// Refused rather than merged when something live is at the path already; the folders above it
    /// are made again when they went too. A share root restored this way revives the grants that
    /// name it, because a grant is dormant exactly while its node is deleted (#12 D12).
    pub fn restore(&self, id: i64, author: Option<&str>) -> Result<TrashEntry> {
        let n = self.trashed(id)?;
        let was = self.trash_entry_from(n.clone(), self.deleted_by(self.trash_item_of(&n)?.id)?)?;
        let deleted_at = n.deleted_at.clone().unwrap_or_default();
        if !self.can_write_store_path(&n.path) {
            return Err(TextdbError::Forbidden(format!(
                "{}: restoring is for whoever may write where it was",
                self.view_path(&n.path).unwrap_or_else(|| "that trash entry".to_string())
            )));
        }
        self.tx(|db| {
            if db.node_by_path(&n.path)?.is_some() {
                return Err(TextdbError::InvalidEdit(format!("{} is taken, so {} was not restored", n.path, n.name)));
            }
            let parent = db.ensure_folder_at(crate::db::parent_of(&n.path))?;
            let now = Self::now();
            let (lo, hi) = crate::db::subtree_bounds(&n.path).ok_or_else(|| TextdbError::InvalidEdit("cannot restore the root".into()))?;
            db.conn
                .prepare_cached(&format!(
                    "UPDATE {}node SET deleted_at = NULL, parent_id = CASE WHEN id = ?5 THEN ?6 ELSE parent_id END \
                     WHERE deleted_at = ?1 AND (path = ?2 OR (path >= ?3 AND path < ?4))",
                    db.p
                ))
                .map_err(sql_err)?
                .execute(params![deleted_at, n.path, lo, hi, n.id, parent])
                .map_err(sql_err)?;
            // Counted after the rows are live again, so the totals are what the tree now holds.
            let back = db.subtree_totals(n.id)?;
            db.add_to_ancestors(&n.path, &back, &now, author)?;
            let op = if n.kind == 1 { "create" } else { "mkdir" };
            db.record_change(op, n.id, n.kind, &n.path, None, None, None, None, author, Some("restore from trash"))?;
            if db.has_links()? {
                let files = db.files_at(&n.path)?;
                let names: Vec<String> = files.iter().map(|(_, p)| crate::links::name_key(p)).collect();
                db.relink(&names, &files.iter().map(|(id, _)| *id).collect::<Vec<_>>())?;
            }
            // What came back, as the trash listed it, with the delete that is now undone cleared.
            Ok(TrashEntry { deleted_at: String::new(), deleted_by: None, ..was })
        })
    }

    pub fn purge(&self, id: i64, author: Option<&str>) -> Result<PurgeStats> {
        self.tx(|db| {
            let n = db.trashed(id)?;
            let whole = db.trash_item_of(&n)?.id == n.id;
            let mut stats = db.purge_nodes(&db.trash_subtree(n.id)?)?;
            stats.items = whole as i64;
            db.record_change("purge", n.id, n.kind, &n.path, None, None, None, None, author, None)?;
            Ok(stats)
        })
    }

    /// Purge every trash item.
    pub fn empty_trash(&self, author: Option<&str>) -> Result<PurgeStats> {
        self.tx(|db| {
            let items = db.trash_items()?;
            let nodes: Vec<(i64, i64)> = db
                .conn
                .prepare_cached(&format!("SELECT id, kind FROM {}node WHERE deleted_at IS NOT NULL", db.p))
                .map_err(sql_err)?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<_>>()
                .map_err(sql_err)?;
            let mut stats = db.purge_nodes(&nodes)?;
            stats.items = items.len() as i64;
            for n in &items {
                db.record_change("purge", n.id, n.kind, &n.path, None, None, None, None, author, None)?;
            }
            Ok(stats)
        })
    }

    fn trash_items(&self) -> Result<Vec<NodeRow>> {
        let cols = Self::NODE_COLS.split(", ").map(|c| format!("n.{c}")).collect::<Vec<_>>().join(", ");
        self.conn
            .prepare_cached(&format!(
                "SELECT {cols} FROM {p}node n LEFT JOIN {p}node up ON up.id = n.parent_id \
                 WHERE n.deleted_at IS NOT NULL \
                   AND (up.id IS NULL OR up.deleted_at IS NULL OR up.deleted_at <> n.deleted_at) \
                 ORDER BY (SELECT max(c.seq) FROM {p}change c WHERE c.node_id = n.id AND c.op = 'delete') DESC, n.path",
                p = self.p
            ))
            .map_err(sql_err)?
            .query_map([], Self::row_from)
            .map_err(sql_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(sql_err)
    }

    fn trashed(&self, id: i64) -> Result<NodeRow> {
        match self.node_by_id(id)? {
            Some(n) if n.deleted_at.is_some() => Ok(n),
            _ => Err(TextdbError::NotFound(format!("trash entry {}", id))),
        }
    }

    fn trashed_file(&self, id: i64) -> Result<NodeRow> {
        let n = self.trashed(id)?;
        if n.kind != 1 {
            return Err(TextdbError::InvalidEdit(format!("{} is a folder", n.path)));
        }
        Ok(n)
    }

    /// The trash item `n` belongs to: its highest ancestor deleted in the same operation.
    fn trash_item_of(&self, n: &NodeRow) -> Result<NodeRow> {
        let mut item = n.clone();
        while let Some(parent) = item.parent_id {
            match self.node_by_id(parent)? {
                Some(up) if up.deleted_at.is_some() && up.deleted_at == item.deleted_at => item = up,
                _ => break,
            }
        }
        Ok(item)
    }

    fn deleted_by(&self, item_id: i64) -> Result<Option<String>> {
        let author: Option<Option<String>> = self
            .conn
            .prepare_cached(&format!(
                "SELECT author FROM {}change WHERE node_id = ?1 AND op = 'delete' ORDER BY seq DESC LIMIT 1",
                self.p
            ))
            .map_err(sql_err)?
            .query_row(params![item_id], |r| r.get(0))
            .optional()
            .map_err(sql_err)?;
        Ok(author.flatten())
    }

    /// `id` and every node deleted with it below, as `(id, kind)`.
    fn trash_subtree(&self, id: i64) -> Result<Vec<(i64, i64)>> {
        self.conn
            .prepare_cached(&format!(
                "WITH RECURSIVE sub(id, kind, deleted_at) AS ( \
                   SELECT id, kind, deleted_at FROM {p}node WHERE id = ?1 AND deleted_at IS NOT NULL \
                   UNION ALL \
                   SELECT n.id, n.kind, n.deleted_at FROM {p}node n JOIN sub ON n.parent_id = sub.id \
                   WHERE n.deleted_at = sub.deleted_at \
                 ) SELECT id, kind FROM sub",
                p = self.p
            ))
            .map_err(sql_err)?
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(sql_err)
    }

    fn trash_entry_from(&self, n: NodeRow, deleted_by: Option<String>) -> Result<TrashEntry> {
        let (files, nbytes) = if n.kind == 1 {
            (1, n.nbytes.unwrap_or(0))
        } else {
            let ids: Vec<i64> = self.trash_subtree(n.id)?.into_iter().filter(|(_, k)| *k == 1).map(|(id, _)| id).collect();
            let nbytes: i64 = self
                .conn
                .prepare_cached(&format!(
                    "SELECT coalesce(sum(nbytes), 0) FROM {}node WHERE id IN (SELECT value FROM json_each(?1))",
                    self.p
                ))
                .map_err(sql_err)?
                .query_row(params![serde_json::to_string(&ids).unwrap_or_default()], |r| r.get(0))
                .map_err(sql_err)?;
            (ids.len() as i64, nbytes)
        };
        Ok(TrashEntry {
            id: n.id,
            name: n.name,
            kind: n.kind,
            path: n.path,
            version: n.version,
            nbytes,
            nlines: n.nlines,
            files,
            updated_at: n.updated_at,
            updated_by: n.updated_by,
            deleted_at: n.deleted_at.unwrap_or_default(),
            deleted_by,
        })
    }

    /// Delete the rows of tombstoned `nodes` (`(id, kind)`) and the content only they reached.
    fn purge_nodes(&self, nodes: &[(i64, i64)]) -> Result<PurgeStats> {
        let p = &self.p;
        let json = |ids: &[i64]| serde_json::to_string(ids).unwrap_or_default();
        let files: Vec<i64> = nodes.iter().filter(|(_, k)| *k == 1).map(|(id, _)| *id).collect();
        let folders: Vec<i64> = nodes.iter().filter(|(_, k)| *k != 1).map(|(id, _)| *id).collect();
        let all: Vec<i64> = nodes.iter().map(|(id, _)| *id).collect();
        let (files_json, folders_json, all_json) = (json(&files), json(&folders), json(&all));
        let mut stats = PurgeStats { files: files.len() as i64, folders: folders.len() as i64, ..Default::default() };

        // Every root the purged files reach, collected before their rows go.
        let mut gone_roots: HashSet<Hash> = HashSet::new();
        for sql in [
            format!("SELECT root FROM {p}commit WHERE file_id IN (SELECT value FROM json_each(?1))"),
            format!("SELECT root FROM {p}checkpoint WHERE file_id IN (SELECT value FROM json_each(?1))"),
            format!("SELECT root FROM {p}node WHERE root IS NOT NULL AND id IN (SELECT value FROM json_each(?1))"),
        ] {
            let roots = self
                .conn
                .prepare_cached(&sql)
                .map_err(sql_err)?
                .query_map(params![files_json], |r| r.get::<_, Vec<u8>>(0))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            gone_roots.extend(roots.into_iter().filter_map(hash_of));
        }

        let run = |sql: String, arg: &str| -> Result<usize> {
            self.conn.prepare_cached(&sql).map_err(sql_err)?.execute(params![arg]).map_err(sql_err)
        };
        stats.versions = run(format!("DELETE FROM {p}commit WHERE file_id IN (SELECT value FROM json_each(?1))"), &files_json)? as i64;
        for table in ["section", "link", "frontmatter", "checkpoint", "chunk_ref", "file_author"] {
            run(format!("DELETE FROM {p}{table} WHERE file_id IN (SELECT value FROM json_each(?1))"), &files_json)?;
        }
        run(format!("DELETE FROM {p}path_event WHERE node_id IN (SELECT value FROM json_each(?1))"), &all_json)?;
        // A tombstone left inside a purged folder (deleted earlier, on its own) stays in the
        // trash as its own item, now without a parent.
        run(format!("UPDATE {p}node SET parent_id = NULL WHERE parent_id IN (SELECT value FROM json_each(?1))"), &folders_json)?;
        run(
            format!("DELETE FROM {p}node WHERE deleted_at IS NOT NULL AND id IN (SELECT value FROM json_each(?1))"),
            &all_json,
        )?;

        if gone_roots.is_empty() {
            return Ok(stats);
        }
        let st = self.storage();
        let (mut gone_nodes, mut gone_leaves) = (HashSet::new(), HashSet::new());
        reach(&st, gone_roots, &mut gone_nodes, &mut gone_leaves, false)?;

        // What everything that remains still reaches. A missing node here means the store is
        // already damaged; freeing content on an incomplete picture could make it worse.
        let mut live_roots: HashSet<Hash> = HashSet::new();
        for sql in [
            format!("SELECT root FROM {p}commit"),
            format!("SELECT root FROM {p}checkpoint"),
            format!("SELECT root FROM {p}node WHERE root IS NOT NULL"),
        ] {
            let roots = self
                .conn
                .prepare_cached(&sql)
                .map_err(sql_err)?
                .query_map([], |r| r.get::<_, Vec<u8>>(0))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            live_roots.extend(roots.into_iter().filter_map(hash_of));
        }
        let (mut live_nodes, mut live_leaves) = (HashSet::new(), HashSet::new());
        reach(&st, live_roots, &mut live_nodes, &mut live_leaves, true)?;

        let mut drop_node = self
            .conn
            .prepare_cached(&format!("DELETE FROM {p}tree_node WHERE hash = ?1"))
            .map_err(sql_err)?;
        for h in gone_nodes.difference(&live_nodes) {
            stats.tree_nodes += drop_node.execute(params![&h[..]]).map_err(sql_err)? as i64;
        }
        for h in gone_leaves.difference(&live_leaves) {
            let row: Option<(i64, Vec<u8>)> = self
                .conn
                .prepare_cached(&format!("SELECT id, bytes FROM {p}chunk WHERE hash = ?1"))
                .map_err(sql_err)?
                .query_row(params![&h[..]], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
                .map_err(sql_err)?;
            let Some((id, bytes)) = row else { continue };
            // The index is contentless: removing a row means handing back the text it was
            // given, which `put_chunk` derived the same way.
            self.conn
                .prepare_cached(&format!("INSERT INTO {p}fts({p}fts, rowid, text) VALUES ('delete', ?1, ?2)"))
                .map_err(sql_err)?
                .execute(params![id, String::from_utf8_lossy(&bytes).as_ref()])
                .map_err(sql_err)?;
            for sql in [format!("DELETE FROM {p}chunk_ref WHERE chunk_id = ?1"), format!("DELETE FROM {p}chunk WHERE id = ?1")] {
                self.conn.prepare_cached(&sql).map_err(sql_err)?.execute(params![id]).map_err(sql_err)?;
            }
            stats.chunks += 1;
            stats.bytes += bytes.len() as i64;
        }
        Ok(stats)
    }
}
