//! Prolly tree over chunk hashes (spec §6.2–§6.4).
//!
//! Level 0 is the sequence of chunk entries. Each higher level groups the entries of
//! the level below into nodes; a node ends after an entry when that entry's hash
//! satisfies the boundary predicate (probability 1/32, mean fanout 32) or when the
//! node reaches `MAX_FANOUT`. The split therefore depends only on the entry sequence
//! (determinism requirement D2), so identical content yields identical roots.

use crate::chunker::{chunk_all, count_newlines, ChunkParams};
use crate::hash::{hash_chunk, Hash};
use crate::node::{Child, Node};
use crate::storage::{Result, Storage};
use crate::TextdbError;

/// Mean fanout is 32 (boundary probability 1/32).
pub const BOUNDARY_MASK: u32 = 31;
/// Hard cap on node width; keeps pathological runs bounded while staying deterministic.
pub const MAX_FANOUT: usize = 512;

/// Does the node end after an entry with this hash?
#[inline]
pub fn is_node_boundary(h: &Hash) -> bool {
    // Use bytes 4..8 so the decision is independent of the chunker's use of the hash.
    let x = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
    x & BOUNDARY_MASK == 0
}

/// Group a sequence of entries into nodes deterministically.
pub fn split_level(entries: &[Child]) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut cur: Vec<Child> = Vec::new();
    for e in entries {
        cur.push(*e);
        if is_node_boundary(&e.hash) || cur.len() >= MAX_FANOUT {
            nodes.push(Node::new(std::mem::take(&mut cur)));
        }
    }
    if !cur.is_empty() {
        nodes.push(Node::new(cur));
    }
    nodes
}

/// Build levels above `entries` until a single node remains; store every node.
/// Returns the root hash. An empty entry list produces an empty root node.
pub fn build_up<S: Storage + ?Sized>(storage: &mut S, mut entries: Vec<Child>) -> Result<Hash> {
    if entries.is_empty() {
        let root = Node::default();
        let h = root.hash();
        storage.put_node(&h, &root)?;
        return Ok(h);
    }
    loop {
        let nodes = split_level(&entries);
        let mut next = Vec::with_capacity(nodes.len());
        for n in &nodes {
            let h = n.hash();
            storage.put_node(&h, n)?;
            next.push(Child {
                hash: h,
                nbytes: n.nbytes(),
                nlines: n.nlines(),
                is_leaf: false,
            });
        }
        if nodes.len() == 1 {
            return Ok(next[0].hash);
        }
        entries = next;
    }
}

/// Chunk `bytes`, store the chunks and build the tree. Returns the root hash.
pub fn build<S: Storage + ?Sized>(storage: &mut S, params: &ChunkParams, bytes: &[u8]) -> Result<Hash> {
    let mut entries = Vec::new();
    for (s, l) in chunk_all(params, bytes) {
        let chunk = &bytes[s..s + l];
        let h = hash_chunk(chunk);
        storage.put_chunk(&h, chunk)?;
        entries.push(Child {
            hash: h,
            nbytes: l as u64,
            nlines: count_newlines(chunk),
            is_leaf: true,
        });
    }
    build_up(storage, entries)
}

/// Build and also return the set of chunk hashes produced (for FTS insertion).
pub fn build_with_chunks<S: Storage + ?Sized>(
    storage: &mut S,
    params: &ChunkParams,
    bytes: &[u8],
) -> Result<(Hash, Vec<Hash>)> {
    let mut entries = Vec::new();
    let mut hashes = Vec::new();
    for (s, l) in chunk_all(params, bytes) {
        let chunk = &bytes[s..s + l];
        let h = hash_chunk(chunk);
        storage.put_chunk(&h, chunk)?;
        hashes.push(h);
        entries.push(Child {
            hash: h,
            nbytes: l as u64,
            nlines: count_newlines(chunk),
            is_leaf: true,
        });
    }
    Ok((build_up(storage, entries)?, hashes))
}

/// Collapse a root whose single child is an internal node (keeps the build invariant).
pub fn normalize_root<S: Storage + ?Sized>(storage: &S, mut root: Hash) -> Result<Hash> {
    loop {
        let n = storage.node(&root)?;
        if n.children.len() == 1 && !n.children[0].is_leaf {
            root = n.children[0].hash;
        } else {
            return Ok(root);
        }
    }
}

