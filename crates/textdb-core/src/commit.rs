//! Commit with automatic rebase at chunk granularity (spec §6.6, §8).

use crate::chunker::ChunkParams;
use crate::diff::{changed_runs, ChangedRun};
use crate::edit::{apply_edits, Edit};
use crate::hash::Hash;
use crate::myers::{diff3, Merge};
use crate::storage::{Result, Storage};
use crate::tree::{line_of_byte, locate_line, materialize_range, totals};
use crate::TextdbError;

/// Default retry budget before `Contention` is surfaced.
pub const DEFAULT_RETRIES: usize = 8;

/// How a successful commit was achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitKind {
    /// CAS succeeded against the caller's base root.
    Direct,
    /// Base had moved; edits were shifted past disjoint changes.
    Rebased,
    /// Base had moved and overlapped; line-level diff3 merged cleanly.
    Merged,
    /// The document already had the requested content (no new version).
    NoOp,
}

impl CommitKind {
    /// Lower-case name, as recorded in commit rows and the change feed.
    pub fn as_str(self) -> &'static str {
        match self {
            CommitKind::Direct => "direct",
            CommitKind::Rebased => "rebased",
            CommitKind::Merged => "merged",
            CommitKind::NoOp => "noop",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Committed {
    pub version: u64,
    pub root: Hash,
    pub kind: CommitKind,
    pub new_chunks: Vec<Hash>,
    pub retries: usize,
}

/// Conflict payload (spec §6.6): the *current* text of the region so an agent can retry
/// without another read.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConflictInfo {
    pub path: String,
    pub region_line_from: u64,
    pub region_line_to: u64,
    pub base: String,
    pub theirs: String,
    pub ours: String,
    pub current_version: u64,
}

/// Commit `edits` (in coordinates of `base_root`) to `file_id`.
///
/// `path` is only used for the conflict payload. Returns `Conflict` / `Contention` errors.
pub fn commit<S: Storage + ?Sized>(
    storage: &mut S,
    params: &ChunkParams,
    file_id: u64,
    path: &str,
    base_root: &Hash,
    edits: &[Edit],
    retries: usize,
) -> Result<Committed> {
    let mut attempt = 0usize;
    // Our proposed new root against the base; computed lazily once.
    let mut r1: Option<(Hash, Vec<Hash>)> = None;
    loop {
        let (rcur, v) = storage
            .get_root(file_id)?
            .ok_or_else(|| TextdbError::NotFound(path.to_string()))?;
        if &rcur == base_root {
            let (root, chunks) = match &r1 {
                Some(x) => x.clone(),
                None => {
                    let er = apply_edits(storage, params, base_root, edits)?;
                    r1 = Some((er.root, er.new_chunks.clone()));
                    (er.root, er.new_chunks)
                }
            };
            if root == rcur {
                return Ok(Committed {
                    version: v,
                    root,
                    kind: CommitKind::NoOp,
                    new_chunks: vec![],
                    retries: attempt,
                });
            }
            if storage.cas_root(file_id, Some(&rcur), &root)? {
                return Ok(Committed {
                    version: v + 1,
                    root,
                    kind: CommitKind::Direct,
                    new_chunks: chunks,
                    retries: attempt,
                });
            }
        } else {
            let (root, chunks, kind) = rebase(storage, params, path, base_root, &rcur, edits, v)?;
            if root == rcur {
                return Ok(Committed {
                    version: v,
                    root,
                    kind: CommitKind::NoOp,
                    new_chunks: vec![],
                    retries: attempt,
                });
            }
            if storage.cas_root(file_id, Some(&rcur), &root)? {
                return Ok(Committed {
                    version: v + 1,
                    root,
                    kind,
                    new_chunks: chunks,
                    retries: attempt,
                });
            }
        }
        attempt += 1;
        if attempt > retries {
            return Err(TextdbError::Contention);
        }
    }
}

/// Translate a base offset that lies outside every changed run into current coordinates.
fn shift(off: u64, runs: &[ChangedRun]) -> u64 {
    let mut delta = 0i64;
    for r in runs {
        if r.a_to <= off {
            delta += r.delta();
        } else {
            break;
        }
    }
    (off as i64 + delta) as u64
}

/// Inclusive line range of `[from, to)` in `root`'s coordinates. An empty range (a pure
/// insertion) is the single line it sits in.
fn line_span_of<S: Storage + ?Sized>(storage: &S, root: &Hash, from: u64, to: u64, len: u64) -> Result<(u64, u64)> {
    let a = line_of_byte(storage, root, from.min(len))?;
    let b = if to > from {
        line_of_byte(storage, root, (to - 1).min(len))?
    } else {
        a
    };
    Ok((a, b))
}

fn overlaps(e: &Edit, r: &ChangedRun) -> bool {
    // Half-open intervals; a pure insertion at exactly a run boundary counts as overlap
    // when the run is an insertion at the same point (ambiguous ordering).
    if e.from == e.to && r.a_from == r.a_to {
        return e.from == r.a_from;
    }
    if e.from == e.to {
        return e.from > r.a_from && e.from < r.a_to;
    }
    if r.a_from == r.a_to {
        return r.a_from > e.from && r.a_from < e.to;
    }
    e.from < r.a_to && r.a_from < e.to
}

/// Rebase `edits` (base coordinates) onto `rcur`. Returns the new root or a Conflict error.
fn rebase<S: Storage + ?Sized>(
    storage: &mut S,
    params: &ChunkParams,
    path: &str,
    base: &Hash,
    rcur: &Hash,
    edits: &[Edit],
    current_version: u64,
) -> Result<(Hash, Vec<Hash>, CommitKind)> {
    let runs = changed_runs(storage, base, rcur)?;
    let (base_len, _) = totals(storage, base)?;
    // Conflict detection has to match the granularity of the merge below, which is
    // line-level (`diff3`). A byte-level test misses exactly the case that matters most:
    // against a base of "count: 18", our "count: 19" is a one-byte replacement of the
    // final digit while their "count: 188" is a pure insertion just past it. Those byte
    // intervals are adjacent, not overlapping, so a collision on one line looked disjoint
    // — and the stale side was then shifted onto the current root and applied verbatim,
    // rolling the value back. Two changes to the same line cannot be merged textually, so
    // they must reach `diff3`, which decides between an identical edit and a conflict.
    let mut run_lines = Vec::with_capacity(runs.len());
    for r in &runs {
        run_lines.push(line_span_of(storage, base, r.a_from, r.a_to, base_len)?);
    }
    // Partition edits into disjoint (shiftable) and overlapping.
    let mut overlapping: Vec<&Edit> = Vec::new();
    let mut shifted: Vec<Edit> = Vec::new();
    for e in edits {
        let el = line_span_of(storage, base, e.from, e.to, base_len)?;
        let hit = runs
            .iter()
            .zip(&run_lines)
            .any(|(r, rl)| overlaps(e, r) || (el.0 <= rl.1 && rl.0 <= el.1));
        if hit {
            overlapping.push(e);
        } else {
            shifted.push(Edit::new(shift(e.from, &runs), shift(e.to, &runs), e.replacement.clone()));
        }
    }
    if overlapping.is_empty() {
        let er = apply_edits(storage, params, rcur, &shifted)?;
        return Ok((er.root, er.new_chunks, CommitKind::Rebased));
    }
    // Region in base coordinates: union of overlapping edits and the runs they touch,
    // expanded to line boundaries, iterated until it no longer cuts into a run.
    let mut lo = overlapping.iter().map(|e| e.from).min().unwrap();
    let mut hi = overlapping.iter().map(|e| e.to).max().unwrap();
    loop {
        for r in &runs {
            if r.a_from < hi && r.a_to > lo || (r.a_from == r.a_to && r.a_from >= lo && r.a_from <= hi) {
                lo = lo.min(r.a_from);
                hi = hi.max(r.a_to);
            }
        }
        let l0 = line_of_byte(storage, base, lo)?;
        // `hi` is exclusive: the last byte inside the region decides its last line.
        let l1 = line_of_byte(storage, base, if hi > lo { hi - 1 } else { lo })?;
        let nlo = locate_line(storage, base, l0)?.unwrap_or(0);
        let nhi = locate_line(storage, base, l1 + 1)?.unwrap_or(base_len);
        if nlo == lo && nhi == hi {
            break;
        }
        lo = nlo;
        hi = nhi;
    }
    // Any shifted edit that now falls inside the region must be folded into "ours".
    let inside = |e: &Edit| e.from >= lo && e.to <= hi && !(e.from == e.to && (e.from == lo || e.from == hi));
    let mut ours_edits: Vec<Edit> = edits.iter().filter(|e| inside(e) || overlapping.iter().any(|o| *o == *e)).cloned().collect();
    ours_edits.sort_by_key(|e| e.from);
    let outside: Vec<Edit> = edits
        .iter()
        .filter(|e| !ours_edits.contains(e))
        .map(|e| Edit::new(shift(e.from, &runs), shift(e.to, &runs), e.replacement.clone()))
        .collect();
    // Texts of the region.
    let base_text = materialize_range(storage, base, lo, hi)?;
    let mut ours = Vec::new();
    let mut pos = lo;
    for e in &ours_edits {
        ours.extend_from_slice(&materialize_range(storage, base, pos, e.from)?);
        ours.extend_from_slice(&e.replacement);
        pos = e.to;
    }
    ours.extend_from_slice(&materialize_range(storage, base, pos, hi)?);
    let cur_lo = shift(lo, &runs);
    let cur_hi = shift(hi, &runs);
    let theirs = materialize_range(storage, rcur, cur_lo, cur_hi)?;
    match diff3(&base_text, &ours, &theirs) {
        Merge::Clean(merged) => {
            let mut all = outside;
            all.push(Edit::new(cur_lo, cur_hi, merged));
            all.sort_by_key(|e| e.from);
            let er = apply_edits(storage, params, rcur, &all)?;
            Ok((er.root, er.new_chunks, CommitKind::Merged))
        }
        Merge::Conflict => {
            let l0 = line_of_byte(storage, rcur, cur_lo)?;
            let l1 = line_of_byte(storage, rcur, cur_hi.saturating_sub(1).max(cur_lo))?;
            Err(TextdbError::Conflict(Box::new(ConflictInfo {
                path: path.to_string(),
                region_line_from: l0 + 1,
                region_line_to: l1 + 1,
                base: String::from_utf8_lossy(&base_text).into_owned(),
                theirs: String::from_utf8_lossy(&theirs).into_owned(),
                ours: String::from_utf8_lossy(&ours).into_owned(),
                current_version,
            })))
        }
    }
}

/// Append `tail` at the current end of the file. Never conflicts: the edit is defined
/// relative to the end, so a moved base only changes where it lands.
pub fn commit_append<S: Storage + ?Sized>(
    storage: &mut S,
    params: &ChunkParams,
    file_id: u64,
    path: &str,
    tail: &[u8],
    retries: usize,
) -> Result<Committed> {
    let mut attempt = 0;
    loop {
        let (rcur, v) = storage
            .get_root(file_id)?
            .ok_or_else(|| TextdbError::NotFound(path.to_string()))?;
        let (len, _) = totals(storage, &rcur)?;
        let er = apply_edits(storage, params, &rcur, &[Edit::new(len, len, tail.to_vec())])?;
        if er.root == rcur {
            return Ok(Committed {
                version: v,
                root: rcur,
                kind: CommitKind::NoOp,
                new_chunks: vec![],
                retries: attempt,
            });
        }
        if storage.cas_root(file_id, Some(&rcur), &er.root)? {
            return Ok(Committed {
                version: v + 1,
                root: er.root,
                kind: if attempt == 0 { CommitKind::Direct } else { CommitKind::Rebased },
                new_chunks: er.new_chunks,
                retries: attempt,
            });
        }
        attempt += 1;
        if attempt > retries {
            return Err(TextdbError::Contention);
        }
    }
}
