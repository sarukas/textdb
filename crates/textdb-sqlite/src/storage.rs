//! `Storage` over the shadow tables. Chunk inserts also feed the FTS5 index, so full-text
//! indexing is insert-only by construction (spec claim 3).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};
use textdb_core::storage::Result;
use textdb_core::{Hash, Node, Storage, TextdbError};

/// Content-addressed caches for chunks and tree nodes.
///
/// Materialising a document walks the tree and reads one row per leaf, so a 1 MiB file
/// costs ~128 statement round-trips where a single-column baseline costs one. The entries
/// are keyed by BLAKE3 hash, which *is* the content: an entry can never go stale, needs no
/// invalidation, and stays valid across connections and databases. Nothing in the schema
/// deletes a chunk or a node, so a hit can never outlive its row either.
///
/// Thread-local because `SqliteStorage` is constructed per operation and a `Connection` is
/// not shared between threads; each writer thread warms its own.
const CHUNK_BUDGET: usize = 96 << 20;
const NODE_BUDGET: usize = 16 << 20;
const DOC_BUDGET: usize = 64 << 20;
/// Documents above this are not cached: a single one would evict everything else, and the
/// tests that use documents this large read each of them once.
const DOC_MAX: usize = 8 << 20;

struct Lru<V> {
    map: HashMap<Hash, (u64, usize, V)>,
    bytes: usize,
    budget: usize,
    tick: u64,
}

impl<V: Clone> Lru<V> {
    fn new(budget: usize) -> Self {
        Lru {
            map: HashMap::new(),
            bytes: 0,
            budget,
            tick: 0,
        }
    }

    fn get(&mut self, h: &Hash) -> Option<V> {
        self.tick += 1;
        let tick = self.tick;
        let e = self.map.get_mut(h)?;
        e.0 = tick;
        Some(e.2.clone())
    }

    fn put(&mut self, h: Hash, size: usize, v: V) {
        self.tick += 1;
        if let Some(old) = self.map.insert(h, (self.tick, size, v)) {
            self.bytes -= old.1;
        }
        self.bytes += size;
        if self.bytes > self.budget {
            // Drop the coldest entries in one pass rather than on every insert.
            let mut by_age: Vec<(u64, Hash)> = self.map.iter().map(|(k, v)| (v.0, *k)).collect();
            by_age.sort_unstable();
            let target = self.budget * 3 / 4;
            for (_, k) in by_age {
                if self.bytes <= target {
                    break;
                }
                if let Some(old) = self.map.remove(&k) {
                    self.bytes -= old.1;
                }
            }
        }
    }
}

thread_local! {
    static CHUNKS: RefCell<Lru<Arc<Vec<u8>>>> = RefCell::new(Lru::new(CHUNK_BUDGET));
    static NODES: RefCell<Lru<Node>> = RefCell::new(Lru::new(NODE_BUDGET));
    /// Whole documents, keyed by root hash. The root hash covers the entire document, so
    /// this is the same content-addressed argument as the chunk cache, one level up: a
    /// document that has not been rewritten needs neither the tree walk nor the
    /// concatenation. The flag records whether the bytes are UTF-8, which is a property of
    /// the content and so equally cacheable — it saves re-validating megabytes on a read
    /// that only wants to hand the text back.
    static DOCS: RefCell<Lru<(Arc<Vec<u8>>, bool)>> = RefCell::new(Lru::new(DOC_BUDGET));
}

/// Connections whose statement cache may be shared, keyed by SQLite handle.
///
/// Scalar SQL functions get a fresh `Connection` from `sqlite3_context_db_handle` on every
/// call, so everything they prepare is compiled from scratch: 13 us of the 20 us
/// `textdb_content` took on an 8 KiB document, against 7 us for the same read through the
/// virtual table, which keeps its own handle. They cannot simply hold one themselves —
/// a `Connection` captured by a function closure is released only *after* `sqlite3_close`
/// checks for unfinalized statements, so its cached statements would make `close` return
/// SQLITE_BUSY and leave the database file locked.
///
/// A virtual table's handle has no such problem: `sqlite3_close` calls `disconnectAllVtab`
/// before that check, explicitly so a vtab implementation can hold statements. So the
/// tables register their handle here when they connect and unregister when they disconnect,
/// and the scalar functions borrow it when one is registered — which is whenever a `kb`
/// table exists, the only way the shadow tables are meant to be reached. With no table
/// registered they fall back to a per-call handle and merely stay slow.
///
/// Thread-local, because a `Connection` is not shared between threads: a handle driven from
/// a thread that did not register it finds nothing and takes the fallback.
mod shared {
    use std::cell::RefCell;
    use std::rc::Rc;

    use rusqlite::{ffi, Connection};

    struct Entry {
        handle: *mut ffi::sqlite3,
        conn: Rc<Connection>,
        /// Number of live registrants; two `kb` tables with different prefixes on one
        /// connection both register, and the handle stays shared until the last disconnects.
        holders: usize,
    }

    thread_local! {
        static CONNS: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
    }

    /// Register `handle` and return the shared connection for it.
    ///
    /// # Safety
    /// `handle` must be an open SQLite connection, and the caller must call `release` with
    /// the same handle before that connection closes. Virtual tables satisfy this by
    /// registering in `xConnect` and releasing in `xDisconnect`.
    pub unsafe fn register(handle: *mut ffi::sqlite3) -> rusqlite::Result<Rc<Connection>> {
        CONNS.with(|c| {
            let mut v = c.borrow_mut();
            if let Some(e) = v.iter_mut().find(|e| e.handle == handle) {
                e.holders += 1;
                return Ok(Rc::clone(&e.conn));
            }
            let conn = Rc::new(unsafe { Connection::from_handle(handle) }?);
            v.push(Entry {
                handle,
                conn: Rc::clone(&conn),
                holders: 1,
            });
            Ok(conn)
        })
    }

