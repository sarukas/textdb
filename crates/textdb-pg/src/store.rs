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
    /// Tree nodes already read in this call.
    ///
    /// Only nodes, never chunks: nodes get asked for twice on every read — once to decide
    /// whether the document is worth batching, once by the walk that assembles it — and
    /// without this a one-leaf document paid three queries where it used to pay two. Chunks
    /// are asked for once each, so caching them would buy nothing and hold the whole
    /// document in memory a second time.
    ///
    /// Content-addressed, so an entry never goes stale and no write has to invalidate it,
    /// and a `SpiStorage` lives for one function call, which bounds it without a policy.
    node_cache: std::cell::RefCell<std::collections::HashMap<Hash, Node>>,
}

/// Hashes per batched `= ANY($1)` lookup.
///
/// The point is to stop paying per row, and one round of planning amortised over a few
/// hundred rows already does that; a single unbounded array would instead build one huge
/// parameter for a document with a hundred thousand leaves.
const FETCH_BATCH: usize = 512;

/// Below this many leaves, read the chunks one query at a time.
///
/// Batching is not free: `unnest(...) WITH ORDINALITY` joined against `kb.chunk` costs more
/// to plan than `WHERE hash = $1`, and on a one-leaf document that planning is the whole
/// read. Measured, the first cut of this made 512 B reads 1.76x *slower* while making 10 MiB
/// reads 2.6x faster. Small documents are also the ones the RT, CR and SR families issue
/// most, so that trade was the wrong way round.
const BATCH_FLOOR: usize = 8;

impl SpiStorage {
    pub fn new() -> Self {
        SpiStorage {
            pending_chunks: Default::default(),
            pending_nodes: Default::default(),
            node_cache: Default::default(),
        }
    }

    /// Materialise by fetching the leaves **in document order** and appending them straight
    /// to the output.
    ///
    /// Batching alone still handled every chunk twice — once into a cache, once out of it
    /// into the result — which at ~10 us a leaf was most of what was left after the queries
    /// were amortised. `unnest(...) WITH ORDINALITY` makes Postgres return the rows in the
    /// order they were asked for, so the bytes can be appended as they arrive and each chunk
    /// is copied once. A hash repeated in one document (chunk sharing does that) is repeated
    /// by `unnest` too, so shared chunks still land in every position they belong to.
    ///
    /// `None` means "not applicable here, use the ordinary walk": there are unflushed
    /// pending writes whose chunks are not in the table yet, or the tree could not be
    /// walked. Never a silent wrong answer.
    fn materialize_ordered(&self, root: &Hash) -> Option<Vec<u8>> {
        if !self.pending_chunks.is_empty() || !self.pending_nodes.is_empty() {
            return None;
        }
        let leaves = self.leaf_order(root)?;
        if leaves.len() < BATCH_FLOOR {
            return None;
        }
        let mut out = Vec::new();
        for want in leaves.chunks(FETCH_BATCH) {
            let arg: Vec<Vec<u8>> = want.iter().map(|h| h.to_vec()).collect();
            let got: Vec<Vec<u8>> = Spi::connect(|client| {
                let rows = client
                    .select(
                        "SELECT c.bytes FROM unnest($1::bytea[]) WITH ORDINALITY AS u(h, ord) \
                         JOIN kb.chunk c ON c.hash = u.h ORDER BY u.ord",
                        None,
                        &[arg.into()],
                    )
                    .ok()?;
                let mut v = Vec::with_capacity(want.len());
                for r in rows {
                    v.push(r.get::<Vec<u8>>(1).ok()??);
                }
                Some(v)
            })?;
            // A short result means a chunk is missing; the ordinary walk will say so
            // properly rather than this silently returning a truncated document.
            if got.len() != want.len() {
                return None;
            }
            for b in got {
                out.extend_from_slice(&b);
            }
        }
        Some(out)
    }

    /// Leaf hashes under `root`, in document order, reading each tree level in one query.
    fn leaf_order(&self, root: &Hash) -> Option<Vec<Hash>> {
        // The root goes through the ordinary single-row lookup, which is one query either
        // way. A shallow document is then recognised without ever building an array
        // parameter, so the batched path costs it nothing.
        let top = self.get_node(root).ok()??;
        let mut order: Vec<Hash> = Vec::new();
        let mut level = Vec::new();
        for c in &top.children {
            if c.is_leaf {
                order.push(c.hash);
            } else {
                level.push(c.hash);
            }
        }
        if level.is_empty() {
            return Some(order);
        }
        if !order.is_empty() {
            return None;
        }
        for _ in 0..64 {
            if level.is_empty() {
                return Some(order);
            }
            let mut by_hash = std::collections::HashMap::new();
            for want in level.chunks(FETCH_BATCH) {
                for (h, enc) in self.fetch_many("SELECT hash, children FROM kb.tree_node WHERE hash = ANY($1)", want).ok()? {
                    let n = Node::decode(&enc)?;
                    self.node_cache.borrow_mut().insert(h, n.clone());
                    by_hash.insert(h, n);
                }
            }
            // Rebuilt in `level` order, not in whatever order the rows came back, or the
            // document would be assembled out of order.
            let mut next = Vec::new();
            for h in &level {
                for c in &by_hash.get(h)?.children {
                    if c.is_leaf {
                        order.push(c.hash);
                    } else {
                        next.push(c.hash);
                    }
                }
            }
            // A tree with leaves and branches at the same level would interleave wrongly;
            // textdb never builds one, and this declines rather than assumes.
            if !next.is_empty() && !order.is_empty() {
                return None;
            }
            level = next;
        }
        None
    }

