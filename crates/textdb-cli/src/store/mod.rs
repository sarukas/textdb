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

    /// It is in your view and you may not do this (#12). Distinct from `not_found` on purpose:
    /// `sync` deletes what is absent from the store and leaves alone what it is merely forbidden,
    /// so conflating the two is what would empty a checkout when a share is revoked.
    pub fn forbidden(message: impl std::fmt::Display) -> Self {
        Self::new("TX005", message)
    }

    /// Someone else holds what this needs right now, and retrying shortly is the answer.
    /// Exit 4, which a hook can branch on without parsing the message.
    pub fn contention(message: impl std::fmt::Display) -> Self {
        Self::new("TX002", message)
    }

    /// Process exit status, distinct per code so a script can branch without parsing output.
    pub fn exit_code(&self) -> i32 {
        match self.code.as_str() {
            "TX001" => 3,
            "TX002" => 4,
            "TX003" => 5,
            "TX004" => 6,
            "TX005" => 7,
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

/// One listing row, the same twenty-four keys on every surface and in this order.
///
/// Every key is always present: a value that does not apply is `null`, never omitted. The
/// old shape skipped `nwords`, `versions`, `authors` and others when absent, which left a
/// consumer unable to tell "not applicable" from "this build does not have it".
#[derive(Debug, Clone, Default, Serialize)]
pub struct Entry {
    // The minimal tier: what every surface carries, whatever the command or format.
    pub path: String,
    pub name: String,
    /// `file` or `folder`.
    pub kind: String,
    /// A file's current version — what `cat -n` shows and `--base-version` takes. `null` for
    /// a folder, which has no version of its own.
    pub version: Option<i64>,
    /// A file's own size; a folder's total over the live files below it.
    pub nbytes: i64,
    pub nlines: i64,
    pub updated_at: String,
    pub updated_by: Option<String>,

    // The rest of the full tier.
    pub id: i64,
    /// The parent folder; `null` for the root.
    pub dir: Option<String>,
    pub depth: i64,
    /// Lower case, no dot; `null` for a folder or a name without one.
    pub ext: Option<String>,
    /// Front matter `title`, else the first level-1 heading, else `null`.
    pub title: Option<String>,
    pub nwords: i64,
    pub nsections: i64,
    pub nprops: i64,
    pub nlinks: i64,
    pub nlinks_broken: i64,
    pub versions: i64,
    pub created_at: String,
    /// Folder: live files and folders anywhere below it; `null` for a file.
    pub files: Option<i64>,
    pub folders: Option<i64>,
    pub nauthors: i64,
    /// Who committed to it, most commits first. Empty for a folder.
    pub authors: Vec<Author>,

    // The access tier (#12): present only in an account's view, absent for the owner, whose
    // paths are the store's own and who holds no shares.
    /// The alias of the share this row was reached through; `""` for a single-root account,
    /// whose root is the share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<String>,
    /// `ro` or `rw`, the rights of that share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rights: Option<String>,
    /// Only on an account's root row (`kind: "root"`): its shares, in path order. The root is
    /// not a node, so this is the only place a caller can read what it is made of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shares: Option<Vec<Share>>,
}

/// One share as a root row lists it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Share {
    pub alias: String,
    pub rights: String,
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
/// One version of a file, in the order the `commits` view and `textdb_history` use — one
/// order on both engines and in every SDK. `SELECT *` consumed positionally used to swap
/// `nbytes` and `kind` between them.
pub struct Commit {
    pub version: i64,
    pub author: Option<String>,
    pub ts: String,
    pub message: Option<String>,
    /// How the commit landed: `direct`, `rebased` or `merged`.
    pub kind: Option<String>,
    /// The version the writer started from; `None` for a file's first version.
    pub base_version: Option<i64>,
    pub nbytes: Option<i64>,
    /// Lines and words as of this version. `nwords` is absent on commits written before the
    /// column existed.
    pub nlines: Option<i64>,
    pub nwords: Option<i64>,
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

/// One matching line, the same seven keys from `search`, `grep` and the SQL functions.
///
/// One row per matching *line* on every surface. `search` used to return one row per document
/// with a best-guess line on three of the four surfaces that carried the same column name.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub path: String,
    /// The version the line number belongs to — what `--base-version` takes. Without it a
    /// caller that searched and then edited by line had nothing to pass.
    pub version: i64,
    pub line: i64,
    /// The whole matching line, cut at one length everywhere. Replaces `snippet` on `search`
    /// and `text` on `grep`, which were the same thing under two names and two cuts.
    pub text: String,
    /// The heading path the line sits under (`API Guide / Errors`), so a caller can jump
    /// with `cat --section`. `None` outside any heading, or in a file with none.
    pub section: Option<String>,
    /// Relevance, higher is better, `None` for `grep`. Replaces `rank`, whose sign was raw
    /// engine output and flipped between SQLite and Postgres.
    pub score: Option<f64>,
    /// Matching lines in this file not listed because of `--per-file`; 0 otherwise. Makes
    /// truncation visible to a JSON consumer, which only ever saw it on stderr.
    pub more: i64,
}

/// One document a search matched, for the paths-only and count modes.
#[derive(Debug, Clone, Serialize)]
pub struct FileHit {
    pub path: String,
    pub version: i64,
    pub matches: i64,
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
    /// The linking file is outside the caller's shares, so it was left alone and `path` is empty.
    /// Counted, never named: the path would be the layout an alias exists to hide (#12 D14).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
}

/// A link as `links` and `backlinks` list it.
#[derive(Debug, Clone, Serialize)]
pub struct LinkRow {
    /// The file the link is written in.
    pub path: String,
    /// The version the line number belongs to. Every row that carries a `line` carries one:
    /// the whole contract is that a line number belongs to a version, and an agent that reads
    /// a link and then edits by line had nothing to pass as `--base-version`.
    pub version: i64,
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
    pub asset: bool,
    /// The store's id for the document it reached. Not printed: it is what turns a target the
    /// reader may not see into its `textdb:<id>` form, and the id is already the `resolved` row's
    /// own elsewhere.
    #[serde(skip)]
    pub resolved_id: Option<i64>,
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
    /// What this base was at when it was read. Saving checks it and bumps it, so a sync that
    /// started from a base another one has since replaced aborts instead of overwriting it.
    /// The directory lock is local; this is what catches a second machine sharing the folder.
    #[serde(skip)]
    pub generation: i64,
    /// The directory's own name for itself (`.textdb/config`), so a directory that moves is
    /// recognised by it rather than by a path that has changed.
    #[serde(skip)]
    pub dir_id: Option<String>,
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

/// One heading, with its document's own figures alongside.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OutlineRow {
    pub path: String,
    /// The last component, as written.
    pub heading: String,
    /// The breadcrumb, `Parent / Child`.
    pub heading_path: String,
    pub level: i64,
    pub line_from: i64,
    pub line_to: i64,
    /// Words in the section's own lines, and in it plus everything nested under it.
    pub nwords: Option<i64>,
    pub nwords_total: Option<i64>,
    pub nbytes: Option<i64>,
    pub nlines: Option<i64>,
    pub file_nwords: Option<i64>,
    pub version: i64,
    pub updated_at: String,
    pub updated_by: Option<String>,
}

/// A distinct heading in use, for autosuggest.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HeadingName {
    pub heading: String,
    pub sections: i64,
    pub docs: i64,
}


// ---------------------------------------------------------------- accounts, tokens and shares

/// One share as a caller sees it listed: the account's own name for it, the rights, and — only
/// for the admin — where it actually is in the store.
#[derive(Debug, Serialize)]
pub struct ShareRow {
    pub account: String,
    pub alias: String,
    pub rights: String,
    /// The share root's store path. `None` in an account's own `whoami`, where naming it would
    /// disclose the layout the alias exists to hide. Serialised as `path`, the name every other
    /// row in this CLI gives to "where this is".
    #[serde(rename = "path", skip_serializing_if = "Option::is_none")]
    pub store_path: Option<String>,
    pub node_id: i64,
    /// The share root is in the trash: the alias is not listed, but the grant is still there.
    pub dormant: bool,
}

/// One item in the trash: what it was, and the delete it went with.
#[derive(Debug, Clone, Serialize)]
pub struct TrashRow {
    pub id: i64,
    pub name: String,
    /// `file` or `folder`.
    pub kind: String,
    /// Where it was when it was deleted, in the caller's own paths.
    pub path: String,
    pub version: i64,
    pub nbytes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nlines: Option<i64>,
    /// 1 for a file; for a folder, the files deleted with it.
    pub files: i64,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    /// Empty for an entry that has just been restored.
    pub deleted_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_by: Option<String>,
}

/// Who this connection is.
#[derive(Debug, Serialize)]
pub struct Whoami {
    /// `None` for the owner, who opened the store without a token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub admin: bool,
    /// `agent`, `person`, or `owner` for the admin.
    pub kind: String,
    /// `aliased` or `single-root`.
    pub namespace: String,
    pub shares: Vec<ShareRow>,
}

#[derive(Debug, Serialize)]
pub struct AccountRow {
    pub name: String,
    pub kind: String,
    /// The store path of a single-root account's root, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub created_at: String,
    pub disabled: bool,
    pub shares: usize,
}

#[derive(Debug, Serialize)]
pub struct TokenRow {
    pub id: i64,
    pub account: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    /// Usable right now: not revoked, not expired.
    pub live: bool,
}

/// How many pointers name one location in an asset store, counted over every pointer the store
/// holds rather than the caller's view. Counts, and nothing else: which asset names those bytes,
/// and where it is, stay inside the binding that looked.
#[derive(Debug, Clone, Copy)]
pub struct ItemUsers {
    /// Pointers other than the asset's own that name the location.
    pub others: usize,
    /// Pointers this build could not read at all, so what their bytes are for is not known.
    pub unreadable: usize,
}

/// Which places in an asset store some pointer names, and how many pointers could not be read.
#[derive(Debug, Clone)]
pub struct ItemsNamed {
    /// One answer per location asked about, in the order they were given.
    pub named: Vec<bool>,
    /// Pointers this build could not read at all, so what their bytes are for is not known and
    /// nothing should be called unnamed on the strength of this.
    pub unreadable: usize,
}

pub trait Store {
    // ------------------------------------------------------------ accounts, tokens and shares
    //
    // Every one of these is the admin's, and each implementation refuses it for a token session
    // rather than leaving that to the CLI: the store is where the rule has to hold, because the
    // CLI is not the only caller (#12 §4).

    /// Present a bearer for this connection. Everything afterwards is answered in that account's
    /// view. Called once, before any other operation.
    fn authenticate(&mut self, _bearer: &str) -> Result<()> {
        Err(StoreError::invalid("this store does not support tokens"))
    }

    /// Who this connection is, and what it can see.
    fn whoami(&mut self) -> Result<Whoami>;

    fn account_create(&mut self, name: &str, kind: &str, root: Option<&str>) -> Result<()>;
    fn account_ls(&mut self) -> Result<Vec<AccountRow>>;
    /// Stop an account without forgetting it: its tokens stop working and its grants stay, so
    /// enabling it again is one command rather than a re-grant of everything it held.
    fn account_disable(&mut self, name: &str, disabled: bool) -> Result<()>;
    /// Turn a single-root account into a multi-share one, keeping its share under `alias`. An
    /// explicit change: every path the account sees gains a `/<alias>` prefix.
    fn account_convert(&mut self, name: &str, alias: Option<&str>) -> Result<String>;

    /// Mint a bearer. Returned once, stored hashed, and not recoverable afterwards.
    fn token_create(&mut self, account: &str, label: Option<&str>, expires_at: Option<&str>) -> Result<(String, i64)>;
    fn token_ls(&mut self, account: Option<&str>) -> Result<Vec<TokenRow>>;
    fn token_revoke(&mut self, id: i64) -> Result<()>;

    /// Share `path` and everything below it with `account`, under `alias` (the folder's own name
    /// by default). Every rule the model decides at grant time is applied here.
    fn access_grant(&mut self, account: &str, path: &str, rights: &str, alias: Option<&str>) -> Result<ShareRow>;
    /// Rename a share in one account's namespace. A move for that account, not a delete.
    fn access_rename(&mut self, account: &str, from: &str, to: &str) -> Result<()>;
    fn access_revoke(&mut self, account: &str, alias: &str) -> Result<()>;
    /// Every share; with an account name, that account's; with a store path, who can see it.
    fn access_ls(&mut self, who: Option<&str>) -> Result<Vec<ShareRow>>;

    /// This connection's shares that exist but may not be used — revoked, or with their folder in
    /// the trash — as the aliases they occupy in its own namespace.
    ///
    /// `sync` needs this and nothing else needs it. A revoked share's files vanish from every
    /// listing, which is indistinguishable from their having been deleted, and sync deletes from
    /// disk what the store no longer has. Without this it would empty a checkout the moment a
    /// share was taken away — the one case in #12 that destroys data. Empty for the owner.
    fn denied_shares(&mut self) -> Result<Vec<String>> {
        Ok(self.share_state()?.into_iter().filter(|(_, s)| s == "denied").map(|(a, _)| a).collect())
    }

    /// Every share of this connection as `(alias, "ro" | "rw" | "denied")`.
    ///
    /// `sync` is the only caller and needs all three: it must not try to push a file under a
    /// read-only share (the store would refuse it file by file, which reads as a failure rather
    /// than as the rule it is), and it must not delete from disk what a denied share left there.
    /// Empty for the owner, who writes everywhere.
    fn share_state(&mut self) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }

    fn backend(&self) -> &'static str;
    /// Make the store usable: create what is missing, upgrade what is old.
    fn init(&mut self) -> Result<()>;
    /// Every folder and file under `prefix` (not `prefix` itself unless it is a file).
    /// The trash: what was deleted and not yet purged, newest delete first; with `parent`, the
    /// entries that went to the trash inside that trashed folder.
    ///
    /// SQLite only. Postgres has no trash yet (docs/assets.md), so the backend says so rather
    /// than answering an empty list, which would read as "nothing is deleted".
    fn trash(&mut self, _parent: Option<i64>) -> Result<Vec<TrashRow>> {
        Err(StoreError::invalid("this store has no trash"))
    }

    /// Put a trash entry back where it was, with everything that went to the trash with it.
    fn trash_restore(&mut self, _id: i64, _author: Option<&str>) -> Result<TrashRow> {
        Err(StoreError::invalid("this store has no trash"))
    }

    fn nodes(&mut self, prefix: &str) -> Result<Vec<Entry>>;
    /// The folder's entries by name, or with `recursive` everything below it by path. A folder's
    /// size, lines, words and versions are totals over the files below it.
    fn ls(&mut self, path: &str, recursive: bool) -> Result<Vec<Entry>>;
    /// One full `Entry` for one path — the same record `ls` returns for it.
    fn stat(&mut self, path: &str) -> Result<Entry>;
    /// Content at `version` (HEAD when `None`) and the version it is.
    fn read(&mut self, path: &str, version: Option<i64>) -> Result<(Vec<u8>, i64)>;
    fn section(&mut self, path: &str, heading: &str) -> Result<Option<Vec<u8>>>;
    /// Matching lines, at most `per_file` from any one document. `limit` counts rows.
    fn search(&mut self, query: &str, prefix: &str, limit: i64, per_file: i64) -> Result<Vec<Hit>>;
    /// A file's heading spans as `(line_from, line_to, heading_path)`, for naming the section
    /// a line falls in. Empty for a file with no headings.
    fn sections_of(&mut self, path: &str) -> Result<Vec<(i64, i64, String)>>;
    /// Front-matter property names in use, most-used first; `prefix` narrows them.
    fn property_keys(&mut self, prefix: &str, limit: i64) -> Result<Vec<PropKey>>;
    /// The values one property takes, most-used first.
    fn property_values(&mut self, key: &str, prefix: &str, limit: i64) -> Result<Vec<PropValue>>;
    /// Documents matching a property query, under `folder`.
    fn property_find(&mut self, query: &str, folder: &str, limit: i64) -> Result<Vec<PropHit>>;
    /// Headings under `prefix`, in document order. `heading` narrows to one, matched folded;
    /// `mode` is `exact`, `prefix` or `contains`. `max_level` caps the depth.
    fn outline(&mut self, prefix: &str, heading: Option<&str>, mode: &str, max_level: Option<i64>, limit: i64) -> Result<Vec<OutlineRow>>;
    /// Distinct headings under `prefix` starting with `starts`, most-used first.
    fn heading_names(&mut self, prefix: &str, starts: &str, limit: i64) -> Result<Vec<HeadingName>>;
    /// Create a folder and any missing parents; returns without complaint if it is already there.
    fn mkdir(&mut self, path: &str) -> Result<()>;
    /// Called after a bulk load — `import`, `sync` — so the store can get itself ready.
    ///
    /// Postgres needs it: a store built in one burst keeps whatever planner statistics
    /// autovacuum worked out while the tables were nearly empty, because autovacuum's
    /// threshold is a share of the rows it already knows about. A heading query over 2,000
    /// notes then plans as a nested loop and takes 74 ms instead of 7.7. SQLite's planner
    /// does not depend on statistics this way, so its implementation does nothing.
    ///
    /// Best effort: a store that will not analyse is slower, not broken, so a failure here
    /// is reported and the command still succeeds.
    fn settle(&mut self) -> Result<()> {
        Ok(())
    }
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

    /// The same paths in the owner's namespace: what an account's view is a projection of.
    ///
    /// An asset store is one place, shared by every account of the store, so where an asset's
    /// bytes belong in it is the owner's path for that asset and not the caller's -- otherwise a
    /// store's layout depends on who pushed to it, and one document's asset fills two places when
    /// two accounts hold its folder under different names. A path that cannot be translated comes
    /// back as it was given. Asked for a vault at a time, since a store may answer over a network.
    fn owner_paths(&mut self, paths: &[String]) -> Result<Vec<String>> {
        Ok(paths.to_vec())
    }

    /// Which of `locations` in the asset store `store` any pointer names, in the order given and
    /// over every pointer the store holds rather than the caller's view.
    ///
    /// The set-shaped form of `asset_item_users`, for saying which of a store's files nothing needs
    /// any more: from a view, files another account's pointers name would read as named by nothing,
    /// and that list is acted on by hand in somebody's drive. `None` where the store cannot answer
    /// without the caller's view.
    fn asset_items_named(&mut self, _store: &str, _locations: &[String]) -> Result<Option<ItemsNamed>> {
        Ok(None)
    }

    /// Whether `location` -- a place in an asset store, which is laid out in the owner's paths --
    /// is one this caller may name at all.
    ///
    /// A pointer's item is whatever the pointer says, and an account with `rw` inside its own share
    /// can write one naming bytes of a folder it was never granted. Where a store addresses its
    /// files by path those are the owner's paths, so this is the question the store already answers
    /// about any other path: can the caller address it. `true` for the owner, and for an item no
    /// path can be made of -- a drive's file id is not a place in a namespace.
    fn may_name(&mut self, _location: &str) -> Result<bool> {
        Ok(true)
    }

    /// Whether any pointer other than the asset `own`'s names `location` in the asset store
    /// `store`, answered over every pointer this store holds and not the caller's view.
    ///
    /// A provider's bytes are shared by every account of a store, so whether they are still needed
    /// is not a question one account's view can answer: answered from a view, a delete takes away
    /// bytes another account's pointer still names. `None` where the store cannot answer it without
    /// that view, and then nothing is taken away.
    fn asset_item_users(&mut self, _store: &str, _location: &str, _own: &str) -> Result<Option<ItemUsers>> {
        Ok(None)
    }
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
