//! Property tests P1–P6 (spec §9.1).

use proptest::prelude::*;
use std::collections::HashSet;
use textdb_core::commit::{commit, CommitKind};
use textdb_core::edit::{apply_edits, Edit};
use textdb_core::myers::byte_edits;
use textdb_core::tree::{depth, leaves, locate_line, materialize};
use textdb_core::{build, changed_runs, ChunkParams, MemStorage, Storage};

const P: ChunkParams = ChunkParams::DEFAULT;

fn text_strategy(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // Line-oriented ASCII text with varied line lengths.
        proptest::collection::vec(
            prop_oneof![
                4 => "[a-z ]{0,120}\n".prop_map(|s| s.into_bytes()),
                1 => "[a-zA-Z0-9 .,;]{0,400}\r\n".prop_map(|s| s.into_bytes()),
                1 => "[ąčęėįšųūž一二三😀 ]{0,60}\n".prop_map(|s| s.into_bytes()),
            ],
            0..(max / 40).max(1)
        )
        .prop_map(|lines| lines.concat()),
        // Arbitrary bytes, including invalid UTF-8.
        proptest::collection::vec(any::<u8>(), 0..max),
        // Highly repetitive content (stress-tests node-boundary resync).
        (0usize..max).prop_map(|n| b"same line\n".repeat(n / 10 + 1)),
    ]
}

