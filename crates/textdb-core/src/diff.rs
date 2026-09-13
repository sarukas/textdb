//! Tree diff (spec §6.7): skip identical subtrees, emit changed leaf runs, and render
//! them as unified diff at line granularity.

use crate::hash::Hash;
use crate::myers::{diff_seq, line_diff, split_lines, unified};
use crate::node::{Child, Node};
use crate::storage::{Result, Storage};
use crate::tree::{leaves, line_of_byte, locate_line, materialize_range, totals, LeafRef};

/// A changed region: bytes `[a_from, a_to)` of `a` became `[b_from, b_to)` of `b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangedRun {
    pub a_from: u64,
    pub a_to: u64,
    pub b_from: u64,
    pub b_to: u64,
}

impl ChangedRun {
    pub fn delta(&self) -> i64 {
        (self.b_to - self.b_from) as i64 - (self.a_to - self.a_from) as i64
    }
}

/// Changed leaf runs between two roots, in ascending order.
///
/// Identical subtrees are skipped from the front and the back; the leaves of the
/// remaining middle are compared with a sequence diff. Cost is proportional to the
/// changed region for local edits.
pub fn changed_runs<S: Storage + ?Sized>(storage: &S, a: &Hash, b: &Hash) -> Result<Vec<ChangedRun>> {
    if a == b {
        return Ok(vec![]);
    }
    let (a_len, _) = totals(storage, a)?;
    let (b_len, _) = totals(storage, b)?;
    let na = storage.node(a)?;
    let nb = storage.node(b)?;
    // Front: common prefix of subtrees.
    let (a_pre, b_pre, a_mid_start, b_mid_start) = common_prefix(storage, &na, &nb)?;
    // Back: common suffix, bounded so it does not cross the prefix.
    let (a_suf, b_suf) = common_suffix(storage, &na, &nb, a_len - a_mid_start, b_len - b_mid_start)?;
    let a_mid_end = a_len - a_suf;
    let b_mid_end = b_len - b_suf;
    let _ = (a_pre, b_pre);
    // Leaves in the middle.
    let al: Vec<LeafRef> = leaves_in(storage, a, a_mid_start, a_mid_end)?;
    let bl: Vec<LeafRef> = leaves_in(storage, b, b_mid_start, b_mid_end)?;
    let ah: Vec<Hash> = al.iter().map(|l| l.hash).collect();
    let bh: Vec<Hash> = bl.iter().map(|l| l.hash).collect();
    let mut runs = Vec::new();
    for h in diff_seq(&ah, &bh, 2048) {
        let a_from = if h.a_from < al.len() { al[h.a_from].byte_off } else { a_mid_end };
        let a_to = if h.a_to < al.len() { al[h.a_to].byte_off } else { a_mid_end };
        let b_from = if h.b_from < bl.len() { bl[h.b_from].byte_off } else { b_mid_end };
        let b_to = if h.b_to < bl.len() { bl[h.b_to].byte_off } else { b_mid_end };
        runs.extend(refine(storage, a, b, ChangedRun { a_from, a_to, b_from, b_to })?);
    }
    Ok(runs)
}

/// Split a chunk-granular run into the line-level differences inside it (byte-trimmed),
/// so concurrent edits inside the same chunk are still recognised as disjoint.
fn refine<S: Storage + ?Sized>(storage: &S, a: &Hash, b: &Hash, r: ChangedRun) -> Result<Vec<ChangedRun>> {
    let at = materialize_range(storage, a, r.a_from, r.a_to)?;
    let bt = materialize_range(storage, b, r.b_from, r.b_to)?;
    let edits = crate::myers::byte_edits(&at, &bt);
    if edits.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::with_capacity(edits.len());
    let mut delta = 0i64;
    for e in edits {
        let b_from = (e.from as i64 + delta) as u64;
        let b_to = b_from + e.replacement.len() as u64;
        out.push(ChangedRun {
            a_from: r.a_from + e.from,
            a_to: r.a_from + e.to,
            b_from: r.b_from + b_from,
            b_to: r.b_from + b_to,
        });
        delta += e.delta();
    }
    Ok(out)
}

