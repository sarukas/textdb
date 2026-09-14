//! `textdb sync`: reconcile a folder in the store with a directory on disk, both ways, and
//! `textdb git-status`: how a folder compares with a git commit.
//!
//! What both sides held after the last sync is recorded in the store (the sync base): each
//! file's version and git blob id. Compared with it, a file changed on one side is copied to the
//! other. Changed on both, the edits are merged line by line; where they overlap, the file on
//! disk gets git-style conflict markers and the store keeps its version until the markers are
//! resolved. A file deleted on one side and untouched on the other is deleted; deleted on one and
//! changed on the other, the changed one is kept. Only files the base or the store knows can be
//! deleted, so files never taken in (images, other types, ignored files) are left alone.
//!
//! In a git checkout the base also records the commit. Changes that came from git since then are
//! committed to the store under their git author and subject, and `--commit` commits what sync
//! wrote to disk, with `Textdb-*` trailers naming the store state and its authors.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use textdb_core::myers::diff3_marked;
use textdb_sqlite::normalize_path;

use crate::portable::{self, Platform};
use crate::store::{BaseFile, FileHead, GitState, Store, StoreError, SyncBase};
use crate::{emit_json, git, out, Result};

const OURS: &str = "textdb";
const THEIRS: &str = "disk";
/// Items listed per kind in the text output; `--json` lists them all.
const LIST_MAX: usize = 200;
/// A file modified this recently may change again within its timestamp's resolution, so its
/// size and time are not trusted to show it unchanged (git's "racy clean" problem).
const RACY_NS: i64 = 2_000_000_000;

pub struct Options {
    pub prefix: String,
    pub dir: PathBuf,
    pub exts: Vec<String>,
    /// First sync only: the commit the store's content came from.
    pub base_rev: Option<String>,
    pub dry_run: bool,
    pub commit: bool,
    pub author: String,
    /// The store as trailers name it, without a password.
    pub store: String,
    /// Take in files that a change of include rules adds since the last sync.
    pub accept_rules: bool,
}

/// What a sync takes in from disk, recorded with its base so the next sync can tell when it
/// changed: the extensions, the folders never read, and `.textdbignore`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Rules {
    pub exts: Vec<String>,
    pub skip_dirs: Vec<String>,
    /// Git blob id of `.textdbignore`, when the directory has one.
    pub ignore_file: Option<String>,
}

impl Rules {
    fn describe(&self) -> String {
        let ignore = if self.ignore_file.is_some() { "; .textdbignore" } else { "" };
        format!("ext {}; skip {}{ignore}", self.exts.join(","), self.skip_dirs.join(","))
    }
}

#[derive(Serialize)]
pub struct RulesChange {
    /// `None`: the last sync was made by a build that recorded no rules, and skipped every
    /// hidden folder.
    pub before: Option<Rules>,
    pub now: Rules,
    /// New files this sync takes in that the last sync's rules left out.
    pub newly_included: Vec<String>,
}

/// `md, .TXT` → `["md", "txt"]`.
pub fn parse_exts(ext: &str) -> Vec<String> {
    ext.split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Git's blob id of `bytes`: the SHA-1 of `blob <size>\0` and the bytes.
pub fn blob_id(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `bytes` with CRLF line ends made LF, as git stores a text file under `core.autocrlf` or
/// `eol=crlf`; `None` when there is no CRLF.
fn crlf_to_lf(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes.windows(2).any(|w| w == b"\r\n") {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        if !(b == b'\r' && bytes.get(i + 1) == Some(&b'\n')) {
            out.push(b);
        }
    }
    Some(out)
}

fn has_markers(bytes: &[u8]) -> bool {
    let lines = || bytes.split(|&b| b == b'\n');
    lines().any(|l| l.starts_with(b"<<<<<<< ")) && lines().any(|l| l.starts_with(b">>>>>>> "))
}

/// A directory as the sync base records it: absolute, without Windows' `\\?\` prefix.
fn dir_key(dir: &Path) -> String {
    let abs = std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf());
    let s = abs.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

fn store_path(prefix: &str, rel: &str) -> String {
    if prefix == "/" {
        format!("/{rel}")
    } else {
        format!("{prefix}/{rel}")
    }
}

/// Files under `prefix` by path relative to it.
fn heads_by_rel(st: &mut dyn Store, prefix: &str) -> Result<BTreeMap<String, FileHead>> {
    let skip = if prefix == "/" { 1 } else { prefix.len() + 1 };
    Ok(st
        .file_heads(prefix)?
        .into_iter()
        .map(|h| (h.path[skip..].to_string(), h))
        .collect())
}

/// Directories `sync` and `import` never read: version control, the textdb app installed in a
/// folder, Obsidian's trash and dependencies. Other hidden directories (`.claude`, `.github`, …)
/// are read like any other.
pub const SKIP_DIRS: &[&str] = &[".git", ".textdb", ".trash", "node_modules"];

/// Whether a file found only on disk is taken in: one of `exts`, and not inside one of
/// [`SKIP_DIRS`] (the same files `import` reads).
fn eligible(rel: &str, exts: &[String]) -> bool {
    let mut segs: Vec<&str> = rel.split('/').collect();
    let name = segs.pop().unwrap_or("");
    if segs.iter().any(|s| SKIP_DIRS.contains(s)) {
        return false;
    }
    exts.iter().any(|e| e == "*")
        || Path::new(name)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .is_some_and(|e| exts.contains(&e))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct OnDisk {
    size: i64,
    mtime: i64,
}

fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64)
}

fn on_disk(meta: &std::fs::Metadata) -> OnDisk {
    OnDisk {
        size: meta.len() as i64,
        mtime: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i64),
    }
}

#[derive(Default)]
struct Walk {
    files: BTreeMap<String, OnDisk>,
    /// Symbolic links: left alone, and what is below them too.
    links: Vec<String>,
}