fn apply_ref(a: &[u8], edits: &[Edit]) -> Vec<u8> {
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

fn random_edits(len: usize, n: usize, seed: u64) -> Vec<Edit> {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut cuts: Vec<u64> = (0..2 * n).map(|_| rng.gen_range(0..=len as u64)).collect();
    cuts.sort();
    let mut edits = Vec::new();
    for i in 0..n {
        let from = cuts[2 * i];
        let to = cuts[2 * i + 1];
        let rl = rng.gen_range(0..300usize);
        let repl: Vec<u8> = (0..rl).map(|_| if rng.gen_bool(0.05) { b'\n' } else { rng.gen_range(b'a'..=b'z') }).collect();
        edits.push(Edit::new(from, to, repl));
    }
    edits
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

    /// P1: materialize(build(bytes)) == bytes.
    #[test]
    fn p1_roundtrip(bytes in text_strategy(40_000)) {
        let mut s = MemStorage::new();
        let root = build(&mut s, &P, &bytes).unwrap();
        prop_assert_eq!(materialize(&s, &root).unwrap(), bytes);
    }

    /// P2: the root is independent of construction path.
    #[test]
    fn p2_history_independence(bytes in text_strategy(30_000), seed in any::<u64>()) {
        let mut s = MemStorage::new();
        let root = build(&mut s, &P, &bytes).unwrap();
        let edits = random_edits(bytes.len(), 3, seed);
        let expected = apply_ref(&bytes, &edits);
        let er = apply_edits(&mut s, &P, &root, &edits).unwrap();
        prop_assert_eq!(materialize(&s, &er.root).unwrap(), expected.clone());
        let mut s2 = MemStorage::new();
        let direct = build(&mut s2, &P, &expected).unwrap();
        prop_assert_eq!(er.root, direct, "edit path root differs from direct build");
        // Applying the inverse edit set also returns to the original root.
        let back = byte_edits(&expected, &bytes);
        let er2 = apply_edits(&mut s, &P, &er.root, &back).unwrap();
        prop_assert_eq!(er2.root, root);
    }

    /// P4: locate_line(root, k) equals the offset of the k-th `\n` + 1.
    #[test]
    fn p4_locate_line(bytes in text_strategy(30_000)) {
        let mut s = MemStorage::new();
        let root = build(&mut s, &P, &bytes).unwrap();
        let mut k = 0u64;
        prop_assert_eq!(locate_line(&s, &root, 0).unwrap(), Some(0));
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                k += 1;
                prop_assert_eq!(locate_line(&s, &root, k).unwrap(), Some(i as u64 + 1));
            }
        }
        prop_assert_eq!(locate_line(&s, &root, k + 1).unwrap(), None);
    }

    /// P5: diff(a, b) applied to materialize(a) yields materialize(b).
    #[test]
    fn p5_diff_applies(bytes in text_strategy(30_000), seed in any::<u64>()) {
        let mut s = MemStorage::new();
        let a = build(&mut s, &P, &bytes).unwrap();
        let edits = random_edits(bytes.len(), 2, seed);
        let b = apply_edits(&mut s, &P, &a, &edits).unwrap().root;
        let runs = changed_runs(&s, &a, &b).unwrap();
        // Apply runs as byte edits taking replacement text from b.
        let bb = materialize(&s, &b).unwrap();
        let as_edits: Vec<Edit> = runs.iter().map(|r| Edit::new(r.a_from, r.a_to, bb[r.b_from as usize..r.b_to as usize].to_vec())).collect();
        prop_assert_eq!(apply_ref(&bytes, &as_edits), bb);
    }

    /// `byte_edits` is the whole-document write path (`UPDATE kb SET content = …`), so its
    /// edits must reconstruct the target exactly — including when the changes are scattered
    /// and the common prefix/suffix trim buys nothing, which is the case the banded Myers
    /// arrays are sized for.
    #[test]
    fn p7_byte_edits_reconstructs_scattered_changes(
        bytes in text_strategy(30_000),
        seed in any::<u64>(),
        every in 1usize..9,
    ) {
        let other = scatter_lines(&bytes, every, seed);
        let edits = byte_edits(&bytes, &other);
        prop_assert_eq!(apply_ref(&bytes, &edits), other.clone());
        // And in the other direction, which reverses which side is longer.
        let back = byte_edits(&other, &bytes);
        prop_assert_eq!(apply_ref(&other, &back), bytes);
    }

    /// P8: line hunks patch one version into the next. Splicing each hunk's new text over
    /// its old line range turns `a` into `b`, each hunk's old text is what `a` holds at those
    /// lines, and `new_from` is `old_from` shifted by the hunks before it — which is what a
    /// viewer relies on when it patches the lines it is showing instead of reloading.
    #[test]
    fn p8_line_hunks_patch_versions(
        bytes in text_strategy(30_000),
        seed in any::<u64>(),
        every in 1usize..9,
        scatter in any::<bool>(),
    ) {
        let mut s = MemStorage::new();
        let a = build(&mut s, &P, &bytes).unwrap();
        let target = if scatter {
            scatter_lines(&bytes, every, seed)
        } else {
            apply_ref(&bytes, &random_edits(bytes.len(), 2, seed))
        };
        let b = build(&mut s, &P, &target).unwrap();
        let hunks = textdb_core::line_hunks(&s, &a, &b).unwrap();
        let old_lines: Vec<&[u8]> = bytes.split_inclusive(|&c| c == b'\n').collect();
        let mut out = Vec::new();
        let mut next = 0usize;
        let mut shift = 0i64;
        for h in &hunks {
            let from = h.old_from as usize;
            let to = from + h.old_count as usize;
            prop_assert!(next <= from && to <= old_lines.len(), "hunk {:?} out of order or range", (h.old_from, h.old_count));
            for l in &old_lines[next..from] {
                out.extend_from_slice(l);
            }
            prop_assert_eq!(old_lines[from..to].concat(), h.old_text.clone());
            prop_assert_eq!(h.new_from as i64, h.old_from as i64 + shift);
            out.extend_from_slice(&h.new_text);
            shift += h.new_count as i64 - h.old_count as i64;
            next = to;
        }
        for l in &old_lines[next..] {
            out.extend_from_slice(l);
        }
        prop_assert_eq!(out, target);
    }
}

/// Change every `every`-th line of `bytes`, leaving the rest alone.
fn scatter_lines(bytes: &[u8], every: usize, seed: u64) -> Vec<u8> {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / every.max(1) + 16);
    for (i, line) in bytes.split_inclusive(|&b| b == b'\n').enumerate() {
        if i % every == 0 {
            match rng.gen_range(0..3) {
                0 => out.extend_from_slice(b"CHANGED "), // prefix
                1 => {}                                  // delete the line
                _ => {
                    out.extend_from_slice(line);
                    out.extend_from_slice(b"INSERTED\n"); // add one after
                    continue;
                }
            }
        }
        out.extend_from_slice(line);
    }
    out
}

