//! CR — concurrent reads (§7.5), with an optional writer for the torn-read oracle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::{Rng, SeedableRng};

use crate::backend::Backend;
use crate::gen::{GenOpts, Generator};
use crate::metrics::Latencies;
use crate::reference::Reference;
use crate::runner::Ctx;
use crate::suites::{line_edit, n_lines, Zipf};

pub fn concurrent_reads(ctx: &Ctx) -> anyhow::Result<()> {
    let readers_list = ctx.params.list_u64("readers", &[1, 10, 100]);
    let duration_s = ctx.params.f64("duration_s", 5.0);
    let variant = ctx.params.str("variant", "read");
    let size = ctx.params.usize("file_size", 102_400);
    let n_files = ctx.params.usize("n_files", 1);
    let writer_every_ms = ctx.params.u64("writer_every_ms", 0);
    let n_versions = ctx.params.usize("n_versions", 0);
    let frag_lines = ctx.params.u64("read_lines", 50);
    let search_terms = ctx.params.usize("search_terms", 2);

    let mut g = Generator::new(ctx.seed);
    let mut reference = Reference::default();
    let paths: Vec<String> = (0..n_files).map(|i| format!("/cr/{}/f{:05}.md", variant, i)).collect();
    for p in &paths {
        let body = g.markdown(size, &GenOpts::default());
        if let Err(e) = ctx.backend.create(p, &body) {
            ctx.err("", "create", &e);
            return Ok(());
        }
        reference.create(p, &body);
    }
    // Pre-create versions for read_version.
    if n_versions > 1 {
        let p = &paths[0];
        for i in 1..n_versions {
            let cur = reference.get(p).to_vec();
            let idx = (i * 31) % n_lines(&cur).max(1);
            if let Some((old, new)) = line_edit(&cur, idx, 1, &format!("v{}", i)) {
                if ctx.backend.replace(p, &old, &new, None).is_err() {
                    break;
                }
                reference.replace(p, &old, &new);
            }
        }
        if ctx.backend.history(&paths[0]).is_err() {
            ctx.cell.na("", "read_version not available");
            return Ok(());
        }
    }
    let vocab: Vec<String> = g.vocab()[..200].to_vec();
    let reference = Mutex::new(reference);
    let backend: &dyn Backend = ctx.backend;

    for &n in &readers_list {
        let n = n as usize;
        let case = format!("N={}", n);
        let stop = AtomicBool::new(false);
        let start = Instant::now();
        let results: Mutex<Vec<(Latencies, u64, u64)>> = Mutex::new(Vec::new()); // (lat, torn, errors)
        let writes = Mutex::new((Latencies::default(), 0u64));
        std::thread::scope(|s| {
            for r in 0..n {
                let paths = &paths;
                let vocab = &vocab;
                let reference = &reference;
                let results = &results;
                let stop = &stop;
                let variant = variant.clone();
                let seed = ctx.seed ^ ((r as u64 + 7) * 0x51ed);
                s.spawn(move || {
                    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
                    let zipf = Zipf::new(paths.len(), 1.0);
                    let mut lat = Latencies::default();
                    let mut torn = 0u64;
                    let mut errors = 0u64;
                    let deadline = start + Duration::from_secs_f64(duration_s);
                    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                        let path = if paths.len() == 1 { &paths[0] } else { &paths[zipf.sample(&mut rng)] };
                        let t = Instant::now();
                        let res: Result<Option<Vec<u8>>, ()> = match variant.as_str() {
                            "read_lines" => {
                                let total = 1000u64;
                                let from = rng.gen_range(1..=total);
                                backend.read_lines(path, from, from + frag_lines - 1).map(|_| None).map_err(|_| ())
                            }
                            "read_version" => {
                                let v = rng.gen_range(1..=n_versions.max(1)) as u64;
                                backend.read_version(path, v).map(|_| None).map_err(|_| ())
                            }
                            "search" => {
                                let q: Vec<&str> = (0..search_terms).map(|_| vocab[rng.gen_range(0..vocab.len())].as_str()).collect();
                                backend.search(&q.join(" "), "/").map(|_| None).map_err(|_| ())
                            }
                            _ => backend.read(path).map(Some).map_err(|_| ()),
                        };
                        lat.push(t.elapsed());
                        match res {
                            Ok(Some(bytes)) => {
                                if writer_every_ms > 0 {
                                    let r = reference.lock().unwrap();
                                    if !r.is_some_version(path, &bytes) {
                                        torn += 1;
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(()) => errors += 1,
                        }
                    }
                    results.lock().unwrap().push((lat, torn, errors));
                    backend.thread_done();
                });
            }
            if writer_every_ms > 0 {
                let paths = &paths;
                let reference = &reference;
                let writes = &writes;
                let stop = &stop;
                s.spawn(move || {
                    let deadline = start + Duration::from_secs_f64(duration_s);
                    let mut i = 0usize;
                    let mut lat = Latencies::default();
                    let mut n = 0u64;
                    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                        let p = &paths[0];
                        let cur = reference.lock().unwrap().get(p).to_vec();
                        let idx = (i * 17) % n_lines(&cur).max(1);
                        if let Some((old, new)) = line_edit(&cur, idx, 3.min(n_lines(&cur) - idx).max(1), &format!("w{}", i)) {
                            // Record the candidate version before the write becomes visible,
                            // so a reader can never see a "future" version unknown to the oracle.
                            reference.lock().unwrap().replace(p, &old, &new);
                            let t = Instant::now();
                            if backend.replace(p, &old, &new, None).is_ok() {
                                n += 1;
                            }
                            lat.push(t.elapsed());
                        }
                        i += 1;
                        std::thread::sleep(Duration::from_millis(writer_every_ms));
                    }
                    *writes.lock().unwrap() = (lat, n);
                    backend.thread_done();
                });
            }
        });
        let elapsed = start.elapsed().as_secs_f64();
        let mut lat = Latencies::default();
        let mut torn = 0;
        let mut errors = 0;
        for (l, t, e) in results.into_inner().unwrap() {
            lat.extend(&l);
            torn += t;
            errors += e;
        }
        ctx.cell.lat(&case, &variant, &lat);
        ctx.cell.metric(&case, "throughput_ops_s", lat.samples.len() as f64 / elapsed.max(1e-9));
        ctx.cell.metric(&case, "errors", errors as f64);
        if writer_every_ms > 0 {
            let (wl, wn) = writes.into_inner().unwrap();
            ctx.cell.lat(&case, "writer_replace", &wl);
            ctx.cell.metric(&case, "writes", wn as f64);
            if torn > 0 {
                ctx.cell.fail(&case, "torn_reads", &format!("{} reads matched no committed version", torn));
            } else {
                ctx.cell.metric(&case, "torn_reads", 0.0);
            }
        }
    }
    Ok(())
}
