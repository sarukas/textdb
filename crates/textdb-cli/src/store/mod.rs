//! The operations the CLI performs, over either backend.

pub mod pg;
pub mod sqlite;

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{parse_store, StoreUrl};

/// An error in the store's own terms: the `TX00n` code the bindings use, a message, and for a
/// conflict the payload with the current text of the contested lines.
#[derive(Debug, Serialize)]
pub struct StoreError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<serde_json::Value>,
}

pub type Result<T> = std::result::Result<T, StoreError>;

impl StoreError {
    fn new(code: &str, message: impl std::fmt::Display) -> Self {
        StoreError {
            code: code.to_string(),
            message: message.to_string(),
            conflict: None,
        }
    }

    pub fn other(message: impl std::fmt::Display) -> Self {
        Self::new("TX000", message)
    }

    pub fn not_found(message: impl std::fmt::Display) -> Self {
        Self::new("TX003", message)
    }

    pub fn invalid(message: impl std::fmt::Display) -> Self {
        Self::new("TX004", message)
    }

    pub fn conflict(message: impl std::fmt::Display) -> Self {
        Self::new("TX001", message)
    }

    /// Process exit status, distinct per code so a script can branch without parsing output.
    pub fn exit_code(&self) -> i32 {
        match self.code.as_str() {
            "TX001" => 3,
            "TX002" => 4,
            "TX003" => 5,
            "TX004" => 6,
            _ => 1,
        }
    }
}