/// Leaves of `root` overlapping `[from, to)`.
fn leaves_in<S: Storage + ?Sized>(storage: &S, root: &Hash, from: u64, to: u64) -> Result<Vec<LeafRef>> {
    let n = storage.node(root)?;
    let mut out = Vec::new();
    collect_leaves(storage, &n, 0, 0, from, to, &mut out)?;
    Ok(out)
}

fn collect_leaves<S: Storage + ?Sized>(
    storage: &S,
    node: &Node,
    mut boff: u64,
    mut loff: u64,
    from: u64,
    to: u64,
    out: &mut Vec<LeafRef>,
) -> Result<()> {
    for c in &node.children {
        let end = boff + c.nbytes;
        if end > from && boff < to {
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
                collect_leaves(storage, &child, boff, loff, from, to, out)?;
            }
        }
        boff = end;
        loff += c.nlines;
        if boff >= to {
            break;
        }
    }
    Ok(())
}

/// Returns (a_children_skipped, b_children_skipped, a_bytes, b_bytes) of the shared prefix.
fn common_prefix<S: Storage + ?Sized>(storage: &S, a: &Node, b: &Node) -> Result<(usize, usize, u64, u64)> {
    let mut a_off = 0u64;
    let mut b_off = 0u64;
    let mut ac = a.children.clone();
    let mut bc = b.children.clone();
    let mut ai = 0;
    let mut bi = 0;
    loop {
        while ai < ac.len() && bi < bc.len() && ac[ai].hash == bc[bi].hash {
            a_off += ac[ai].nbytes;
            b_off += bc[bi].nbytes;
            ai += 1;
            bi += 1;
        }
        if ai < ac.len() && bi < bc.len() && !ac[ai].is_leaf && !bc[bi].is_leaf {
            ac = storage.node(&ac[ai].hash)?.children;
            bc = storage.node(&bc[bi].hash)?.children;
            ai = 0;
            bi = 0;
            continue;
        }
        // Different depths: descend the deeper side only.
        if ai < ac.len() && bi < bc.len() && ac[ai].is_leaf != bc[bi].is_leaf {
            if !ac[ai].is_leaf {
                ac = storage.node(&ac[ai].hash)?.children;
                ai = 0;
            } else {
                bc = storage.node(&bc[bi].hash)?.children;
                bi = 0;
            }
            continue;
        }
        return Ok((ai, bi, a_off, b_off));
    }
}

/// Shared suffix in bytes for both sides, not exceeding the given budgets.
fn common_suffix<S: Storage + ?Sized>(storage: &S, a: &Node, b: &Node, a_budget: u64, b_budget: u64) -> Result<(u64, u64)> {
    let mut a_suf = 0u64;
    let mut b_suf = 0u64;
    let mut ac = a.children.clone();
    let mut bc = b.children.clone();
    loop {
        while let (Some(x), Some(y)) = (ac.last(), bc.last()) {
            if x.hash == y.hash && a_suf + x.nbytes <= a_budget && b_suf + y.nbytes <= b_budget {
                a_suf += x.nbytes;
                b_suf += y.nbytes;
                ac.pop();
                bc.pop();
            } else {
                break;
            }
        }
        match (ac.last().copied(), bc.last().copied()) {
            (Some(x), Some(y)) if !x.is_leaf && !y.is_leaf => {
                ac = storage.node(&x.hash)?.children;
                bc = storage.node(&y.hash)?.children;
            }
            (Some(x), Some(y)) if x.is_leaf != y.is_leaf => {
                if !x.is_leaf {
                    ac = storage.node(&x.hash)?.children;
                } else {
                    bc = storage.node(&y.hash)?.children;
                }
            }
            _ => return Ok((a_suf, b_suf)),
        }
    }
}

/// A change at line granularity: lines `[old_from, old_from + old_count)` of `a` (0-based)
/// became lines `[new_from, new_from + new_count)` of `b`. A zero count is a pure insertion
/// or deletion in front of that line.
///
/// This is the diff for callers that patch a document they already display, rather than
/// print one: each hunk carries the replaced and the replacing text, so applying it needs no
/// further read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineHunk {
    pub old_from: u64,
    pub old_count: u64,
    pub new_from: u64,
    pub new_count: u64,
    pub old_text: Vec<u8>,
    pub new_text: Vec<u8>,
}