/// Every regular file below `root`. `.git` is never entered; the rest of [`SKIP_DIRS`] only
/// when the store or the base has files in them.
fn walk(root: &Path, tracked_dirs: &HashSet<String>) -> Result<Walk> {
    let mut w = Walk::default();
    if !root.is_dir() {
        return Ok(w);
    }
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel_dir)) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| StoreError::other(format!("{}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if rel_dir.is_empty() { name.clone() } else { format!("{rel_dir}/{name}") };
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                w.links.push(rel);
            } else if kind.is_dir() {
                let skipped = SKIP_DIRS.contains(&name.as_str());
                if name != ".git" && (!skipped || tracked_dirs.contains(&rel)) {
                    pending.push((entry.path(), rel));
                }
            } else if kind.is_file() {
                w.files.insert(rel, on_disk(&entry.metadata()?));
            }
        }
    }
    Ok(w)
}

fn write_disk(root: &Path, rel: &str, bytes: &[u8]) -> std::io::Result<()> {
    let target = root.join(rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Truncated in place rather than replaced, so the file keeps its permissions.
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&target)?;
    file.write_all(bytes)
}

/// Delete a file, then the directories it leaves empty (never `root`).
fn remove_disk(root: &Path, rel: &str) -> std::io::Result<()> {
    let file = root.join(rel);
    if let Err(e) = std::fs::remove_file(&file) {
        if !clear_read_only(&file) {
            return Err(e);
        }
        std::fs::remove_file(&file)?;
    }
    let mut rel = rel;
    while let Some(i) = rel.rfind('/') {
        rel = &rel[..i];
        let dir = root.join(rel);
        if std::fs::remove_dir(&dir).is_ok() {
            continue;
        }
        let empty = std::fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_none());
        if !(empty && clear_read_only(&dir) && std::fs::remove_dir(&dir).is_ok()) {
            break;
        }
    }
    Ok(())
}

/// Windows refuses to delete a file or directory carrying the read-only attribute, which the
/// folders of synced and cloud-backed trees often have. Clear it; `false` when it was not set.
fn clear_read_only(path: &Path) -> bool {
    #[cfg(windows)]
    {
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            if perms.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                return std::fs::set_permissions(path, perms).is_ok();
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// The base row for `rel` as it now is on disk.
fn base_row(root: &Path, rel: &str, version: Option<i64>, blob: String, conflict: bool) -> BaseFile {
    let trusted = std::fs::metadata(root.join(rel))
        .ok()
        .map(|m| on_disk(&m))
        .filter(|m| now_ns() - m.mtime > RACY_NS);
    BaseFile {
        rel: rel.to_string(),
        version,
        blob,
        disk_size: trusted.map(|m| m.size),
        disk_mtime: trusted.map(|m| m.mtime),
        conflict,
    }
}

/// A base entry being compared.
struct Base {
    version: Option<i64>,
    blob: String,
    disk: Option<OnDisk>,
    conflict: bool,
    /// Taken from a git commit (`--base`): the blob is as git stores it, LF line ends included.
    from_git: bool,
}

impl Base {
    fn matches(&self, bytes: &[u8]) -> bool {
        blob_id(bytes) == self.blob || (self.from_git && crlf_to_lf(bytes).is_some_and(|lf| blob_id(&lf) == self.blob))
    }
}

/// Both sides' content, each read at most once.
struct Sides<'a> {
    st: &'a mut dyn Store,
    dir: PathBuf,
    prefix: String,
    textdb: HashMap<String, (Vec<u8>, i64)>,
    disk: HashMap<String, Vec<u8>>,
}

impl Sides<'_> {
    fn textdb(&mut self, rel: &str) -> Result<(Vec<u8>, i64)> {
        if let Some(found) = self.textdb.get(rel) {
            return Ok(found.clone());
        }
        let found = self.st.read(&store_path(&self.prefix, rel), None)?;
        self.textdb.insert(rel.to_string(), found.clone());
        Ok(found)
    }

    fn disk(&mut self, rel: &str) -> Result<Vec<u8>> {
        if let Some(found) = self.disk.get(rel) {
            return Ok(found.clone());
        }
        let found = std::fs::read(self.dir.join(rel)).map_err(|e| StoreError::other(format!("{rel}: {e}")))?;
        self.disk.insert(rel.to_string(), found.clone());
        Ok(found)
    }
}

#[derive(Default)]
struct Plan {
    /// Written from the store to disk.
    to_disk: Vec<String>,
    disk_delete: Vec<String>,
    /// Written from disk to the store, against this version.
    to_textdb: Vec<(String, Option<i64>)>,
    textdb_delete: Vec<String>,
    moves: Vec<(String, String)>,
    /// Both changed, merged cleanly: (rel, store version merged against, content).
    merges: Vec<(String, i64, Vec<u8>)>,
    /// Both changed, overlapping: (rel, store version, its blob, content with markers).
    conflicts: Vec<(String, i64, String, Vec<u8>)>,
    /// The same content on both sides without a base row: (rel, store version).
    adopt: Vec<(String, i64)>,
    /// Unchanged on both sides.
    keep: Vec<String>,
    /// Left exactly as the base has it: unresolved conflicts and links.
    hold: Vec<String>,
    /// Found only on disk, not yet checked against .gitignore or paired as moves.
    candidates: Vec<String>,
}

#[derive(Serialize, Default)]
pub struct Changes {
    pub new: Vec<String>,
    pub changed: Vec<String>,
    pub deleted: Vec<String>,
}

#[derive(Serialize)]
pub struct Move {
    pub from: String,
    pub to: String,
}

#[derive(Serialize)]
pub struct Note {
    pub path: String,
    pub reason: String,
}

fn note(path: &str, reason: impl Into<String>) -> Note {
    Note {
        path: path.to_string(),
        reason: reason.into(),
    }
}

