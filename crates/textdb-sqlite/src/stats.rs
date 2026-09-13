//! The figures a folder listing sorts by: each file's word count and authors, and each
//! folder's totals over everything below it. They are kept current in the transaction of
//! every commit, mkdir, move and delete, so a listing reads them off the node rows instead of
//! aggregating a subtree per row.

use std::collections::HashMap;

use rusqlite::{params, Connection};
use textdb_core::path::ancestors;
use textdb_core::storage::Result;
use textdb_core::words_at;

use crate::db::{to_hash, TextDb};
use crate::storage::sql_err;

/// What a node adds to every folder above it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub files: i64,
    pub folders: i64,
    pub bytes: i64,
    pub lines: i64,
    pub words: i64,
    pub versions: i64,
}

impl Totals {
    pub fn neg(self) -> Self {
        Totals {
            files: -self.files,
            folders: -self.folders,
            bytes: -self.bytes,
            lines: -self.lines,
            words: -self.words,
            versions: -self.versions,
        }
    }

    fn add(&mut self, o: &Totals) {
        self.files += o.files;
        self.folders += o.folders;
        self.bytes += o.bytes;
        self.lines += o.lines;
        self.words += o.words;
        self.versions += o.versions;
    }
}

impl TextDb<'_> {
    /// Add `t` to the totals of every live folder above `path` and mark them changed at `ts`.
    pub(crate) fn add_to_ancestors(&self, path: &str, t: &Totals, ts: &str) -> Result<()> {
        let list = serde_json::to_string(&ancestors(path)).expect("paths serialize");
        self.conn
            .prepare_cached(&format!(
                "UPDATE {}node SET t_files = t_files + ?2, t_folders = t_folders + ?3, t_bytes = t_bytes + ?4, \
                 t_lines = t_lines + ?5, t_words = t_words + ?6, t_versions = t_versions + ?7, \
                 t_updated_at = max(coalesce(t_updated_at, ''), ?8) \
                 WHERE path IN (SELECT value FROM json_each(?1)) AND deleted_at IS NULL",
                self.p
            ))
            .map_err(sql_err)?
            .execute(params![list, t.files, t.folders, t.bytes, t.lines, t.words, t.versions, ts])
            .map_err(sql_err)?;
        Ok(())
    }

    /// What the node `id` and everything below it add to the folders above: a file counts
    /// itself, a folder its totals plus itself.
    pub(crate) fn subtree_totals(&self, id: i64) -> Result<Totals> {
        self.conn
            .prepare_cached(&format!(
                "SELECT CASE kind WHEN 1 THEN 1 ELSE t_files END, CASE kind WHEN 1 THEN 0 ELSE t_folders + 1 END, \
                 CASE kind WHEN 1 THEN coalesce(nbytes, 0) ELSE t_bytes END, CASE kind WHEN 1 THEN coalesce(nlines, 0) ELSE t_lines END, \
                 CASE kind WHEN 1 THEN coalesce(nwords, 0) ELSE t_words END, CASE kind WHEN 1 THEN version ELSE t_versions END \
                 FROM {}node WHERE id = ?1",
                self.p
            ))
            .map_err(sql_err)?
            .query_row(params![id], |r| {
                Ok(Totals {
                    files: r.get(0)?,
                    folders: r.get(1)?,
                    bytes: r.get(2)?,
                    lines: r.get(3)?,
                    words: r.get(4)?,
                    versions: r.get(5)?,
                })
            })
            .map_err(sql_err)
    }
}

/// Compute word counts, authors and folder totals for a store whose rows predate them. Reads
/// every file's current content once; run by [`crate::schema::migrate`] when it adds the
/// columns, inside its savepoint.
pub fn backfill(conn: &Connection, p: &str) -> Result<()> {
    let db = TextDb::attach(conn, p, false);
    let st = db.storage();
    let files: Vec<(i64, Vec<u8>)> = {
        let mut stmt = conn
            .prepare(&format!("SELECT id, root FROM {p}node WHERE kind = 1 AND root IS NOT NULL"))
            .map_err(sql_err)?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)?
    };
    {
        let mut set = conn.prepare(&format!("UPDATE {p}node SET nwords = ?1 WHERE id = ?2")).map_err(sql_err)?;
        for (id, root) in files {
            let words = words_at(&st, &to_hash(&root)?)?;
            set.execute(params![words as i64, id]).map_err(sql_err)?;
        }
    }
    conn.execute_batch(&format!(
        "DELETE FROM {p}file_author;
         INSERT INTO {p}file_author(file_id, author, commits, first_ts, last_ts)
           SELECT file_id, coalesce(author, ''), count(*), min(ts), max(ts) FROM {p}commit GROUP BY file_id, coalesce(author, '');
         UPDATE {p}node SET nauthors = (SELECT count(*) FROM {p}file_author a WHERE a.file_id = {p}node.id) WHERE kind = 1;"
    ))
    .map_err(sql_err)?;

    let sums = folder_sums(conn, p).map_err(sql_err)?;
    let mut set = conn
        .prepare(&format!(
            "UPDATE {p}node SET t_files = ?2, t_folders = ?3, t_bytes = ?4, t_lines = ?5, t_words = ?6, t_versions = ?7, \
             t_updated_at = nullif(?8, '') WHERE path = ?1 AND deleted_at IS NULL"
        ))
        .map_err(sql_err)?;
    for (path, (t, ts)) in sums {
        set.execute(params![path, t.files, t.folders, t.bytes, t.lines, t.words, t.versions, ts])
            .map_err(sql_err)?;
    }
    Ok(())
}

/// Totals and the latest change for every folder that has anything live below it.
fn folder_sums(conn: &Connection, p: &str) -> rusqlite::Result<HashMap<String, (Totals, String)>> {
    let mut sums: HashMap<String, (Totals, String)> = HashMap::new();
    let mut stmt = conn.prepare(&format!(
        "SELECT path, kind, coalesce(nbytes, 0), coalesce(nlines, 0), coalesce(nwords, 0), version, updated_at \
         FROM {p}node WHERE deleted_at IS NULL AND path <> '/'"
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let path: String = r.get(0)?;
        let kind: i64 = r.get(1)?;
        let updated_at: String = r.get(6)?;
        let t = if kind == 1 {
            Totals {
                files: 1,
                bytes: r.get(2)?,
                lines: r.get(3)?,
                words: r.get(4)?,
                versions: r.get(5)?,
                ..Totals::default()
            }
        } else {
            Totals {
                folders: 1,
                ..Totals::default()
            }
        };
        for a in ancestors(&path) {
            let e = sums.entry(a.to_string()).or_default();
            e.0.add(&t);
            if updated_at > e.1 {
                e.1.clone_from(&updated_at);
            }
        }
    }
    Ok(sums)
}
