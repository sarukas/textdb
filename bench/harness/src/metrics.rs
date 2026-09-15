//! Uniform metrics (test spec §8) and the JSONL result sink.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Default, Clone)]
pub struct Latencies {
    pub samples: Vec<f64>, // microseconds
}

impl Latencies {
    pub fn push(&mut self, d: Duration) {
        self.samples.push(d.as_secs_f64() * 1e6);
    }
    pub fn extend(&mut self, other: &Latencies) {
        self.samples.extend_from_slice(&other.samples);
    }
    /// Median, for suites that form a ratio between two sets of samples.
    pub fn p50(&self) -> Option<f64> {
        self.pct(0.5)
    }
    fn pct(&self, p: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut s = self.samples.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = ((s.len() as f64 - 1.0) * p).round() as usize;
        Some(s[idx.min(s.len() - 1)])
    }
    pub fn summary(&self) -> Vec<(&'static str, f64)> {
        let mut v = Vec::new();
        if let Some(p) = self.pct(0.5) {
            v.push(("p50_us", p));
        }
        if let Some(p) = self.pct(0.95) {
            v.push(("p95_us", p));
        }
        if let Some(p) = self.pct(0.99) {
            v.push(("p99_us", p));
        }
        if let Some(p) = self.pct(1.0) {
            v.push(("max_us", p));
        }
        v.push(("n", self.samples.len() as f64));
        v
    }
}

#[derive(Default, Clone, Debug)]
pub struct Outcomes {
    pub committed_direct: u64,
    pub committed_rebased: u64,
    pub conflict: u64,
    pub contention: u64,
    pub error: u64,
    pub lost_updates: u64,
}

impl Outcomes {
    pub fn total(&self) -> u64 {
        self.committed_direct + self.committed_rebased + self.conflict + self.contention + self.error
    }
    pub fn add(&mut self, o: &Outcomes) {
        self.committed_direct += o.committed_direct;
        self.committed_rebased += o.committed_rebased;
        self.conflict += o.conflict;
        self.contention += o.contention;
        self.error += o.error;
        self.lost_updates += o.lost_updates;
    }
    pub fn rows(&self) -> Vec<(&'static str, f64)> {
        vec![
            ("committed_direct", self.committed_direct as f64),
            ("committed_rebased", self.committed_rebased as f64),
            ("conflict", self.conflict as f64),
            ("contention", self.contention as f64),
            ("error", self.error as f64),
            ("lost_updates", self.lost_updates as f64),
        ]
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Row {
    pub test: String,
    pub family: String,
    pub backend: String,
    pub mode: String,
    pub cache: String,
    pub rep: u32,
    /// Sub-case label (e.g. size or N); empty when the test has one cell.
    pub case: String,
    pub metric: String,
    pub value: Option<f64>,
    /// "ok" | "FAIL" | "N/A: reason" | free text
    pub note: String,
}

pub struct Sink {
    path: std::path::PathBuf,
    rows: Mutex<Vec<Row>>,
}

impl Sink {
    pub fn new(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Sink {
            path: dir.join("results.jsonl"),
            rows: Mutex::new(Vec::new()),
        })
    }

    pub fn push(&self, row: Row) {
        let line = serde_json::to_string(&row).unwrap();
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{}", line);
        }
        self.rows.lock().unwrap().push(row);
    }

    pub fn rows(&self) -> Vec<Row> {
        self.rows.lock().unwrap().clone()
    }
}

/// Shared across every clone of a `Cell`, including the ones handed to writer threads.
#[derive(Default)]
struct CellState {
    /// Set by `fail`, and by a failed accuracy check. Once true the cell publishes no timings.
    failed: bool,
    /// Reasons, for the single voiding row the report shows in place of the timings.
    reasons: Vec<String>,
    /// Latency rows held back until the cell finishes, because a failure can still arrive
    /// after them — a backend that errors half way through must not publish a fast p50.
    pending: Vec<Row>,
}

/// Context for one (test, backend, mode, cache, rep) cell.
#[derive(Clone)]
pub struct Cell<'a> {
    pub sink: &'a Sink,
    pub test: String,
    pub family: String,
    pub backend: String,
    pub mode: String,
    pub cache: String,
    pub rep: u32,
    state: std::sync::Arc<Mutex<CellState>>,
}

