//! Test-case loading (TOML data, not code) and dispatch to the suite implementations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::backend::{Backend, BackendError, Mode, R};
use crate::metrics::{Cell, Latencies, Sink};
use crate::reference::Reference;

#[derive(Clone, Debug, serde::Deserialize)]
pub struct TestDef {
    pub id: String,
    pub family: String,
    pub kind: String,
    #[serde(default = "one")]
    pub reps: u32,
    #[serde(default)]
    pub note: String,
    /// Parameters at the scale the specification asks for.
    #[serde(default)]
    pub spec: toml::Table,
    /// Parameters at proof-of-concept scale (falls back to `spec`).
    #[serde(default)]
    pub poc: toml::Table,
    /// Backends this test does not apply to (recorded as N/A with `note`).
    #[serde(default)]
    pub na: Vec<String>,
}

fn one() -> u32 {
    1
}

#[derive(Clone, Debug, serde::Deserialize)]
struct File {
    test: Vec<TestDef>,
}

pub fn load_tests(dir: &Path) -> anyhow::Result<Vec<TestDef>> {
    let mut out = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "toml"))
        .collect();
    files.sort();
    for f in files {
        let text = std::fs::read_to_string(&f)?;
        let parsed: File = toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {}", f.display(), e))?;
        out.extend(parsed.test);
    }
    Ok(out)
}

/// Parameter access with profile fallback.
pub struct Params<'a> {
    pub primary: &'a toml::Table,
    pub fallback: &'a toml::Table,
}

impl Params<'_> {
    fn get(&self, k: &str) -> Option<&toml::Value> {
        self.primary.get(k).or_else(|| self.fallback.get(k))
    }
    pub fn u64(&self, k: &str, default: u64) -> u64 {
        self.get(k).and_then(|v| v.as_integer()).map(|v| v as u64).unwrap_or(default)
    }
    pub fn usize(&self, k: &str, default: usize) -> usize {
        self.u64(k, default as u64) as usize
    }
    pub fn f64(&self, k: &str, default: f64) -> f64 {
        self.get(k)
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
            .unwrap_or(default)
    }
    pub fn str(&self, k: &str, default: &str) -> String {
        self.get(k).and_then(|v| v.as_str()).unwrap_or(default).to_string()
    }
    pub fn bool(&self, k: &str, default: bool) -> bool {
        self.get(k).and_then(|v| v.as_bool()).unwrap_or(default)
    }
    pub fn list_u64(&self, k: &str, default: &[u64]) -> Vec<u64> {
        self.get(k)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_integer()).map(|x| x as u64).collect())
            .unwrap_or_else(|| default.to_vec())
    }
    pub fn list_f64(&self, k: &str, default: &[f64]) -> Vec<f64> {
        self.get(k)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)))
                    .collect()
            })
            .unwrap_or_else(|| default.to_vec())
    }
    pub fn list_str(&self, k: &str, default: &[&str]) -> Vec<String> {
        self.get(k)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_else(|| default.iter().map(|s| s.to_string()).collect())
    }
}

/// Everything a suite needs for one cell.
pub struct Ctx<'a> {
    pub cell: Cell<'a>,
    pub backend: &'a dyn Backend,
    pub params: Params<'a>,
    pub seed: u64,
    pub work: PathBuf,
    pub verbose: bool,
    /// Run-wide latency per operation name, filled in by `op`.
    ops: Mutex<BTreeMap<&'static str, Latencies>>,
    /// Oracle the suite built, compared against the backend after the suite finishes.
    reference: Mutex<Option<Reference>>,
}

impl<'a> Ctx<'a> {
    pub fn new(cell: Cell<'a>, backend: &'a dyn Backend, params: Params<'a>, seed: u64, work: PathBuf, verbose: bool) -> Self {
        Ctx {
            cell,
            backend,
            params,
            seed,
            work,
            verbose,
            ops: Mutex::new(BTreeMap::new()),
            reference: Mutex::new(None),
        }
    }

    /// Time one call and attribute it to a named operation from [`crate::ops`].
    ///
    /// The latency lands both in the suite's per-case bucket and in the run-wide bucket
    /// for that operation. The lock is taken *after* the elapsed time is read, so the
    /// bookkeeping never appears in the measurement.
    pub fn op<T>(&self, op: &'static str, lat: &mut Latencies, f: impl FnOnce() -> R<T>) -> R<T> {
        let t = Instant::now();
        let r = f();
        let d = t.elapsed();
        lat.push(d);
        self.ops.lock().unwrap().entry(op).or_default().push(d);
        r
    }

    /// `op` for a call whose latency the suite does not bucket per case.
    pub fn op_only<T>(&self, op: &'static str, f: impl FnOnce() -> R<T>) -> R<T> {
        let mut sink = Latencies::default();
        self.op(op, &mut sink, f)
    }

    /// Merge an already-collected batch of samples into an operation's bucket.
    ///
    /// The concurrency suites time inside their worker threads and merge once at the end.
    /// Calling `op` per operation there would put a process-wide lock between every write
    /// of every writer, serialising the threads and changing the contention the test
    /// exists to measure.
    pub fn record_op(&self, op: &'static str, l: &Latencies) {
        if !l.samples.is_empty() {
            self.ops.lock().unwrap().entry(op).or_default().extend(l);
        }
    }