/// The working arrays of `myers` are sized by the distance bound, so a large document with
/// many scattered one-line changes costs `O(D^2)` rather than `O(D * (N + M))`. Sized by the
/// inputs instead, this case took 42 seconds and 7 GiB of resident memory, and a slightly
/// larger one was killed by the OOM reaper before `max_d` could fall back — so this test is
/// a guard against a crash, not only against a slowdown.
///
/// The assertion is deliberately loose: it only has to fail if the quadratic-in-input
/// behaviour comes back, and that costs three orders of magnitude, not a factor of two.
#[test]
fn byte_edits_scattered_changes_stay_bounded() {
    let mut bytes = Vec::new();
    for i in 0..120_000 {
        bytes.extend_from_slice(format!("line {:06} lorem ipsum dolor sit amet consectetur adipiscing\n", i).as_bytes());
    }
    assert!(bytes.len() > 7 << 20, "document should be ~8 MiB, got {}", bytes.len());
    let other = scatter_lines(&bytes, 100, 99);
    let t = std::time::Instant::now();
    let edits = byte_edits(&bytes, &other);
    let elapsed = t.elapsed();
    eprintln!(
        "8 MiB document, {} changed lines -> {} edits in {:?}",
        120_000 / 100,
        edits.len(),
        elapsed
    );
    assert_eq!(apply_ref(&bytes, &edits), other, "edits must reconstruct the target");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "byte_edits took {:?}; the Myers arrays are probably sized by the inputs again",
        elapsed
    );
}

/// P3: a single contiguous edit produces ≤ ⌈len(replacement)/min⌉ + 3 new leaves in
/// the common case. Reported as a distribution; the assertion is on the 99th percentile
/// because CDC resynchronisation is probabilistic (spec §13, risk 1).
#[test]
fn p3_edit_locality() {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut excess = Vec::new();
    let mut worst = 0i64;
    for _ in 0..500 {
        let n_lines = rng.gen_range(50..3000);
        let mut bytes = Vec::new();
        for i in 0..n_lines {
            let len = rng.gen_range(0..160);
            bytes.extend((0..len).map(|_| rng.gen_range(b'a'..=b'z')));
            bytes.extend_from_slice(format!(" {}\n", i).as_bytes());
        }
        let mut s = MemStorage::new();
        let root = build(&mut s, &P, &bytes).unwrap();
        let before: HashSet<_> = leaves(&s, &root).unwrap().into_iter().map(|l| l.hash).collect();
        let from = rng.gen_range(0..=bytes.len()) as u64;
        let to = (from + rng.gen_range(0..200)).min(bytes.len() as u64);
        let rl = rng.gen_range(0..3000usize);
        let repl: Vec<u8> = (0..rl).map(|_| if rng.gen_bool(0.02) { b'\n' } else { b'x' }).collect();
        let er = apply_edits(&mut s, &P, &root, &[Edit::new(from, to, repl.clone())]).unwrap();
        let after: Vec<_> = leaves(&s, &er.root).unwrap();
        let new_leaves = after.iter().filter(|l| !before.contains(&l.hash)).count() as i64;
        let bound = (rl as i64 + P.min as i64 - 1) / P.min as i64 + 3;
        excess.push(new_leaves - bound);
        worst = worst.max(new_leaves - bound);
    }
    excess.sort();
    let p99 = excess[excess.len() * 99 / 100];
    let within = excess.iter().filter(|&&e| e <= 0).count();
    eprintln!(
        "P3: {}/{} edits within bound; p99 excess {} leaves; worst excess {}",
        within,
        excess.len(),
        p99,
        worst
    );
    assert!(within as f64 / excess.len() as f64 >= 0.95, "P3 holds for fewer than 95% of edits");
}