impl From<textdb_core::TextdbError> for StoreError {
    fn from(e: textdb_core::TextdbError) -> Self {
        let conflict = match &e {
            textdb_core::TextdbError::Conflict(c) => serde_json::to_value(c).ok(),
            _ => None,
        };
        StoreError {
            code: e.code().to_string(),
            message: e.to_string(),
            conflict,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::other(e)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Entry {
    pub path: String,
    pub name: String,
    /// `file` or `folder`.
    pub kind: String,
    /// In `ls`, a folder's size, lines, words and versions are totals over every file below it.
    pub nbytes: Option<i64>,
    pub nlines: Option<i64>,
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nwords: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versions: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    /// Folder: files and folders anywhere below it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folders: Option<i64>,
    /// File: who committed to it, most commits first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<Author>,
}

/// One author's commits to a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    /// `None` for commits made without an author.
    pub author: Option<String>,
    pub commits: i64,
    #[serde(default)]
    pub last_ts: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Stat {
    pub path: String,
    pub kind: String,
    pub version: i64,
    pub nbytes: Option<i64>,
    pub nlines: Option<i64>,
    pub updated_at: Option<String>,
    pub updated_by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Commit {
    pub version: i64,
    pub author: Option<String>,
    pub ts: String,
    pub message: Option<String>,
    pub nbytes: Option<i64>,
    pub kind: Option<String>,
    pub base_version: Option<i64>,
}

/// A rename, move or delete as it touched one file or folder.
#[derive(Debug, Serialize)]
pub struct PathEvent {
    pub id: i64,
    pub ts: String,
    /// `rename`, `move` or `delete`.
    pub op: String,
    pub old_path: String,
    /// Where it went; absent for a delete.
    pub new_path: Option<String>,
    /// The folder the operation named, when this file or folder went along with it.
    pub via: Option<String>,
    /// A file's version when it happened.
    pub version: Option<i64>,
    pub author: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub path: String,
    pub line: i64,
    pub snippet: String,
    pub rank: f64,
}

/// Lines `old_from..old_from + old_count` (1-based) became `new_from..new_from + new_count`.
#[derive(Debug, Serialize)]
pub struct Hunk {
    pub old_from: i64,
    pub old_count: i64,
    pub new_from: i64,
    pub new_count: i64,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Serialize)]
pub struct Chunk {
    pub ord: i64,
    pub hash: String,
    pub byte_from: i64,
    pub nbytes: i64,
    pub line_from: i64,
    pub nlines: i64,
}

#[derive(Debug, Serialize)]
pub struct Change {
    pub seq: i64,
    pub ts: String,
    pub op: String,
    pub path: String,
    pub old_path: Option<String>,
    pub node_kind: String,
    pub version: Option<i64>,
    pub base_version: Option<i64>,
    pub commit_kind: Option<String>,
    pub author: Option<String>,
    pub message: Option<String>,
}

/// How a write landed: the version it produced and `direct`, `rebased`, `merged` or `noop`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Written {
    pub version: i64,
    pub kind: String,
}

/// A live file's current version, as `sync` compares it with the sync base.
#[derive(Debug, Clone)]
pub struct FileHead {
    pub path: String,
    pub version: i64,
    pub updated_by: Option<String>,
}

/// The git checkout a directory was in when it was synced.
#[derive(Debug, Clone, Serialize)]
pub struct GitState {
    pub commit: Option<String>,
    pub branch: Option<String>,
    /// `origin`'s URL, without credentials.
    pub remote: Option<String>,
    /// No uncommitted changes below the directory.
    pub clean: bool,
}

/// What a store folder and a directory held when `sync` last reconciled them.
#[derive(Debug, Clone, Serialize)]
pub struct SyncBase {
    pub prefix: String,
    pub dir: String,
    /// The store's last change number after the sync.
    pub seq: i64,
    /// Set by the store when saved.
    pub synced_at: Option<String>,
    pub author: Option<String>,
    /// `None` when the directory is not in a git checkout.
    pub git: Option<GitState>,
    #[serde(skip)]
    pub files: Vec<BaseFile>,
}

/// One file both sides agreed on at the last sync.
#[derive(Debug, Clone)]
pub struct BaseFile {
    /// Relative to the folder and the directory, `/`-separated.
    pub rel: String,
    /// The store's version of that content; `None` when unknown (a write was rebased).
    pub version: Option<i64>,
    /// Git blob id of the content.
    pub blob: String,
    /// Size and modification time on disk (nanoseconds), when recent enough to trust.
    pub disk_size: Option<i64>,
    pub disk_mtime: Option<i64>,
    /// Conflict markers were written to the file on disk.
    pub conflict: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct ImportStats {
    pub files: usize,
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub bytes: u64,
}

/// A textdb store. Paths are `/folder/file.md`; versions are per file and consecutive.
pub trait Store {
    fn backend(&self) -> &'static str;
    /// Make the store usable: create what is missing, upgrade what is old.
    fn init(&mut self) -> Result<()>;
    /// Every folder and file under `prefix` (not `prefix` itself unless it is a file).
    fn nodes(&mut self, prefix: &str) -> Result<Vec<Entry>>;
    /// The folder's entries by name, or with `recursive` everything below it by path. A folder's
    /// size, lines, words and versions are totals over the files below it.
    fn ls(&mut self, path: &str, recursive: bool) -> Result<Vec<Entry>>;
    fn stat(&mut self, path: &str) -> Result<Stat>;
    /// Content at `version` (HEAD when `None`) and the version it is.
    fn read(&mut self, path: &str, version: Option<i64>) -> Result<(Vec<u8>, i64)>;
    fn section(&mut self, path: &str, heading: &str) -> Result<Option<Vec<u8>>>;
    fn search(&mut self, query: &str, prefix: &str, limit: i64) -> Result<Vec<Hit>>;
    fn write(
        &mut self,
        path: &str,
        content: &[u8],
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<Written>;
    fn edit(&mut self, path: &str, old: &[u8], new: &[u8], author: Option<&str>) -> Result<Written>;
    fn append(&mut self, path: &str, tail: &[u8], author: Option<&str>) -> Result<Written>;
    fn replace_lines(
        &mut self,
        path: &str,
        from: i64,
        to: i64,
        text: &[u8],
        base_version: Option<i64>,
        author: Option<&str>,
    ) -> Result<Written>;
    fn history(&mut self, path: &str) -> Result<Vec<Commit>>;
    fn diff(&mut self, path: &str, v1: i64, v2: i64) -> Result<String>;
    fn hunks(&mut self, path: &str, v1: i64, v2: i64) -> Result<Vec<Hunk>>;
    fn chunks(&mut self, path: &str, version: Option<i64>) -> Result<Vec<Chunk>>;
    fn mv(&mut self, from: &str, to: &str, author: Option<&str>) -> Result<()>;
    fn rm(&mut self, path: &str, author: Option<&str>) -> Result<()>;
    /// Renames, moves and deletes of the file or folder at `path`, oldest first.
    fn path_history(&mut self, path: &str) -> Result<Vec<PathEvent>>;
    /// Record renames, moves and deletes on this connection (`Some(true)`), don't
    /// (`Some(false)`), or follow the store's `path_history` setting (`None`).
    fn set_session_path_history(&mut self, on: Option<bool>) -> Result<()>;
    /// Whether this connection records renames, moves and deletes.
    fn path_history_enabled(&mut self) -> Result<bool>;
    /// A store setting's value; `None` at its default.
    fn setting(&mut self, key: &str) -> Result<Option<String>>;
    /// Set a store setting, or return it to its default with `None`; answers the stored value.
    fn set_setting(&mut self, key: &str, value: Option<&str>) -> Result<Option<String>>;
    fn last_seq(&mut self) -> Result<i64>;
    fn feed(&mut self, since: i64, limit: i64) -> Result<Vec<Change>>;
    /// Return once another writer may have committed, or after `timeout`; waking early for
    /// nothing is allowed. The first call only starts listening, so a caller primes it before
    /// reading the feed and cannot miss a commit that lands in between.
    fn wait(&mut self, timeout: Duration) -> Result<()>;
    /// Create or update many files, `batch` to a transaction. A file that fails is reported
    /// to `on_error` and does not stop the rest.
    fn import(
        &mut self,
        files: &mut dyn Iterator<Item = (String, Vec<u8>)>,
        author: Option<&str>,
        batch: usize,
        progress: &mut dyn FnMut(&ImportStats),
        on_error: &mut dyn FnMut(&str, &StoreError),
    ) -> Result<ImportStats>;
    /// Every live file under `prefix` (which need not exist) with its current version.
    fn file_heads(&mut self, prefix: &str) -> Result<Vec<FileHead>>;
    /// The directories `prefix` has been synced with, newest first, without their files.
    fn sync_bases(&mut self, prefix: &str) -> Result<Vec<SyncBase>>;
    /// The base of `prefix` synced with `dir`, with its files.
    fn sync_base(&mut self, prefix: &str, dir: &str) -> Result<Option<SyncBase>>;
    /// Replace the base of `base.prefix` and `base.dir`, files included, in one transaction.
    fn save_sync_base(&mut self, base: &SyncBase) -> Result<()>;
}

pub fn open(store: &str) -> Result<Box<dyn Store>> {
    Ok(match parse_store(store) {
        StoreUrl::Sqlite(path) => Box::new(sqlite::SqliteStore::open(&path)?),
        StoreUrl::Postgres(url) => Box::new(pg::PgStore::connect(&url)?),
    })
}
