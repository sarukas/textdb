//! `Storage` over the shadow tables. Chunk inserts also feed the FTS5 index, so full-text
//! indexing is insert-only by construction (spec claim 3).

use rusqlite::{params, Connection, OptionalExtension};
use textdb_core::storage::Result;
use textdb_core::{Hash, Node, Storage, TextdbError};

pub struct SqliteStorage<'c> {
    pub conn: &'c Connection,
    pub p: String,
    /// Chunk bytes inserted (new chunks only) since construction.
    pub chunk_bytes_written: u64,
    pub node_bytes_written: u64,
}

pub fn sql_err(e: rusqlite::Error) -> TextdbError {
    TextdbError::Storage(e.to_string())
}

impl<'c> SqliteStorage<'c> {
    pub fn new(conn: &'c Connection, prefix: &str) -> Self {
        SqliteStorage {
            conn,
            p: prefix.to_string(),
            chunk_bytes_written: 0,
            node_bytes_written: 0,
        }
    }

    pub fn now() -> String {
        // ISO-8601 UTC with milliseconds; computed in SQL so it is identical across paths.
        chrono_free_now()
    }
}

fn chrono_free_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let ms = d.subsec_millis();
    // Civil-from-days (Howard Hinnant).
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        ms
    )
}

impl Storage for SqliteStorage<'_> {
    fn get_chunk(&self, h: &Hash) -> Result<Option<Vec<u8>>> {
        self.conn
            .prepare_cached(&format!("SELECT bytes FROM {}chunk WHERE hash = ?1", self.p))
            .map_err(sql_err)?
            .query_row(params![&h[..]], |r| r.get::<_, Vec<u8>>(0))
            .optional()
            .map_err(sql_err)
    }

    fn put_chunk(&mut self, h: &Hash, b: &[u8]) -> Result<()> {
        let nlines = textdb_core::chunker::count_newlines(b) as i64;
        let inserted = self
            .conn
            .prepare_cached(&format!(
                "INSERT OR IGNORE INTO {}chunk(hash, bytes, nlines) VALUES (?1, ?2, ?3)",
                self.p
            ))
            .map_err(sql_err)?
            .execute(params![&h[..], b, nlines])
            .map_err(sql_err)?;
        if inserted == 1 {
            self.chunk_bytes_written += b.len() as u64;
            let id = self.conn.last_insert_rowid();
            let text = String::from_utf8_lossy(b);
            self.conn
                .prepare_cached(&format!("INSERT INTO {}fts(rowid, text) VALUES (?1, ?2)", self.p))
                .map_err(sql_err)?
                .execute(params![id, text.as_ref()])
                .map_err(sql_err)?;
        }
        Ok(())
    }

    fn get_node(&self, h: &Hash) -> Result<Option<Node>> {
        let enc: Option<Vec<u8>> = self
            .conn
            .prepare_cached(&format!("SELECT children FROM {}tree_node WHERE hash = ?1", self.p))
            .map_err(sql_err)?
            .query_row(params![&h[..]], |r| r.get(0))
            .optional()
            .map_err(sql_err)?;
        match enc {
            None => Ok(None),
            Some(e) => Node::decode(&e)
                .map(Some)
                .ok_or_else(|| TextdbError::Storage("corrupt tree node".into())),
        }
    }

    fn put_node(&mut self, h: &Hash, n: &Node) -> Result<()> {
        let enc = n.encode();
        let inserted = self
            .conn
            .prepare_cached(&format!(
                "INSERT OR IGNORE INTO {}tree_node(hash, children) VALUES (?1, ?2)",
                self.p
            ))
            .map_err(sql_err)?
            .execute(params![&h[..], &enc])
            .map_err(sql_err)?;
        if inserted == 1 {
            self.node_bytes_written += enc.len() as u64;
        }
        Ok(())
    }

    fn get_root(&self, file_id: u64) -> Result<Option<(Hash, u64)>> {
        let row: Option<(Option<Vec<u8>>, i64)> = self
            .conn
            .prepare_cached(&format!(
                "SELECT root, version FROM {}node WHERE id = ?1 AND kind = 1 AND deleted_at IS NULL",
                self.p
            ))
            .map_err(sql_err)?
            .query_row(params![file_id as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(sql_err)?;
        Ok(match row {
            Some((Some(root), v)) => {
                let mut h = [0u8; 32];
                if root.len() != 32 {
                    return Err(TextdbError::Storage("bad root length".into()));
                }
                h.copy_from_slice(&root);
                Some((h, v as u64))
            }
            _ => None,
        })
    }

    fn cas_root(&mut self, file_id: u64, expect: Option<&Hash>, new: &Hash) -> Result<bool> {
        let now = Self::now();
        let n = match expect {
            Some(e) => self
                .conn
                .prepare_cached(&format!(
                    "UPDATE {}node SET root = ?1, version = version + 1, updated_at = ?2 WHERE id = ?3 AND root = ?4 AND deleted_at IS NULL",
                    self.p
                ))
                .map_err(sql_err)?
                .execute(params![&new[..], now, file_id as i64, &e[..]])
                .map_err(sql_err)?,
            None => self
                .conn
                .prepare_cached(&format!(
                    "UPDATE {}node SET root = ?1, version = version + 1, updated_at = ?2 WHERE id = ?3 AND root IS NULL AND deleted_at IS NULL",
                    self.p
                ))
                .map_err(sql_err)?
                .execute(params![&new[..], now, file_id as i64])
                .map_err(sql_err)?,
        };
        Ok(n == 1)
    }
}