/// P6: rebase of disjoint edits is commutative.
#[test]
fn p6_rebase_commutes() {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let mut rebased = 0;
    for _ in 0..200 {
        let n_lines = rng.gen_range(20..400);
        let mut bytes = Vec::new();
        for i in 0..n_lines {
            bytes.extend_from_slice(format!("line {} {}\n", i, "x".repeat(rng.gen_range(0..80))).as_bytes());
        }
        // Two edits on distinct lines.
        let l1 = rng.gen_range(0..n_lines / 2) as usize;
        let l2 = rng.gen_range(n_lines / 2 + 1..n_lines) as usize;
        let mut s_a = MemStorage::new();
        let root = build(&mut s_a, &P, &bytes).unwrap();
        s_a.cas_root(1, None, &root).unwrap();
        let mut s_b = s_a.clone();
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(bytes.iter().enumerate().filter(|(_, &b)| b == b'\n').map(|(i, _)| i + 1))
            .collect();
        let e1 = Edit::new(line_starts[l1] as u64, (line_starts[l1 + 1] - 1) as u64, b"EDIT ONE".to_vec());
        let e2 = Edit::new(line_starts[l2] as u64, (line_starts[l2 + 1] - 1) as u64, b"EDIT TWO longer".to_vec());
        // Order A: e1 then e2 (e2 rebased against e1's result).
        let c1 = commit(&mut s_a, &P, 1, "f", &root, &[e1.clone()], 8).unwrap();
        assert_eq!(c1.kind, CommitKind::Direct);
        let c2 = commit(&mut s_a, &P, 1, "f", &root, &[e2.clone()], 8).unwrap();
        assert_eq!(c2.kind, CommitKind::Rebased);
        rebased += 1;
        // Order B.
        commit(&mut s_b, &P, 1, "f", &root, &[e2.clone()], 8).unwrap();
        let c2b = commit(&mut s_b, &P, 1, "f", &root, &[e1.clone()], 8).unwrap();
        assert_eq!(c2.root, c2b.root, "roots differ between orders");
        let expected = apply_ref(&bytes, &[e1, e2]);
        assert_eq!(materialize(&s_a, &c2.root).unwrap(), expected);
        assert_eq!(s_a.get_root(1).unwrap().unwrap().1, 3); // initial + two commits
    }
    eprintln!("P6: {} commutative rebase pairs verified", rebased);
}

#[test]
fn same_line_edits_conflict_and_merge() {
    let bytes = b"alpha\nbeta\ngamma\n".to_vec();
    let mut s = MemStorage::new();
    let root = build(&mut s, &P, &bytes).unwrap();
    s.cas_root(1, None, &root).unwrap();
    commit(&mut s, &P, 1, "f", &root, &[Edit::new(6, 10, b"BETA".to_vec())], 8).unwrap();
    let err = commit(&mut s, &P, 1, "f", &root, &[Edit::new(6, 10, b"Beta!".to_vec())], 8).unwrap_err();
    match err {
        textdb_core::TextdbError::Conflict(c) => {
            assert_eq!(c.theirs, "BETA\n");
            assert_eq!(c.ours, "Beta!\n");
            assert_eq!(c.base, "beta\n");
            assert_eq!(c.region_line_from, 2);
        }
        e => panic!("expected conflict, got {:?}", e),
    }
    // Identical concurrent change merges cleanly.
    let c = commit(&mut s, &P, 1, "f", &root, &[Edit::new(6, 10, b"BETA".to_vec())], 8).unwrap();
    assert_eq!(c.kind, CommitKind::NoOp);
}

#[test]
fn large_doc_tree_depth_and_sharing() {
    let mut s = MemStorage::new();
    let mut bytes = Vec::new();
    for i in 0..200_000 {
        bytes.extend_from_slice(format!("row {} some filler text to make lines realistic\n", i).as_bytes());
    }
    let root = build(&mut s, &P, &bytes).unwrap();
    let d = depth(&s, &root).unwrap();
    let n_leaves = leaves(&s, &root).unwrap().len();
    let chunks_before = s.chunks.len();
    let nodes_before = s.nodes.len();
    let er = apply_edits(&mut s, &P, &root, &[Edit::new(5_000_000, 5_000_010, b"CHANGED".to_vec())]).unwrap();
    let new_chunks = s.chunks.len() - chunks_before;
    let new_nodes = s.nodes.len() - nodes_before;
    eprintln!(
        "10 MB doc: {} leaves, depth {}, edit wrote {} chunks and {} nodes ({} chunk bytes)",
        n_leaves, d, new_chunks, new_nodes, er.new_chunks.len()
    );
    assert!(new_chunks <= 6, "too many new chunks: {}", new_chunks);
    assert!(new_nodes <= 3 * d + 3, "too many new nodes: {}", new_nodes);
    let mut s2 = MemStorage::new();
    let mut expected = bytes.clone();
    expected.splice(5_000_000..5_000_010, b"CHANGED".iter().copied());
    assert_eq!(build(&mut s2, &P, &expected).unwrap(), er.root);
}
