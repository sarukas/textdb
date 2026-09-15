//! Uniform backend interface (test spec §4). The runner sees nothing else.

use std::fmt;

pub type Version = u64;

#[derive(Debug)]
pub enum BackendError {
    /// Operation is not available on this backend (recorded as N/A, never skipped silently).
    NotSupported(&'static str),
    NotFound(String),
    Other(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::NotSupported(s) => write!(f, "N/A: {}", s),
            BackendError::NotFound(s) => write!(f, "not found: {}", s),
            BackendError::Other(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<rusqlite::Error> for BackendError {
    fn from(e: rusqlite::Error) -> Self {
        BackendError::Other(e.to_string())
    }
}
impl From<postgres::Error> for BackendError {
    fn from(e: postgres::Error) -> Self {
        match e.as_db_error() {
            Some(db) => BackendError::Other(format!("{} {}: {}", e, db.code().code(), db.message())),
            None => BackendError::Other(e.to_string()),
        }
    }
}
impl From<std::io::Error> for BackendError {
    fn from(e: std::io::Error) -> Self {
        BackendError::Other(e.to_string())
    }
}
impl From<anyhow::Error> for BackendError {
    fn from(e: anyhow::Error) -> Self {
        BackendError::Other(e.to_string())
    }
}

pub type R<T> = Result<T, BackendError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum Cap {
    Native,
    Emulated,
    NA,
}

/// Which operations are native / emulated / not available.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Caps {
    pub replace: Cap,
    pub read_lines: Cap,
    pub read_version: Cap,
    pub history: Cap,
    pub search: Cap,
    pub rename_folder: Cap,
    pub concurrency_guard: &'static str,
    pub invalid_utf8: Cap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub is_dir: bool,
    pub nbytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub path: String,
    pub line: u64,
}

#[derive(Clone, Debug)]
pub enum WriteOutcome {
    /// `direct` is true when no other writer committed between the caller's base and this write.
    Committed { version: Version, direct: bool },
    /// The write succeeded but produced no new version: an identical change had already
    /// been committed concurrently (textdb merges identical concurrent edits).
    Absorbed { version: Version },
    Conflict { current_region: Vec<u8> },
    Contention,
}

/// Durability mode (fairness rule 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Durable,
    Fast,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Durable => "durable",
            Mode::Fast => "fast",
        }
    }
}

pub trait Backend: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Caps;

    // namespace
    fn create(&self, path: &str, body: &[u8]) -> R<Version>;
    fn delete(&self, path: &str) -> R<()>;
    fn rename(&self, from: &str, to: &str) -> R<()>;
    fn list(&self, prefix: &str) -> R<Vec<Entry>>;

    // read
    fn read(&self, path: &str) -> R<Vec<u8>>;
    /// Current `(body, version)`; backends without versions return version 0.
    fn read_versioned(&self, path: &str) -> R<(Vec<u8>, Version)> {
        Ok((self.read(path)?, 0))
    }
    /// Lines `[from, to]`, 1-based inclusive.
    fn read_lines(&self, path: &str, from: u64, to: u64) -> R<Vec<u8>>;
    fn read_version(&self, path: &str, v: Version) -> R<Vec<u8>>;

    // write
    fn overwrite(&self, path: &str, body: &[u8]) -> R<Version>;
    /// Replace `old` (unique in the document the caller last read) with `new`. `base` is the
    /// version the caller read; `None` means "against the current version".
    fn replace(&self, path: &str, old: &[u8], new: &[u8], base: Option<Version>) -> R<WriteOutcome>;
    fn append(&self, path: &str, tail: &[u8]) -> R<Version>;

    // query
    /// Query syntax: whitespace-separated terms are ANDed at document level; `"a b"` is a
    /// phrase; `foo*` is a prefix. Hits report the first matching line per document.
    fn search(&self, query: &str, prefix: &str) -> R<Vec<Hit>>;
    fn history(&self, path: &str) -> R<Vec<Version>>;

    // measurement hooks
    fn storage_bytes(&self) -> R<u64>;
    fn bytes_written_since_reset(&self) -> R<u64>;
    fn reset_counters(&self) -> R<()>;
    /// Backend-specific maintenance (git gc, VACUUM, optimize). Returns a label.
    fn maintenance(&self) -> R<&'static str> {
        Err(BackendError::NotSupported("no maintenance step"))
    }
    /// Backend-specific structural counters, e.g. textdb leaf/chunk counts.
    fn extra_stats(&self, _path: &str) -> R<Vec<(&'static str, f64)>> {
        Ok(vec![])
    }
    /// Leaf chunk hashes of a document at HEAD, for "how many leaves did this edit change".
    ///
    /// `None` from a backend that has no leaves — the ME-04 counters are simply not reported
    /// for it. A chunked backend that returns `None` silently withholds the measurement claim
    /// 1 is judged on, which is how `textdb-pg` came to look like a failure.
    fn leaf_hashes(&self, _path: &str) -> R<Option<std::collections::HashSet<textdb_core::Hash>>> {
        Ok(None)
    }
    /// Cheap warm-up so first-use costs (thread-local connections) stay out of timings.
    fn warm(&self) -> R<()> {
        Ok(())
    }
    /// Called by every agent thread before it exits, so per-thread connections close cleanly.
    fn thread_done(&self) {}
}

/// Process-level write accounting shared by the in-process backends.
pub mod io_counters {
    /// Bytes passed to write(2) by this process (`/proc/self/io` `wchar`).
    pub fn self_wchar() -> u64 {
        std::fs::read_to_string("/proc/self/io")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("wchar:"))
                    .and_then(|l| l[6..].trim().parse().ok())
            })
            .unwrap_or(0)
    }

    /// Blocks written by waited-for children (git), in bytes.
    #[cfg(unix)]
    pub fn children_write_bytes() -> u64 {
        unsafe {
            let mut ru: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) == 0 {
                (ru.ru_oublock as u64) * 512
            } else {
                0
            }
        }
    }

    /// No child rusage accounting outside Unix; the metric reports 0.
    #[cfg(not(unix))]
    pub fn children_write_bytes() -> u64 {
        0
    }
}