    /// Hand the suite's oracle to the post-run accuracy check. Suites that model what the
    /// backend should contain call this; the rest get the structural checks only.
    pub fn set_reference(&self, r: Reference) {
        *self.reference.lock().unwrap() = Some(r);
    }

    /// Emit `op_<name>_<stat>` rows for every operation the cell timed.
    pub fn flush_ops(&self) {
        let ops = self.ops.lock().unwrap();
        for name in crate::ops::ALL {
            if let Some(l) = ops.get(name) {
                if !l.samples.is_empty() {
                    self.cell.lat("ops", &format!("op_{}", name), l);
                }
            }
        }
    }

    pub fn take_reference(&self) -> Option<Reference> {
        self.reference.lock().unwrap().take()
    }

    /// Time one call; on N/A record it and return None.
    pub fn timed<T>(&self, lat: &mut Latencies, f: impl FnOnce() -> R<T>) -> R<T> {
        let t = Instant::now();
        let r = f();
        lat.push(t.elapsed());
        r
    }

    /// Record a backend error as N/A or FAIL for a case.
    pub fn err(&self, case: &str, what: &str, e: &BackendError) -> bool {
        match e {
            BackendError::NotSupported(reason) => {
                self.cell.na(case, &format!("{}: {}", what, reason));
                false
            }
            other => {
                self.cell.fail(case, what, &other.to_string());
                true
            }
        }
    }

    pub fn log(&self, msg: &str) {
        if self.verbose {
            eprintln!("      {}", msg);
        }
    }
}

pub struct RunOpts {
    pub profile: String,
    pub mode: Mode,
    pub seed: u64,
    pub work: PathBuf,
    pub filter: Vec<String>,
    pub pg_url: Option<String>,
    pub verbose: bool,
    pub drop_caches: bool,
}

/// Try to drop the page cache (fairness rule 3). Returns whether it worked.
#[cfg(not(unix))]
pub fn drop_page_cache() -> bool {
    false
}

#[cfg(unix)]
pub fn drop_page_cache() -> bool {
    unsafe {
        libc::sync();
    }
    std::fs::write("/proc/sys/vm/drop_caches", b"3\n").is_ok()
}

pub fn run_all(tests: &[TestDef], backends: &[String], sink: &Sink, o: &RunOpts) -> anyhow::Result<()> {
    let mut drop_ok: Option<bool> = None;
    for t in tests {
        if !o.filter.is_empty() && !o.filter.iter().any(|f| t.id.starts_with(f) || t.family == *f) {
            continue;
        }
        eprintln!("== {} ({}) {}", t.id, t.kind, t.note);
        for b in backends {
            let t0 = Instant::now();
            for rep in 1..=t.reps {
                let cache = if rep == 1 && o.drop_caches {
                    let ok = *drop_ok.get_or_insert_with(drop_page_cache);
                    if ok {
                        "cold"
                    } else {
                        "warm"
                    }
                } else {
                    "warm"
                };
                let cell = Cell::new(
                    sink,
                    t.id.clone(),
                    t.family.clone(),
                    b.clone(),
                    o.mode.name().to_string(),
                    cache.to_string(),
                    rep,
                );
                if t.na.iter().any(|x| x == b) {
                    cell.na("", &t.note);
                    continue;
                }
                let backend = match crate::backends::make(b, &o.work, o.mode, o.pg_url.as_deref()) {
                    Ok(Some(x)) => x,
                    Ok(None) => {
                        cell.na("", "backend unavailable (no Postgres URL)");
                        continue;
                    }
                    Err(e) => {
                        cell.fail("", "setup", &e.to_string());
                        continue;
                    }
                };
                let _ = backend.warm();
                let (primary, fallback) = if o.profile == "spec" { (&t.spec, &t.spec) } else { (&t.poc, &t.spec) };
                let ctx = Ctx::new(
                    cell,
                    backend.as_ref(),
                    Params { primary, fallback },
                    o.seed.wrapping_add(rep as u64),
                    o.work.join(b),
                    o.verbose,
                );

                // Untimed: prove the store is clean and the backend actually works before
                // anything it does is allowed to count as a measurement.
                let ready = crate::verify::precheck(&ctx);
                let _ = ctx.backend.reset_counters();

                let started = Instant::now();
                if ready {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::suites::dispatch(&t.kind, &ctx)));
                    match r {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => ctx.cell.fail("", "suite", &e.to_string()),
                        Err(_) => ctx.cell.fail("", "suite", "panicked"),
                    }
                }
                ctx.cell.metric("", "wall_s", started.elapsed().as_secs_f64());

                // Untimed: the suite's own claims, checked against the store it left behind.
                if ready {
                    let reference = ctx.take_reference();
                    crate::verify::postcheck(&ctx, reference.as_ref());
                }
                ctx.flush_ops();
                // Publishes the held-back timings, or voids them if any check failed.
                ctx.cell.finish();
            }
            eprintln!("   {:<16} {:>7.1}s", b, t0.elapsed().as_secs_f64());
        }
    }
    Ok(())
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}
