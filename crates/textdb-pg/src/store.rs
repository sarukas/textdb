//! `Storage` over SPI and small namespace helpers.

use pgrx::prelude::*;
use textdb_core::storage::Result;
use textdb_core::{Hash, Node, Storage, TextdbError};

pub fn spi_err(e: pgrx::spi::Error) -> ! {
    raise("TX000", &format!("storage error: {}", e), "")
}

/// Raise a Postgres error with a custom SQLSTATE through the PL/pgSQL helper.
pub fn raise(code: &str, msg: &str, detail: &str) -> ! {
    // The SPI call itself raises; if it somehow returns, fall back to a plain error.
    let _ = Spi::run_with_args("SELECT kb._raise($1, $2, $3)", &[code.into(), msg.into(), detail.into()]);
    pgrx::error!("{} {}", code, msg);
}

pub fn to_hash(v: &[u8]) -> Result<Hash> {
    if v.len() != 32 {
        return Err(TextdbError::Storage("bad hash length".into()));
    }
    let mut h = [0u8; 32];
    h.copy_from_slice(v);
    Ok(h)
}

pub fn normalize_path(p: &str) -> Result<String> {
    if p.contains('\0') {
        return Err(TextdbError::InvalidEdit("path contains NUL".into()));
    }
    let mut segs = Vec::new();
    for seg in p.split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg == "." || seg == ".." {
            return Err(TextdbError::InvalidEdit(format!("invalid path segment '{}' in {}", seg, p)));
        }
        segs.push(seg);
    }
    Ok(format!("/{}", segs.join("/")))
}

pub fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

pub fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

#[derive(Clone, Debug)]
pub struct NodeRow {
    pub id: i64,
    pub kind: i16,
    pub path: String,
    pub root: Option<Hash>,
    pub version: i64,
}

impl NodeRow {
    /// Live node at `path`; with `any`, fall back to the most recently deleted one.
    pub fn by_path(path: &str, any: bool) -> Option<NodeRow> {
        let live = Self::query("SELECT id, kind, path, root, version FROM kb.node WHERE path = $1 AND deleted_at IS NULL", path);
        if live.is_some() || !any {
            return live;
        }
        Self::query(
            "SELECT id, kind, path, root, version FROM kb.node WHERE path = $1 ORDER BY deleted_at DESC LIMIT 1",
            path,
        )
    }

    fn query(sql: &str, path: &str) -> Option<NodeRow> {
        Spi::connect(|client| {
            let t = client.select(sql, Some(1), &[path.into()]).unwrap_or_else(|e| spi_err(e));
            let mut out = None;
            for r in t {
                let root: Option<Vec<u8>> = r.get(4).unwrap_or_else(|e| spi_err(e));
                out = Some(NodeRow {
                    id: r.get::<i64>(1).unwrap_or_else(|e| spi_err(e)).unwrap_or(0),
                    kind: r.get::<i16>(2).unwrap_or_else(|e| spi_err(e)).unwrap_or(0),
                    path: r.get::<String>(3).unwrap_or_else(|e| spi_err(e)).unwrap_or_default(),
                    root: root.and_then(|v| to_hash(&v).ok()),
                    version: r.get::<i64>(5).unwrap_or_else(|e| spi_err(e)).unwrap_or(0),
                });
            }
            out
        })
    }

    pub fn files_under(prefix: &str) -> Vec<NodeRow> {
        Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT id, kind, path, root, version FROM kb.node WHERE kind = 1 AND deleted_at IS NULL AND ($1 = '/' OR left(path, length($1) + 1) = $1 || '/') ORDER BY path",
                    None,
                    &[prefix.into()],
                )
                .unwrap_or_else(|e| spi_err(e));
            let mut v = Vec::new();
            for r in t {
                let root: Option<Vec<u8>> = r.get(4).unwrap_or_else(|e| spi_err(e));
                v.push(NodeRow {
                    id: r.get::<i64>(1).unwrap_or_else(|e| spi_err(e)).unwrap_or(0),
                    kind: 1,
                    path: r.get::<String>(3).unwrap_or_else(|e| spi_err(e)).unwrap_or_default(),
                    root: root.and_then(|v| to_hash(&v).ok()),
                    version: r.get::<i64>(5).unwrap_or_else(|e| spi_err(e)).unwrap_or(0),
                });
            }
            v
        })
    }
}

/// Storage over the `kb` tables via SPI. Chunk and node writes are idempotent
/// (`ON CONFLICT DO NOTHING`); `cas_root` is the only mutation.
///
/// Writes are buffered and flushed in ascending hash order immediately before the CAS.
/// Two transactions inserting the same not-yet-committed chunks in opposite orders would
/// otherwise wait on each other's unique-index entries and deadlock (seen with identical
/// concurrent edits on a hot file); a global insertion order makes lock waits acyclic.
pub struct SpiStorage {
    pending_chunks: std::collections::BTreeMap<Hash, Vec<u8>>,
    pending_nodes: std::collections::BTreeMap<Hash, Vec<u8>>,
}