/// Total bytes and lines under a root.
pub fn totals<S: Storage + ?Sized>(storage: &S, root: &Hash) -> Result<(u64, u64)> {
    let n = storage.node(root)?;
    Ok((n.nbytes(), n.nlines()))
}

/// In-order leaf traversal and concatenation (spec §6.3).
pub fn materialize<S: Storage + ?Sized>(storage: &S, root: &Hash) -> Result<Vec<u8>> {
    let n = storage.node(root)?;
    let mut out = Vec::with_capacity(n.nbytes() as usize);
    materialize_into(storage, &n, &mut out)?;
    Ok(out)
}

fn materialize_into<S: Storage + ?Sized>(storage: &S, node: &Node, out: &mut Vec<u8>) -> Result<()> {
    for c in &node.children {
        if c.is_leaf {
            let bytes = storage.chunk_shared(&c.hash)?;
            out.extend_from_slice(&bytes);
        } else {
            let child = storage.node(&c.hash)?;
            materialize_into(storage, &child, out)?;
        }
    }
    Ok(())
}

/// Materialize the byte range `[from, to)`. O(range + log n).
pub fn materialize_range<S: Storage + ?Sized>(storage: &S, root: &Hash, from: u64, to: u64) -> Result<Vec<u8>> {
    let n = storage.node(root)?;
    let total = n.nbytes();
    let to = to.min(total);
    if from >= to {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity((to - from) as usize);
    range_into(storage, &n, 0, from, to, &mut out)?;
    Ok(out)
}

fn range_into<S: Storage + ?Sized>(
    storage: &S,
    node: &Node,
    mut off: u64,
    from: u64,
    to: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    for c in &node.children {
        let end = off + c.nbytes;
        if end > from && off < to {
            if c.is_leaf {
                let bytes = storage.chunk_shared(&c.hash)?;
                let s = from.saturating_sub(off) as usize;
                let e = (to.min(end) - off) as usize;
                out.extend_from_slice(&bytes[s..e]);
            } else {
                let child = storage.node(&c.hash)?;
                range_into(storage, &child, off, from, to, out)?;
            }
        }
        off = end;
        if off >= to {
            break;
        }
    }
    Ok(())
}

/// A leaf as seen from the root: hash, byte offset, line offset, size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeafRef {
    pub hash: Hash,
    pub byte_off: u64,
    pub line_off: u64,
    pub nbytes: u64,
    pub nlines: u64,
}

/// All leaves in order with their offsets. O(n) — used by search result mapping and tests.
pub fn leaves<S: Storage + ?Sized>(storage: &S, root: &Hash) -> Result<Vec<LeafRef>> {
    let n = storage.node(root)?;
    let mut out = Vec::new();
    leaves_into(storage, &n, 0, 0, &mut out)?;
    Ok(out)
}

fn leaves_into<S: Storage + ?Sized>(
    storage: &S,
    node: &Node,
    mut boff: u64,
    mut loff: u64,
    out: &mut Vec<LeafRef>,
) -> Result<()> {
    for c in &node.children {
        if c.is_leaf {
            out.push(LeafRef {
                hash: c.hash,
                byte_off: boff,
                line_off: loff,
                nbytes: c.nbytes,
                nlines: c.nlines,
            });
        } else {
            let child = storage.node(&c.hash)?;
            leaves_into(storage, &child, boff, loff, out)?;
        }
        boff += c.nbytes;
        loff += c.nlines;
    }
    Ok(())
}

/// Number of levels of internal nodes (a tree whose root holds leaves has depth 1).
pub fn depth<S: Storage + ?Sized>(storage: &S, root: &Hash) -> Result<usize> {
    let mut d = 1;
    let mut n = storage.node(root)?;
    while let Some(c) = n.children.first() {
        if c.is_leaf {
            break;
        }
        n = storage.node(&c.hash)?;
        d += 1;
    }
    Ok(d)
}

/// Locate the leaf containing byte offset `off` (spec §6.4).
/// Returns `(leaf_hash, offset_in_leaf, leaf_start)`. `off == len` selects the last leaf.
pub fn locate_byte<S: Storage + ?Sized>(storage: &S, root: &Hash, off: u64) -> Result<Option<(Hash, u64, u64)>> {
    let cur = Cursor::at_byte(storage, root, off)?;
    Ok(cur.map(|(c, start)| (c.entry().hash, off - start, start)))
}