#[derive(Serialize)]
pub struct GitReport {
    pub commit: Option<String>,
    pub branch: Option<String>,
    pub remote: Option<String>,
    pub clean: bool,
    /// Store commits that took in changes from git were attributed from `authors_from..commit`.
    pub authors_from: Option<String>,
    /// The commit `--commit` made.
    pub committed: Option<String>,
    pub commit_error: Option<String>,
}

#[derive(Serialize, Default)]
pub struct Report {
    pub prefix: String,
    pub dir: String,
    pub dry_run: bool,
    pub first_sync: bool,
    /// `--base`: the commit the first sync compared both sides with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    pub to_disk: Changes,
    pub to_textdb: Changes,
    /// Moved in the store because the file moved on disk.
    pub moved: Vec<Move>,
    pub merged: Vec<String>,
    /// Conflict markers written to these files on disk.
    pub conflicts: Vec<String>,
    /// Still have conflict markers on disk from an earlier sync.
    pub unresolved: Vec<String>,
    /// Deleted on one side and changed on the other: the changed file was kept.
    pub kept: Vec<Note>,
    pub unchanged: usize,
    pub skipped: Vec<Note>,
    pub failed: Vec<Note>,
    pub problems: Vec<portable::Problem>,
    /// Blocking name problems: nothing was written.
    pub stopped: bool,
    pub seq: Option<i64>,
    pub git: Option<GitReport>,
    /// The include rules differ from the last sync's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<RulesChange>,
    /// They would take in new files, and `--accept-rules` was not given: nothing was written.
    pub stopped_by_rules: bool,
}

fn failure(e: &StoreError) -> String {
    if e.code == "TX001" {
        "changed in textdb while syncing; run sync again".to_string()
    } else {
        e.message.clone()
    }
}