impl SpiStorage {
    pub fn new() -> Self {
        SpiStorage {
            pending_chunks: Default::default(),
            pending_nodes: Default::default(),
        }
    }

    /// Persist buffered chunks and nodes (sorted by hash) — called before every CAS and
    /// at the end of a write that created no new version.
    pub fn flush(&mut self) -> Result<()> {
        for (h, b) in std::mem::take(&mut self.pending_chunks) {
            let nlines = textdb_core::chunker::count_newlines(&b) as i32;
            Spi::run_with_args(
                "INSERT INTO kb.chunk(hash, bytes, nlines) VALUES ($1, $2, $3) ON CONFLICT (hash) DO NOTHING",
                &[h.to_vec().into(), b.into(), nlines.into()],
            )
            .map_err(map)?;
        }
        for (h, enc) in std::mem::take(&mut self.pending_nodes) {
            Spi::run_with_args(
                "INSERT INTO kb.tree_node(hash, children) VALUES ($1, $2) ON CONFLICT (hash) DO NOTHING",
                &[h.to_vec().into(), enc.into()],
            )
            .map_err(map)?;
        }
        Ok(())
    }
}

impl Drop for SpiStorage {
    fn drop(&mut self) {
        // Anything still pending belongs to a write that did not reach CAS (no-op or
        // error); persisting it is harmless (content-addressed, idempotent) and keeps
        // `chunk_ref`/search consistent for callers that recorded the hashes.
        //
        // Not while unwinding: a Postgres ERROR reaches Rust as a panic, and another SPI call
        // then raises again inside the unwind (a read-only transaction refuses the INSERT), a
        // panic while panicking, which aborts the backend and puts the server into recovery.
        if !std::thread::panicking() {
            let _ = self.flush();
        }
    }
}

fn map(e: pgrx::spi::Error) -> TextdbError {
    TextdbError::Storage(e.to_string())
}

impl Storage for SpiStorage {
    fn get_chunk(&self, h: &Hash) -> Result<Option<Vec<u8>>> {
        if let Some(b) = self.pending_chunks.get(h) {
            return Ok(Some(b.clone()));
        }
        Spi::get_one_with_args::<Vec<u8>>("SELECT bytes FROM kb.chunk WHERE hash = $1", &[h.to_vec().into()]).map_err(map)
    }

    fn put_chunk(&mut self, h: &Hash, b: &[u8]) -> Result<()> {
        self.pending_chunks.entry(*h).or_insert_with(|| b.to_vec());
        Ok(())
    }

    fn get_node(&self, h: &Hash) -> Result<Option<Node>> {
        if let Some(e) = self.pending_nodes.get(h) {
            return Node::decode(e).map(Some).ok_or_else(|| TextdbError::Storage("corrupt tree node".into()));
        }
        let enc = Spi::get_one_with_args::<Vec<u8>>("SELECT children FROM kb.tree_node WHERE hash = $1", &[h.to_vec().into()]).map_err(map)?;
        match enc {
            None => Ok(None),
            Some(e) => Node::decode(&e).map(Some).ok_or_else(|| TextdbError::Storage("corrupt tree node".into())),
        }
    }

    fn put_node(&mut self, h: &Hash, n: &Node) -> Result<()> {
        self.pending_nodes.entry(*h).or_insert_with(|| n.encode());
        Ok(())
    }

    fn get_root(&self, file_id: u64) -> Result<Option<(Hash, u64)>> {
        let (root, version) = Spi::get_two_with_args::<Vec<u8>, i64>(
            "SELECT root, version FROM kb.node WHERE id = $1 AND kind = 1 AND deleted_at IS NULL",
            &[(file_id as i64).into()],
        )
        .map_err(map)?;
        Ok(match (root, version) {
            (Some(r), Some(v)) => Some((to_hash(&r)?, v as u64)),
            _ => None,
        })
    }

    fn cas_root(&mut self, file_id: u64, expect: Option<&Hash>, new: &Hash) -> Result<bool> {
        self.flush()?;
        let n = match expect {
            Some(e) => Spi::get_one_with_args::<i64>(
                "WITH u AS (UPDATE kb.node SET root = $1, version = version + 1, updated_at = now() WHERE id = $2 AND root = $3 AND deleted_at IS NULL RETURNING 1) SELECT count(*) FROM u",
                &[new.to_vec().into(), (file_id as i64).into(), e.to_vec().into()],
            ),
            None => Spi::get_one_with_args::<i64>(
                "WITH u AS (UPDATE kb.node SET root = $1, version = version + 1, updated_at = now() WHERE id = $2 AND root IS NULL AND deleted_at IS NULL RETURNING 1) SELECT count(*) FROM u",
                &[new.to_vec().into(), (file_id as i64).into()],
            ),
        }
        .map_err(map)?;
        Ok(n == Some(1))
    }
}