/// Byte offset at which line `line` (0-based) starts: the offset just past the
/// `line`-th `\n`. `None` if the document has fewer newlines than `line`.
pub fn locate_line<S: Storage + ?Sized>(storage: &S, root: &Hash, line: u64) -> Result<Option<u64>> {
    if line == 0 {
        return Ok(Some(0));
    }
    let mut node = storage.node(root)?;
    let mut boff = 0u64;
    let mut remaining = line; // newlines still to skip
    'descend: loop {
        for c in &node.children {
            if c.nlines >= remaining {
                if c.is_leaf {
                    let bytes = storage.chunk(&c.hash)?;
                    let mut k = remaining;
                    for (i, &b) in bytes.iter().enumerate() {
                        if b == b'\n' {
                            k -= 1;
                            if k == 0 {
                                return Ok(Some(boff + i as u64 + 1));
                            }
                        }
                    }
                    return Ok(None);
                } else {
                    node = storage.node(&c.hash)?;
                    continue 'descend;
                }
            }
            remaining -= c.nlines;
            boff += c.nbytes;
        }
        return Ok(None);
    }
}

/// Bytes of lines `[from, to]` inclusive, 0-based. Lines past the end are ignored.
pub fn lines<S: Storage + ?Sized>(storage: &S, root: &Hash, from: u64, to: u64) -> Result<Vec<u8>> {
    if to < from {
        return Ok(Vec::new());
    }
    let (total, _) = totals(storage, root)?;
    let start = match locate_line(storage, root, from)? {
        Some(s) => s,
        None => return Ok(Vec::new()),
    };
    let end = locate_line(storage, root, to + 1)?.unwrap_or(total);
    materialize_range(storage, root, start, end)
}

/// Line number (0-based) of byte offset `off` (`off == len` gives the last line).
pub fn line_of_byte<S: Storage + ?Sized>(storage: &S, root: &Hash, off: u64) -> Result<u64> {
    let mut node = storage.node(root)?;
    let mut boff = 0u64;
    let mut lines = 0u64;
    'descend: loop {
        let last = node.children.len().saturating_sub(1);
        for (i, c) in node.children.iter().enumerate() {
            let end = boff + c.nbytes;
            if off < end || (i == last && off == end) {
                if c.is_leaf {
                    let bytes = storage.chunk(&c.hash)?;
                    let within = ((off - boff) as usize).min(bytes.len());
                    return Ok(lines + count_newlines(&bytes[..within]));
                }
                node = storage.node(&c.hash)?;
                continue 'descend;
            }
            boff = end;
            lines += c.nlines;
        }
        return Ok(lines);
    }
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Frame {
    pub node: Node,
    pub idx: usize,
}

/// A position in the entry sequence of some level: a path of `(node, index)` frames
/// from the root down to the node containing the entry.
#[derive(Clone, Debug)]
pub struct Cursor {
    pub path: Vec<Frame>,
}

impl Cursor {
    /// Cursor at the leaf entry containing `off` (the last leaf when `off == len`).
    /// Also returns the leaf's start offset. `None` for an empty document.
    pub fn at_byte<S: Storage + ?Sized>(storage: &S, root: &Hash, off: u64) -> Result<Option<(Cursor, u64)>> {
        let mut node = storage.node(root)?;
        if node.children.is_empty() {
            return Ok(None);
        }
        let mut path = Vec::new();
        let mut start = 0u64;
        loop {
            let mut idx = node.children.len() - 1;
            let mut acc = start;
            for (i, c) in node.children.iter().enumerate() {
                if off < acc + c.nbytes {
                    idx = i;
                    break;
                }
                if i + 1 < node.children.len() {
                    acc += c.nbytes;
                }
            }
            start = acc;
            let child = node.children[idx];
            path.push(Frame { node, idx });
            if child.is_leaf {
                return Ok(Some((Cursor { path }, start)));
            }
            node = storage.node(&child.hash)?;
        }
    }

    pub fn depth(&self) -> usize {
        self.path.len()
    }

    pub fn frame(&self) -> &Frame {
        self.path.last().expect("cursor has at least one frame")
    }

    pub fn entry(&self) -> Child {
        let f = self.frame();
        f.node.children[f.idx]
    }

    pub fn is_last_in_node(&self) -> bool {
        let f = self.frame();
        f.idx + 1 == f.node.children.len()
    }

    /// Cursor to the entry one level up (the node containing this entry). `None` at the root.
    pub fn parent(&self) -> Option<Cursor> {
        if self.path.len() <= 1 {
            return None;
        }
        Some(Cursor {
            path: self.path[..self.path.len() - 1].to_vec(),
        })
    }