    /// `(hash, bytes)` for every row whose hash is in `want`, in one query.
    fn fetch_many(&self, sql: &str, want: &[Hash]) -> Result<Vec<(Hash, Vec<u8>)>> {
        let arg: Vec<Vec<u8>> = want.iter().map(|h| h.to_vec()).collect();
        Spi::connect(|client| {
            let rows = client.select(sql, None, &[arg.into()]).map_err(map)?;
            let mut out = Vec::with_capacity(want.len());
            for r in rows {
                let (Some(h), Some(b)) = (r.get::<Vec<u8>>(1).map_err(map)?, r.get::<Vec<u8>>(2).map_err(map)?) else {
                    continue;
                };
                out.push((to_hash(&h)?, b));
            }
            Ok(out)
        })
    }

    /// Persist buffered chunks and nodes (sorted by hash) — called before every CAS and
    /// at the end of a write that created no new version.
    pub fn flush(&mut self) -> Result<()> {
        // One INSERT per chunk and per node meant 652 statements to store a 1 MiB document —
        // the write-side twin of the per-chunk SELECT that `materialize_ordered` replaced.
        // Unnested arrays instead: one statement per batch, whatever the document's size.
        let chunks = std::mem::take(&mut self.pending_chunks);
        let entries: Vec<(Hash, Vec<u8>)> = chunks.into_iter().collect();
        // Below the floor, one statement each: building three arrays to insert a single
        // chunk costs more than the statement it saves, and measured that way round a
        // 513 B create came out 1.17x slower while the 10 MiB one gained. Same trade, and
        // same answer, as the read path.
        if entries.len() < BATCH_FLOOR {
            for (h, b) in &entries {
                let nlines = textdb_core::chunker::count_newlines(b) as i32;
                Spi::run_with_args(
                    "INSERT INTO kb.chunk(hash, bytes, nlines) VALUES ($1, $2, $3) ON CONFLICT (hash) DO NOTHING",
                    &[h.to_vec().into(), b.clone().into(), nlines.into()],
                )
                .map_err(map)?;
            }
        }
        for batch in entries.chunks(FETCH_BATCH).filter(|_| entries.len() >= BATCH_FLOOR) {
            let hashes: Vec<Vec<u8>> = batch.iter().map(|(h, _)| h.to_vec()).collect();
            let bytes: Vec<Vec<u8>> = batch.iter().map(|(_, b)| b.clone()).collect();
            let nlines: Vec<i32> = batch.iter().map(|(_, b)| textdb_core::chunker::count_newlines(b) as i32).collect();
            Spi::run_with_args(
                "INSERT INTO kb.chunk(hash, bytes, nlines) \
                 SELECT * FROM unnest($1::bytea[], $2::bytea[], $3::int[]) ON CONFLICT (hash) DO NOTHING",
                &[hashes.into(), bytes.into(), nlines.into()],
            )
            .map_err(map)?;
        }
        let nodes = std::mem::take(&mut self.pending_nodes);
        let entries: Vec<(Hash, Vec<u8>)> = nodes.into_iter().collect();
        if entries.len() < BATCH_FLOOR {
            for (h, enc) in &entries {
                Spi::run_with_args(
                    "INSERT INTO kb.tree_node(hash, children) VALUES ($1, $2) ON CONFLICT (hash) DO NOTHING",
                    &[h.to_vec().into(), enc.clone().into()],
                )
                .map_err(map)?;
            }
        }
        for batch in entries.chunks(FETCH_BATCH).filter(|_| entries.len() >= BATCH_FLOOR) {
            let hashes: Vec<Vec<u8>> = batch.iter().map(|(h, _)| h.to_vec()).collect();
            let encoded: Vec<Vec<u8>> = batch.iter().map(|(_, e)| e.clone()).collect();
            Spi::run_with_args(
                "INSERT INTO kb.tree_node(hash, children) \
                 SELECT * FROM unnest($1::bytea[], $2::bytea[]) ON CONFLICT (hash) DO NOTHING",
                &[hashes.into(), encoded.into()],
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
        if let Some(n) = self.node_cache.borrow().get(h) {
            return Ok(Some(n.clone()));
        }
        let enc = Spi::get_one_with_args::<Vec<u8>>("SELECT children FROM kb.tree_node WHERE hash = $1", &[h.to_vec().into()]).map_err(map)?;
        match enc {
            None => Ok(None),
            Some(e) => {
                let n = Node::decode(&e).ok_or_else(|| TextdbError::Storage("corrupt tree node".into()))?;
                self.node_cache.borrow_mut().insert(*h, n.clone());
                Ok(Some(n))
            }
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

/// Materialise a whole document, reading its tree in bulk first.
///
/// Every full-document read goes through here rather than calling
/// [`textdb_core::tree::materialize`] directly, so no read path can be left paying one
/// query per chunk by omission. Range reads deliberately do not: they touch a slice of the
/// leaves, and fetching the whole tree for them would read what they will not use.
pub fn materialize_all(st: &SpiStorage, root: &Hash) -> Result<Vec<u8>> {
    match st.materialize_ordered(root) {
        Some(out) => Ok(out),
        // Anything the fast path declines — a pending write not yet flushed, a tree it could
        // not walk — falls back to the ordinary walk, which is always correct.
        None => textdb_core::tree::materialize(st, root),
    }
}
