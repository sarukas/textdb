//! SR — search (§7.7). Ground truth is a reference tokenizer over the reference model
//! (word-boundary, case-insensitive: what `rg -w -i -F` reports), never another backend.

use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use rand::{Rng, SeedableRng};

use crate::gen::Generator;
use crate::metrics::Latencies;
use crate::reference::Reference;
use crate::ops;
use crate::runner::Ctx;
use crate::suites::{import_corpus, line_edit, n_lines};

fn tokens(body: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(body)
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum QKind {
    Single,
    And2,
    Phrase,
    Prefix,
}

fn matches(toks: &[String], kind: QKind, terms: &[String]) -> bool {
    match kind {
        QKind::Single => toks.iter().any(|t| t == &terms[0]),
        QKind::And2 => terms.iter().all(|term| toks.iter().any(|t| t == term)),
        QKind::Phrase => toks.windows(terms.len()).any(|w| w == terms),
        QKind::Prefix => toks.iter().any(|t| t.starts_with(terms[0].as_str())),
    }
}

pub fn search(ctx: &Ctx) -> anyhow::Result<()> {
    let n_files = ctx.params.usize("n_files", 500);
    let size = ctx.params.usize("file_size", 4096);
    let n_queries = ctx.params.usize("n_queries", 50);
    let n_edits = ctx.params.usize("edits_before_search", 0);
    let prefix_filter = ctx.params.str("prefix", "/");
    let mut reference = Reference::default();
    let t0 = Instant::now();
    let (import_lat, paths) = import_corpus(ctx, &mut reference, n_files, size, "/sr")?;
    let import_s = t0.elapsed().as_secs_f64();
    ctx.cell.metric("", "import_s", import_s);
    ctx.cell.lat("", "create", &import_lat);
    let fp_after_import = ctx.backend.storage_bytes().unwrap_or(0);
    ctx.cell.metric("", "footprint_after_import", fp_after_import as f64);
    let raw: usize = paths.iter().map(|p| reference.get(p).len()).sum();
    ctx.cell.metric("", "footprint_over_raw", fp_after_import as f64 / raw.max(1) as f64);

    // SR-04: edits before search, then index growth and maintenance.
    if n_edits > 0 {
        let mut rng = rand::rngs::StdRng::seed_from_u64(ctx.seed ^ 0x5e);
        ctx.backend.reset_counters()?;
        let before = ctx.backend.storage_bytes().unwrap_or(0);
        let mut done = 0;
        for i in 0..n_edits {
            let p = &paths[rng.gen_range(0..paths.len())];
            let cur = reference.get(p).to_vec();
            let idx = rng.gen_range(0..n_lines(&cur).max(1));
            if let Some((old, new)) = line_edit(&cur, idx, 1, &format!("sredit{}", i)) {
                if ctx.backend.replace(p, &old, &new, None).is_ok() {
                    reference.replace(p, &old, &new);
                    done += 1;
                }
            }
        }
        let after = ctx.backend.storage_bytes().unwrap_or(0);
        ctx.cell.metric("", "edits", done as f64);
        ctx.cell.metric("", "footprint_growth_after_edits", after.saturating_sub(before) as f64);
        if done > 0 {
            ctx.cell.metric("", "footprint_growth_per_edit", after.saturating_sub(before) as f64 / done as f64);
        }
        ctx.cell.metric("", "bytes_written_edits", ctx.backend.bytes_written_since_reset().unwrap_or(0) as f64);
        let t = Instant::now();
        match ctx.backend.maintenance() {
            Ok(label) => {
                ctx.cell.metric("", "maintenance_s", t.elapsed().as_secs_f64());
                ctx.cell.note("", "maintenance", label);
                ctx.cell.metric("", "footprint_after_maintenance", ctx.backend.storage_bytes().unwrap_or(0) as f64);
            }
            Err(e) => {
                ctx.err("", "maintenance", &e);
            }
        }
    }

    // Token index of the reference for ground truth and query selection.
    let toks: HashMap<&String, Vec<String>> = paths.iter().map(|p| (p, tokens(reference.get(p)))).collect();
    let mut df: HashMap<String, usize> = HashMap::new();
    for t in toks.values() {
        for w in t.iter().collect::<BTreeSet<_>>() {
            *df.entry(w.clone()).or_default() += 1;
        }
    }
    // Terms with mid-range document frequency (1%–30%) make informative queries.
    let mut candidates: Vec<String> = df
        .iter()
        .filter(|(w, &c)| w.len() >= 3 && c >= (n_files / 100).max(1) && c <= n_files * 3 / 10)
        .map(|(w, _)| w.clone())
        .collect();
    candidates.sort();
    if candidates.is_empty() {
        candidates = df.keys().cloned().collect();
        candidates.sort();
    }
    let mut rng = rand::rngs::StdRng::seed_from_u64(ctx.seed ^ 0x5ea);
    let g = Generator::new(ctx.seed);
    let _ = g;
    for kind in [QKind::Single, QKind::And2, QKind::Phrase, QKind::Prefix] {
        let case = format!("{:?}", kind).to_lowercase();
        let mut lat = Latencies::default();
        // Snippet quality, alongside recall and precision: how many hits carried a snippet,
        // and how many of those actually showed a searched-for term.
        let (mut snippets_ok, mut snippets_wrong, mut snippets_absent) = (0usize, 0usize, 0usize);
        let mut snippet_examples: Vec<String> = Vec::new();
        let (mut tp, mut fp, mut fn_) = (0usize, 0usize, 0usize);
        let mut na = false;
        for _ in 0..n_queries {
            let (terms, query): (Vec<String>, String) = match kind {
                QKind::Single => {
                    let t = candidates[rng.gen_range(0..candidates.len())].clone();
                    (vec![t.clone()], t)
                }
                QKind::And2 => {
                    let a = candidates[rng.gen_range(0..candidates.len())].clone();
                    let b = candidates[rng.gen_range(0..candidates.len())].clone();
                    (vec![a.clone(), b.clone()], format!("{} {}", a, b))
                }
                QKind::Phrase => {
                    // Take two consecutive tokens from a random document.
                    let p = &paths[rng.gen_range(0..paths.len())];
                    let t = &toks[p];
                    if t.len() < 2 {
                        continue;
                    }
                    let i = rng.gen_range(0..t.len() - 1);
                    (vec![t[i].clone(), t[i + 1].clone()], format!("\"{} {}\"", t[i], t[i + 1]))
                }
                QKind::Prefix => {
                    let t = candidates[rng.gen_range(0..candidates.len())].clone();
                    let stem: String = t.chars().take(4.min(t.chars().count())).collect();
                    (vec![stem.clone()], format!("{}*", stem))
                }
            };
            let truth: BTreeSet<&String> = paths
                .iter()
                .filter(|p| p.starts_with(&prefix_filter) || prefix_filter == "/")
                .filter(|p| matches(&toks[*p], kind, &terms))
                .collect();
            match ctx.op(ops::SEARCH, &mut lat, || ctx.backend.search(&query, &prefix_filter)) {
                Ok(hits) => {
                    // A snippet is what the user reads, so a search that finds the right
                    // document and shows the wrong line is still wrong. The check is the
                    // weakest one that would catch that: the snippet has to contain a term
                    // that was searched for. A prefix query matches on the stem, and a phrase
                    // on either word, since a snippet is a window and may cut at either end.
                    for h in &hits {
                        // Only documents that genuinely match are judged. A false positive's
                        // snippet is meaningless by construction — there is no term in the
                        // document to show — and `precision` already counts it.
                        if !truth.contains(&h.path) {
                            continue;
                        }
                        let Some(sn) = h.snippet.as_deref() else {
                            snippets_absent += 1;
                            continue;
                        };
                        // Folded the way the index and the store fold, and word by word so a
                        // phrase is not looked for with its spaces intact.
                        let low = textdb_core::fold::fold(sn);
                        if terms
                            .iter()
                            .flat_map(|t| t.split_whitespace())
                            .any(|t| low.contains(&textdb_core::fold::fold(t.trim_end_matches('*'))))
                        {
                            snippets_ok += 1;
                        } else {
                            if snippet_examples.len() < 3 {
                                snippet_examples.push(format!("{} @{} for {:?}: {:?}", h.path, h.line, query, sn));
                            }
                            snippets_wrong += 1;
                        }
                    }
                    let got: BTreeSet<String> = hits.into_iter().map(|h| h.path).collect();
                    for p in &truth {
                        if got.contains(*p) {
                            tp += 1;
                        } else {
                            fn_ += 1;
                        }
                    }
                    for p in &got {
                        if !truth.contains(&p) {
                            fp += 1;
                        }
                    }
                }
                Err(e) => {
                    na = ctx.err(&case, "search", &e) == false;
                    break;
                }
            }
        }
        if na {
            continue;
        }
        ctx.cell.lat(&case, "search", &lat);
        let recall = if tp + fn_ > 0 { tp as f64 / (tp + fn_) as f64 } else { 1.0 };
        let precision = if tp + fp > 0 { tp as f64 / (tp + fp) as f64 } else { 1.0 };
        ctx.cell.metric(&case, "recall", recall);
        ctx.cell.metric(&case, "precision", precision);
        // Reported as a share of the hits that carried one, so a backend with no snippets is
        // not scored as if it had wrong ones — that shows up in `snippet_coverage` instead.
        let with = snippets_ok + snippets_wrong;
        if with + snippets_absent > 0 {
            ctx.cell
                .metric(&case, "snippet_coverage", with as f64 / (with + snippets_absent) as f64);
        }
        if snippets_wrong > 0 {
            ctx.cell.fail(
                &case,
                "snippet_shows_the_term",
                &format!("{} of {} hits; e.g. {}", snippets_wrong, with, snippet_examples.join("; ")),
            );
        } else if with > 0 {
            ctx.cell.metric(&case, "snippet_shows_the_term", 1.0);
        }
    }
    ctx.set_reference(reference);
    Ok(())
}
