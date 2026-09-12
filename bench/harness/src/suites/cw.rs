//! CW — concurrent writes (§7.6) and ME-06. One OS thread per simulated agent; every
//! agent runs the same code, only the backend differs.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::{Rng, SeedableRng};

use crate::backend::{Backend, WriteOutcome};
use crate::gen::{GenOpts, Generator};
use crate::metrics::{Latencies, Outcomes};
use crate::reference::{find_unique, splice};
use crate::runner::Ctx;
use crate::suites::{line_span, n_lines, Zipf};

#[derive(Default)]
struct WriterResult {
    lat: Latencies,
    out: Outcomes,
    absorbed: u64,
    /// (path, line index, marker) of the last committed write per (path, line) by this writer.
    last_markers: Vec<(String, usize, String)>,
    committed: u64,
}

pub fn concurrent_writes(ctx: &Ctx) -> anyhow::Result<()> {
    let writers_list = ctx.params.list_u64("writers", &[5, 20]);
    let pattern = ctx.params.str("pattern", "disjoint_sections");
    let ops = ctx.params.usize("ops_per_writer", 50);
    let duration_s = ctx.params.f64("duration_s", 0.0);
    let size = ctx.params.usize("file_size", 102_400);
    let n_files = ctx.params.usize("n_files", 1);
    let think_ms = ctx.params.u64("think_ms", 0);
    let base_mode = ctx.params.str("base", "last_seen");

    for &n in &writers_list {
        let n = n as usize;
        let case = format!("N={}", n);
        // Fresh document(s) for each N.
        let mut g = Generator::new(ctx.seed.wrapping_add(n as u64));
        let paths: Vec<String> = (0..n_files).map(|i| format!("/cw/{}/n{}/f{:05}.md", pattern, n, i)).collect();
        let mut bodies = Vec::new();
        let mut counter_line = 0usize;
        for p in &paths {
            let mut body = g.markdown(size, &GenOpts::default());
            // Make lines unique so single-line replacements are unambiguous, and put the
            // counter / section markers in place.
            body = uniquify_lines(&body);
            if pattern == "same_line" {
                counter_line = 2.min(n_lines(&body) - 1);
                let (s, e) = line_span(&body, counter_line).unwrap();
                body.splice(s..e, b"count: 0".iter().copied());
            }
            if let Err(e) = ctx.backend.create(p, &body) {
                ctx.err(&case, "create", &e);
                return Ok(());
            }
            bodies.push(body);
        }
        if pattern == "rename_race" {
            // 20 writers under /a, one renamer moving /a → /b → /a every second.
        }
        ctx.backend.reset_counters()?;
        let backend: &dyn Backend = ctx.backend;
        let stop = AtomicBool::new(false);
        let renames = AtomicU64::new(0);
        let rename_failures = AtomicU64::new(0);
        let start = Instant::now();
        let results: Mutex<Vec<WriterResult>> = Mutex::new(Vec::new());
        let nl0 = n_lines(&bodies[0]);
        std::thread::scope(|s| {
            for w in 0..n {
                let paths = &paths;
                let results = &results;
                let stop = &stop;
                let pattern = pattern.clone();
                let base_mode = base_mode.clone();
                let seed = ctx.seed ^ ((w as u64 + 1) * 0x9e37);
                s.spawn(move || {
                    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
                    let mut res = WriterResult::default();
                    let zipf = Zipf::new(paths.len(), 1.0);
                    let mut i = 0usize;
                    let mut last_seen: std::collections::HashMap<String, (Vec<u8>, u64)> = Default::default();
                    let deadline = if duration_s > 0.0 { Some(start + Duration::from_secs_f64(duration_s)) } else { None };
                    loop {
                        if let Some(d) = deadline {
                            if Instant::now() >= d || stop.load(Ordering::Relaxed) {
                                break;
                            }
                        } else if i >= ops {
                            break;
                        }
                        let path = if paths.len() == 1 { &paths[0] } else { &paths[zipf.sample(&mut rng)] };
                        // Current view: last seen (stale after others' commits) or fresh read.
                        let (seen, ver) = match last_seen.get(path) {
                            Some(x) if base_mode == "last_seen" => x.clone(),
                            _ => match backend.read_versioned(path) {
                                Ok(x) => {
                                    last_seen.insert(path.clone(), x.clone());
                                    x
                                }
                                Err(_) => {
                                    res.out.error += 1;
                                    i += 1;
                                    continue;
                                }
                            },
                        };
                        let nl = n_lines(&seen).max(1);
                        let marker = format!("w{:03} n{:06} {:010}", w, i, rng.gen::<u32>());
                        // Candidate line for this op; blank or non-unique lines are skipped
                        // (a few tries) so the oracle measures concurrency, not string ambiguity.
                        let pick = |seen: &[u8], first: usize, span: usize| -> Option<(usize, Vec<u8>)> {
                            for t in 0..span.min(8) {
                                let idx = first + (t % span.max(1));
                                if idx >= nl {
                                    break;
                                }
                                let (s, e) = line_span(seen, idx)?;
                                let old = &seen[s..e];
                                if !old.is_empty() && find_unique(seen, old).is_some() {
                                    return Some((idx, old.to_vec()));
                                }
                            }
                            None
                        };
                        let picked: Option<(usize, Vec<u8>, Vec<u8>)> = match pattern.as_str() {
                            "same_line" => {
                                let (s, e) = line_span(&seen, counter_line).unwrap_or((0, 0));
                                let old = seen[s..e].to_vec();
                                let cur: u64 = std::str::from_utf8(&old)
                                    .ok()
                                    .and_then(|t| t.strip_prefix("count: "))
                                    .and_then(|t| t.split_whitespace().next())
                                    .and_then(|t| t.parse().ok())
                                    .unwrap_or(0);
                                Some((counter_line, old, format!("count: {}", cur + 1).into_bytes()))
                            }
                            "same_section" => {
                                let lo = 10.min(nl - 1);
                                let hi = (lo + 2 * n).min(nl);
                                let idx = lo + (w + i * n) % (hi - lo).max(1);
                                pick(&seen, idx, hi - idx).map(|(idx, old)| (idx, old, marker.clone().into_bytes()))
                            }
                            "append" => Some((usize::MAX, Vec::new(), format!("{}\n", marker).into_bytes())),
                            "zipf_files" | "rename_race" => {
                                let idx = rng.gen_range(0..nl);
                                pick(&seen, idx, nl - idx).map(|(idx, old)| (idx, old, marker.clone().into_bytes()))
                            }
                            _ => {
                                // disjoint_sections: writer w owns line block [w*k, (w+1)*k)
                                let k = (nl0 / n).max(1);
                                let first = (w * k + i % k).min(nl - 1);
                                let span = ((w + 1) * k).min(nl).saturating_sub(first).max(1);
                                pick(&seen, first, span).map(|(idx, old)| (idx, old, marker.clone().into_bytes()))
                            }
                        };
                        let (line_idx, old, new) = match picked {
                            Some(x) => x,
                            None => {
                                i += 1;
                                continue;
                            }
                        };
                        let t = Instant::now();
                        let r = if pattern == "append" {
                            backend.append(path, &new).map(|v| WriteOutcome::Committed { version: v, direct: true })
                        } else {
                            let base = if base_mode == "last_seen" && ver > 0 { Some(ver) } else { None };
                            backend.replace(path, &old, &new, base)
                        };
                        res.lat.push(t.elapsed());
                        match r {
                            Ok(WriteOutcome::Absorbed { .. }) => {
                                // No new version: an identical concurrent change was absorbed.
                                res.absorbed += 1;
                                if let Ok(x) = backend.read_versioned(path) {
                                    last_seen.insert(path.clone(), x);
                                }
                            }
                            Ok(WriteOutcome::Committed { version, direct }) => {
                                {
                                    if direct {
                                        res.out.committed_direct += 1;
                                    } else {
                                        res.out.committed_rebased += 1;
                                    }
                                    res.committed += 1;
                                    if line_idx != usize::MAX {
                                        res.last_markers.retain(|(p, l, _)| !(p == path && *l == line_idx));
                                        res.last_markers.push((path.clone(), line_idx, String::from_utf8_lossy(&new).into_owned()));
                                    } else {
                                        res.last_markers.push((path.clone(), usize::MAX, String::from_utf8_lossy(&new).into_owned()));
                                    }
                                }
                                // Refresh the view after any successful write.
                                if let Ok(x) = backend.read_versioned(path) {
                                    last_seen.insert(path.clone(), x);
                                } else if pattern == "append" {
                                    let mut b = seen.clone();
                                    b.extend_from_slice(&new);
                                    last_seen.insert(path.clone(), (b, version));
                                } else if let Some(b) = splice(&seen, &old, &new) {
                                    last_seen.insert(path.clone(), (b, version));
                                }
                            }
                            Ok(WriteOutcome::Conflict { .. }) => {
                                res.out.conflict += 1;
                                if let Ok(x) = backend.read_versioned(path) {
                                    last_seen.insert(path.clone(), x);
                                }
                            }
                            Ok(WriteOutcome::Contention) => {
                                res.out.contention += 1;
                                if let Ok(x) = backend.read_versioned(path) {
                                    last_seen.insert(path.clone(), x);
                                }
                            }
                            Err(_) => {
                                res.out.error += 1;
                                last_seen.remove(path);
                            }
                        }
                        i += 1;
                        if think_ms > 0 {
                            std::thread::sleep(Duration::from_millis(rng.gen_range(0..=think_ms)));
                        }
                    }
                    results.lock().unwrap().push(res);
                    backend.thread_done();
                });
            }
            if pattern == "rename_race" {
                let renames = &renames;
                let rename_failures = &rename_failures;
                let stop = &stop;
                let dir_a = format!("/cw/{}/n{}", pattern, n);
                s.spawn(move || {
                    let tmp = format!("{}-moved", dir_a);
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(200));
                        if backend.rename(&dir_a, &tmp).is_err() {
                            rename_failures.fetch_add(1, Ordering::Relaxed);
                        }
                        std::thread::sleep(Duration::from_millis(50));
                        if backend.rename(&tmp, &dir_a).is_err() {
                            rename_failures.fetch_add(1, Ordering::Relaxed);
                        }
                        renames.fetch_add(2, Ordering::Relaxed);
                    }
                    backend.thread_done();
                });
                // Writers use fixed paths; stop the renamer when they finish.
                // (scope waits for writer threads; signal by polling results count)
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    if results.lock().unwrap().len() >= n {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
        let elapsed = start.elapsed().as_secs_f64();
        let results = results.into_inner().unwrap();
        let mut lat = Latencies::default();
        let mut out = Outcomes::default();
        let mut absorbed = 0;
        let mut committed = 0u64;
        let mut markers: Vec<(String, usize, String)> = Vec::new();
        for r in &results {
            lat.extend(&r.lat);
            out.add(&r.out);
            absorbed += r.absorbed;
            committed += r.committed;
            markers.extend(r.last_markers.iter().cloned());
        }
        // Oracles.
        let mut lost = 0u64;
        let mut final_bodies = std::collections::HashMap::new();
        for p in &paths {
            if let Ok(b) = backend.read(p) {
                final_bodies.insert(p.clone(), b);
            }
        }
        match pattern.as_str() {
            "same_line" => {
                let b = final_bodies.get(&paths[0]).cloned().unwrap_or_default();
                let (s, e) = line_span(&b, counter_line).unwrap_or((0, 0));
                let final_count: u64 = std::str::from_utf8(&b[s..e])
                    .ok()
                    .and_then(|t| t.strip_prefix("count: "))
                    .and_then(|t| t.split_whitespace().next())
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0);
                ctx.cell.metric(&case, "final_counter", final_count as f64);
                lost = committed.saturating_sub(final_count);
            }
            "append" => {
                // Every committed append must be present exactly once, per writer counts.
                let b = final_bodies.get(&paths[0]).cloned().unwrap_or_default();
                let text = String::from_utf8_lossy(&b);
                for (_, _, m) in &markers {
                    if !text.contains(m.trim_end()) {
                        lost += 1;
                    }
                }
                // all markers (not only last) for append
            }
            _ => {
                // The last committed marker per (path, line) of each writer must be present,
                // unless another writer later committed on the same line (same_section/zipf).
                for (p, l, m) in &markers {
                    let b = match final_bodies.get(p) {
                        Some(b) => b,
                        None => {
                            lost += 1;
                            continue;
                        }
                    };
                    if find_unique(b, m.as_bytes()).is_some() {
                        continue;
                    }
                    if pattern == "disjoint_sections" {
                        lost += 1;
                    } else {
                        // Shared line: accept if the line now holds a marker from any writer.
                        let (s, e) = line_span(b, *l).unwrap_or((0, 0));
                        let line = String::from_utf8_lossy(&b[s..e]);
                        if !line.starts_with('w') || !line.contains(" n") {
                            lost += 1;
                        }
                    }
                }
            }
        }
        out.lost_updates = lost;
        ctx.cell.outcomes(&case, &out);
        ctx.cell.metric(&case, "absorbed_identical", absorbed as f64);
        ctx.cell.lat(&case, "write", &lat);
        ctx.cell.metric(&case, "throughput_ops_s", lat.samples.len() as f64 / elapsed.max(1e-9));
        ctx.cell.metric(&case, "elapsed_s", elapsed);
        if out.total() > 0 {
            ctx.cell.metric(&case, "conflict_rate", out.conflict as f64 / out.total() as f64);
            ctx.cell.metric(&case, "contention_rate", out.contention as f64 / out.total() as f64);
        }
        // Version-count oracle for versioned backends: committed (non-absorbed) == versions created.
        let mut versions_created = 0u64;
        let mut has_versions = true;
        for p in &paths {
            match backend.history(p) {
                Ok(h) => versions_created += h.len().saturating_sub(1) as u64,
                Err(_) => {
                    has_versions = false;
                    break;
                }
            }
        }
        if has_versions {
            ctx.cell.metric(&case, "versions_created", versions_created as f64);
            if versions_created != committed {
                ctx.cell.fail(&case, "version_count_matches_commits", &format!("{} versions for {} commits", versions_created, committed));
            } else {
                ctx.cell.metric(&case, "version_count_matches_commits", 1.0);
            }
        }
        if pattern == "rename_race" {
            ctx.cell.metric(&case, "renames", renames.load(Ordering::Relaxed) as f64);
            ctx.cell.metric(&case, "rename_failures", rename_failures.load(Ordering::Relaxed) as f64);
        }
        if lost > 0 {
            ctx.cell.fail(&case, "no_lost_updates", &format!("{} lost updates", lost));
        } else {
            ctx.cell.metric(&case, "no_lost_updates", 1.0);
        }
        for (k, v) in backend.extra_stats(&paths[0]).unwrap_or_default() {
            ctx.cell.metric(&case, k, v);
        }
    }
    Ok(())
}

/// Append a unique suffix to every line so single-line replacements are unambiguous.
pub fn uniquify_lines(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + body.len() / 20);
    let mut i = 0;
    for (n, line) in body.split(|&b| b == b'\n').enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(line);
        if !line.is_empty() {
            out.extend_from_slice(format!(" ·{:06}", n).as_bytes());
        }
        i += 1;
    }
    out
}