    /// Drop one registration. At zero the shared connection goes, finalizing its statements.
    pub fn release(handle: *mut ffi::sqlite3) {
        let _ = CONNS.try_with(|c| {
            let mut v = c.borrow_mut();
            if let Some(i) = v.iter().position(|e| e.handle == handle) {
                v[i].holders -= 1;
                if v[i].holders == 0 {
                    v.swap_remove(i);
                }
            }
        });
    }

    /// The shared connection for `handle`, if a virtual table has registered it.
    pub fn get(handle: *mut ffi::sqlite3) -> Option<Rc<Connection>> {
        CONNS
            .try_with(|c| c.borrow().iter().find(|e| e.handle == handle).map(|e| Rc::clone(&e.conn)))
            .ok()
            .flatten()
    }
}

pub use shared::{get as shared_conn, register as register_shared_conn, release as release_shared_conn};

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

    /// Chunk bytes for `h`, from the thread's cache when present.
    fn chunk_cached(&self, h: &Hash) -> Result<Option<Arc<Vec<u8>>>> {
        if let Some(b) = CHUNKS.with(|c| c.borrow_mut().get(h)) {
            return Ok(Some(b));
        }
        let got: Option<Vec<u8>> = self
            .conn
            .prepare_cached(&format!("SELECT bytes FROM {}chunk WHERE hash = ?1", self.p))
            .map_err(sql_err)?
            .query_row(params![&h[..]], |r| r.get::<_, Vec<u8>>(0))
            .optional()
            .map_err(sql_err)?;
        Ok(got.map(|b| {
            let a = Arc::new(b);
            CHUNKS.with(|c| c.borrow_mut().put(*h, a.len(), a.clone()));
            a
        }))
    }

    /// The whole document under `root`, with whether it is valid UTF-8.
    pub fn document(&self, root: &Hash) -> Result<(Arc<Vec<u8>>, bool)> {
        if let Some(d) = DOCS.with(|c| c.borrow_mut().get(root)) {
            return Ok(d);
        }
        let bytes = textdb_core::materialize(self, root)?;
        let utf8 = std::str::from_utf8(&bytes).is_ok();
        let entry = (Arc::new(bytes), utf8);
        if entry.0.len() <= DOC_MAX {
            DOCS.with(|c| c.borrow_mut().put(*root, entry.0.len(), entry.clone()));
        }
        Ok(entry)
    }

    /// Record bytes the caller already holds as the content of `root`.
    ///
    /// A write knows the new content before it builds the tree for it, but nothing used to
    /// tell the cache, so the very next reader of that root — `record_commit`, extracting
    /// markdown structure, and then whoever reads the file back — walked the tree and
    /// concatenated every chunk to rebuild bytes that were in hand a moment earlier.
    ///
    /// The caller must pass exactly the bytes `root` was built from. Every caller here does
    /// so immediately after `build_with_chunks` on the same buffer; `debug_assert` checks it
    /// in test builds, where the hash is cheap next to the rest of the suite.
    pub fn remember_document(&self, root: &Hash, bytes: &[u8]) {
        // Checked against the tree's own totals rather than by re-chunking: one cached node
        // fetch, no hashing, and it catches the mistake that could actually happen — a
        // caller passing the buffer for a different root. Debug only; a release build takes
        // the caller at its word, as `put_chunk` already does.
        debug_assert!(
            textdb_core::tree::totals(self, root).map(|(n, _)| n as usize).ok() == Some(bytes.len()),
            "remember_document: {} bytes do not match the length of the tree under this root",
            bytes.len()
        );
        if bytes.len() > DOC_MAX {
            return;
        }
        let utf8 = std::str::from_utf8(bytes).is_ok();
        let entry = (Arc::new(bytes.to_vec()), utf8);
        DOCS.with(|c| c.borrow_mut().put(*root, entry.0.len(), entry));
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
        Ok(self.chunk_cached(h)?.map(|b| (*b).clone()))
    }

    fn chunk_shared(&self, h: &Hash) -> Result<Arc<Vec<u8>>> {
        self.chunk_cached(h)?.ok_or(TextdbError::MissingChunk(*h))
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
        CHUNKS.with(|c| c.borrow_mut().put(*h, b.len(), Arc::new(b.to_vec())));
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
        if let Some(n) = NODES.with(|c| c.borrow_mut().get(h)) {
            return Ok(Some(n));
        }
        let enc: Option<Vec<u8>> = self
            .conn
            .prepare_cached(&format!("SELECT children FROM {}tree_node WHERE hash = ?1", self.p))
            .map_err(sql_err)?
            .query_row(params![&h[..]], |r| r.get(0))
            .optional()
            .map_err(sql_err)?;
        match enc {
            None => Ok(None),
            Some(e) => {
                let n = Node::decode(&e).ok_or_else(|| TextdbError::Storage("corrupt tree node".into()))?;
                NODES.with(|c| c.borrow_mut().put(*h, e.len(), n.clone()));
                Ok(Some(n))
            }
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
        NODES.with(|c| c.borrow_mut().put(*h, enc.len(), n.clone()));
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
