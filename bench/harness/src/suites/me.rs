//! ME — many edits on the same region (§7.4), also RT-04/05 edit round-trips and
//! LL-04-style leaf stability. Sequential, one writer.

use std::collections::HashSet;

use rand::{Rng, SeedableRng};

use crate::backend::WriteOutcome;
use crate::gen::{Charset, GenOpts, Generator};
use crate::metrics::Latencies;
use crate::reference::Reference;
use crate::ops;
use crate::runner::Ctx;
use crate::suites::{line_edit, line_span, n_lines, Zipf};

pub fn make_content(ctx: &Ctx, g: &mut Generator, size: usize) -> Vec<u8> {
    match ctx.params.str("content", "markdown").as_str() {
        "single_line" => g.single_line(size, Charset::Ascii),
        "json" => g.json_like(size),
        "fixed_lines" => {
            let len = ctx.params.usize("fixed_line_len", 10240);
            g.fixed_lines(size / len, len)
        }
        _ => g.markdown(size, &GenOpts::default()),
    }
}

pub fn edit_sequence(ctx: &Ctx) -> anyhow::Result<()> {
    let size = ctx.params.usize("size", 102_400);
    let n_edits = ctx.params.usize("n_edits", 1000);
    let pattern = ctx.params.str("pattern", "random_line");
    let check_versions = ctx.params.bool("check_versions", false);
    let footprint_every = ctx.params.usize("footprint_every", 0);
    let lines_per_edit = ctx.params.usize("lines_per_edit", 1);
    let content_kind = ctx.params.str("content", "markdown");
    // Markdown documents exercise structure extraction too; other content is plain text.
    let path = if content_kind == "markdown" { "/me/doc.md" } else { "/me/doc.txt" };
    let mut g = Generator::new(ctx.seed);
    let body = make_content(ctx, &mut g, size);
    let mut reference = Reference::default();
    let mut rng = rand::rngs::StdRng::seed_from_u64(ctx.seed ^ 0xabc);
    if let Err(e) = ctx.backend.create(path, &body) {
        ctx.err("", "create", &e);
        return Ok(());
    }
    reference.create(path, &body);
    ctx.backend.reset_counters()?;
    let fp0 = ctx.backend.storage_bytes().unwrap_or(0);

    let is_textdb = ctx.backend.id().starts_with("textdb");
    let mut leaf_prev: Option<HashSet<textdb_core::Hash>> = None;
    let mut leaf_changed: Vec<f64> = Vec::new();
    let mut leaf_unchanged_frac: Vec<f64> = Vec::new();
    // The engine, with any `@account` suffix taken off: a delegated twin is the same engine and
    // must report the same counters, or ME-04 would be blank for it exactly as it once was for
    // `textdb-pg` — a measurement missing rather than failing, which is the harder kind to see.
    let (engine, delegated) = crate::backends::delegate::split(ctx.backend.id());
    let leaf_set = |ctx: &Ctx| -> Option<HashSet<textdb_core::Hash>> {
        if !is_textdb {
            return None;
        }
        if engine == "textdb-sqlite" {
            // Reach the leaf hashes through a fresh embedded handle on the same file. It reads
            // the node table, which speaks store paths whatever the caller holds.
            let store_path = crate::backends::delegate::store_path(delegated, path);
            let db_path = ctx.work.join("textdb.db");
            let conn = rusqlite::Connection::open(db_path).ok()?;
            let db = textdb_sqlite::TextDb::attach(&conn, "kb_", true);
            let n = db.node_by_path(&store_path).ok()??;
            let st = textdb_sqlite::SqliteStorage::new(&conn, "kb_");
            return Some(textdb_core::leaves(&st, &n.root?).ok()?.into_iter().map(|l| l.hash).collect());
        }
        // Postgres answers through `kb.leaf_hashes`, a hook on the extension. Without this
        // `leaves_changed` was collected for `textdb-sqlite` only, so ME-04 reported nothing
        // for `textdb-pg` and claim 1 could never pass for it whatever it did. It resolves the
        // path through the view, so the account's own path is what it wants.
        if engine == "textdb-pg" {
            return ctx.backend.leaf_hashes(path).ok().flatten();
        }
        None
    };
    if is_textdb {
        leaf_prev = leaf_set(ctx);
    }

    let mut lat = Latencies::default();
    let mut lat_trend: Vec<(usize, f64)> = Vec::new();
    let mut counter = 0u64;
    let zipf = Zipf::new(n_lines(&body).max(1), 1.0);
    let mut bytes_changed = 0u64;
    let mut failures = 0;
    for i in 0..n_edits {
        let cur = reference.get(path).to_vec();
        let nl = n_lines(&cur).max(1);
        // Choose the edit.
        let (old, new): (Vec<u8>, Vec<u8>) = match pattern.as_str() {
            "same_line" => {
                // Counter increment on line 2 (or 0 for tiny docs).
                let idx = if nl > 2 { 2 } else { 0 };
                let (s, e) = line_span(&cur, idx).unwrap();
                let old = cur[s..e].to_vec();
                counter += 1;
                (old, format!("count: {} {}", counter, "#".repeat(i % 7)).into_bytes())
            }
            "append" => (Vec::new(), format!("appended line {}\n", i).into_bytes()),
            "alternate_ends" => {
                let idx = if i % 2 == 0 { 0 } else { nl - 1 };
                let idx = if idx == nl - 1 && line_span(&cur, idx).map_or(true, |(s, e)| s == e) && nl > 1 { nl - 2 } else { idx };
                match line_edit(&cur, idx, 1, &format!("edit{}", i)) {
                    Some(x) => x,
                    None => continue,
                }
            }
            "zipf_lines" => {
                let idx = zipf.sample(&mut rng).min(nl - 1);
                match line_edit(&cur, idx, 1, &format!("edit{}", i)) {
                    Some(x) => x,
                    None => continue,
                }
            }
            "middle_bytes" => {
                // 10 bytes at the middle of the document (LL-02).
                let mid = cur.len() / 2;
                let old = cur[mid..(mid + 10).min(cur.len())].to_vec();
                if crate::reference::find_unique(&cur, &old).is_none() {
                    continue;
                }
                (old, format!("<{:08}>", i).into_bytes())
            }
            "random_bytes" => {
                // Random byte-range edits, snapped to UTF-8 character boundaries so every
                // backend (including TEXT-typed baselines) can express them.
                let mut at = rng.gen_range(0..cur.len().max(1));
                while at > 0 && at < cur.len() && (cur[at] & 0xC0) == 0x80 {
                    at -= 1;
                }
                let mut end = (at + rng.gen_range(8..64)).min(cur.len());
                while end < cur.len() && (cur[end] & 0xC0) == 0x80 {
                    end += 1;
                }
                let old = cur[at..end].to_vec();
                if crate::reference::find_unique(&cur, &old).is_none() {
                    continue;
                }
                (old, format!("[e{}:{}]", i, "z".repeat(rng.gen_range(0..40))).into_bytes())
            }
            _ => {
                // random_line: 1–10 line edit at a random line
                let idx = rng.gen_range(0..nl);
                let k = lines_per_edit.max(rng.gen_range(1..=10.min(lines_per_edit.max(1))));
                match line_edit(&cur, idx, k.min(nl - idx).max(1), &format!("edit{}", i)) {
                    Some(x) => x,
                    None => continue,
                }
            }
        };
        bytes_changed += (old.len().max(new.len())) as u64;
        let r = if pattern == "append" {
            ctx.op(ops::APPEND, &mut lat, || ctx.backend.append(path, &new).map(|v| WriteOutcome::Committed { version: v, direct: true }))
        } else {
            ctx.op(ops::REPLACE, &mut lat, || ctx.backend.replace(path, &old, &new, None))
        };
        match r {
            Ok(WriteOutcome::Committed { .. }) | Ok(WriteOutcome::Absorbed { .. }) => {}
            Ok(other) => {
                failures += 1;
                ctx.cell.fail("", "replace", &format!("edit {} unexpected outcome {:?}", i, other));
                break;
            }
            Err(e) => {
                if ctx.err("", "replace", &e) {
                    failures += 1;
                }
                break;
            }
        }
        if pattern == "append" {
            reference.append(path, &new);
        } else if !reference.replace(path, &old, &new) {
            ctx.cell.fail("", "reference", "old text not unique in reference");
            break;
        }
        // Oracle: every read equals the reference copy (RT-04).
        if check_versions || i % 50 == 0 || i + 1 == n_edits {
            match ctx.backend.read(path) {
                Ok(got) if got == reference.get(path) => {}
                Ok(got) => {
                    ctx.cell.fail("", "identical_after_edit", &format!("edit {}: {} vs {} bytes", i, got.len(), reference.get(path).len()));
                    failures += 1;
                    break;
                }
                Err(e) => {
                    ctx.err("", "read", &e);
                    break;
                }
            }
        }
        if let Some(prev) = &leaf_prev {
            if let Some(now) = leaf_set(ctx) {
                let changed = now.iter().filter(|h| !prev.contains(*h)).count();
                leaf_changed.push(changed as f64);
                leaf_unchanged_frac.push(1.0 - changed as f64 / now.len().max(1) as f64);
                leaf_prev = Some(now);
            }
        }
        if (i + 1) % (n_edits / 10).max(1) == 0 {
            lat_trend.push((i + 1, lat.samples[lat.samples.len() - 1]));
        }
        if footprint_every > 0 && (i + 1) % footprint_every == 0 {
            if let Ok(fp) = ctx.backend.storage_bytes() {
                ctx.cell.metric(&format!("after_{}", i + 1), "footprint_bytes", fp as f64);
            }
        }
    }
    ctx.cell.lat("", if pattern == "append" { "append" } else { "replace" }, &lat);
    if lat.samples.len() >= 20 {
        let n = lat.samples.len();
        let first: f64 = lat.samples[..n / 10].iter().sum::<f64>() / (n / 10) as f64;
        let last: f64 = lat.samples[n - n / 10..].iter().sum::<f64>() / (n / 10) as f64;
        ctx.cell.metric("", "latency_trend_last_over_first", last / first.max(1e-9));
    }
    let _ = lat_trend;
    ctx.cell.metric("", "edits_done", (lat.samples.len() - failures.min(lat.samples.len())) as f64);
    let written = ctx.backend.bytes_written_since_reset().unwrap_or(0);
    ctx.cell.metric("", "bytes_written", written as f64);
    ctx.cell.metric("", "bytes_changed", bytes_changed as f64);
    if bytes_changed > 0 {
        ctx.cell.metric("", "write_amplification", written as f64 / bytes_changed as f64);
    }
    let fp1 = ctx.backend.storage_bytes().unwrap_or(0);
    ctx.cell.metric("", "footprint_bytes", fp1 as f64);
    ctx.cell.metric("", "footprint_growth_bytes", fp1.saturating_sub(fp0) as f64);
    ctx.cell.metric("", "footprint_over_raw", fp1 as f64 / reference.get(path).len().max(1) as f64);
    if !leaf_changed.is_empty() {
        let mut lc = leaf_changed.clone();
        lc.sort_by(|a, b| a.partial_cmp(b).unwrap());
        ctx.cell.metric("", "leaves_changed_p50", lc[lc.len() / 2]);
        ctx.cell.metric("", "leaves_changed_max", *lc.last().unwrap());
        let frac_min = leaf_unchanged_frac.iter().cloned().fold(1.0, f64::min);
        ctx.cell.metric("", "leaves_unchanged_frac_min", frac_min);
        ctx.cell.metric(
            "",
            "leaves_unchanged_frac_mean",
            leaf_unchanged_frac.iter().sum::<f64>() / leaf_unchanged_frac.len() as f64,
        );
    }
    for (k, v) in ctx.backend.extra_stats(path).unwrap_or_default() {
        ctx.cell.metric("", k, v);
    }
    // History and historical reads (ME-02 / RT-05 / XL-06).
    let mut hl = Latencies::default();
    match ctx.op(ops::HISTORY, &mut hl, || ctx.backend.history(path)) {
        Ok(h) => {
            ctx.cell.lat("", "history", &hl);
            ctx.cell.metric("", "versions", h.len() as f64);
            let expected_versions = reference.history[path].len();
            if h.len() != expected_versions {
                ctx.cell.fail("", "version_count", &format!("{} versions, reference has {}", h.len(), expected_versions));
            }
            let picks: Vec<usize> = if check_versions {
                (0..expected_versions).collect()
            } else {
                let n = expected_versions;
                vec![0, n / 2, n - 1].into_iter().filter(|&i| i < n).collect()
            };
            let mut rv = Latencies::default();
            let mut mismatches = 0;
            for &i in &picks {
                let v = h.get(i).copied().unwrap_or(i as u64 + 1);
                match ctx.op(ops::READ_VERSION, &mut rv, || ctx.backend.read_version(path, v)) {
                    Ok(got) => {
                        if got != reference.history[path][i] {
                            mismatches += 1;
                        }
                    }
                    Err(e) => {
                        ctx.err("", "read_version", &e);
                        break;
                    }
                }
            }
            if !rv.samples.is_empty() {
                ctx.cell.lat("", "read_version", &rv);
                if mismatches == 0 {
                    ctx.cell.metric("", "versions_identical", 1.0);
                } else {
                    ctx.cell.fail("", "versions_identical", &format!("{} of {} historical versions differ", mismatches, picks.len()));
                }
            }
        }
        Err(e) => {
            ctx.err("", "history", &e);
        }
    }
    ctx.set_reference(reference);
    Ok(())
}
