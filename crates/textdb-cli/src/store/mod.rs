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

/// A link that pointed at what a move took elsewhere and no longer reaches it.
#[derive(Debug, Clone, Serialize)]
pub struct MovedLink {
    /// The file the link is written in.
    pub path: String,
    pub line: i64,
    pub kind: String,
    /// The target as it was written.
    pub target: String,
    /// Where the file it pointed at is now.
    pub now_at: String,
    /// The version the link was rewritten in; absent when only reported.
    pub version: Option<i64>,
}

/// A link as `links` and `backlinks` list it.
#[derive(Debug, Clone, Serialize)]
pub struct LinkRow {
    /// The file the link is written in.
    pub path: String,
    pub line: i64,
    /// `wiki`, `embed`, `md` or `image`.
    pub kind: String,
    pub target: String,
    pub anchor: Option<String>,
    pub alias: Option<String>,
    /// `ok`, `ambiguous`, `anchor-missing`, `broken`, `not-in-store` or `external`.
    pub status: Option<String>,
    /// The file it points to; for an asset, the asset (its pointer is that with `.tdbasset`).
    pub resolved: Option<String>,
    /// It resolves to an asset.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub asset: bool,
}

/// A link row as a store holds it, with a link to an asset's pointer shown as the asset.
pub fn asset_link(mut l: LinkRow) -> LinkRow {
    if let Some(r) = l.resolved.clone().filter(|r| crate::assets::pointer::is_asset_pointer(r)) {
        l.resolved = Some(crate::assets::pointer::asset_path(&r).to_string());
        l.asset = true;
    }
    l
}

/// Lines `from..=to` (1-based) as numbered in the base version become `text`; `to = from - 1`
/// inserts before `from`. Several make one commit (`replace-lines --stdin-json`, `meta`).
#[derive(Debug, Clone, Deserialize)]
pub struct LineRange {
    pub from: i64,
    pub to: i64,
    pub text: String,
}

/// `ranges` in line order; refuses an invalid range, and ranges that overlap or start at the
/// same line.
pub fn sorted_ranges(ranges: &[LineRange]) -> Result<Vec<&LineRange>> {
    if ranges.is_empty() {
        return Err(StoreError::invalid("no line ranges given"));
    }
    let mut sorted: Vec<&LineRange> = ranges.iter().collect();
    sorted.sort_by_key(|r| r.from);
    if let Some(r) = sorted.iter().find(|r| r.from < 1 || r.to < r.from - 1) {
        return Err(StoreError::invalid(format!(
            "invalid line range {}..{}: FROM starts at 1, and TO is at least FROM-1 (which inserts)",
            r.from, r.to
        )));
    }
    if let Some(w) = sorted.windows(2).find(|w| w[1].from <= w[0].to || w[1].from == w[0].from) {
        return Err(StoreError::invalid(format!(
            "line ranges {}..{} and {}..{} overlap: give each line once, all numbered as in the same version",
            w[0].from, w[0].to, w[1].from, w[1].to
        )));
    }
    Ok(sorted)
}

/// `content` with `ranges` replaced, as `replace_line_ranges` in the SQLite binding does it.
#[cfg(test)]
pub fn splice_lines(content: &[u8], ranges: &[LineRange]) -> Result<Vec<u8>> {
    let sorted = sorted_ranges(ranges)?;
    let starts: Vec<usize> = std::iter::once(0)
        .chain(content.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i + 1))
        .collect();
    let unterminated = content.last().is_some_and(|b| *b != b'\n');
    let nlines = (starts.len() - 1 + unterminated as usize) as i64;
    let (mut out, mut pos) = (Vec::with_capacity(content.len()), 0);
    for r in sorted {
        if r.from - 1 > nlines || r.to > nlines {
            return Err(StoreError::invalid(format!("lines {}..{} are outside the file, which has {nlines} lines", r.from, r.to)));
        }
        let start = starts.get(r.from as usize - 1).copied();
        if start.is_none() {
            out.extend_from_slice(&content[pos..]);
            out.push(b'\n');
            pos = content.len();
        }
        let start = start.unwrap_or(content.len());
        let end = if r.to < r.from { start } else { starts.get(r.to as usize).copied().unwrap_or(content.len()) };
        out.extend_from_slice(&content[pos..start.max(pos)]);
        out.extend_from_slice(r.text.as_bytes());
        pos = end.max(pos);
    }
    out.extend_from_slice(&content[pos..]);
    Ok(out)
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

/// What one SQL statement returned.
#[derive(Debug, Default)]
pub struct SqlResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    /// With `--write`: changes the store's change log gained (commits, moves, deletes).
    pub store_changes: Option<i64>,
    /// With `--write`: the batch the changes were recorded under, for `revert-batch`.
    pub batch: Option<String>,
    /// With `--dry-run`: the statement ran and was undone; `changes` is what it did.
    pub dry_run: bool,
    pub changes: Vec<BatchChange>,
}

