//! Sequence diff (Myers, O(ND)) over hashed lines, plus a line-level three-way merge.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher};

/// Split bytes into lines, each including its trailing `\n` if present.
pub fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

fn hash_line(l: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    l.hash(&mut h);
    h.finish()
}

/// One hunk of a diff: `a[a_from..a_to)` is replaced by `b[b_from..b_to)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub a_from: usize,
    pub a_to: usize,
    pub b_from: usize,
    pub b_to: usize,
}

/// Diff two sequences, returning hunks in ascending order. If the edit distance
/// exceeds `max_d`, falls back to one hunk covering everything between the common
/// prefix and suffix (bounded memory).
pub fn diff_seq<T: PartialEq>(a: &[T], b: &[T], max_d: usize) -> Vec<Hunk> {
    // Trim common prefix/suffix.
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf] {
        suf += 1;
    }
    let a2 = &a[pre..a.len() - suf];
    let b2 = &b[pre..b.len() - suf];
    if a2.is_empty() && b2.is_empty() {
        return vec![];
    }
    if a2.is_empty() || b2.is_empty() {
        return vec![Hunk {
            a_from: pre,
            a_to: pre + a2.len(),
            b_from: pre,
            b_to: pre + b2.len(),
        }];
    }
    let hunks = match myers(a2, b2, max_d) {
        Some(h) => h,
        None => vec![Hunk {
            a_from: 0,
            a_to: a2.len(),
            b_from: 0,
            b_to: b2.len(),
        }],
    };
    hunks
        .into_iter()
        .map(|h| Hunk {
            a_from: h.a_from + pre,
            a_to: h.a_to + pre,
            b_from: h.b_from + pre,
            b_to: h.b_to + pre,
        })
        .collect()
}

/// Classic Myers forward algorithm with a trace, returning hunks. `None` if D > max_d.
fn myers<T: PartialEq>(a: &[T], b: &[T], max_d: usize) -> Option<Vec<Hunk>> {
    let n = a.len() as i64;
    let m = b.len() as i64;
    let max = (n + m) as usize;
    let off = max as i64 + 1;
    let width = 2 * max + 3;
    let mut v = vec![0i64; width];
    let mut trace: Vec<Vec<i64>> = Vec::new();
    let mut found = false;
    let mut d_final = 0usize;
    'outer: for d in 0..=max {
        if d > max_d {
            return None;
        }
        trace.push(v.clone());
        let mut k = -(d as i64);
        while k <= d as i64 {
            let idx = (k + off) as usize;
            let mut x = if k == -(d as i64) || (k != d as i64 && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = true;
                d_final = d;
                break 'outer;
            }
            k += 2;
        }
    }
    if !found {
        return None;
    }
    // Backtrack.
    let mut x = n;
    let mut y = m;
    let mut ops: Vec<(i64, i64, u8)> = Vec::new(); // (x, y, kind) kind: 0 del a[x], 1 ins b[y]
    for d in (0..=d_final).rev() {
        let vprev = &trace[d];
        let k = x - y;
        let idx = (k + off) as usize;
        let prev_k = if k == -(d as i64) || (k != d as i64 && vprev[idx - 1] < vprev[idx + 1]) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = vprev[(prev_k + off) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                ops.push((prev_x, prev_y, 1)); // insertion of b[prev_y]
            } else {
                ops.push((prev_x, prev_y, 0)); // deletion of a[prev_x]
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    // Group consecutive ops into hunks.
    let mut hunks: Vec<Hunk> = Vec::new();
    for (ox, oy, kind) in ops {
        let (a_to, b_to) = if kind == 0 { (ox + 1, oy) } else { (ox, oy + 1) };
        if let Some(h) = hunks.last_mut() {
            if h.a_to as i64 == ox && h.b_to as i64 == oy {
                h.a_to = a_to as usize;
                h.b_to = b_to as usize;
                continue;
            }
        }
        hunks.push(Hunk {
            a_from: ox as usize,
            a_to: a_to as usize,
            b_from: oy as usize,
            b_to: b_to as usize,
        });
    }
    Some(hunks)
}

/// Byte-range edits that transform `a` into `b`, computed as a line diff refined by
/// trimming common bytes inside each hunk (spec O3: line + byte refinement).
pub fn byte_edits(a: &[u8], b: &[u8]) -> Vec<crate::edit::Edit> {
    let al = split_lines(a);
    let bl = split_lines(b);
    let ah: Vec<u64> = al.iter().map(|l| hash_line(l)).collect();
    let bh: Vec<u64> = bl.iter().map(|l| hash_line(l)).collect();
    let a_off: Vec<usize> = std::iter::once(0)
        .chain(al.iter().scan(0, |s, l| {
            *s += l.len();
            Some(*s)
        }))
        .collect();
    let b_off: Vec<usize> = std::iter::once(0)
        .chain(bl.iter().scan(0, |s, l| {
            *s += l.len();
            Some(*s)
        }))
        .collect();
    let mut out = Vec::new();
    for h in diff_seq(&ah, &bh, 4096) {
        let mut af = a_off[h.a_from];
        let mut at = a_off[h.a_to];
        let mut bf = b_off[h.b_from];
        let mut bt = b_off[h.b_to];
        // Byte refinement.
        while af < at && bf < bt && a[af] == b[bf] {
            af += 1;
            bf += 1;
        }
        while at > af && bt > bf && a[at - 1] == b[bt - 1] {
            at -= 1;
            bt -= 1;
        }
        if af == at && bf == bt {
            continue;
        }
        out.push(crate::edit::Edit::new(af as u64, at as u64, b[bf..bt].to_vec()));
    }
    out
}

/// Unified-diff text for two byte strings, with `context` lines of context.
pub fn unified(a: &[u8], b: &[u8], context: usize, a_line_base: usize, b_line_base: usize) -> String {
    let al = split_lines(a);
    let bl = split_lines(b);
    let ah: Vec<u64> = al.iter().map(|l| hash_line(l)).collect();
    let bh: Vec<u64> = bl.iter().map(|l| hash_line(l)).collect();
    let hunks = diff_seq(&ah, &bh, 4096);
    let mut out = String::new();
    let render = |l: &[u8]| -> String {
        let s = String::from_utf8_lossy(l);
        if s.ends_with('\n') {
            s.into_owned()
        } else {
            format!("{}\n\\ No newline at end of file\n", s)
        }
    };
    // Merge hunks whose context overlaps.
    let mut groups: Vec<Vec<Hunk>> = Vec::new();
    for h in hunks {
        if let Some(g) = groups.last_mut() {
            let last = g.last().unwrap();
            if h.a_from <= last.a_to + 2 * context {
                g.push(h);
                continue;
            }
        }
        groups.push(vec![h]);
    }
    for g in groups {
        let first = g[0];
        let last = *g.last().unwrap();
        let a_start = first.a_from.saturating_sub(context);
        let a_end = (last.a_to + context).min(al.len());
        let b_start = first.b_from.saturating_sub(context);
        let b_end = (last.b_to + context).min(bl.len());
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            a_start + a_line_base,
            a_end - a_start,
            b_start + b_line_base,
            b_end - b_start
        ));
        let mut ai = a_start;
        for h in g {
            while ai < h.a_from {
                out.push(' ');
                out.push_str(&render(al[ai]));
                ai += 1;
            }
            for i in h.a_from..h.a_to {
                out.push('-');
                out.push_str(&render(al[i]));
            }
            for i in h.b_from..h.b_to {
                out.push('+');
                out.push_str(&render(bl[i]));
            }
            ai = h.a_to;
        }
        while ai < a_end {
            out.push(' ');
            out.push_str(&render(al[ai]));
            ai += 1;
        }
    }
    out
}