impl<'a> Cell<'a> {
    pub fn new(sink: &'a Sink, test: String, family: String, backend: String, mode: String, cache: String, rep: u32) -> Self {
        Cell {
            sink,
            test,
            family,
            backend,
            mode,
            cache,
            rep,
            state: std::sync::Arc::new(Mutex::new(CellState::default())),
        }
    }

    pub fn metric(&self, case: &str, metric: &str, value: f64) {
        self.push(case, metric, Some(value), "ok");
    }
    pub fn note(&self, case: &str, metric: &str, note: &str) {
        self.push(case, metric, None, note);
    }
    pub fn fail(&self, case: &str, metric: &str, detail: &str) {
        eprintln!("    FAIL {} {} [{}] {}: {}", self.test, self.backend, case, metric, detail);
        self.mark_failed(&format!("{} {}: {}", case, metric, detail));
        self.push(case, metric, Some(1.0), &format!("FAIL: {}", detail));
    }
    pub fn na(&self, case: &str, reason: &str) {
        self.push(case, "na", None, &format!("N/A: {}", reason));
    }

    /// An oracle violation that is the expected, documented behaviour of this backend, and
    /// therefore the measurement rather than a defect — `fs` has no concurrency control,
    /// so its lost updates are the number the suite exists to report. Recorded and shown,
    /// but it does not void the cell's timings: voiding them would delete the very
    /// baseline the other backends are compared against.
    pub fn expected(&self, case: &str, metric: &str, detail: &str) {
        self.push(case, metric, Some(1.0), &format!("EXPECTED: {}", detail));
    }

    /// Void this cell's timings without emitting a per-case FAIL row (the caller has
    /// already described the problem some other way).
    pub fn mark_failed(&self, reason: &str) {
        let mut s = self.state.lock().unwrap();
        s.failed = true;
        s.reasons.push(reason.to_string());
    }

    pub fn has_failed(&self) -> bool {
        self.state.lock().unwrap().failed
    }

    /// Publish or void the held-back latency rows. Called once, after the suite and the
    /// post-run accuracy checks have both had their say.
    pub fn finish(&self) {
        let (failed, reasons, pending) = {
            let mut s = self.state.lock().unwrap();
            (s.failed, std::mem::take(&mut s.reasons), std::mem::take(&mut s.pending))
        };
        if !failed {
            for row in pending {
                self.sink.push(row);
            }
            return;
        }
        let detail = if reasons.len() > 3 {
            format!("{}; +{} more", reasons[..3].join("; "), reasons.len() - 3)
        } else {
            reasons.join("; ")
        };
        self.push(
            "",
            "timings_voided",
            Some(pending.len() as f64),
            &format!("FAIL: {} latency rows withheld: {}", pending.len(), detail),
        );
    }

    /// Latency rows are buffered, not written: see `CellState::pending`.
    pub fn lat(&self, case: &str, op: &str, l: &Latencies) {
        for (k, v) in l.summary() {
            self.state.lock().unwrap().pending.push(self.row(case, &format!("{}_{}", op, k), Some(v), "ok"));
        }
    }
    pub fn outcomes(&self, case: &str, o: &Outcomes) {
        for (k, v) in o.rows() {
            self.metric(case, k, v);
        }
    }
    fn row(&self, case: &str, metric: &str, value: Option<f64>, note: &str) -> Row {
        Row {
            test: self.test.clone(),
            family: self.family.clone(),
            backend: self.backend.clone(),
            mode: self.mode.clone(),
            cache: self.cache.clone(),
            rep: self.rep,
            case: case.to_string(),
            metric: metric.to_string(),
            value,
            note: note.to_string(),
        }
    }

    fn push(&self, case: &str, metric: &str, value: Option<f64>, note: &str) {
        self.sink.push(self.row(case, metric, value, note));
    }
}

/// Group rows for the report: (test, case, metric) → backend → value/note.
pub type Matrix = BTreeMap<(String, String, String), BTreeMap<String, (Option<f64>, String)>>;