/// One change a statement made: a file's content (`create`, `edit`: versions and a unified
/// diff), a `move`, `delete` or `mkdir`.
#[derive(Debug, Serialize)]
pub struct BatchChange {
    pub op: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// What `revert-batch` did, or with `dry_run` would do.
#[derive(Debug, Serialize)]
pub struct RevertOutcome {
    pub batch: String,
    pub dry_run: bool,
    /// The batch the revert itself was recorded under (so it can be reverted too).
    pub revert_batch: Option<String>,
    pub restored: Vec<RestoredFile>,
    pub removed: Vec<String>,
    pub moved_back: Vec<MovedBack>,
    pub recreated: Vec<String>,
    pub skipped: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RestoredFile {
    pub path: String,
    pub version: i64,
}

#[derive(Debug, Serialize)]
pub struct MovedBack {
    pub from: String,
    pub to: String,
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
    /// The include rules the sync used, as JSON; `None` for a base an older build saved.
    pub rules: Option<String>,
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
/// A property name in use across the store.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PropKey {
    pub key: String,
    pub docs: i64,
    pub values: i64,
    /// `number`, `text` or `mixed`.
    pub kind: String,
}

/// One value a property takes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PropValue {
    pub value: Option<String>,
    pub docs: i64,
}

/// A document a property query matched.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PropHit {
    pub path: String,
    pub nbytes: i64,
    pub updated_at: String,
    /// The document's whole front matter, so a table view needs no query per row.
    pub frontmatter: Option<serde_json::Value>,
}

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
    /// Front-matter property names in use, most-used first; `prefix` narrows them.
    fn property_keys(&mut self, prefix: &str, limit: i64) -> Result<Vec<PropKey>>;
    /// The values one property takes, most-used first.
    fn property_values(&mut self, key: &str, prefix: &str, limit: i64) -> Result<Vec<PropValue>>;
    /// Documents matching a property query, under `folder`.
    fn property_find(&mut self, query: &str, folder: &str, limit: i64) -> Result<Vec<PropHit>>;
    fn write(
        &mut self,
        path: &str,
        content: &[u8],
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<Written>;
    /// `message` replaces the default commit message (`edit`, `append`, `replace-lines`).
    fn edit(&mut self, path: &str, old: &[u8], new: &[u8], author: Option<&str>, message: Option<&str>) -> Result<Written>;
    fn append(&mut self, path: &str, tail: &[u8], author: Option<&str>, message: Option<&str>) -> Result<Written>;
    #[allow(clippy::too_many_arguments)]
    fn replace_lines(
        &mut self,
        path: &str,
        from: i64,
        to: i64,
        text: &[u8],
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<Written>;
    /// Several line ranges, all numbered as in `base_version`, replaced in one commit.
    fn replace_ranges(
        &mut self,
        path: &str,
        ranges: &[LineRange],
        base_version: Option<i64>,
        author: Option<&str>,
        message: Option<&str>,
    ) -> Result<Written>;
    /// The links written in the file at `path` or files below it; only those with one of
    /// `statuses` unless it is empty.
    fn links(&mut self, path: &str, statuses: &[&str]) -> Result<Vec<LinkRow>>;
    /// Links, in any file, that resolve to the file at `path` or a file below it.
    fn backlinks(&mut self, path: &str) -> Result<Vec<LinkRow>>;
    fn history(&mut self, path: &str) -> Result<Vec<Commit>>;
    fn diff(&mut self, path: &str, v1: i64, v2: i64) -> Result<String>;
    fn hunks(&mut self, path: &str, v1: i64, v2: i64) -> Result<Vec<Hunk>>;
    fn chunks(&mut self, path: &str, version: Option<i64>) -> Result<Vec<Chunk>>;
    /// `message` is recorded on the change in the store's change log.
    /// Links are left as they are (a sync's move follows one made on disk).
    fn mv(&mut self, from: &str, to: &str, author: Option<&str>, message: Option<&str>) -> Result<()>;
    /// Move, then rewrite (`Some(true)`), leave (`Some(false)`) or do what the store's
    /// `link_updates` setting says (`None`) with the links that pointed at what moved.
    fn mv_links(&mut self, from: &str, to: &str, author: Option<&str>, message: Option<&str>, update: Option<bool>) -> Result<Vec<MovedLink>>;
    fn rm(&mut self, path: &str, author: Option<&str>, message: Option<&str>) -> Result<()>;
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
    /// Put `files` in the base of `prefix` synced with `dir`, replacing the rows of the same paths
    /// and leaving the others, and when it was synced, as they are; `false` when there is no base.
    fn put_sync_files(&mut self, prefix: &str, dir: &str, files: &[BaseFile]) -> Result<bool>;
    /// Record the base of `prefix` synced with `from` as synced with `to` (the same directory
    /// under another form of its name).
    fn rename_sync_dir(&mut self, prefix: &str, from: &str, to: &str) -> Result<()>;
    /// Run one statement with `params` bound as text, read-only unless `write`. The views
    /// `files`, `folders`, `frontmatter`, `sections`, `links`, `commits` and `authors` are there
    /// to query; in SQLite, `:author` is bound to `author`.
    /// With `dry_run` (and `write`), the statement runs and is rolled back, and the result lists
    /// the changes it made.
    fn sql(&mut self, query: &str, params: &[String], author: Option<&str>, write: bool, dry_run: bool) -> Result<SqlResult>;
    /// Undo the changes recorded under `batch`; with `dry_run`, only report what that would do.
    fn revert_batch(&mut self, batch: &str, author: Option<&str>, skip_changed: bool, dry_run: bool) -> Result<RevertOutcome>;
    /// Every sync base, newest first, without their files.
    fn all_sync_bases(&mut self) -> Result<Vec<SyncBase>>;
    /// The asset stores declared in this store, by name.
    fn asset_stores(&mut self) -> Result<Vec<AssetStore>>;
    /// Declare an asset store, or change the one of that name.
    fn put_asset_store(&mut self, store: &AssetStore) -> Result<()>;
    /// Remove an asset store's declaration; `false` when there was none.
    fn remove_asset_store(&mut self, name: &str) -> Result<bool>;
}

pub use crate::assets::driver::AssetStore;

pub fn open(store: &str) -> Result<Box<dyn Store>> {
    Ok(match parse_store(store) {
        StoreUrl::Sqlite(path) => Box::new(sqlite::SqliteStore::open(&path)?),
        StoreUrl::Postgres(url) => Box::new(pg::PgStore::connect(&url)?),
    })
}