pub fn sync(st: &mut dyn Store, o: Options, json: bool) -> Result<()> {
    let prefix = normalize_path(&o.prefix)?;
    if !o.dir.exists() && !o.dry_run {
        std::fs::create_dir_all(&o.dir).map_err(|e| StoreError::other(format!("{}: {e}", o.dir.display())))?;
    }
    let key = dir_key(&o.dir);
    let repo = git::repo(&o.dir);
    if o.commit && repo.is_none() {
        return Err(StoreError::invalid(format!("--commit needs {} to be in a git checkout", o.dir.display())));
    }
    let stored = st.sync_base(&prefix, &key)?;
    if stored.is_some() && o.base_rev.is_some() {
        return Err(StoreError::invalid(format!(
            "{prefix} has been synced with {key} before and has a base already; --base is for the first sync only"
        )));
    }
    let heads = heads_by_rel(st, &prefix)?;
    let mut report = Report {
        prefix: prefix.clone(),
        dir: key.clone(),
        dry_run: o.dry_run,
        first_sync: stored.is_none(),
        ..Default::default()
    };

    let stored_rows: HashMap<String, BaseFile> = stored
        .as_ref()
        .map(|b| b.files.iter().map(|f| (f.rel.clone(), f.clone())).collect())
        .unwrap_or_default();
    let mut base: BTreeMap<String, Base> = stored_rows
        .values()
        .map(|f| {
            let base = Base {
                version: f.version,
                blob: f.blob.clone(),
                disk: f.disk_size.zip(f.disk_mtime).map(|(size, mtime)| OnDisk { size, mtime }),
                conflict: f.conflict,
                from_git: false,
            };
            (f.rel.clone(), base)
        })
        .collect();
    let mut from_commit = stored.as_ref().and_then(|b| b.git.as_ref()).and_then(|g| g.commit.clone());
    if let (None, Some(rev)) = (&stored, &o.base_rev) {
        if repo.is_none() {
            return Err(StoreError::invalid(format!("--base needs {} to be in a git checkout", o.dir.display())));
        }
        let commit = git::resolve(&o.dir, rev)?;
        // Only files the store has take their base from the commit: one the store lacks may
        // never have been imported, and must not be deleted from disk on that account.
        for (rel, oid) in git::tree(&o.dir, &commit)? {
            if heads.contains_key(&rel) {
                let base_entry = Base { version: None, blob: oid, disk: None, conflict: false, from_git: true };
                base.insert(rel, base_entry);
            }
        }
        from_commit = Some(commit.clone());
        report.base_commit = Some(commit);
    }

    let mut tracked_dirs = HashSet::new();
    for rel in base.keys().chain(heads.keys()) {
        let mut at = 0;
        while let Some(i) = rel[at..].find('/') {
            tracked_dirs.insert(rel[..at + i].to_string());
            at += i + 1;
        }
    }
    let walked = walk(&o.dir, &tracked_dirs)?;
    let under_link = |rel: &str| {
        walked
            .links
            .iter()
            .any(|l| rel == l || rel.strip_prefix(l.as_str()).is_some_and(|r| r.starts_with('/')))
    };
    for link in &walked.links {
        report.skipped.push(note(link, "a symbolic link, left as it is"));
    }

    let mut sides = Sides {
        st,
        dir: o.dir.clone(),
        prefix: prefix.clone(),
        textdb: HashMap::new(),
        disk: HashMap::new(),
    };
    let mut plan = Plan::default();
    let all: BTreeSet<String> = base.keys().chain(heads.keys()).chain(walked.files.keys()).cloned().collect();
    for rel in all {
        if under_link(&rel) {
            plan.hold.push(rel);
            continue;
        }
        let t = heads.get(&rel);
        let d = walked.files.get(&rel).copied();
        let Some(b) = base.get(&rel) else {
            match (t, d) {
                (Some(_), None) => plan.to_disk.push(rel),
                (None, Some(_)) => {
                    if eligible(&rel, &o.exts) {
                        plan.candidates.push(rel);
                    }
                }
                (Some(_), Some(_)) => {
                    let (tb, tv) = sides.textdb(&rel)?;
                    let db = sides.disk(&rel)?;
                    if tb == db {
                        plan.adopt.push((rel, tv));
                    } else {
                        // Nothing says which side changed: all of it is a conflict.
                        let (marked, _) = diff3_marked(b"", &tb, &db, OURS, THEIRS);
                        plan.conflicts.push((rel, tv, blob_id(&tb), marked));
                    }
                }
                (None, None) => {}
            }
            continue;
        };
        if b.conflict {
            match d {
                Some(_) if has_markers(&sides.disk(&rel)?) => {
                    report.unresolved.push(rel.clone());
                    plan.hold.push(rel);
                    continue;
                }
                None if t.is_some() => {
                    report.kept.push(note(&rel, "the file with conflict markers was deleted on disk: the textdb version is written back"));
                    plan.to_disk.push(rel);
                    continue;
                }
                _ => {}
            }
        }
        let t_changed = match t {
            None => None,
            Some(t) => Some(match b.version {
                Some(v) => t.version != v,
                None => !b.matches(&sides.textdb(&rel)?.0),
            }),
        };
        let d_changed = match d {
            None => None,
            Some(d) if b.disk == Some(d) => Some(false),
            Some(_) => Some(!b.matches(&sides.disk(&rel)?)),
        };
        match (t_changed, d_changed) {
            (None, None) => {}
            (Some(false), Some(false)) => plan.keep.push(rel),
            (Some(true), Some(false)) => plan.to_disk.push(rel),
            (Some(false), Some(true)) => plan.to_textdb.push((rel, t.map(|t| t.version))),
            (Some(true), Some(true)) => {
                let (tb, tv) = sides.textdb(&rel)?;
                let db = sides.disk(&rel)?;
                if tb == db {
                    plan.adopt.push((rel, tv));
                    continue;
                }
                let base_bytes = match (b.from_git, b.version, &report.base_commit) {
                    (true, _, Some(commit)) => git::show(&o.dir, commit, &rel).ok(),
                    (false, Some(v), _) => sides
                        .st
                        .read(&store_path(&prefix, &rel), Some(v))
                        .ok()
                        .map(|(bytes, _)| bytes)
                        .filter(|bytes| blob_id(bytes) == b.blob),
                    _ => None,
                };
                let (merged, conflicts) = diff3_marked(base_bytes.as_deref().unwrap_or(b""), &tb, &db, OURS, THEIRS);
                if conflicts == 0 {
                    plan.merges.push((rel, tv, merged));
                } else {
                    plan.conflicts.push((rel, tv, blob_id(&tb), merged));
                }
            }
            (None, Some(false)) => plan.disk_delete.push(rel),
            (Some(false), None) => plan.textdb_delete.push(rel),
            (None, Some(true)) => {
                report.kept.push(note(&rel, "deleted in textdb but changed on disk: the file on disk is added back"));
                plan.to_textdb.push((rel, None));
            }
            (Some(true), None) => {
                report.kept.push(note(&rel, "deleted on disk but changed in textdb: written back to disk"));
                plan.to_disk.push(rel);
            }
        }
    }

    if repo.is_some() && !plan.candidates.is_empty() {
        let ignored = git::ignored(&o.dir, &plan.candidates);
        plan.candidates.retain(|rel| !ignored.contains(rel));
    }
    // `.textdbignore` in the directory, in .gitignore syntax: files it matches are not taken in.
    let ignore_path = o.dir.join(".textdbignore");
    let ignore_bytes = std::fs::read(&ignore_path).ok();
    if ignore_bytes.is_some() && !plan.candidates.is_empty() {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&o.dir);
        let read = match builder.add(&ignore_path) {
            Some(e) => Err(e),
            None => builder.build(),
        };
        match read {
            Ok(gi) => plan.candidates.retain(|rel| !gi.matched_path_or_any_parents(rel, false).is_ignore()),
            Err(e) => report.skipped.push(note(".textdbignore", format!("not read: {e}"))),
        }
    }
    let rules = {
        let mut exts = o.exts.clone();
        exts.sort();
        exts.dedup();
        Rules {
            exts,
            skip_dirs: SKIP_DIRS.iter().map(|s| s.to_string()).collect(),
            ignore_file: ignore_bytes.as_deref().map(blob_id),
        }
    };
    // A file gone from disk and an identical new one elsewhere is a move: the store moves the
    // file, keeping its history.
    let mut by_blob: HashMap<String, Vec<String>> = HashMap::new();
    for rel in &plan.candidates {
        by_blob.entry(blob_id(&sides.disk(rel)?)).or_default().push(rel.clone());
    }
    let mut moved_to = HashSet::new();
    let mut moves = Vec::new();
    plan.textdb_delete.retain(|from| {
        let target = base.get(from).and_then(|b| by_blob.get(&b.blob)).filter(|to| to.len() == 1).map(|to| &to[0]);
        match target {
            Some(to) if moved_to.insert(to.clone()) => {
                moves.push((from.clone(), to.clone()));
                false
            }
            _ => true,
        }
    });
    plan.moves = moves;
    for rel in std::mem::take(&mut plan.candidates) {
        if !moved_to.contains(&rel) {
            plan.to_textdb.push((rel, None));
        }
    }

    // Names written to disk must be able to exist next to what is there.
    let new_on_disk: Vec<&str> = plan
        .to_disk
        .iter()
        .chain(plan.conflicts.iter().map(|c| &c.0))
        .filter(|rel| !walked.files.contains_key(*rel))
        .map(String::as_str)
        .collect();
    if !new_on_disk.is_empty() {
        let deleted: HashSet<&String> = plan.disk_delete.iter().collect();
        let mut after: Vec<String> = walked.files.keys().filter(|r| !deleted.contains(r)).cloned().collect();
        after.extend(new_on_disk.iter().map(|r| r.to_string()));
        let mut problems = portable::Problems::new(Platform::current());
        portable::check_names(&after, &mut problems);
        report.problems = problems
            .into_vec()
            .into_iter()
            .filter(|p| new_on_disk.iter().any(|rel| *rel == p.path || (p.path.ends_with('/') && rel.starts_with(&p.path))))
            .collect();
    }
    report.stopped = report.problems.iter().any(|p| p.blocking);

    for rel in &plan.to_disk {
        let side = if walked.files.contains_key(rel) { &mut report.to_disk.changed } else { &mut report.to_disk.new };
        side.push(rel.clone());
    }
    report.to_disk.deleted = plan.disk_delete.clone();
    for (rel, _) in &plan.to_textdb {
        let side = if heads.contains_key(rel) { &mut report.to_textdb.changed } else { &mut report.to_textdb.new };
        side.push(rel.clone());
    }
    report.to_textdb.deleted = plan.textdb_delete.clone();
    report.moved = plan.moves.iter().map(|(from, to)| Move { from: from.clone(), to: to.clone() }).collect();
    report.merged = plan.merges.iter().map(|m| m.0.clone()).collect();
    report.conflicts = plan.conflicts.iter().map(|c| c.0.clone()).collect();
    report.unchanged = plan.keep.len() + plan.adopt.len();

    // Include rules that changed since the last sync must not take in files unnoticed.
    if let Some(stored) = &stored {
        let before: Option<Rules> = stored.rules.as_deref().and_then(|r| serde_json::from_str(r).ok());
        if before.as_ref() != Some(&rules) {
            let left_out = |rel: &str| {
                let dirs: Vec<&str> = rel.split('/').rev().skip(1).collect();
                match &before {
                    None => dirs.iter().any(|s| s.starts_with('.') || *s == "node_modules"),
                    Some(b) => dirs.iter().any(|s| b.skip_dirs.iter().any(|d| d == s)) || !eligible(rel, &b.exts),
                }
            };
            let newly_included: Vec<String> = plan
                .to_textdb
                .iter()
                .filter(|(rel, v)| v.is_none() && !heads.contains_key(rel) && !base.contains_key(rel) && left_out(rel))
                .map(|(rel, _)| rel.clone())
                .collect();
            report.stopped_by_rules = !newly_included.is_empty() && !o.accept_rules;
            report.rules = Some(RulesChange { before, now: rules.clone(), newly_included });
        }
    }

    let changes = match (repo.as_ref().and_then(|r| r.commit.as_deref()), from_commit.as_deref()) {
        (Some(head), Some(from)) if head != from => git::changes(&o.dir, from, head),
        _ => HashMap::new(),
    };
    report.git = repo.as_ref().map(|r| GitReport {
        commit: r.commit.clone(),
        branch: r.branch.clone(),
        remote: r.remote.clone(),
        clean: r.clean,
        authors_from: (!changes.is_empty()).then(|| from_commit.clone()).flatten(),
        committed: None,
        commit_error: None,
    });

    if !report.stopped && !report.stopped_by_rules && !o.dry_run {
        apply(&mut sides, &o, &plan, &base, &stored_rows, &heads, &changes, &key, from_commit.as_deref(), &mut report, &rules)?;
    }
    let rules_stop = report.stopped_by_rules && !o.dry_run;

    for list in [
        &mut report.to_disk.new,
        &mut report.to_disk.changed,
        &mut report.to_disk.deleted,
        &mut report.to_textdb.new,
        &mut report.to_textdb.changed,
        &mut report.to_textdb.deleted,
    ] {
        list.sort();
    }
    let conflicted = report.conflicts.len() + report.unresolved.len();
    let commit_error = report.git.as_ref().and_then(|g| g.commit_error.clone());
    if json {
        emit_json(&report)?;
        if report.stopped || rules_stop {
            std::process::exit(6);
        }
        if commit_error.is_some() {
            std::process::exit(1);
        }
        if conflicted > 0 && !o.dry_run {
            std::process::exit(3);
        }
        return Ok(());
    }
    print_report(&report)?;
    let blocking = report.problems.iter().filter(|p| p.blocking).count();
    if report.stopped {
        return Err(StoreError::invalid(format!(
            "sync stopped before writing anything: {blocking} {} cannot be written on this computer; rename {} in the store",
            if blocking == 1 { "name" } else { "names" },
            if blocking == 1 { "it" } else { "them" }
        )));
    }
    if rules_stop {
        let n = report.rules.as_ref().map_or(0, |r| r.newly_included.len());
        return Err(StoreError::invalid(format!(
            "sync stopped before writing anything: the include rules changed since the last sync, and {n} {} would be \
             taken in that it left out (listed above). Pass --accept-rules to take them in, or list them in .textdbignore",
            if n == 1 { "file" } else { "files" }
        )));
    }
    if let Some(e) = commit_error {
        return Err(StoreError::other(format!("synced, but the git commit failed: {e}")));
    }
    if conflicted > 0 && !o.dry_run {
        return Err(StoreError::conflict(format!(
            "{conflicted} {} conflict markers on disk: resolve them, then run sync again",
            if conflicted == 1 { "file has" } else { "files have" }
        )));
    }
    Ok(())
}

