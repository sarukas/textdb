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
}

impl Cell<'_> {
    pub fn metric(&self, case: &str, metric: &str, value: f64) {
        self.push(case, metric, Some(value), "ok");
    }
    pub fn note(&self, case: &str, metric: &str, note: &str) {
        self.push(case, metric, None, note);
    }
    pub fn fail(&self, case: &str, metric: &str, detail: &str) {
        eprintln!("    FAIL {} {} [{}] {}: {}", self.test, self.backend, case, metric, detail);
        self.push(case, metric, Some(1.0), &format!("FAIL: {}", detail));
    }
    pub fn na(&self, case: &str, reason: &str) {
        self.push(case, "na", None, &format!("N/A: {}", reason));
    }
    pub fn lat(&self, case: &str, op: &str, l: &Latencies) {
        for (k, v) in l.summary() {
            self.metric(case, &format!("{}_{}", op, k), v);
        }
    }
    pub fn outcomes(&self, case: &str, o: &Outcomes) {
        for (k, v) in o.rows() {
            self.metric(case, k, v);
        }
    }
    fn push(&self, case: &str, metric: &str, value: Option<f64>, note: &str) {
        self.sink.push(Row {
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
        });
    }
}

/// Group rows for the report: (test, case, metric) → backend → value/note.
pub type Matrix = BTreeMap<(String, String, String), BTreeMap<String, (Option<f64>, String)>>;
