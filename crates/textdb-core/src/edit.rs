//! Localised edits (spec §6.5): re-chunk only the window around each edit and stop as
//! soon as the chunk boundaries resynchronise with the pre-existing ones.

use std::collections::VecDeque;

use crate::chunker::{count_newlines, cut, ChunkParams};
use crate::hash::{hash_chunk, Hash};
use crate::node::Child;
use crate::storage::{Result, Storage};
use crate::tree::{build_with_chunks, materialize_range, replace_entries, Cursor};
use crate::TextdbError;

/// Replace bytes `[from, to)` with `replacement`. `from == to` is an insertion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub from: u64,
    pub to: u64,
    pub replacement: Vec<u8>,
}

impl Edit {
    pub fn new(from: u64, to: u64, replacement: impl Into<Vec<u8>>) -> Self {
        Edit {
            from,
            to,
            replacement: replacement.into(),
        }
    }
    pub fn delta(&self) -> i64 {
        self.replacement.len() as i64 - (self.to - self.from) as i64
    }
}

#[derive(Clone, Debug)]
pub struct EditResult {
    pub root: Hash,
    /// Chunk hashes produced by the edit (superset of the new ones; bindings dedupe).
    pub new_chunks: Vec<Hash>,
    /// Changed byte range in the new root's coordinates.
    pub changed: (u64, u64),
    /// Number of leaves produced across all re-chunk windows.
    pub leaves_written: usize,
}

/// Validate that edits are non-overlapping, ascending and within `len`.
pub fn validate_edits(edits: &[Edit], len: u64) -> Result<()> {
    let mut prev_end = 0u64;
    for (i, e) in edits.iter().enumerate() {
        if e.from > e.to || e.to > len {
            return Err(TextdbError::InvalidEdit(format!(
                "edit {} range {}..{} out of bounds (len {})",
                i, e.from, e.to, len
            )));
        }
        if i > 0 && e.from < prev_end {
            return Err(TextdbError::InvalidEdit(format!(
                "edit {} starts at {} before previous edit ended at {}",
                i, e.from, prev_end
            )));
        }
        prev_end = e.to;
    }
    Ok(())
}

/// Apply an edit set to `root` and return the new root (spec §6.5).
pub fn apply_edits<S: Storage + ?Sized>(
    storage: &mut S,
    params: &ChunkParams,
    root: &Hash,
    edits: &[Edit],
) -> Result<EditResult> {
    let node = storage.node(root)?;
    let len = node.nbytes();
    validate_edits(edits, len)?;
    if edits.is_empty() {
        return Ok(EditResult {
            root: *root,
            new_chunks: vec![],
            changed: (0, 0),
            leaves_written: 0,
        });
    }
    let mut cur = *root;
    let mut new_chunks = Vec::new();
    let mut leaves_written = 0;
    // Descending order keeps earlier offsets valid.
    for e in edits.iter().rev() {
        let (r, chunks, n) = apply_one(storage, params, &cur, e)?;
        cur = r;
        new_chunks.extend(chunks);
        leaves_written += n;
    }
    let delta: i64 = edits.iter().map(|e| e.delta()).sum();
    let lo = edits[0].from;
    let hi = (edits.last().unwrap().to as i64 + delta).max(lo as i64) as u64;
    Ok(EditResult {
        root: cur,
        new_chunks,
        changed: (lo, hi),
        leaves_written,
    })
}

fn apply_one<S: Storage + ?Sized>(
    storage: &mut S,
    params: &ChunkParams,
    root: &Hash,
    e: &Edit,
) -> Result<(Hash, Vec<Hash>, usize)> {
    let node = storage.node(root)?;
    let len = node.nbytes();
    if node.children.is_empty() {
        let (r, chunks) = build_with_chunks(storage, params, &e.replacement)?;
        let n = chunks.len();
        return Ok((r, chunks, n));
    }
    let (mut a, mut a_start) = Cursor::at_byte(storage, root, e.from)?.expect("non-empty");
    // A chunk that does not end in `\n` was cut knowing the following `snap` bytes hold no
    // newline (see `chunker::snap`); an edit inside that lookahead can invalidate the cut,
    // so the previous leaf joins the re-chunk window in that case.
    if a_start > 0 && e.from - a_start < params.snap as u64 {
        if let Some((prev, prev_start)) = Cursor::at_byte(storage, root, a_start - 1)? {
            let bytes = storage.chunk(&prev.entry().hash)?;
            if bytes.last() != Some(&b'\n') {
                a = prev;
                a_start = prev_start;
            }
        }
    }

    // Window: [a_start, from) ++ replacement ++ old bytes from `to`.
    let mut buf = materialize_range(storage, root, a_start, e.from)?;
    buf.extend_from_slice(&e.replacement);
    let repl_end = buf.len();

    // Old leaf boundaries after `to`, in buffer coordinates, with the cursor that follows.
    let mut old_bounds: VecDeque<(usize, Option<Cursor>)> = VecDeque::new();
    let mut suffix: Option<(Cursor, u64)> = if e.to < len {
        Cursor::at_byte(storage, root, e.to)?
    } else {
        None
    };
    if let Some((c, start)) = &suffix {
        if *start == e.to {
            old_bounds.push_back((repl_end, Some(c.clone())));
        }
    }
    let mut first_suffix = true;

    let mut new_leaves: Vec<Child> = Vec::new();
    let mut new_chunks: Vec<Hash> = Vec::new();
    let mut pos = 0usize;
    let k: Option<Cursor>;
    loop {
        // Keep enough lookahead for boundary decisions to be final.
        while buf.len() - pos < params.lookahead() {
            match suffix.take() {
                None => break,
                Some((c, start)) => {
                    let bytes = storage.chunk(&c.entry().hash)?;
                    let skip = if first_suffix { (e.to - start) as usize } else { 0 };
                    first_suffix = false;
                    buf.extend_from_slice(&bytes[skip..]);
                    let mut nc = c;
                    let after = if nc.advance(storage)? { Some(nc) } else { None };
                    old_bounds.push_back((buf.len(), after.clone()));
                    suffix = after.map(|c| (c, 0));
                }
            }
        }
        let eof = suffix.is_none();
        if pos == buf.len() {
            debug_assert!(eof);
            k = None;
            break;
        }
        let clen = cut(params, &buf[pos..], eof).expect("decision available");
        let end = pos + clen;
        let chunk = &buf[pos..end];
        let h = hash_chunk(chunk);
        storage.put_chunk(&h, chunk)?;
        new_chunks.push(h);
        new_leaves.push(Child {
            hash: h,
            nbytes: clen as u64,
            nlines: count_newlines(chunk),
            is_leaf: true,
        });
        pos = end;
        while let Some((o, _)) = old_bounds.front() {
            if *o < pos {
                old_bounds.pop_front();
            } else {
                break;
            }
        }
        if pos >= repl_end {
            if let Some((o, after)) = old_bounds.front() {
                if *o == pos {
                    // Resynchronised with an old boundary: the rest of the document is shared.
                    k = after.clone();
                    break;
                }
            }
        }
    }
    let n = new_leaves.len();
    let root = replace_entries(storage, a, k, new_leaves)?;
    Ok((root, new_chunks, n))
}