/// Carry out `plan`: the store first, then disk, then the git commit and the new base.
#[allow(clippy::too_many_arguments)]
fn apply(
    sides: &mut Sides,
    o: &Options,
    plan: &Plan,
    base: &BTreeMap<String, Base>,
    stored_rows: &HashMap<String, BaseFile>,
    heads: &BTreeMap<String, FileHead>,
    changes: &HashMap<String, git::Change>,
    key: &str,
    from_commit: Option<&str>,
    report: &mut Report,
    rules: &Rules,
) -> Result<()> {
    let prefix = sides.prefix.clone();
    let dir = o.dir.as_path();
    let author = o.author.as_str();
    let mut rows: BTreeMap<String, BaseFile> = BTreeMap::new();
    // A failed step leaves the file's base as it was, so the next sync sees the same change.
    let failed = |report: &mut Report, rows: &mut BTreeMap<String, BaseFile>, rel: &str, reason: String| {
        report.failed.push(note(rel, reason));
        if let Some(row) = stored_rows.get(rel) {
            rows.insert(rel.to_string(), row.clone());
        }
    };
    // A store write that had to rebase over a concurrent commit holds more than the disk does:
    // its version is left unknown, so the next sync compares content and writes it out.
    let exact = |kind: &str, version: i64| matches!(kind, "direct" | "noop").then_some(version);

    for (from, to) in &plan.moves {
        match sides.st.mv(&store_path(&prefix, from), &store_path(&prefix, to), Some(author), Some(&format!("sync: moved in {key}"))) {
            Ok(()) => {
                let version = heads.get(from).map(|h| h.version);
                rows.insert(to.clone(), base_row(dir, to, version, base[from].blob.clone(), false));
            }
            Err(e) => failed(report, &mut rows, from, failure(&e)),
        }
    }
    for (rel, base_version) in &plan.to_textdb {
        let bytes = match sides.disk(rel) {
            Ok(bytes) => bytes,
            Err(e) => {
                failed(report, &mut rows, rel, e.message);
                continue;
            }
        };
        let (who, message) = match changes.get(rel) {
            Some(change) => (change.author.as_str(), change.message()),
            None => (author, format!("sync from {key}")),
        };
        match sides.st.write(&store_path(&prefix, rel), &bytes, *base_version, Some(who), Some(&message)) {
            Ok(w) => {
                rows.insert(rel.clone(), base_row(dir, rel, exact(&w.kind, w.version), blob_id(&bytes), false));
            }
            Err(e) => failed(report, &mut rows, rel, failure(&e)),
        }
    }
    for (rel, version, merged) in &plan.merges {
        let message = format!("sync: merged with the changes in {key}");
        match sides.st.write(&store_path(&prefix, rel), merged, Some(*version), Some(author), Some(&message)) {
            Ok(w) => match write_disk(dir, rel, merged) {
                Ok(()) => {
                    rows.insert(rel.clone(), base_row(dir, rel, exact(&w.kind, w.version), blob_id(merged), false));
                }
                Err(e) => failed(report, &mut rows, rel, e.to_string()),
            },
            Err(e) => failed(report, &mut rows, rel, failure(&e)),
        }
    }
    for rel in &plan.textdb_delete {
        if let Err(e) = sides.st.rm(&store_path(&prefix, rel), Some(author), Some(&format!("sync: deleted in {key}"))) {
            failed(report, &mut rows, rel, failure(&e));
        }
    }

    for rel in &plan.to_disk {
        match sides.textdb(rel) {
            Ok((bytes, version)) => match write_disk(dir, rel, &bytes) {
                Ok(()) => {
                    rows.insert(rel.clone(), base_row(dir, rel, Some(version), blob_id(&bytes), false));
                }
                Err(e) => failed(report, &mut rows, rel, e.to_string()),
            },
            Err(e) => failed(report, &mut rows, rel, e.message),
        }
    }
    for (rel, version, blob, marked) in &plan.conflicts {
        match write_disk(dir, rel, marked) {
            Ok(()) => {
                rows.insert(rel.clone(), base_row(dir, rel, Some(*version), blob.clone(), true));
            }
            Err(e) => failed(report, &mut rows, rel, e.to_string()),
        }
    }
    for rel in &plan.disk_delete {
        if let Err(e) = remove_disk(dir, rel) {
            failed(report, &mut rows, rel, e.to_string());
        }
    }
    for (rel, version) in &plan.adopt {
        let (bytes, _) = sides.textdb(rel)?;
        rows.insert(rel.clone(), base_row(dir, rel, Some(*version), blob_id(&bytes), false));
    }
    for rel in &plan.keep {
        let b = &base[rel];
        // A base taken from git holds git's blob; record the content as it is here.
        let blob = match sides.disk.get(rel) {
            Some(bytes) if b.from_git => blob_id(bytes),
            _ => b.blob.clone(),
        };
        rows.insert(rel.clone(), base_row(dir, rel, heads.get(rel).map(|h| h.version), blob, false));
    }
    for rel in &plan.hold {
        if let Some(row) = stored_rows.get(rel) {
            rows.insert(rel.clone(), row.clone());
        }
    }

    let seq = sides.st.last_seq()?;
    report.seq = Some(seq);
    if o.commit {
        let not_done: HashSet<&str> = report.failed.iter().map(|f| f.path.as_str()).collect();
        let paths: Vec<String> = plan
            .to_disk
            .iter()
            .chain(plan.merges.iter().map(|m| &m.0))
            .chain(plan.disk_delete.iter())
            .filter(|rel| !not_done.contains(rel.as_str()))
            .cloned()
            .collect();
        if !paths.is_empty() {
            let message = commit_message(o, &prefix, plan, heads, from_commit, seq, &not_done);
            let result = git::commit(dir, &paths, &message);
            if let Some(g) = report.git.as_mut() {
                match result {
                    Ok(committed) => g.committed = committed,
                    Err(e) => g.commit_error = Some(e.message),
                }
            }
        }
    }

    let after = git::repo(dir);
    if let (Some(g), Some(r)) = (report.git.as_mut(), after.as_ref()) {
        g.commit = r.commit.clone();
        g.clean = r.clean;
    }
    sides.st.save_sync_base(&SyncBase {
        prefix: prefix.clone(),
        dir: key.to_string(),
        seq,
        synced_at: None,
        author: Some(author.to_string()),
        git: after.map(|r| GitState {
            commit: r.commit,
            branch: r.branch,
            remote: r.remote,
            clean: r.clean,
        }),
        rules: serde_json::to_string(rules).ok(),
        files: rows.into_values().collect(),
    })
}