/// Result of a three-way line merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merge {
    Clean(Vec<u8>),
    Conflict,
}

/// Line-level diff3: merge `ours` and `theirs` against `base`. Conflicts when both sides
/// change overlapping line ranges differently.
pub fn diff3(base: &[u8], ours: &[u8], theirs: &[u8]) -> Merge {
    let bl = split_lines(base);
    let ol = split_lines(ours);
    let tl = split_lines(theirs);
    let bh: Vec<u64> = bl.iter().map(|l| hash_line(l)).collect();
    let oh: Vec<u64> = ol.iter().map(|l| hash_line(l)).collect();
    let th: Vec<u64> = tl.iter().map(|l| hash_line(l)).collect();
    let ho = diff_seq(&bh, &oh, 4096);
    let ht = diff_seq(&bh, &th, 4096);

    let mut out: Vec<u8> = Vec::new();
    let mut bi = 0usize; // base line index emitted so far
    let (mut i, mut j) = (0usize, 0usize);
    while i < ho.len() || j < ht.len() {
        // Pick the next hunk(s) by base position.
        let next_o = ho.get(i);
        let next_t = ht.get(j);
        let (lo, mut hi, mut use_o, mut use_t) = match (next_o, next_t) {
            (Some(o), Some(t)) => {
                if o.a_to <= t.a_from && !(o.a_from == t.a_from) {
                    (o.a_from, o.a_to, true, false)
                } else if t.a_to <= o.a_from && !(o.a_from == t.a_from) {
                    (t.a_from, t.a_to, false, true)
                } else {
                    (o.a_from.min(t.a_from), o.a_to.max(t.a_to), true, true)
                }
            }
            (Some(o), None) => (o.a_from, o.a_to, true, false),
            (None, Some(t)) => (t.a_from, t.a_to, false, true),
            (None, None) => break,
        };
        // Emit unchanged base lines before the region.
        for l in &bl[bi..lo] {
            out.extend_from_slice(l);
        }
        if use_o && !use_t {
            let o = ho[i];
            for l in &ol[o.b_from..o.b_to] {
                out.extend_from_slice(l);
            }
            i += 1;
            bi = o.a_to;
            continue;
        }
        if use_t && !use_o {
            let t = ht[j];
            for l in &tl[t.b_from..t.b_to] {
                out.extend_from_slice(l);
            }
            j += 1;
            bi = t.a_to;
            continue;
        }
        // Overlapping region: absorb all hunks from both sides touching [lo, hi).
        let (o_start, t_start) = (i, j);
        loop {
            let mut grew = false;
            while i < ho.len() && ho[i].a_from <= hi && (ho[i].a_from < hi || ho[i].a_from == lo || ho[i].a_to > hi) {
                hi = hi.max(ho[i].a_to);
                i += 1;
                grew = true;
            }
            while j < ht.len() && ht[j].a_from <= hi && (ht[j].a_from < hi || ht[j].a_from == lo || ht[j].a_to > hi) {
                hi = hi.max(ht[j].a_to);
                j += 1;
                grew = true;
            }
            if !grew {
                break;
            }
        }
        use_o = i > o_start;
        use_t = j > t_start;
        // Build each side's text for base region [lo, hi).
        let side = |hunks: &[Hunk], lines: &[&[u8]]| -> Vec<u8> {
            let mut v = Vec::new();
            let mut b = lo;
            for h in hunks {
                for l in &bl[b..h.a_from] {
                    v.extend_from_slice(l);
                }
                for l in &lines[h.b_from..h.b_to] {
                    v.extend_from_slice(l);
                }
                b = h.a_to;
            }
            for l in &bl[b..hi] {
                v.extend_from_slice(l);
            }
            v
        };
        let o_text = if use_o { side(&ho[o_start..i], &ol) } else { concat(&bl[lo..hi]) };
        let t_text = if use_t { side(&ht[t_start..j], &tl) } else { concat(&bl[lo..hi]) };
        let base_text = concat(&bl[lo..hi]);
        if o_text == t_text {
            out.extend_from_slice(&o_text);
        } else if o_text == base_text {
            out.extend_from_slice(&t_text);
        } else if t_text == base_text {
            out.extend_from_slice(&o_text);
        } else {
            return Merge::Conflict;
        }
        bi = hi;
    }
    for l in &bl[bi..] {
        out.extend_from_slice(l);
    }
    Merge::Clean(out)
}