    /// The node containing the current entry.
    pub fn node(&self) -> &Node {
        &self.frame().node
    }

    pub fn idx(&self) -> usize {
        self.frame().idx
    }

    /// Advance to the next entry at the same level. Returns `false` at end of sequence.
    pub fn advance<S: Storage + ?Sized>(&mut self, storage: &S) -> Result<bool> {
        let target = self.path.len();
        loop {
            let f = self.path.last_mut().unwrap();
            f.idx += 1;
            if f.idx < f.node.children.len() {
                break;
            }
            self.path.pop();
            if self.path.is_empty() {
                return Ok(false);
            }
        }
        while self.path.len() < target {
            let c = self.entry();
            let n = storage.node(&c.hash)?;
            self.path.push(Frame { node: n, idx: 0 });
        }
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Rebuild after a leaf-level replacement
// ---------------------------------------------------------------------------

/// Replace the entries `[a, k)` at the level of `a` with `new_entries`, then rebuild all
/// ancestors under D2 and return the new root. `k == None` means "to end of sequence".
///
/// Every node written is recorded in storage. Untouched subtrees are shared.
pub fn replace_entries<S: Storage + ?Sized>(
    storage: &mut S,
    a: Cursor,
    k: Option<Cursor>,
    new_entries: Vec<Child>,
) -> Result<Hash> {
    let mut a = a;
    let mut k = k;
    let mut entries = new_entries;
    loop {
        let (nodes, a_up, k_up) = rebuild_level(storage, &a, k, &entries)?;
        let mut next = Vec::with_capacity(nodes.len());
        for n in &nodes {
            let h = n.hash();
            storage.put_node(&h, n)?;
            next.push(n.as_child());
        }
        match a_up {
            None => {
                // `a` was inside the root: `nodes` are the new top-level nodes.
                let root = build_up(storage, next)?;
                return normalize_root(storage, root);
            }
            Some(up) => {
                a = up;
                k = k_up;
                entries = next;
            }
        }
    }
}

/// One level of the rebuild. Returns the new nodes replacing the affected old nodes and
/// the replaced range `[a', k')` one level up (`a' == None` when the parent was the root).
fn rebuild_level<S: Storage + ?Sized>(
    storage: &S,
    a: &Cursor,
    k: Option<Cursor>,
    new_entries: &[Child],
) -> Result<(Vec<Node>, Option<Cursor>, Option<Cursor>)> {
    let parent_is_root = a.depth() == 1;
    let mut nodes = Vec::new();
    let mut cur: Vec<Child> = Vec::new();

    // Prefix: entries of the node containing `a` that precede `a`.
    for e in &a.node().children[..a.idx()] {
        cur.push(*e);
        if is_node_boundary(&e.hash) || cur.len() >= MAX_FANOUT {
            nodes.push(Node::new(std::mem::take(&mut cur)));
        }
    }
    for e in new_entries {
        cur.push(*e);
        if is_node_boundary(&e.hash) || cur.len() >= MAX_FANOUT {
            nodes.push(Node::new(std::mem::take(&mut cur)));
        }
    }
    // Suffix: old entries from `k` onward until the node split resynchronises.
    let mut k_up: Option<Cursor> = None;
    let mut resynced = false;
    if let Some(mut c) = k {
        loop {
            let e = c.entry();
            cur.push(e);
            let closes = is_node_boundary(&e.hash) || cur.len() >= MAX_FANOUT;
            if closes {
                nodes.push(Node::new(std::mem::take(&mut cur)));
                if c.is_last_in_node() && !parent_is_root {
                    // New boundary coincides with an old node end: everything after is shared.
                    let mut up = c.parent().expect("depth > 1");
                    k_up = if up.advance(storage)? { Some(up) } else { None };
                    resynced = true;
                    break;
                }
            }
            if !c.advance(storage)? {
                break;
            }
        }
    }
    if !cur.is_empty() {
        nodes.push(Node::new(cur));
    }
    if parent_is_root {
        return Ok((nodes, None, None));
    }
    let a_up = a.parent();
    if !resynced {
        k_up = None;
    }
    Ok((nodes, a_up, k_up))
}

/// Raise a missing-file error helper for bindings.
pub fn missing_file(path: &str) -> TextdbError {
    TextdbError::NotFound(path.to_string())
}