/// Line hunks between two roots, in ascending order.
///
/// Built on the same chunk-granular walk as [`unified_diff`], so the cost follows the size
/// of the change rather than the size of the document; each changed region is then diffed
/// line by line so the hunks are as tight as a plain line diff would make them.
pub fn line_hunks<S: Storage + ?Sized>(storage: &S, a: &Hash, b: &Hash) -> Result<Vec<LineHunk>> {
    let mut out = Vec::new();
    for r in changed_line_regions(storage, a, b)? {
        let at = materialize_range(storage, a, r.a_from, r.a_to)?;
        let bt = materialize_range(storage, b, r.b_from, r.b_to)?;
        let a_line = line_of_byte(storage, a, r.a_from)?;
        let b_line = line_of_byte(storage, b, r.b_from)?;
        let al = split_lines(&at);
        let bl = split_lines(&bt);
        for h in line_diff(&al, &bl) {
            out.push(LineHunk {
                old_from: a_line + h.a_from as u64,
                old_count: (h.a_to - h.a_from) as u64,
                new_from: b_line + h.b_from as u64,
                new_count: (h.b_to - h.b_from) as u64,
                old_text: al[h.a_from..h.a_to].concat(),
                new_text: bl[h.b_from..h.b_to].concat(),
            });
        }
    }
    Ok(out)
}

/// Unified diff between two roots at line granularity, with `context` lines.
pub fn unified_diff<S: Storage + ?Sized>(storage: &S, a: &Hash, b: &Hash, context: usize) -> Result<String> {
    let regions = changed_line_regions(storage, a, b)?;
    let mut out = String::new();
    for r in regions {
        let at = materialize_range(storage, a, r.a_from, r.a_to)?;
        let bt = materialize_range(storage, b, r.b_from, r.b_to)?;
        let a_line = line_of_byte(storage, a, r.a_from)? as usize + 1;
        let b_line = line_of_byte(storage, b, r.b_from)? as usize + 1;
        out.push_str(&unified(&at, &bt, context, a_line, b_line));
    }
    Ok(out)
}

/// [`changed_runs`] widened to whole lines on both sides, with runs that touch merged.
fn changed_line_regions<S: Storage + ?Sized>(storage: &S, a: &Hash, b: &Hash) -> Result<Vec<ChangedRun>> {
    let runs = changed_runs(storage, a, b)?;
    if runs.is_empty() {
        return Ok(Vec::new());
    }
    let (a_len, _) = totals(storage, a)?;
    let (b_len, _) = totals(storage, b)?;
    let mut regions: Vec<ChangedRun> = Vec::new();
    for r in runs {
        let a_line0 = line_of_byte(storage, a, r.a_from)?;
        let b_line0 = line_of_byte(storage, b, r.b_from)?;
        let a_from = locate_line(storage, a, a_line0)?.unwrap_or(0);
        let b_from = locate_line(storage, b, b_line0)?.unwrap_or(0);
        let a_line1 = line_of_byte(storage, a, r.a_to)?;
        let b_line1 = line_of_byte(storage, b, r.b_to)?;
        let a_to = locate_line(storage, a, a_line1 + 1)?.unwrap_or(a_len);
        let b_to = locate_line(storage, b, b_line1 + 1)?.unwrap_or(b_len);
        let nr = ChangedRun {
            a_from,
            a_to,
            b_from,
            b_to,
        };
        if let Some(last) = regions.last_mut() {
            if nr.a_from <= last.a_to {
                last.a_to = last.a_to.max(nr.a_to);
                last.b_to = last.b_to.max(nr.b_to);
                continue;
            }
        }
        regions.push(nr);
    }
    Ok(regions)
}

/// Debug helper: all leaves of a root as children.
pub fn leaf_children<S: Storage + ?Sized>(storage: &S, root: &Hash) -> Result<Vec<Child>> {
    Ok(leaves(storage, root)?
        .into_iter()
        .map(|l| Child {
            hash: l.hash,
            nbytes: l.nbytes,
            nlines: l.nlines,
            is_leaf: true,
        })
        .collect())
}
