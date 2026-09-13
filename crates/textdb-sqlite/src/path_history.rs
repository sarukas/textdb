//! Path history and store settings.
//!
//! Renames, moves and deletes are recorded per node in `{p}path_event` (see
//! `textdb_core::path`) while path history is on: this handle's own choice if it made one
//! ([`TextDb::with_path_history`]), else the store's `path_history` setting, else on.

use rusqlite::{params, OptionalExtension};
use textdb_core::path::{parse_switch, PathOp, PATH_HISTORY_DEFAULT, PATH_HISTORY_SETTING};
use textdb_core::storage::Result;
use textdb_core::TextdbError;

use crate::db::{normalize_path, subtree_bounds, NodeRow, TextDb};
use crate::storage::sql_err;

/// A rename, move or delete as it touched one file or folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathEventRow {
    pub id: i64,
    pub ts: String,
    /// `rename`, `move` or `delete`.
    pub op: String,
    pub old_path: String,
    /// Where it went; `None` for a delete.
    pub new_path: Option<String>,
    /// The folder the operation named, when this node went along with it.
    pub via: Option<String>,
    /// A file's version when it happened.
    pub version: Option<i64>,
    pub author: Option<String>,
}

/// The settings a store knows.
const SETTINGS: &[&str] = &[PATH_HISTORY_SETTING];

fn known(key: &str) -> Result<()> {
    if SETTINGS.contains(&key) {
        Ok(())
    } else {
        Err(TextdbError::InvalidEdit(format!("unknown setting '{key}' (known: {})", SETTINGS.join(", "))))
    }
}

impl<'c> TextDb<'c> {
    /// A store setting's value, `None` when it is at its default.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        known(key)?;
        self.conn
            .prepare_cached(&format!("SELECT value FROM {}setting WHERE key = ?1", self.p))
            .map_err(sql_err)?
            .query_row(params![key], |r| r.get(0))
            .optional()
            .map_err(sql_err)
    }

    /// Set a store setting, or with `None` return it to its default.
    pub fn set_setting(&self, key: &str, value: Option<&str>) -> Result<()> {
        known(key)?;
        match value {
            Some(v) => {
                let on = parse_switch(v).ok_or_else(|| TextdbError::InvalidEdit(format!("{key} is on or off, not '{v}'")))?;
                self.conn
                    .prepare_cached(&format!(
                        "INSERT INTO {}setting(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        self.p
                    ))
                    .map_err(sql_err)?
                    .execute(params![key, if on { "on" } else { "off" }])
                    .map_err(sql_err)?;
            }
            None => {
                self.conn
                    .prepare_cached(&format!("DELETE FROM {}setting WHERE key = ?1", self.p))
                    .map_err(sql_err)?
                    .execute(params![key])
                    .map_err(sql_err)?;
            }
        }
        Ok(())
    }

    /// Whether this handle records renames, moves and deletes.
    pub fn path_history_enabled(&self) -> Result<bool> {
        if let Some(on) = self.path_history {
            return Ok(on);
        }
        Ok(self
            .setting(PATH_HISTORY_SETTING)?
            .as_deref()
            .and_then(parse_switch)
            .unwrap_or(PATH_HISTORY_DEFAULT))
    }

    /// Record `op` for `node` and, for a folder, every live node below it. Call before the
    /// paths change: the rows keep the paths as they were.
    pub(crate) fn record_path_events(
        &self,
        op: PathOp,
        node: &NodeRow,
        to: Option<&str>,
        author: Option<&str>,
        change_seq: i64,
        ts: &str,
    ) -> Result<()> {
        let p = &self.p;
        if node.kind == 0 {
            if let Some((lo, hi)) = subtree_bounds(&node.path) {
                self.conn
                    .prepare_cached(&format!(
                        "INSERT INTO {p}path_event(node_id, node_kind, op, ts, author, old_path, new_path, via, version, change_seq) \
                         SELECT id, kind, ?1, ?2, ?3, path, \
                                CASE WHEN ?4 IS NULL THEN NULL ELSE ?4 || substr(path, length(?5) + 1) END, \
                                ?5, CASE WHEN kind = 1 THEN version END, ?6 \
                         FROM {p}node WHERE path >= ?7 AND path < ?8 AND deleted_at IS NULL"
                    ))
                    .map_err(sql_err)?
                    .execute(params![op.as_str(), ts, author, to, node.path, change_seq, lo, hi])
                    .map_err(sql_err)?;
            }
        }
        self.conn
            .prepare_cached(&format!(
                "INSERT INTO {p}path_event(node_id, node_kind, op, ts, author, old_path, new_path, via, version, change_seq) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9)"
            ))
            .map_err(sql_err)?
            .execute(params![
                node.id,
                node.kind,
                op.as_str(),
                ts,
                author,
                node.path,
                to,
                (node.kind == 1).then_some(node.version),
                change_seq
            ])
            .map_err(sql_err)?;
        Ok(())
    }

    /// Renames, moves and deletes of the file or folder at `path` (the live one, else the one
    /// most recently deleted there), oldest first.
    pub fn path_history(&self, path: &str) -> Result<Vec<PathEventRow>> {
        let path = normalize_path(path)?;
        let n = self.node_by_path_any(&path)?.ok_or_else(|| TextdbError::NotFound(path.clone()))?;
        self.path_history_of(n.id)
    }

    /// As [`path_history`](Self::path_history), for the node with id `node_id` — how a trashed
    /// entry is addressed.
    pub fn path_history_of(&self, node_id: i64) -> Result<Vec<PathEventRow>> {
        self.conn
            .prepare_cached(&format!(
                "SELECT id, ts, op, old_path, new_path, via, version, author FROM {}path_event WHERE node_id = ?1 ORDER BY id",
                self.p
            ))
            .map_err(sql_err)?
            .query_map(params![node_id], |r| {
                Ok(PathEventRow {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    op: r.get(2)?,
                    old_path: r.get(3)?,
                    new_path: r.get(4)?,
                    via: r.get(5)?,
                    version: r.get(6)?,
                    author: r.get(7)?,
                })
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(sql_err)
    }
}