/// `textdb sync /docs: 2 changed, 1 added` with who made the changes in the store and
/// `Textdb-*` trailers.
fn commit_message(
    o: &Options,
    prefix: &str,
    plan: &Plan,
    heads: &BTreeMap<String, FileHead>,
    from_commit: Option<&str>,
    seq: i64,
    not_done: &HashSet<&str>,
) -> String {
    let done = |rel: &&String| !not_done.contains(rel.as_str());
    let added = plan.to_disk.iter().filter(done).filter(|rel| !o.dir.join(rel).exists() || !heads.contains_key(*rel)).count();
    let changed = plan.to_disk.iter().filter(done).count() - added + plan.merges.iter().map(|m| &m.0).filter(done).count();
    let deleted = plan.disk_delete.iter().filter(done).count();
    let mut parts = Vec::new();
    for (n, what) in [(changed, "changed"), (added, "added"), (deleted, "deleted")] {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    let mut authors: BTreeMap<String, usize> = BTreeMap::new();
    for rel in plan.to_disk.iter().filter(done) {
        let who = heads.get(rel).and_then(|h| h.updated_by.clone()).unwrap_or_else(|| "unknown".to_string());
        *authors.entry(who).or_default() += 1;
    }
    let merged = plan.merges.iter().map(|m| &m.0).filter(done).count();
    if merged > 0 {
        *authors.entry(o.author.clone()).or_default() += merged;
    }
    let mut m = format!("textdb sync {prefix}: {}\n\n", parts.join(", "));
    if !authors.is_empty() {
        let since = from_commit.map(|c| format!(" since {}", git::short(c))).unwrap_or_default();
        let by: Vec<String> = authors
            .iter()
            .map(|(a, n)| format!("{a} ({n} {})", if *n == 1 { "file" } else { "files" }))
            .collect();
        m.push_str(&format!("Changed in textdb{since} by {}.\n\n", by.join(", ")));
    }
    m.push_str(&format!("Textdb-Store: {}\nTextdb-Prefix: {prefix}\nTextdb-Seq: {seq}\n", o.store));
    for a in authors.keys() {
        m.push_str(&format!("Textdb-Author: {a}\n"));
    }
    m
}

fn list(s: &mut String, label: &str, items: &[String]) {
    for rel in items.iter().take(LIST_MAX) {
        s.push_str(&format!("{label:<15} {rel}\n"));
    }
    if items.len() > LIST_MAX {
        s.push_str(&format!("{label:<15} … and {} more\n", items.len() - LIST_MAX));
    }
}

fn print_report(r: &Report) -> Result<()> {
    let mut s = String::new();
    if let Some(rc) = &r.rules {
        let before = match &rc.before {
            Some(b) => b.describe(),
            None => "not recorded (an older build, which skipped every hidden folder)".to_string(),
        };
        s.push_str(&format!("{:<15} include rules changed since the last sync: {before} -> {}\n", "rules", rc.now.describe()));
        list(&mut s, "newly included", &rc.newly_included);
    }
    list(&mut s, "disk new", &r.to_disk.new);
    list(&mut s, "disk changed", &r.to_disk.changed);
    list(&mut s, "disk deleted", &r.to_disk.deleted);
    list(&mut s, "textdb new", &r.to_textdb.new);
    list(&mut s, "textdb changed", &r.to_textdb.changed);
    list(&mut s, "textdb deleted", &r.to_textdb.deleted);
    for m in &r.moved {
        s.push_str(&format!("{:<15} {} -> {}\n", "textdb moved", m.from, m.to));
    }
    list(&mut s, "merged", &r.merged);
    list(&mut s, "conflict", &r.conflicts);
    list(&mut s, "unresolved", &r.unresolved);
    for (label, notes) in [("kept", &r.kept), ("skipped", &r.skipped), ("failed", &r.failed)] {
        for n in notes {
            s.push_str(&format!("{label:<15} {}: {}\n", n.path, n.reason));
        }
    }
    for p in &r.problems {
        if p.blocking {
            s.push_str(&format!("{:<15} {}: {}\n", "problem", p.path, p.detail));
        } else {
            let on: Vec<&str> = p.platforms.iter().map(|x| x.name()).collect();
            s.push_str(&format!("{:<15} {}: {} (on {})\n", "warning", p.path, p.detail, on.join(", ")));
        }
    }
    let verb = if r.stopped || (r.stopped_by_rules && !r.dry_run) {
        "nothing synced"
    } else if r.dry_run {
        "dry run, nothing written"
    } else {
        "synced"
    };
    s.push_str(&format!(
        "{verb}: {} with {}: disk {} new, {} changed, {} deleted; textdb {} new, {} changed, {} deleted, {} moved; {} merged, {} conflicts; {} unchanged\n",
        r.prefix,
        r.dir,
        r.to_disk.new.len(),
        r.to_disk.changed.len(),
        r.to_disk.deleted.len(),
        r.to_textdb.new.len(),
        r.to_textdb.changed.len(),
        r.to_textdb.deleted.len(),
        r.moved.len(),
        r.merged.len(),
        r.conflicts.len(),
        r.unchanged
    ));
    if let Some(g) = &r.git {
        s.push_str(&format!(
            "git: {} on {}{}{}\n",
            g.commit.as_deref().map(git::short).unwrap_or("no commits"),
            g.branch.as_deref().unwrap_or("a detached HEAD"),
            if g.clean { "" } else { ", with uncommitted changes" },
            g.committed.as_deref().map(|c| format!("; committed {}", git::short(c))).unwrap_or_default()
        ));
    }
    out(s.as_bytes())
}

#[derive(Serialize, Default)]
pub struct TreeCompare {
    pub rev: String,
    pub commit: String,
    /// Identical, counting files that differ only in CRLF vs LF line ends.
    pub same: usize,
    pub line_endings_only: usize,
    pub differ: Vec<String>,
    pub only_textdb: Vec<String>,
    /// Files of the commit that `sync` would take in (by `--ext`) but the store lacks.
    pub only_git: Vec<String>,
}

#[derive(Serialize)]
pub struct GitStatusReport {
    pub prefix: String,
    pub dir: String,
    /// Every directory the folder has been synced with, newest first.
    pub synced: Vec<SyncBase>,
    /// Store changes since the last sync with `dir`.
    pub since_sync: Option<Changes>,
    /// `None` when `dir` is not in a git checkout.
    pub git: Option<TreeCompare>,
}

pub fn git_status(st: &mut dyn Store, prefix: &str, dir: &Path, rev: &str, exts: &[String], json: bool) -> Result<()> {
    let prefix = normalize_path(prefix)?;
    let key = dir_key(dir);
    let heads = heads_by_rel(st, &prefix)?;
    let synced = st.sync_bases(&prefix)?;
    let since_sync = st.sync_base(&prefix, &key)?.map(|b| {
        let rows: HashMap<&str, &BaseFile> = b.files.iter().map(|f| (f.rel.as_str(), f)).collect();
        let mut c = Changes::default();
        for (rel, h) in &heads {
            match rows.get(rel.as_str()) {
                None => c.new.push(rel.clone()),
                Some(f) if f.version != Some(h.version) => c.changed.push(rel.clone()),
                Some(_) => {}
            }
        }
        c.deleted = b.files.iter().filter(|f| !heads.contains_key(&f.rel)).map(|f| f.rel.clone()).collect();
        c.deleted.sort();
        c
    });
    let git = match git::repo(dir) {
        None => None,
        Some(_) => {
            let commit = git::resolve(dir, rev)?;
            let tree = git::tree(dir, &commit)?;
            let mut cmp = TreeCompare {
                rev: rev.to_string(),
                commit,
                ..Default::default()
            };
            for (rel, h) in &heads {
                let Some(oid) = tree.get(rel) else {
                    cmp.only_textdb.push(rel.clone());
                    continue;
                };
                let (bytes, _) = st.read(&h.path, None)?;
                if &blob_id(&bytes) == oid {
                    cmp.same += 1;
                } else if crlf_to_lf(&bytes).is_some_and(|lf| &blob_id(&lf) == oid) {
                    cmp.same += 1;
                    cmp.line_endings_only += 1;
                } else {
                    cmp.differ.push(rel.clone());
                }
            }
            cmp.only_git = tree.keys().filter(|rel| !heads.contains_key(*rel) && eligible(rel, exts)).cloned().collect();
            Some(cmp)
        }
    };
    let report = GitStatusReport {
        prefix: prefix.clone(),
        dir: key.clone(),
        synced,
        since_sync,
        git,
    };
    if json {
        return emit_json(&report);
    }
    let mut s = String::new();
    if report.synced.is_empty() {
        s.push_str(&format!("{prefix} has not been synced with a directory\n"));
    }
    for b in &report.synced {
        let git = b
            .git
            .as_ref()
            .map(|g| {
                format!(
                    ", git {} on {}{}{}",
                    g.commit.as_deref().map(git::short).unwrap_or("(no commits)"),
                    g.branch.as_deref().unwrap_or("a detached HEAD"),
                    if g.clean { "" } else { " with uncommitted changes" },
                    g.remote.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()
                )
            })
            .unwrap_or_default();
        s.push_str(&format!(
            "synced with {} at {} (change #{}){git}\n",
            b.dir,
            b.synced_at.as_deref().unwrap_or("?"),
            b.seq
        ));
    }
    if let Some(c) = &report.since_sync {
        s.push_str(&format!(
            "in textdb since the last sync with {key}: {} changed, {} new, {} deleted\n",
            c.changed.len(),
            c.new.len(),
            c.deleted.len()
        ));
        list(&mut s, "changed", &c.changed);
        list(&mut s, "new", &c.new);
        list(&mut s, "deleted", &c.deleted);
    }
    match &report.git {
        None => s.push_str(&format!("{key} is not in a git checkout\n")),
        Some(g) => {
            let eol = if g.line_endings_only > 0 { format!(" ({} only by line endings)", g.line_endings_only) } else { String::new() };
            s.push_str(&format!(
                "compared with {} ({}): {} same{eol}, {} differ, {} only in textdb, {} only in git\n",
                g.rev,
                git::short(&g.commit),
                g.same,
                g.differ.len(),
                g.only_textdb.len(),
                g.only_git.len()
            ));
            list(&mut s, "differs", &g.differ);
            list(&mut s, "only textdb", &g.only_textdb);
            list(&mut s, "only git", &g.only_git);
        }
    }
    out(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_ids_match_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(blob_id(b"hello\n"), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(blob_id(b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(crlf_to_lf(b"a\r\nb\r\n"), Some(b"a\nb\n".to_vec()));
        assert_eq!(crlf_to_lf(b"a\nb"), None);
    }

    #[cfg(windows)]
    #[test]
    fn removes_read_only_directories_it_empties() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("x.md"), "x").unwrap();
        std::fs::write(tmp.path().join("a/keep.md"), "k").unwrap();
        for dir in [tmp.path().join("a"), deep.clone()] {
            let mut perms = std::fs::metadata(&dir).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&dir, perms).unwrap();
        }
        remove_disk(tmp.path(), "a/b/x.md").unwrap();
        assert!(!deep.exists());
        // A directory that still holds files keeps its attribute and stays.
        assert!(std::fs::metadata(tmp.path().join("a")).unwrap().permissions().readonly());
    }

    #[test]
    fn only_wanted_files_on_disk_are_taken_in() {
        let exts = parse_exts("md, .TXT");
        assert!(eligible("notes/a.md", &exts) && eligible("B.Txt", &exts));
        assert!(eligible(".claude/instructions/rules.md", &exts) && eligible("docs/.drafts/a.md", &exts));
        assert!(!eligible("logo.png", &exts) && !eligible("x/node_modules/a.md", &exts));
        assert!(!eligible(".git/a.md", &exts) && !eligible(".trash/a.md", &exts) && !eligible(".textdb/app/a.md", &exts));
        assert!(eligible("logo.png", &parse_exts("*")));
        assert!(has_markers(b"a\n<<<<<<< textdb\nb\n=======\nc\n>>>>>>> disk\n"));
        assert!(!has_markers(b"<<<<<<< only an opening line\n"));
    }
}