fn concat(lines: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::new();
    for l in lines {
        v.extend_from_slice(l);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(a: &[u8], edits: &[crate::edit::Edit]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut pos = 0u64;
        for e in edits {
            out.extend_from_slice(&a[pos as usize..e.from as usize]);
            out.extend_from_slice(&e.replacement);
            pos = e.to;
        }
        out.extend_from_slice(&a[pos as usize..]);
        out
    }

    #[test]
    fn byte_edits_roundtrip() {
        let a = b"alpha\nbeta\ngamma\ndelta\n";
        let b = b"alpha\nbeta2\ngamma\nnew\ndelta";
        let e = byte_edits(a, b);
        assert_eq!(apply(a, &e), b.to_vec());
        let e = byte_edits(b, a);
        assert_eq!(apply(b, &e), a.to_vec());
        assert_eq!(byte_edits(a, a), vec![]);
        assert_eq!(apply(b"", &byte_edits(b"", a)), a.to_vec());
        assert_eq!(apply(a, &byte_edits(a, b"")), b"".to_vec());
    }

    #[test]
    fn diff3_cases() {
        let base = b"a\nb\nc\nd\ne\n";
        assert_eq!(diff3(base, b"a\nB\nc\nd\ne\n", b"a\nb\nc\nd\nE\n"), Merge::Clean(b"a\nB\nc\nd\nE\n".to_vec()));
        assert_eq!(diff3(base, b"a\nB\nc\nd\ne\n", b"a\nX\nc\nd\ne\n"), Merge::Conflict);
        assert_eq!(diff3(base, b"a\nB\nc\nd\ne\n", b"a\nB\nc\nd\ne\n"), Merge::Clean(b"a\nB\nc\nd\ne\n".to_vec()));
        assert_eq!(diff3(base, base, b"a\nb\nc\nd\ne\nf\n"), Merge::Clean(b"a\nb\nc\nd\ne\nf\n".to_vec()));
        assert_eq!(diff3(base, b"a\nc\nd\ne\n", b"a\nb\nc\nd\n"), Merge::Clean(b"a\nc\nd\n".to_vec()));
        // Adjacent but non-overlapping line changes merge cleanly.
        assert_eq!(diff3(base, b"a\nB\nc\nd\ne\n", b"a\nb\nC\nd\ne\n"), Merge::Clean(b"a\nB\nC\nd\ne\n".to_vec()));
    }

    #[test]
    fn unified_has_hunk() {
        let u = unified(b"a\nb\nc\n", b"a\nB\nc\n", 3, 1, 1);
        assert!(u.starts_with("@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"), "{}", u);
    }
}
