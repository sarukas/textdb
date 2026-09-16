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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
/// Whether names that differ only in letter case are the same file here.
const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

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
    /// The store as given, so a SQLite file inside the directory can be left out of the sync.
    /// Unredacted, since it is compared as a path and never printed.
    pub store_file: String,
    /// Take in files that a change of include rules adds since the last sync.
    pub accept_rules: bool,
    /// Remove directories on disk that hold no files.
    pub prune_empty_dirs: bool,
    /// What to do with assets besides listing them: `push`, `pull` or `both`; `None` for what the
    /// store's `asset_sync` setting says.
    pub assets: Option<String>,
    /// How long to wait for another sync of the same directory. Zero — the default — fails at
    /// once, so two syncs at once are one success and one visible failure rather than two
    /// successes where the second silently found nothing left to do.
    pub lock_wait: Duration,
    /// Print the summary line and what went wrong, nothing else. What a hook wants.
    pub quiet: bool,
}

/// What a sync takes in from disk, recorded with its base so the next sync can tell when it
/// changed: the extensions, the folders never read, and `.textdbignore`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Rules {
    pub exts: Vec<String>,
    pub skip_dirs: Vec<String>,
    /// Git blob id of `.textdbignore`, when the directory has one.
    pub ignore_file: Option<String>,
    /// The text of `.textdbignore`, so the next sync can tell what loosening it lets through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_text: Option<String>,
    /// Whether sync has put its default lines in `.textdbignore`, which it does once per directory.
    #[serde(default)]
    pub ignore_seeded: bool,
    /// One id for the `.gitattributes` files, which say which files are assets (`""` when there
    /// are none); `None` for a base an older build saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitattributes: Option<String>,
    /// A digest of the files this sync walked past, so the next one can say so only when the set
    /// changed. `Rules::same` does not look at it: a new `.csv` on disk is not a rules change, it
    /// is something to mention once. `None` for a base an older build saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_out: Option<String>,
}

impl Rules {
    fn describe(&self) -> String {
        let ignore = self.ignore_file.as_deref().map_or(String::new(), |id| format!("; .textdbignore {}", &id[..id.len().min(7)]));
        let attrs = if self.gitattributes.as_deref().is_some_and(|g| !g.is_empty()) { "; .gitattributes" } else { "" };
        format!("ext {}; skip {}{ignore}{attrs}", self.exts.join(","), self.skip_dirs.join(","))
    }

    /// Whether `now` takes in what these rules did. The `.gitattributes` id is compared on its own:
    /// it decides which assets a sync pushes, not which documents it takes in.
    fn same(&self, now: &Rules) -> bool {
        self.exts == now.exts && self.skip_dirs == now.skip_dirs && self.ignore_file == now.ignore_file
    }
}

/// The `.gitattributes` files among `walked`'s below the top.
fn nested_rules_files(walked: &Walk) -> Vec<String> {
    walked
        .files
        .keys()
        .filter(|r| r.rsplit_once('/').is_some_and(|(_, name)| crate::assets::classify::is_rules_file(name)))
        .cloned()
        .collect()
}

/// One id for the `.gitattributes` files among `walked`'s (`""` when there are none).
fn gitattributes_id(dir: &Path, walked: &Walk) -> String {
    let mut all = Vec::new();
    // Any letter case counts, wherever it is: the id only holds back pushes when it changes.
    for rel in walked.files.keys().filter(|r| r.rsplit('/').next().is_some_and(|n| n.eq_ignore_ascii_case(".gitattributes"))) {
        all.extend_from_slice(rel.as_bytes());
        all.push(0);
        all.extend(std::fs::read(dir.join(rel)).unwrap_or_default());
        all.push(0);
    }
    if all.is_empty() {
        String::new()
    } else {
        blob_id(&all)
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

/// One id for the set of files a sync walked past, so the next one can tell whether it changed.
///
/// The paths themselves, not a count: the same number of different files is a different set, and
/// that is what a reader wants told.
fn left_out_digest(left_out: &BTreeMap<String, Vec<String>>) -> String {
    let mut flat: Vec<&str> = left_out.values().flatten().map(String::as_str).collect();
    flat.sort_unstable();
    blob_id(flat.join("\n").as_bytes())
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
pub fn dir_key(dir: &Path) -> String {
    let abs = std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf());
    let s = abs.display().to_string();
    // A share comes back as `\\?\UNC\server\share\…`: that is `\\server\share\…`.
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// `\\server\share\…` as builds before this one recorded it: `UNC\server\share\…`.
fn legacy_dir_key(key: &str) -> Option<String> {
    key.strip_prefix(r"\\").map(|rest| format!(r"UNC\{rest}"))
}

/// Whether the directory a sync base recorded is the one with [`dir_key`] `key`.
pub fn is_dir_key(recorded: &str, key: &str) -> bool {
    let same = |a: &str, b: &str| if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b };
    same(recorded, key) || legacy_dir_key(key).is_some_and(|l| same(recorded, &l))
}

/// The base of `prefix` synced with the directory `key`, under the form an older build may have
/// recorded it in.
pub fn find_sync_base(st: &mut dyn Store, prefix: &str, key: &str) -> Result<Option<SyncBase>> {
    match (st.sync_base(prefix, key)?, legacy_dir_key(key)) {
        (Some(b), _) => Ok(Some(b)),
        (None, Some(legacy)) => st.sync_base(prefix, &legacy),
        (None, None) => Ok(None),
    }
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

/// The directory's own rules for what sync leaves out, in `.gitignore` syntax.
const IGNORE_FILE: &str = ".textdbignore";

/// The lines sync puts in a directory's `.textdbignore` once: Obsidian's settings sync, but not the
/// code and styles it loads (at any depth, and a file of that name too), which anyone who can write
/// to the store could otherwise put there.
const DEFAULT_IGNORE_LINES: &[&str] = &["**/.obsidian/plugins", "**/.obsidian/snippets", "**/.obsidian/themes"];

const DEFAULT_IGNORE_HEADER: &str = "# What sync leaves out, in .gitignore syntax: not taken in, and never written, moved or deleted.\n\
# Obsidian's plugins, snippets and themes are code and styles it loads: delete these lines to sync them.\n";

/// What sync adds to a `.textdbignore` holding `text` the first time: the default lines for the
/// folders it has no say on yet. A say is a pattern for the folder itself at any depth
/// (`**/.obsidian/plugins/`) or one bringing it back (`!.obsidian/plugins`); comments, patterns
/// for the top folder only or for some files in it leave the default to add.
fn default_ignore_additions(text: &str) -> Option<String> {
    let said: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('!') || l.starts_with("**/"))
        .map(|l| l.trim_start_matches('!').trim_start_matches("**/").trim_start_matches('/').trim_end_matches('/').to_ascii_lowercase())
        .collect();
    let missing: Vec<&str> = DEFAULT_IGNORE_LINES
        .iter()
        .copied()
        .filter(|line| !said.iter().any(|s| s == line.trim_start_matches("**/")))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let mut add = String::from(DEFAULT_IGNORE_HEADER);
    for line in missing {
        add.push_str(line);
        add.push('\n');
    }
    Some(add)
}

/// Why a file with NUL bytes is not taken in: UTF-16 text (it starts with a byte order mark), or a
/// binary file.
fn binary_note(utf16: bool) -> &'static str {
    if utf16 {
        "UTF-16 text: not taken in (save it as UTF-8 to sync it)"
    } else {
        "a binary file: not taken in (list it in .textdbignore, or make it an asset with a textdb=asset line in .gitattributes)"
    }
}

/// Whether `.textdbignore`'s rules leave `rel` out. As in git, a file inside a folder they leave out
/// stays out, whatever a later `!` line says about the file.
fn ignored_by(gi: &ignore::gitignore::Gitignore, rel: &str) -> bool {
    let mut end = 0;
    while let Some(i) = rel[end..].find('/') {
        end += i;
        if gi.matched(&rel[..end], true).is_ignore() {
            return true;
        }
        end += 1;
    }
    gi.matched(rel, false).is_ignore()
}

/// Whether `rel` is, or is inside, a folder sync never writes a store file into: version control,
/// textdb's own folder, trash, dependencies and system folders (the folders assets are never in,
/// `.obsidian` excepted: which of its folders sync is up to `.textdbignore`); or `.textdbignore`
/// itself. Names are taken as Windows resolves them, so `.GIT`, `.git.`,
/// `.git::$INDEX_ALLOCATION` and `GIT~1` are `.git`. A file anyone put in the store must not
/// become a git hook, a script textdb's wrappers run, or a dependency.
fn protected_rel(rel: &str) -> bool {
    use crate::assets::classify::{names_dir, IGNORED_DIRS};
    let dirs: Vec<&str> = IGNORED_DIRS.iter().copied().filter(|d| *d != ".obsidian").collect();
    let segs: Vec<&str> = rel.split('/').collect();
    segs.iter().any(|s| names_dir(s, &dirs))
        // The rules that keep files out are the directory's own: anyone who can write to the store
        // could otherwise take away the lines that keep Obsidian's plugins out, or put a folder in
        // their place.
        || names_dir(segs[0], &[IGNORE_FILE])
}

/// The rules of a `.textdbignore` (in any letter case where the file system ignores it), and a
/// note for each line that is not a pattern.
fn ignore_rules(dir: &Path, text: &str) -> (Option<ignore::gitignore::Gitignore>, Vec<String>) {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
    let _ = builder.case_insensitive(CASE_INSENSITIVE);
    let mut notes = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if let Err(e) = builder.add_line(None, line) {
            notes.push(format!("line {}: {e}", n + 1));
        }
    }
    match builder.build() {
        Ok(gi) => (Some(gi), notes),
        Err(e) => {
            notes.push(e.to_string());
            (None, notes)
        }
    }
}

fn refuse_protected(rel: &str) -> std::io::Result<()> {
    if protected_rel(rel) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{rel} is inside a folder sync never writes into (.git, .textdb, node_modules, …)"),
        ));
    }
    Ok(())
}

/// A write or move through another name Windows gives a file or folder already on disk, an 8.3
/// short name (`GITATT~1`, `PROJEC~1/note.md`, `MY~SEC~1.JSO`, or one set by hand with no `~`), is
/// refused: it would reach that file (`.gitattributes`, say) under a name sync does not know. The
/// part of the path already on disk must be what the file system calls it, in any letter case.
fn refuse_short_alias(root: &Path, rel: &str) -> std::io::Result<()> {
    let segs: Vec<&str> = rel.split('/').collect();
    let mut on_disk = segs.len();
    while on_disk > 0 && std::fs::symlink_metadata(root.join(segs[..on_disk].join("/"))).is_err() {
        on_disk -= 1;
    }
    if on_disk == 0 {
        return Ok(());
    }
    let wanted: Vec<String> = segs[..on_disk].iter().map(|s| s.to_lowercase()).collect();
    let named = match (std::fs::canonicalize(root), std::fs::canonicalize(root.join(segs[..on_disk].join("/")))) {
        (Ok(base), Ok(real)) => real
            .strip_prefix(&base)
            .ok()
            .map(|tail| tail.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>())
            == Some(wanted),
        // Without the file system's own name, each part must be listed in its folder by that name.
        _ => {
            let mut at = root.to_path_buf();
            wanted.iter().all(|seg| {
                let listed = std::fs::read_dir(&at).is_ok_and(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().to_lowercase() == *seg));
                at.push(seg);
                listed
            })
        }
    };
    if named {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("{rel} is another name (an 8.3 short name) of a file or folder already on disk, which sync never writes through"),
    ))
}

/// A write, move or delete that would go through a symbolic link or junction already on disk,
/// whose folder may be anywhere (a `.git` included), is refused.
fn refuse_link(root: &Path, rel: &str) -> std::io::Result<()> {
    if crate::assets::through_link(root, rel) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{rel} is reached through a link or junction on disk, which sync never writes through"),
        ));
    }
    Ok(())
}

/// The store's own files, relative to the synced directory, when the store lives inside it.
///
/// A SQLite store is a file plus its `-wal`, `-shm` and `-journal` siblings, and `-wal` is
/// text-shaped enough that `--ext '*'` offered it for import. None of the four is a document,
/// and writing one from the store would corrupt the store this sync is reading.
pub fn store_rels(store: &str, dir: &Path) -> Vec<String> {
    let crate::config::StoreUrl::Sqlite(path) = crate::config::parse_store(store) else {
        return Vec::new();
    };
    // Compared as canonical paths: `-s ./kb.db` and `-s /abs/kb.db` name the same file, and the
    // directory may be reached through a link.
    let file = std::fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
    let root = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let Ok(rel) = file.strip_prefix(&root) else {
        return Vec::new();
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    ["", "-wal", "-shm", "-journal"].iter().map(|suffix| format!("{rel}{suffix}")).collect()
}

/// Whether a file found only on disk is taken in: one of `exts`, and not inside one of
/// [`SKIP_DIRS`] (the same files `import` reads).
fn eligible(rel: &str, exts: &[String]) -> bool {
    let mut segs: Vec<&str> = rel.split('/').collect();
    let name = segs.pop().unwrap_or("");
    if segs.iter().any(|s| SKIP_DIRS.contains(s)) {
        return false;
    }
    // Asset pointers are documents whatever the extensions (and are not part of the rules).
    crate::assets::pointer::is_asset_pointer(name)
        || exts.iter().any(|e| e == "*")
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
    /// Directories entered.
    dirs: Vec<String>,
    /// Directories not entered (`.git`, `node_modules`, …): content all the same.
    skipped: Vec<String>,
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
                    w.dirs.push(rel.clone());
                    pending.push((entry.path(), rel));
                } else {
                    w.skipped.push(rel);
                }
            } else if kind.is_file() {
                w.files.insert(rel, on_disk(&entry.metadata()?));
            }
        }
    }
    Ok(w)
}

/// `DIR/.textdb`, created if it is not there, ignoring itself.
///
/// Everything textdb keeps beside a synced directory lives here: the lock, the staging files and
/// the trash. It carries a `.gitignore` of `*` so a checkout stays clean without the user adding
/// anything to theirs — the same trick git's own tooling uses for generated directories.
pub fn textdb_dir(root: &Path) -> std::io::Result<PathBuf> {
    let dir = root.join(".textdb");
    std::fs::create_dir_all(&dir)?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        // Best effort: a read-only directory is not a reason to fail the sync.
        let _ = std::fs::write(&ignore, "# textdb's own files; not yours to track.\n*\n");
    }
    Ok(dir)
}

/// Write `rel` whole, or not at all.
///
/// The bytes go to a staging file and are renamed over the target, as git does with its `.lock`
/// files: a reader never sees half a note, and a sync killed mid-write — a hook that timed out,
/// say — leaves the old content rather than an empty file. Truncating in place was worth one
/// thing, that the file kept its permissions; those are copied onto the staging file instead.
fn write_disk(root: &Path, rel: &str, bytes: &[u8]) -> std::io::Result<()> {
    refuse_protected(rel)?;
    refuse_link(root, rel)?;
    refuse_short_alias(root, rel)?;
    let target = root.join(rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = textdb_dir(root)?.join("tmp");
    std::fs::create_dir_all(&staging)?;
    // Unique per process and call: two syncs of different directories may share a root, and one
    // sync writes many files.
    let tmp = staging.join(format!("{}-{}.tdbtmp", std::process::id(), now_ns()));
    let done = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        // Durable before the rename, so a crash cannot leave the name pointing at empty bytes.
        file.sync_all()?;
        drop(file);
        if let Ok(meta) = std::fs::metadata(&target) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &target)
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// Delete a file, then the directories it leaves empty (never `root`).
fn remove_disk(root: &Path, rel: &str) -> std::io::Result<()> {
    refuse_protected(rel)?;
    refuse_link(root, rel)?;
    let file = root.join(rel);
    if let Err(e) = std::fs::remove_file(&file) {
        if !clear_read_only(&file) {
            return Err(e);
        }
        std::fs::remove_file(&file)?;
    }
    prune_parents(root, rel);
    Ok(())
}

/// Remove the directories above `rel` that are left empty, deepest first (never `root`).
fn prune_parents(root: &Path, rel: &str) {
    let mut rel = rel;
    while let Some(i) = rel.rfind('/') {
        rel = &rel[..i];
        if !remove_empty_dir(&root.join(rel)) {
            break;
        }
    }
}

/// Remove `dir` when it is empty, read-only or not.
fn remove_empty_dir(dir: &Path) -> bool {
    std::fs::remove_dir(dir).is_ok()
        || (std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none()) && clear_read_only(dir) && std::fs::remove_dir(dir).is_ok())
}

/// Move a file on disk from `from` to `to`, creating the folders it needs and removing the ones
/// it leaves empty.
fn move_disk(root: &Path, from: &str, to: &str) -> std::io::Result<()> {
    // textdb's own trash (`.textdb/trash/<time>/<file>`) is the one such folder files move into;
    // the file's own path is checked all the same.
    refuse_protected(to.strip_prefix(".textdb/trash/").and_then(|rest| rest.split_once('/')).map_or(to, |(_, file)| file))?;
    refuse_protected(from)?;
    refuse_link(root, from)?;
    refuse_link(root, to)?;
    refuse_short_alias(root, to)?;
    let target = root.join(to);
    let case_only = from != to && from.to_lowercase() == to.to_lowercase();
    if target.exists() && !(case_only && CASE_INSENSITIVE) {
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{to} exists already")));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if case_only {
        // The same file under another case: renamed through a name of its own, so the new case sticks.
        let aside = root.join(format!("{to}.{}.tdbcase", std::process::id()));
        std::fs::rename(root.join(from), &aside)?;
        std::fs::rename(&aside, &target)?;
    } else {
        std::fs::rename(root.join(from), &target)?;
    }
    prune_parents(root, from);
    Ok(())
}

/// `1 json, 2 png`: how many of `rels` have each extension, most first.
pub fn kinds<'a>(rels: impl Iterator<Item = &'a str>) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for rel in rels {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let ext = match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
            _ => "no extension".to_string(),
        };
        *counts.entry(ext).or_default() += 1;
    }
    let mut counted: Vec<(String, usize)> = counts.into_iter().collect();
    counted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counted.iter().map(|(ext, n)| format!("{n} {ext}")).collect::<Vec<_>>().join(", ")
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
pub(crate) fn base_row(root: &Path, rel: &str, version: Option<i64>, blob: String, conflict: bool) -> BaseFile {
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
    /// Files textdb does not track, moved on disk with their folder: (from, to).
    carry: Vec<(String, String)>,
    /// Real files of asset pointers deleted in the store, moved to the vault's trash before their
    /// pointer leaves disk: (file, pointer).
    asset_trash: Vec<(String, String)>,
    /// Files this directory no longer has as the asset they were: forgotten from what it last had.
    forget_had: Vec<String>,
    /// Asset pointers deleted in the store that stay on disk for now: (pointer, why).
    asset_hold: Vec<(String, String)>,
    /// The files this sync found, recorded for the next (less the ones it could not look at).
    present_now: BTreeSet<String>,
    /// Asset pointers that follow their real file, renamed on disk: (from, to).
    pointer_renames: Vec<(String, String)>,
    /// Real files set aside for a keep-both conflict: (from, to).
    conflict_copies: Vec<(String, String)>,
}

/// What pairing asset pointers with their real files adds to a plan.
#[derive(Default)]
struct AssetPairs {
    carry: Vec<(String, String)>,
    trash: Vec<(String, String)>,
    renames: Vec<(String, String)>,
    copies: Vec<(String, String)>,
    forget: Vec<String>,
    hold: Vec<(String, String)>,
    /// Files that may be an asset's new name but could not be hashed: not recorded as present, so
    /// the next sync looks at them again.
    unsure: HashSet<String>,
    /// Asset files gone since the last sync that wait on those: still recorded as present.
    still_present: Vec<String>,
}

/// Pair asset pointers with their real files (docs/assets.md, "Sync"): a pointer moved in the
/// store takes its file along; one deleted there sends its file to the vault's trash when it is
/// the bytes the pointer named; a file renamed on disk takes its pointer along (matched by
/// content, one to one); and a file changed here whose pointer names other new bytes in the store
/// is set aside as a conflict copy, never overwritten.
#[allow(clippy::too_many_arguments)]
fn pair_assets(
    sides: &mut Sides,
    dir: &Path,
    plan: &Plan,
    walked: &Walk,
    base: &BTreeMap<String, Base>,
    heads: &BTreeMap<String, FileHead>,
    untracked: &[&String],
    carry: &[(String, String)],
    report: &mut Report,
) -> Result<AssetPairs> {
    use crate::assets::classify::{Class, Classifier};
    use crate::assets::pairing;
    use crate::assets::pointer::{self, is_asset_pointer, Pointer};
    let real = |rel: &str| pointer::asset_path(rel).to_string();
    let untracked: HashSet<&str> = untracked.iter().map(|r| r.as_str()).collect();
    let carried: HashSet<&str> = carry.iter().map(|(from, _)| from.as_str()).collect();
    let free = |rel: &str| untracked.contains(rel) && !carried.contains(rel);
    let on_disk = |rel: &str| walked.files.contains_key(rel);
    // A file's name on disk: `rel`, or on Windows and macOS a name differing only in letter case.
    let folded: HashMap<String, &String> =
        if CASE_INSENSITIVE { walked.files.keys().map(|r| (r.to_lowercase(), r)).collect() } else { HashMap::new() };
    let disk_name = |rel: &str| -> Option<String> {
        if on_disk(rel) {
            Some(rel.to_string())
        } else {
            folded.get(&rel.to_lowercase()).map(|r| (*r).clone())
        }
    };
    let mut had = crate::assets::DirCache::open(dir);
    let mut pairs = AssetPairs::default();
    // Only asset files follow their pointers or go to the trash with them: a pointer anyone put in
    // the store may name a `.gitattributes` or a `.env`.
    let classifier = std::cell::OnceCell::new();
    let is_asset = |rel: &str| classifier.get_or_init(|| Classifier::load(dir, &nested_rules_files(walked))).classify(dir, rel) == Class::Asset;

    // Moved in the store: a pointer leaves disk and a new one with its id arrives.
    let mut leaving = Vec::new();
    for rel in plan.disk_delete.iter().filter(|r| is_asset_pointer(r)) {
        if let Ok(p) = Pointer::parse(&sides.disk(rel)?) {
            leaving.push((p.id.clone(), rel.clone(), p));
        }
    }
    let mut arriving = Vec::new();
    for rel in plan.to_disk.iter().filter(|r| is_asset_pointer(r) && !on_disk(r)) {
        if let Ok(p) = Pointer::parse(&sides.textdb(rel)?.0) {
            arriving.push((p.id, rel.clone()));
        }
    }
    let moved = pairing::one_to_one(leaving.iter().map(|(id, rel, _)| (id.clone(), rel.clone())), arriving);
    let moved_from: HashSet<&String> = moved.iter().map(|(from, _)| from).collect();
    for (from, to) in &moved {
        let (Some(file), target) = (disk_name(&real(from)), real(to)) else { continue };
        if !free(&file) {
            continue;
        }
        if !is_asset(&file) {
            report.kept.push(note(&file, format!("its asset pointer moved in textdb to {target}, but it is not an asset file: left here")));
            continue;
        }
        if disk_name(&target).is_some_and(|there| there != file) {
            report.kept.push(note(&file, format!("its asset pointer moved in textdb to {target}, where a file is already: left here")));
            continue;
        }
        pairs.carry.push((file, target));
    }
    // Deleted in the store: the file goes to the trash when it is the bytes the pointer named.
    for (_, rel, p) in leaving.iter().filter(|(_, rel, _)| !moved_from.contains(rel)) {
        let Some(file) = disk_name(&real(rel)) else { continue };
        if !free(&file) {
            continue;
        }
        if !is_asset(&file) {
            report.kept.push(note(&file, "its asset pointer was deleted in textdb, but it is not an asset file: kept"));
            continue;
        }
        match pointer::hash_file(&dir.join(&file)) {
            Ok((sha, _)) if sha == p.sha256 => pairs.trash.push((file, rel.clone())),
            Ok(_) => {
                report.kept.push(note(&file, "its asset pointer was deleted in textdb, and this file is not the bytes it named: kept, as a new asset"));
                pairs.forget.push(file);
            }
            // Not readable now: its pointer stays, and the next sync looks again.
            Err(e) => pairs.hold.push((rel.clone(), format!("its asset pointer was deleted in textdb, but {file} could not be read ({e}): kept until the next sync"))),
        }
    }

    // Renamed on disk: a pointer unchanged on both sides whose file is gone (in any case), and an
    // asset file of the same bytes with no pointer, one each. What this directory last had tells a
    // rename here (it had the pointer's file, not the other) from a move in textdb its file did not
    // follow (it had the other file, not the pointer's), and leaves a stray copy alone.
    // Only a pointer whose file this directory had can have been renamed here, and only a file new
    // since the last sync can be its new name; a file it had whose pointer is gone can follow a
    // pointer moved in textdb earlier. A copy of anything else is left alone.
    let mut lost = Vec::new();
    for rel in plan.keep.iter().filter(|r| is_asset_pointer(r) && disk_name(&real(r)).is_none()) {
        if let Ok(p) = Pointer::parse(&sides.disk(rel)?) {
            lost.push((p.sha256, p.size, rel.clone()));
        }
    }
    // A pointer's file gone before the last sync (left unpulled on purpose, say) was not renamed.
    fn renamed_here(had: &crate::assets::DirCache, rel: &str, sha: &str) -> bool {
        had.had(rel) == Some(sha) && had.was_present(rel) != Some(false)
    }
    let renames_possible = lost.iter().any(|(sha, _, rel)| renamed_here(&had, &real(rel), sha));
    let orphans = !lost.is_empty()
        && walked.files.keys().any(|r| free(r) && !is_asset_pointer(r) && had.had(r).is_some() && !on_disk(&format!("{r}{}", pointer::SUFFIX)));
    if renames_possible || orphans {
        let store_folders = crate::assets::store_folders_inside(&mut *sides.st, dir);
        let sizes: HashSet<u64> = lost.iter().map(|(_, size, _)| *size).collect();
        let (mut found, mut unsure_sizes) = (Vec::new(), HashSet::new());
        for (rel, d) in &walked.files {
            let pointer_rel = format!("{rel}{}", pointer::SUFFIX);
            let lower = rel.to_lowercase();
            let had_file = had.had(rel).is_some();
            if !free(rel)
                || is_asset_pointer(rel)
                || pairing::is_conflict_copy(rel)
                || !sizes.contains(&(d.size as u64))
                || on_disk(&pointer_rel)
                || heads.contains_key(&pointer_rel)
                || base.contains_key(&pointer_rel)
                || (!had_file && (!renames_possible || had.was_present(rel) == Some(true)))
                || store_folders.iter().any(|f| lower.starts_with(&format!("{f}/")))
                || !is_asset(rel)
            {
                continue;
            }
            match had.sha(rel, d.size as u64, d.mtime) {
                Ok(sha) => found.push((sha.clone(), (rel.clone(), sha))),
                Err(_) => {
                    pairs.unsure.insert(rel.clone());
                    unsure_sizes.insert(d.size as u64);
                }
            }
        }
        // Until those can be read, the files of their size they may be the new names of count as
        // there, unless this sync pairs them now.
        let waiting: Vec<(u64, String)> = lost.iter().filter(|(_, size, _)| unsure_sizes.contains(size)).map(|(_, size, rel)| (*size, real(rel))).collect();
        let lost = lost.into_iter().map(|(sha, _, rel)| (sha.clone(), (rel, sha)));
        let mut paired = HashSet::new();
        for ((pointer_rel, sha), (file, _)) in pairing::one_to_one(lost, found) {
            let lost_file = real(&pointer_rel);
            let had_it = |rel: &str| had.had(rel) == Some(sha.as_str());
            match (renamed_here(&had, &lost_file, &sha), had_it(&file)) {
                (true, false) => {
                    paired.insert(lost_file);
                    pairs.renames.push((pointer_rel, format!("{file}{}", pointer::SUFFIX)));
                }
                (false, true) => pairs.carry.push((file, lost_file)),
                _ => {}
            }
        }
        pairs.still_present.extend(waiting.into_iter().map(|(_, file)| file).filter(|file| !paired.contains(file) && had.was_present(file) != Some(false)));
    }

    // Changed on both sides: the store's pointer names new bytes, and the file here is neither
    // those nor the ones the pointer on disk named.
    let host = crate::assets::host();
    let changed: Vec<String> = plan.to_disk.iter().filter(|r| is_asset_pointer(r) && on_disk(r)).cloned().collect();
    for rel in changed {
        let Some(file) = disk_name(&real(&rel)) else { continue };
        if !free(&file) || pairing::is_conflict_copy(&file) || !is_asset(&file) {
            continue;
        }
        let (Ok(old), Ok(new)) = (Pointer::parse(&sides.disk(&rel)?), Pointer::parse(&sides.textdb(&rel)?.0)) else { continue };
        if old.sha256 == new.sha256 {
            continue;
        }
        if matches!(pointer::hash_file(&dir.join(&file)), Ok((sha, _)) if sha != old.sha256 && sha != new.sha256) {
            let copy = pairing::free_conflict_copy(dir, &file, &host);
            pairs.copies.push((file, copy));
        }
    }
    Ok(pairs)
}

/// What a sync did with assets.
#[derive(Serialize, Default)]
pub struct AssetsReport {
    /// `off`, `push`, `pull` or `both`.
    pub mode: String,
    /// How many assets of the directory are in each state after the sync.
    pub counts: BTreeMap<String, usize>,
    /// Real files whose pointer moved in the store, moved on disk too, are in `carried`.
    /// Real files whose pointer was deleted in the store, moved to `trash`.
    pub trashed: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash: Option<String>,
    /// Pointers moved in the store because their real file was renamed on disk (asset paths).
    pub renamed: Vec<Move>,
    /// Real files changed here and in the store, kept under another name; the store's bytes are
    /// pulled to their path.
    pub conflict_copies: Vec<Move>,
    pub pushed: Vec<String>,
    pub pulled: Vec<String>,
    pub conflicts: Vec<String>,
    pub failed: Vec<String>,
    pub notes: Vec<String>,
}

/// Assets after the documents: pushed and pulled as the options or the store's `asset_sync`
/// setting say, the files set aside for a conflict pulled again, and how many assets are in
/// each state.
#[allow(clippy::too_many_arguments)]
fn sync_assets(st: &mut dyn Store, o: &Options, prefix: &str, plan: &Plan, attrs_changed: bool, has_pointers: bool, has_copies: bool, report: &mut Report) {
    use crate::assets::{self, PushOptions};
    if report.assets.is_none() && !has_pointers && st.asset_stores().map_or(true, |s| s.is_empty()) {
        return;
    }
    let a = report.assets.get_or_insert_with(Default::default);
    // The settings `asset_sync` (off, push, pull, both) and `asset_pull` (linked, all).
    let mode = o.assets.clone().or_else(|| st.setting("asset_sync").ok().flatten()).unwrap_or_else(|| "off".to_string());
    let v = assets::Vault { prefix: prefix.to_string(), dir: o.dir.clone() };
    if !o.dry_run {
        // A .gitattributes this very sync brought from textdb, or moved, set aside or deleted on
        // disk, is not trusted to decide what is an asset: a file anyone put in the store could name
        // `.env`, or take away the rule that kept it out. The next sync sees the rules changed.
        let attrs_arriving = plan
            .to_disk
            .iter()
            .chain(&plan.disk_delete)
            .chain(plan.merges.iter().map(|(rel, ..)| rel))
            .chain(plan.conflicts.iter().map(|(rel, ..)| rel))
            .chain(plan.moves.iter().flat_map(|(from, to)| [from, to]))
            .chain(plan.carry.iter().flat_map(|(from, to)| [from, to]))
            .chain(plan.conflict_copies.iter().flat_map(|(from, to)| [from, to]))
            .chain(plan.asset_trash.iter().map(|(file, _)| file))
            .any(|rel| rel.rsplit('/').next().is_some_and(|name| crate::assets::classify::names_dir(name, &[".gitattributes"])));
        if matches!(mode.as_str(), "push" | "both") {
            if attrs_changed || attrs_arriving {
                a.notes.push(
                    "the .gitattributes files changed since the last sync, so no assets were pushed: check `textdb assets status`, then sync with --accept-rules (or push)".to_string(),
                );
            } else {
                let push = PushOptions { to: None, message: Some("sync: assets push"), author: Some(&o.author), dry_run: false, force: false };
                match assets::push_run(st, &v, &[], &push) {
                    Ok(r) => {
                        a.pushed = r.pushed.iter().filter_map(|p| p["path"].as_str().map(str::to_string)).collect();
                        a.conflicts.extend(r.conflicts);
                        a.failed.extend(r.failed);
                    }
                    Err(e) => a.failed.push(format!("push: {}", e.message)),
                }
            }
        }
        let mut set_aside: BTreeSet<String> = plan.conflict_copies.iter().map(|(from, _)| store_path(prefix, from)).collect();
        // With those an earlier sync set a file aside for and could not pull then.
        if has_copies {
            if let Ok(earlier) = assets::not_pulled_after_conflict(st, &v) {
                set_aside.extend(earlier);
            }
        }
        let pull = matches!(mode.as_str(), "pull" | "both");
        if pull || !set_aside.is_empty() {
            let linked = if !pull {
                Ok(Some(set_aside.clone()))
            } else if st.setting("asset_pull").ok().flatten().as_deref() == Some("all") {
                Ok(None)
            } else {
                assets::linked_assets(st, prefix).map(|mut linked| {
                    linked.extend(set_aside.iter().cloned());
                    Some(linked)
                })
            };
            match linked.and_then(|linked| assets::pull_run(st, &v, &[], linked.as_ref(), false)) {
                Ok(r) => {
                    a.pulled = r.pulled.iter().filter_map(|p| p["path"].as_str().map(str::to_string)).collect();
                    a.notes.extend(r.kept);
                    a.failed.extend(r.failed);
                }
                Err(e) => a.failed.push(format!("pull: {}", e.message)),
            }
        }
    }
    match assets::state_counts(st, &v, !o.dry_run) {
        Ok(counts) => a.counts = counts,
        Err(e) => a.notes.push(format!("assets not listed: {}", e.message)),
    }
    a.mode = mode;
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
    /// Files textdb does not track, moved on disk with a folder the store moved.
    pub carried: Vec<Move>,
    /// Folders whose text files were deleted or moved in textdb but that still hold files
    /// textdb does not track.
    pub left_behind: Vec<Note>,
    /// Directories on disk that hold no files.
    pub empty_dirs: Vec<String>,
    /// Where this directory was when it was last synced, when it has moved since. The base
    /// follows the directory's id rather than its path, so a move is not a re-import.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_dir: Option<String>,
    /// Files on disk this sync did not take in, by reason: `extension`, `binary`, `ignored`.
    /// A `.csv` created after the first sync used to produce no line at all, so "why is my file
    /// not there" had no answer anywhere in the output.
    pub left_out: BTreeMap<String, Vec<String>>,
    /// Of those, removed by `--prune-empty-dirs`.
    pub removed_empty_dirs: usize,
    /// Assets, when the directory or the store has any, or an asset store is declared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assets: Option<AssetsReport>,
}

fn failure(e: &StoreError) -> String {
    if e.code == "TX001" {
        "changed in textdb while syncing; run sync again".to_string()
    } else {
        e.message.clone()
    }
}

/// The alias of the denied share a relative path falls under, if any.
///
/// `rel` is relative to the synced prefix, so for an account syncing its root the first segment
/// is the alias; syncing one share, the alias is the prefix itself and every file is under it.
fn denied_share_of(denied: &[String], rel: &str) -> Option<String> {
    denied
        .iter()
        .find(|a| rel == a.as_str() || rel.starts_with(&format!("{a}/")) || a.is_empty())
        .cloned()
}

pub fn sync(st: &mut dyn Store, o: Options, json: bool) -> Result<()> {
    let prefix = normalize_path(&o.prefix)?;
    if !o.dir.exists() && !o.dry_run {
        std::fs::create_dir_all(&o.dir).map_err(|e| StoreError::other(format!("{}: {e}", o.dir.display())))?;
    }
    // Held for the whole run, before anything is read: two syncs of one directory each compute
    // both sides from the same base and commit, so one disk edit lands twice — which is what a
    // turn-end hook in one agent and a turn-start hook in another produce. A dry run reads only,
    // so it does not queue behind a real one.
    let _lock = if o.dry_run { None } else { Some(crate::lock::acquire(&o.dir, o.lock_wait)?) };
    let key = dir_key(&o.dir);
    let repo = git::repo(&o.dir);
    if o.commit && repo.is_none() {
        return Err(StoreError::invalid(format!("--commit needs {} to be in a git checkout", o.dir.display())));
    }
    // The name this directory calls itself. Settled before the base is looked up, because a
    // directory that moved is found by it rather than by a path that has changed.
    let dir_id = crate::root::Config::read(&o.dir)?
        .map(|c| c.id)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    // Shares this connection holds but may not use. Read before anything is planned: what the
    // store no longer lists is otherwise indistinguishable from what it no longer has, and the
    // plan below turns the second into a delete on disk.
    let denied = st.denied_shares()?;
    let mut stored = find_sync_base(st, &prefix, &key)?;
    let mut moved_from: Option<String> = None;
    // A directory that was moved or renamed: the base is its own, found by the id it carries, so
    // the sync continues from where it left off instead of treating every file as new.
    if stored.is_none() {
        let moved = st
            .all_sync_bases()?
            .into_iter()
            .find(|b| b.prefix == prefix && b.dir_id.as_deref() == Some(dir_id.as_str()) && b.dir != key);
        if let Some(b) = moved {
            if !o.dry_run {
                st.rename_sync_dir(&prefix, &b.dir, &key)?;
            }
            moved_from = Some(b.dir.clone());
            stored = find_sync_base(st, &prefix, &key)?.or_else(|| Some(SyncBase { dir: key.clone(), ..b }));
        }
    }
    // A base an older build recorded under another form of the directory's name takes this one,
    // so the sync saves over it rather than next to it.
    if let Some(b) = stored.as_mut().filter(|b| b.dir != key && !o.dry_run) {
        st.rename_sync_dir(&prefix, &b.dir, &key)?;
        b.dir = key.clone();
    }
    if stored.is_some() && o.base_rev.is_some() {
        return Err(StoreError::invalid(format!(
            "{prefix} has been synced with {key} before and has a base already; --base is for the first sync only"
        )));
    }
    let generation = stored.as_ref().map_or(0, |b| b.generation);
    let heads = heads_by_rel(st, &prefix)?;
    let mut report = Report {
        prefix: prefix.clone(),
        dir: key.clone(),
        dry_run: o.dry_run,
        first_sync: stored.is_none(),
        moved_dir: moved_from,
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
    // In letter case too where the file system ignores it: `LINK/x` goes through the link `link`.
    let fold = |s: &str| if CASE_INSENSITIVE { s.to_lowercase() } else { s.to_string() };
    let links_folded: Vec<String> = walked.links.iter().map(|l| fold(l)).collect();
    let under_link = |rel: &str| {
        let rel = fold(rel);
        links_folded.iter().any(|l| rel == *l || rel.strip_prefix(l.as_str()).is_some_and(|r| r.starts_with('/')))
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
    // Paths deleted or moved away in the store since the last sync: a file there now may be another
    // with the same version number, so its content tells.
    let mut gone_since: HashSet<String> = HashSet::new();
    if let Some(b) = &stored {
        const PAGE: i64 = 10_000;
        let mut since = b.seq;
        loop {
            let page = sides.st.feed(since, PAGE)?;
            for c in &page {
                match c.op.as_str() {
                    "delete" => drop(gone_since.insert(c.path.clone())),
                    "move" => gone_since.extend(c.old_path.clone()),
                    _ => {}
                }
            }
            match page.last() {
                Some(last) if page.len() as i64 == PAGE => since = last.seq,
                _ => break,
            }
        }
    }
    // The path, or a folder it is in, went away.
    let recreated = |rel: &str| {
        if gone_since.is_empty() {
            return false;
        }
        let path = store_path(&prefix, rel);
        let mut at = path.as_str();
        loop {
            if gone_since.contains(at) {
                return true;
            }
            match at.rfind('/') {
                Some(i) if i > 0 => at = &at[..i],
                _ => return false,
            }
        }
    };
    // `.textdbignore` in the directory, in .gitignore syntax: what it matches is not synced either
    // way. Rules that are there but cannot be read stop the sync: nothing they keep out is written.
    let ignore_path = o.dir.join(IGNORE_FILE);
    let unreadable = |why: String| StoreError::invalid(format!("sync stopped before writing anything: {} could not be read ({why})", ignore_path.display()));
    let mut ignore_text = match std::fs::read(&ignore_path) {
        // Text read another way than it was written would be other rules than the user's.
        Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| unreadable("it is not UTF-8 text: save it as UTF-8".to_string()))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(unreadable(e.to_string())),
    };
    // Once per synced folder and directory the default lines go in: a new file, or the ones a file
    // has no say on yet. Lines deleted after that stay deleted. The file itself is written with the
    // sync's changes, only when the sync goes ahead.
    let mut ignore_seeded = stored
        .as_ref()
        .and_then(|b| b.rules.as_deref())
        .and_then(|r| serde_json::from_str::<Rules>(r).ok())
        .is_some_and(|r| r.ignore_seeded);
    // The text to write, and what the file held when read.
    let mut ignore_write: Option<(String, Option<String>)> = None;
    if !ignore_seeded {
        ignore_seeded = true;
        // Only where Obsidian is: a Rust repository does not need three lines about plugins, and
        // the file they were written into stayed untracked for no reason. The store counts as
        // well as the disk — a sync into an empty directory would otherwise write out the very
        // plugin code these lines exist to keep from being written.
        let vault_rel = |rel: &&String| rel.starts_with(".obsidian/") || rel.contains("/.obsidian/");
        let obsidian = o.dir.join(".obsidian").is_dir()
            || heads.keys().any(|r| vault_rel(&r))
            || walked.files.keys().any(|r| vault_rel(&r));
        if let Some(add) = obsidian
            .then(|| default_ignore_additions(ignore_text.as_deref().unwrap_or("").trim_start_matches('\u{feff}')))
            .flatten()
        {
            let mut text = ignore_text.clone().unwrap_or_default();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&add);
            ignore_write = Some((text.clone(), ignore_text.clone()));
            ignore_text = Some(text);
        }
    }
    let (ignore, ignore_notes) = match &ignore_text {
        Some(text) => ignore_rules(&o.dir, text.trim_start_matches('\u{feff}')),
        None => (None, Vec::new()),
    };
    if ignore_text.is_some() && ignore.is_none() {
        return Err(StoreError::invalid(format!("sync stopped before writing anything: {} could not be read ({})", ignore_path.display(), ignore_notes.join("; "))));
    }
    report.skipped.extend(ignore_notes.into_iter().map(|e| note(IGNORE_FILE, format!("not read: {e}"))));
    let ignored = |rel: &str| ignore.as_ref().is_some_and(|gi| ignored_by(gi, rel));
    let store_rels = store_rels(&o.store_file, &o.dir);
    if !store_rels.is_empty() {
        // Allowed, since a store beside its notes is a reasonable thing to want, but said once:
        // a reader wondering why `kb.db` is not in the listing has an answer.
        report.skipped.push(note(&store_rels[0], "the store itself, inside the directory it syncs: left out of the sync"));
    }

    let mut plan = Plan::default();
    let all: BTreeSet<String> = base.keys().chain(heads.keys()).chain(walked.files.keys()).cloned().collect();
    for rel in all {
        if under_link(&rel) {
            plan.hold.push(rel);
            continue;
        }
        // In a folder sync never writes into, moves out of or deletes in, or left out by
        // `.textdbignore`: nothing there is synced either way, whether textdb, the last sync or only
        // the disk has it (a submodule's `.git` file under its short name, a plugin deleted in
        // textdb, a plugin taken in from disk).
        // The store's own file is never a document, whichever way it would travel: taking it in
        // would version the database this sync is reading, and writing it from the store would
        // corrupt it. `--ext '*'` used to offer `kb.db-wal`, which is text-shaped enough to pass.
        let is_store = store_rels.iter().any(|s| s == &rel);
        let protected = protected_rel(&rel) || is_store;
        if protected || ignored(&rel) {
            // Gone from both sides: the last sync's record of it goes too.
            if !heads.contains_key(&rel) && !walked.files.contains_key(&rel) {
                continue;
            }
            if heads.contains_key(&rel) || base.contains_key(&rel) {
                let why = if is_store {
                    "the store's own file: not synced"
                } else if rel.split('/').next().is_some_and(|s| crate::assets::classify::names_dir(s, &[IGNORE_FILE])) {
                    "the directory's own .textdbignore, which sync never writes from textdb: not synced"
                } else if protected {
                    "inside a folder sync never writes into (.git, .textdb, node_modules, …): not synced"
                } else {
                    "left out by .textdbignore: not synced"
                };
                report.skipped.push(note(&rel, why));
            }
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
                    } else {
                        // Only files in folders sync reads at all: a `node_modules` tree is not
                        // something a user is waiting to see synced.
                        let segs: Vec<&str> = rel.split('/').collect();
                        if !segs.iter().any(|s| SKIP_DIRS.contains(s)) {
                            report.left_out.entry("extension".to_string()).or_default().push(rel);
                        }
                    }
                }
                (Some(_), Some(_)) => {
                    let (tb, tv) = sides.textdb(&rel)?;
                    let db = sides.disk(&rel)?;
                    let binary = |bytes: &[u8]| bytes.iter().take(8000).any(|&c| c == 0);
                    if tb == db {
                        plan.adopt.push((rel, tv));
                    } else if binary(&db)
                        || binary(&tb)
                        || crate::assets::classify::Classifier::defaults().rule_class(&rel) == crate::assets::classify::Class::Asset
                    {
                        // Conflict markers would destroy a binary: the file on disk stays as it is.
                        report.skipped.push(note(&rel, "a binary file on disk has the name of a file in textdb: left as it is"));
                        plan.hold.push(rel);
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
                // A file deleted or moved away and made again at the same path starts again at
                // version 1: its content tells.
                Some(v) if t.version == v && recreated(&rel) => !b.matches(&sides.textdb(&rel)?.0),
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
                // A file that is binary on disk now is not merged: conflict markers would destroy it,
                // and merged bytes would pass it into the store. Both sides keep what they have.
                let disk_bytes = sides.disk(&rel)?;
                if disk_bytes.iter().take(8000).any(|&c| c == 0) {
                    let utf16 = disk_bytes.starts_with(&[0xff, 0xfe]) || disk_bytes.starts_with(&[0xfe, 0xff]);
                    report.skipped.push(note(&rel, binary_note(utf16)));
                    plan.hold.push(rel);
                    continue;
                }
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
            // Gone from the store and unchanged here — unless it is under a share this account
            // holds but may not use. A revoked share's files disappear from every listing, which
            // looks exactly like a delete; deleting them would empty a vault because someone
            // changed a permission. Forbidden is left alone, and said out loud.
            (None, Some(false)) if denied_share_of(&denied, &rel).is_some() => {
                let alias = denied_share_of(&denied, &rel).unwrap_or_default();
                report.kept.push(note(&rel, &format!("{alias}/ is no longer shared with you; left alone")));
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

    // A name the store changed only in letter case is the same file here on Windows and macOS: it
    // is renamed in place (with its real file, for an asset pointer), never written and then deleted.
    let mut case_renames: Vec<(String, String)> = Vec::new();
    if CASE_INSENSITIVE {
        let arriving: HashMap<String, String> =
            plan.to_disk.iter().filter(|r| !walked.files.contains_key(*r)).map(|r| (r.to_lowercase(), r.clone())).collect();
        plan.disk_delete.retain(|old| match arriving.get(&old.to_lowercase()) {
            Some(new) if new != old => {
                case_renames.push((old.clone(), new.clone()));
                false
            }
            _ => true,
        });
        for (old, new) in case_renames.clone() {
            use crate::assets::pointer::{asset_path, is_asset_pointer};
            if is_asset_pointer(&old) && walked.files.contains_key(asset_path(&old)) {
                case_renames.push((asset_path(&old).to_string(), asset_path(&new).to_string()));
            }
        }
    }
    if repo.is_some() && !plan.candidates.is_empty() {
        let ignored = git::ignored(&o.dir, &plan.candidates);
        plan.candidates.retain(|rel| !ignored.contains(rel));
    }
    // Files the asset rules (or their bytes) make assets belong to the asset store, never taken in
    // as documents, whatever `--ext` would take.
    if !plan.candidates.is_empty() {
        use crate::assets::classify::{Class, Classifier};
        let nested = nested_rules_files(&walked);
        let classifier = Classifier::load(&o.dir, &nested);
        plan.candidates.retain(|rel| classifier.classify(&o.dir, rel) != Class::Asset);
    }
    let has_pointers = heads.keys().chain(walked.files.keys()).any(|r| crate::assets::pointer::is_asset_pointer(r));
    let has_copies = walked.files.keys().any(|r| crate::assets::pairing::is_conflict_copy(r));
    let mut rules = {
        let mut exts = o.exts.clone();
        exts.sort();
        exts.dedup();
        Rules {
            exts,
            skip_dirs: SKIP_DIRS.iter().map(|s| s.to_string()).collect(),
            ignore_file: ignore_text.as_deref().map(|t| blob_id(t.as_bytes())),
            ignore_text: ignore_text.clone(),
            ignore_seeded,
            gitattributes: Some(gitattributes_id(&o.dir, &walked)),
            left_out: Some(left_out_digest(&report.left_out)),
        }
    };
    // Say what was walked past when the set has changed — a `.csv` added after the first sync
    // must not go unmentioned — and not on every run of a hook over a code directory, where the
    // same 1200 files are left out every time and the line is pure noise.
    let said_before = stored.as_ref().and_then(|b| b.rules.as_deref()).and_then(|r| serde_json::from_str::<Rules>(r).ok());
    if said_before.as_ref().and_then(|r| r.left_out.clone()).as_deref() == rules.left_out.as_deref() {
        report.left_out.clear();
    }
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
    // A binary file (a NUL byte in its first 8000 bytes, as git tells) is not text for the store:
    // it stays on disk, and the store keeps what it had, with a word on what to do about it.
    let mut binary = Vec::new();
    for (rel, _) in &plan.to_textdb {
        let bytes = sides.disk(rel)?;
        if bytes.iter().take(8000).any(|&c| c == 0) {
            binary.push((rel.clone(), bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff])));
        }
    }
    if !binary.is_empty() {
        plan.to_textdb.retain(|(rel, _)| !binary.iter().any(|(b, _)| b == rel));
        for (rel, utf16) in binary {
            report.skipped.push(note(&rel, binary_note(utf16)));
            plan.hold.push(rel);
        }
    }

    // Files textdb does not track (images, JSON, …) go with their folder when the store moved it:
    // every tracked file of the folder left for the same new place, which does not have them yet.
    let taken: HashSet<String> = plan.to_textdb.iter().map(|(rel, _)| rel.clone()).collect();
    let untracked: Vec<&String> = walked
        .files
        .keys()
        .filter(|rel| !base.contains_key(*rel) && !heads.contains_key(*rel) && !taken.contains(*rel) && !under_link(rel))
        .collect();
    let new_on_disk_rels: HashSet<&String> = plan.to_disk.iter().filter(|rel| !walked.files.contains_key(*rel)).collect();
    let deleted: HashSet<&String> = plan.disk_delete.iter().collect();
    let mut carry: Vec<(String, String)> = Vec::new();
    if !plan.disk_delete.is_empty() && !untracked.is_empty() {
        let mut new_by_blob: HashMap<String, Vec<&String>> = HashMap::new();
        for rel in &new_on_disk_rels {
            new_by_blob.entry(blob_id(&sides.textdb(rel)?.0)).or_default().push(*rel);
        }
        let mut maps: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut paired: HashSet<&String> = HashSet::new();
        for old in &plan.disk_delete {
            let Some(b) = base.get(old) else { continue };
            let name = old.rsplit('/').next();
            let Some(new) = new_by_blob.get(&b.blob).and_then(|c| c.iter().find(|n| n.rsplit('/').next() == name).copied()) else {
                continue;
            };
            let (o, n): (Vec<&str>, Vec<&str>) = (old.split('/').collect(), new.split('/').collect());
            let common = o.iter().rev().zip(n.iter().rev()).take_while(|(a, b)| a == b).count().min(o.len() - 1);
            if common == 0 || n.len() == common {
                continue;
            }
            let (od, nd) = (o[..o.len() - common].join("/"), n[..n.len() - common].join("/"));
            paired.insert(old);
            let entry = maps.entry(od).or_insert_with(|| Some(nd.clone()));
            if entry.as_deref() != Some(nd.as_str()) {
                *entry = None;
            }
        }
        for (od, nd) in &maps {
            let Some(nd) = nd else { continue };
            let inside = format!("{od}/");
            let whole = base.keys().filter(|r| r.starts_with(&inside)).all(|r| paired.contains(r) || !walked.files.contains_key(r));
            if !whole {
                continue;
            }
            for rel in untracked.iter().filter(|r| r.starts_with(&inside)) {
                let to = format!("{nd}/{}", &rel[inside.len()..]);
                let taken_in_other_case = CASE_INSENSITIVE && walked.files.keys().any(|k| k.to_lowercase() == to.to_lowercase());
                if !walked.files.contains_key(&to) && !new_on_disk_rels.contains(&&to) && !taken_in_other_case {
                    carry.push((rel.to_string(), to));
                }
            }
        }
    }
    let mut pairs = pair_assets(&mut sides, &o.dir, &plan, &walked, &base, &heads, &untracked, &carry, &mut report)?;
    carry.extend(std::mem::take(&mut pairs.carry));
    carry.extend(case_renames);
    // Nothing is trashed, renamed or set aside in what `.textdbignore` leaves out.
    pairs.trash.retain(|(file, pointer)| !ignored(file) && !ignored(pointer));
    pairs.renames.retain(|(from, to)| !ignored(from) && !ignored(to));
    pairs.copies.retain(|(from, to)| !ignored(from) && !ignored(to));
    // Folders whose tracked files all leave disk but that keep untracked ones.
    let carried: HashSet<&str> = carry.iter().map(|(from, _)| from.as_str()).chain(pairs.trash.iter().map(|(file, _)| file.as_str())).collect();
    let mut emptied: BTreeSet<String> = BTreeSet::new();
    for rel in &plan.disk_delete {
        let mut d = rel.as_str();
        while let Some(i) = d.rfind('/') {
            d = &d[..i];
            if emptied.contains(d) {
                break;
            }
            let inside = format!("{d}/");
            let stays = walked
                .files
                .keys()
                .any(|r| r.starts_with(&inside) && !deleted.contains(r) && (base.contains_key(r) || heads.contains_key(r) || taken.contains(r)))
                || plan.to_disk.iter().any(|r| r.starts_with(&inside));
            if stays {
                break;
            }
            emptied.insert(d.to_string());
        }
    }
    for d in &emptied {
        if emptied.iter().any(|up| d.starts_with(&format!("{up}/"))) {
            continue;
        }
        let inside = format!("{d}/");
        let left: Vec<&str> = untracked.iter().map(|r| r.as_str()).filter(|r| r.starts_with(&inside) && !carried.contains(r)).collect();
        if !left.is_empty() {
            let (noun, verb) = if left.len() == 1 { ("file", "stays") } else { ("files", "stay") };
            report.left_behind.push(note(
                d,
                format!(
                    "{} {noun} textdb does not track {verb} here ({}), though its text files were deleted or moved in textdb",
                    left.len(),
                    kinds(left.iter().copied())
                ),
            ));
        }
    }
    // Directories that hold nothing, and will not once sync has written and deleted files.
    let gone: HashSet<&str> = plan.disk_delete.iter().map(String::as_str).chain(carried.iter().copied()).collect();
    let mut holds: HashSet<String> = HashSet::new();
    let mut emptied_by_sync: HashSet<String> = HashSet::new();
    let ancestors = |set: &mut HashSet<String>, rel: &str| {
        let mut d = rel;
        while let Some(i) = d.rfind('/') {
            d = &d[..i];
            if !set.insert(d.to_string()) {
                break;
            }
        }
    };
    for rel in walked.files.keys().filter(|r| !gone.contains(r.as_str())).chain(walked.links.iter()).chain(walked.skipped.iter()) {
        ancestors(&mut holds, rel);
    }
    for rel in plan.to_disk.iter().chain(plan.conflicts.iter().map(|c| &c.0)).chain(carry.iter().map(|(_, to)| to)) {
        ancestors(&mut holds, rel);
    }
    for rel in &gone {
        ancestors(&mut emptied_by_sync, rel);
    }
    // A build tree or an ignored folder holds no documents by design, so counting its empty
    // directories — and recommending --prune-empty-dirs, which would delete Cargo's output —
    // was advice to act on the one thing sync must not touch.
    report.empty_dirs = walked
        .dirs
        .iter()
        .filter(|d| !holds.contains(*d) && !emptied_by_sync.contains(*d) && !protected_rel(d) && !ignored(d))
        .cloned()
        .collect();
    report.empty_dirs.sort();
    plan.carry = carry.into_iter().filter(|(from, to)| !ignored(from) && !ignored(to)).collect();
    plan.keep.retain(|rel| !pairs.renames.iter().any(|(from, _)| from == rel));
    plan.present_now = walked.files.keys().filter(|rel| !pairs.unsure.contains(*rel)).cloned().chain(pairs.still_present.iter().cloned()).collect();
    (plan.asset_trash, plan.pointer_renames, plan.conflict_copies, plan.forget_had, plan.asset_hold) =
        (pairs.trash, pairs.renames, pairs.copies, pairs.forget, pairs.hold);

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
        // Files moving away (a name changed only in case among them) leave their names free.
        let mut after: Vec<String> = walked
            .files
            .keys()
            .filter(|r| !deleted.contains(r) && !plan.carry.iter().any(|(from, _)| from == *r))
            .cloned()
            .collect();
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
    report.carried = plan.carry.iter().map(|(from, to)| Move { from: from.clone(), to: to.clone() }).collect();
    if !(plan.asset_trash.is_empty() && plan.pointer_renames.is_empty() && plan.conflict_copies.is_empty()) {
        use crate::assets::pointer::asset_path;
        let a = report.assets.get_or_insert_with(Default::default);
        a.trashed = plan.asset_trash.iter().map(|(file, _)| file.clone()).collect();
        a.renamed = plan.pointer_renames.iter().map(|(from, to)| Move { from: asset_path(from).to_string(), to: asset_path(to).to_string() }).collect();
        a.conflict_copies = plan.conflict_copies.iter().map(|(from, to)| Move { from: from.clone(), to: to.clone() }).collect();
    }
    report.merged = plan.merges.iter().map(|m| m.0.clone()).collect();
    report.conflicts = plan.conflicts.iter().map(|c| c.0.clone()).collect();
    report.unchanged = plan.keep.len() + plan.adopt.len();

    // Include rules that changed since the last sync must not take in files unnoticed.
    // Changed .gitattributes files are not taken for pushes until accepted (--accept-rules): until
    // then the base keeps the id it had.
    let attrs_before = stored
        .as_ref()
        .and_then(|b| b.rules.as_deref())
        .and_then(|r| serde_json::from_str::<Rules>(r).ok())
        .and_then(|b| b.gitattributes);
    let attrs_changed = !o.accept_rules && attrs_before.as_ref().is_some_and(|before| rules.gitattributes.as_ref() != Some(before));
    if attrs_changed {
        rules.gitattributes = attrs_before;
    }
    if let Some(stored) = &stored {
        let before: Option<Rules> = stored.rules.as_deref().and_then(|r| serde_json::from_str(r).ok());
        if !before.as_ref().is_some_and(|b| b.same(&rules)) {
            // What the last sync's `.textdbignore` left out, taken in or written now, counts too.
            let before_ignore = before.as_ref().and_then(|b| b.ignore_text.as_deref()).and_then(|t| ignore_rules(&o.dir, t.trim_start_matches('\u{feff}')).0);
            let was_ignored = |rel: &str| before_ignore.as_ref().is_some_and(|gi| ignored_by(gi, rel));
            let left_out = |rel: &str| {
                let dirs: Vec<&str> = rel.split('/').rev().skip(1).collect();
                match &before {
                    None => dirs.iter().any(|s| s.starts_with('.') || *s == "node_modules"),
                    Some(b) => dirs.iter().any(|s| b.skip_dirs.iter().any(|d| d == s)) || !eligible(rel, &b.exts) || was_ignored(rel),
                }
            };
            let mut newly_included: Vec<String> = plan
                .to_textdb
                .iter()
                .filter(|(rel, v)| v.is_none() && !heads.contains_key(rel) && !base.contains_key(rel) && left_out(rel))
                .map(|(rel, _)| rel.clone())
                .chain(
                    plan.to_disk
                        .iter()
                        .chain(plan.to_textdb.iter().map(|(rel, _)| rel))
                        .chain(plan.moves.iter().flat_map(|(from, to)| [from, to]))
                        .chain(plan.merges.iter().map(|(rel, ..)| rel))
                        .chain(plan.conflicts.iter().map(|(rel, ..)| rel))
                        .chain(&plan.disk_delete)
                        .chain(&plan.textdb_delete)
                        .chain(plan.carry.iter().chain(&plan.pointer_renames).chain(&plan.conflict_copies).flat_map(|(from, to)| [from, to]))
                        .chain(plan.asset_trash.iter().map(|(file, _)| file))
                        .filter(|rel| was_ignored(rel))
                        .cloned(),
                )
                .collect();
            newly_included.sort();
            newly_included.dedup();
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

    // The default lines for `.textdbignore`, written first when the sync goes ahead (never through
    // a link); if they cannot be, they are tried again next time.
    if let Some((text, before)) = ignore_write.as_ref().filter(|_| !report.stopped && !report.stopped_by_rules) {
        let written = if o.dry_run {
            Ok(())
        } else if std::fs::symlink_metadata(&ignore_path).is_ok_and(|m| m.file_type().is_symlink()) {
            Err(std::io::Error::other("it is a link, which sync never writes through"))
        } else {
            let now = std::fs::read(&ignore_path);
            let unchanged = match (&now, before) {
                (Ok(bytes), Some(b)) => bytes.as_slice() == b.as_bytes(),
                (Err(e), None) => e.kind() == std::io::ErrorKind::NotFound,
                _ => false,
            };
            match now {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ if !unchanged => Err(std::io::Error::other("it changed during the sync")),
                _ => std::fs::create_dir_all(&o.dir).and_then(|()| std::fs::write(&ignore_path, text)),
            }
        };
        match written {
            Ok(()) if before.is_none() => report.to_disk.new.push(IGNORE_FILE.to_string()),
            Ok(()) => report.to_disk.changed.push(IGNORE_FILE.to_string()),
            Err(e) => {
                report.skipped.push(note(IGNORE_FILE, format!("the default lines could not be added ({e}): used for this sync, and tried again next time")));
                rules.ignore_seeded = false;
            }
        }
    }
    if !report.stopped && !report.stopped_by_rules && !o.dry_run {
        apply(
            &mut sides,
            &o,
            &plan,
            &base,
            &stored_rows,
            &heads,
            &changes,
            &key,
            from_commit.as_deref(),
            &mut report,
            &rules,
            generation,
            &dir_id,
        )?;
    }
    if !report.stopped && !report.stopped_by_rules {
        sync_assets(&mut *sides.st, &o, &prefix, &plan, attrs_changed, has_pointers, has_copies, &mut report);
    }
    let asset_failures = report.assets.as_ref().map_or(0, |a| a.failed.len());
    let asset_conflicts = report.assets.as_ref().map_or(0, |a| a.conflicts.len());
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
        if asset_failures > 0 {
            std::process::exit(1);
        }
        if asset_conflicts > 0 {
            std::process::exit(3);
        }
        return Ok(());
    }
    print_report(&report, o.quiet)?;
    let blocking = report.problems.iter().filter(|p| p.blocking).count();
    if report.stopped {
        return Err(StoreError::invalid(format!(
            "sync stopped before writing anything: {blocking} {} cannot be written on this computer; rename {} in the store",
            if blocking == 1 { "name" } else { "names" },
            if blocking == 1 { "it" } else { "them" }
        )));
    }
    // A sync that wrote anything is a bulk load; let the store get ready before the error
    // checks below can return early, since those still leave the writes in place.
    if !o.dry_run {
        crate::settle(sides.st);
    }
    if rules_stop {
        let n = report.rules.as_ref().map_or(0, |r| r.newly_included.len());
        return Err(StoreError::invalid(format!(
            "sync stopped before writing anything: the include rules changed since the last sync, and {n} {} they left out \
             would be synced now (listed above). Pass --accept-rules to go ahead, or list them in .textdbignore",
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
    if asset_failures > 0 {
        return Err(StoreError::other(format!("synced, but {asset_failures} assets could not be pushed or pulled (listed above)")));
    }
    if asset_conflicts > 0 {
        return Err(StoreError::conflict(format!("synced, but {asset_conflicts} assets were not pushed because of conflicts (listed above)")));
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
    // What the base was at when this sync read it; saving checks it has not moved since.
    generation: i64,
    // The id this directory calls itself, recorded with the base so a move does not lose it.
    dir_id: &str,
) -> Result<()> {
    let prefix = sides.prefix.clone();
    let dir = o.dir.as_path();
    let author = o.author.as_str();
    let mut rows: BTreeMap<String, BaseFile> = BTreeMap::new();
    let mut had = crate::assets::DirCache::open(dir);
    // The files there after this sync: those found, as they move.
    let mut present = plan.present_now.clone();
    let mut moved = |from: &str, to: &str| {
        if present.remove(from) {
            present.insert(to.to_string());
        }
    };
    for rel in &plan.forget_had {
        had.forget(rel);
    }
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
                if crate::assets::pointer::is_asset_pointer(from) {
                    had.moved(crate::assets::pointer::asset_path(from), crate::assets::pointer::asset_path(to));
                }
            }
            Err(e) => failed(report, &mut rows, from, failure(&e)),
        }
    }
    // A pointer follows its real file, renamed on disk; the links to it were rewritten there.
    for (from, to) in &plan.pointer_renames {
        match sides.st.mv(&store_path(&prefix, from), &store_path(&prefix, to), Some(author), Some(&format!("sync: its file was renamed in {key}"))) {
            Ok(()) => match move_disk(dir, from, to) {
                Ok(()) => {
                    rows.insert(to.clone(), base_row(dir, to, heads.get(from).map(|h| h.version), base[from].blob.clone(), false));
                    had.moved(crate::assets::pointer::asset_path(from), crate::assets::pointer::asset_path(to));
                    moved(from, to);
                }
                Err(e) => failed(report, &mut rows, from, e.to_string()),
            },
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
        match sides.st.rm(&store_path(&prefix, rel), Some(author), Some(&format!("sync: deleted in {key}"))) {
            Err(e) => failed(report, &mut rows, rel, failure(&e)),
            Ok(()) if crate::assets::pointer::is_asset_pointer(rel) => had.forget(crate::assets::pointer::asset_path(rel)),
            Ok(()) => {}
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
    // A pointer whose file goes to the trash leaves disk after it, below; one held stays.
    for (pointer_rel, why) in &plan.asset_hold {
        failed(report, &mut rows, pointer_rel, why.clone());
    }
    for rel in plan
        .disk_delete
        .iter()
        .filter(|rel| !plan.asset_trash.iter().any(|(_, pointer)| pointer == *rel) && !plan.asset_hold.iter().any(|(pointer, _)| pointer == *rel))
    {
        if let Err(e) = remove_disk(dir, rel) {
            failed(report, &mut rows, rel, e.to_string());
        }
    }
    for (from, to) in &plan.carry {
        match move_disk(dir, from, to) {
            Ok(()) => {
                had.moved(from, to);
                moved(from, to);
            }
            Err(e) => report.failed.push(note(from, format!("not moved to {to} with its folder: {e}"))),
        }
    }
    // A pointer deleted in textdb leaves disk only once its file is in the trash, so a move that
    // fails is tried again by the next sync rather than leaving the file to be pushed as new.
    let mut trash = None;
    for (file, pointer_rel) in &plan.asset_trash {
        let folder = trash
            .get_or_insert_with(|| {
                let now = SystemTime::now();
                let nanos = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
                format!(".textdb/trash/{}-{nanos:09}", crate::assets::driver::stamp(now))
            })
            .clone();
        match move_disk(dir, file, &format!("{folder}/{file}")) {
            Ok(()) => {
                had.forget(file);
                moved(file, &format!("{folder}/{file}"));
                if let Err(e) = remove_disk(dir, pointer_rel) {
                    failed(report, &mut rows, pointer_rel, e.to_string());
                }
            }
            Err(e) => {
                failed(report, &mut rows, pointer_rel, format!("its asset pointer was deleted in textdb, but {file} was not moved to the trash: {e}"));
                if let Some(a) = report.assets.as_mut() {
                    a.trashed.retain(|t| t != file);
                }
            }
        }
    }
    if let (Some(folder), Some(a)) = (trash, report.assets.as_mut()) {
        a.trash = Some(folder);
    }
    for (from, to) in &plan.conflict_copies {
        if let Err(e) = crate::assets::driver::rename_new(&dir.join(from), &dir.join(to)) {
            report.failed.push(note(from, format!("changed here and in textdb, but not set aside as {to}: {e}")));
        }
    }
    had.record_present(present);
    had.save();
    if o.prune_empty_dirs {
        let mut dirs = report.empty_dirs.clone();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
        report.removed_empty_dirs = dirs.iter().filter(|d| remove_empty_dir(&dir.join(d))).count();
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
            // The default lines this sync put in `.textdbignore`.
            .chain(report.to_disk.new.iter().chain(&report.to_disk.changed).filter(|rel| rel.as_str() == IGNORE_FILE))
            .filter(|rel| !not_done.contains(rel.as_str()))
            .cloned()
            .collect();
        if !paths.is_empty() {
            let message = commit_message(o, &prefix, plan, report, heads, from_commit, seq, &not_done);
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
        generation,
        dir_id: Some(dir_id.to_string()),
        files: rows.into_values().collect(),
    })?;
    // Written last, and only by a sync that got this far: the pairing this directory will be
    // found by from now on. Kept as it was when it is already right, so an id stays stable.
    let paired = crate::root::Config {
        store: crate::root::Config::record_store(&o.store_file, dir),
        prefix: prefix.clone(),
        id: dir_id.to_string(),
        created: crate::assets::driver::stamp(SystemTime::now()),
        // Normalised, so it round-trips through `parse_exts` unchanged.
        ext: Some(o.exts.join(",")),
    };
    match crate::root::Config::read(dir)? {
        Some(before) if before.store == paired.store && before.prefix == paired.prefix && before.ext == paired.ext => Ok(()),
        Some(before) => crate::root::Config { id: before.id, created: before.created, ..paired }.write(dir),
        None => paired.write(dir),
    }
}

/// `textdb sync /docs: 2 changed, 1 added` with who made the changes in the store and
/// `Textdb-*` trailers.
fn commit_message(
    o: &Options,
    prefix: &str,
    plan: &Plan,
    report: &Report,
    heads: &BTreeMap<String, FileHead>,
    from_commit: Option<&str>,
    seq: i64,
    not_done: &HashSet<&str>,
) -> String {
    let done = |rel: &&String| !not_done.contains(rel.as_str());
    // What was written, as the report has it (a file new on disk is added, `.textdbignore` too).
    let added = report.to_disk.new.iter().filter(done).count();
    let changed = report.to_disk.changed.iter().filter(done).count() + plan.merges.iter().map(|m| &m.0).filter(done).count();
    let deleted = report.to_disk.deleted.iter().filter(done).count();
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
    if parts.is_empty() {
        // Nothing else written: the default lines sync put in `.textdbignore`.
        parts.push(format!("{IGNORE_FILE} default lines"));
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

fn print_report(r: &Report, quiet: bool) -> Result<()> {
    let mut s = String::new();
    if quiet {
        // The summary line, and anything that went wrong. A hook wants one line on success and
        // the reason on failure; the file-by-file list is for a person reading along.
        for (label, notes) in [("failed", &r.failed), ("conflict", &r.conflicts.iter().map(|c| note(c, "")).collect::<Vec<_>>())] {
            for n in notes {
                s.push_str(&format!("{label:<15} {}{}{}\n", n.path, if n.reason.is_empty() { "" } else { ": " }, n.reason));
            }
        }
        for p in r.problems.iter().filter(|p| p.blocking) {
            s.push_str(&format!("{:<15} {}: {}\n", "problem", p.path, p.detail));
        }
        return summary(r, s);
    }
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
    for m in r.carried.iter().take(LIST_MAX) {
        s.push_str(&format!("{:<15} {} -> {}\n", "disk carried", m.from, m.to));
    }
    if r.carried.len() > LIST_MAX {
        s.push_str(&format!("{:<15} … and {} more\n", "disk carried", r.carried.len() - LIST_MAX));
    }
    for n in &r.left_behind {
        s.push_str(&format!("{:<15} {}/: {}\n", "left behind", n.path, n.reason));
    }
    if r.removed_empty_dirs > 0 {
        s.push_str(&format!("{:<15} removed {} directories that held no files\n", "empty dirs", r.removed_empty_dirs));
    } else if !r.empty_dirs.is_empty() {
        let shown: Vec<&str> = r.empty_dirs.iter().take(5).map(String::as_str).collect();
        let more = if r.empty_dirs.len() > 5 { ", …" } else { "" };
        s.push_str(&format!(
            "{:<15} {} directories hold no files ({}{more}); --prune-empty-dirs removes them\n",
            "empty dirs",
            r.empty_dirs.len(),
            shown.join(", ")
        ));
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
    if let Some(a) = &r.assets {
        for m in &a.renamed {
            s.push_str(&format!("{:<15} {} -> {} (its file was renamed here)\n", "asset renamed", m.from, m.to));
        }
        list(&mut s, "asset trashed", &a.trashed);
        for m in &a.conflict_copies {
            s.push_str(&format!("{:<15} {}: changed here and in textdb; this copy is kept as {}\n", "asset conflict", m.from, m.to));
        }
        list(&mut s, "asset pushed", &a.pushed);
        list(&mut s, "asset pulled", &a.pulled);
        list(&mut s, "asset conflict", &a.conflicts);
        list(&mut s, "asset failed", &a.failed);
        list(&mut s, "asset note", &a.notes);
        if !a.counts.is_empty() {
            let counts: Vec<String> = a.counts.iter().map(|(state, n)| format!("{n} {}", state.replace('-', " "))).collect();
            s.push_str(&format!("{:<15} {} (push and pull: {})\n", "assets", counts.join(", "), a.mode));
        }
    }
    summary(r, s)
}

/// The line every sync ends with, plus what it left out and the git line. Shared with `--quiet`,
/// which prints this and nothing else.
fn summary(r: &Report, mut s: String) -> Result<()> {
    // One line naming the files sync walked past and why. Without it a `.csv` added after the
    // first sync produced no output at all, and the only way to find out was to go looking.
    if let Some(from) = &r.moved_dir {
        s.push_str(&format!("{:<15} this directory was {from} at the last sync; its base came with it\n", "moved"));
    }
    for (why, rels) in &r.left_out {
        let shown: Vec<&str> = rels.iter().take(3).map(String::as_str).collect();
        let more = if rels.len() > 3 { ", …" } else { "" };
        let fix = match why.as_str() {
            "extension" => "; --ext to include them",
            "binary" => "; .textdbignore, or an asset rule in .gitattributes",
            _ => "",
        };
        s.push_str(&format!(
            "{:<15} {} {} by {why} ({}{more}){fix}\n",
            "left out",
            rels.len(),
            if rels.len() == 1 { "file" } else { "files" },
            shown.join(", ")
        ));
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
    let since_sync = find_sync_base(st, &prefix, &key)?.map(|b| {
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
    fn never_deletes_moves_or_writes_in_protected_folders_or_through_short_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dep = tmp.path().join("node_modules/p");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(dep.join("main.js"), "run()").unwrap();
        assert!(remove_disk(tmp.path(), "node_modules/p/main.js").is_err());
        assert!(move_disk(tmp.path(), "node_modules/p/main.js", "elsewhere.js").is_err());
        assert!(write_disk(tmp.path(), ".textdb/bin/textdb.cmd", b"x").is_err());
        assert!(write_disk(tmp.path(), ".textdbignore", b"").is_err());
        assert!(dep.join("main.js").exists() && !tmp.path().join("elsewhere.js").exists());
        // A name shaped like a short name is written when it is its own name, not another's.
        std::fs::write(tmp.path().join("report~1.pdf"), "a").unwrap();
        write_disk(tmp.path(), "report~1.pdf", b"b").unwrap();
        assert_eq!(std::fs::read(tmp.path().join("report~1.pdf")).unwrap(), b"b");
    }

    #[test]
    fn only_wanted_files_on_disk_are_taken_in() {
        let exts = parse_exts("md, .TXT");
        assert!(eligible("notes/a.md", &exts) && eligible("B.Txt", &exts));
        assert!(eligible(".claude/instructions/rules.md", &exts) && eligible("docs/.drafts/a.md", &exts));
        assert!(!eligible("logo.png", &exts) && !eligible("x/node_modules/a.md", &exts));
        assert!(!eligible(".git/a.md", &exts) && !eligible(".trash/a.md", &exts) && !eligible(".textdb/app/a.md", &exts));
        assert!(protected_rel(".git/hooks/post-checkout") && protected_rel("sub/.GIT/config") && protected_rel("vendor/lib/.git"));
        assert!(protected_rel(".textdb/bin/textdb.cmd") && protected_rel("web/node_modules/x.js") && protected_rel(".trash/a.md"));
        assert!(protected_rel("GIT~1/hooks/x") && protected_rel(".git./hooks/x") && protected_rel(".git::$INDEX_ALLOCATION/hooks/x"));
        assert!(!protected_rel(".gitignore") && !protected_rel("a/.github/workflows/ci.yml") && !protected_rel("notes/git/a.md"));
        assert!(!protected_rel(".obsidian/app.json") && !protected_rel("scans/report~1.pdf") && !protected_rel("photos~1/a.md"), "settings sync; other names may look like short names");
        assert!(protected_rel("sub/GIT~1") && protected_rel("NODE_M~1/x.js") && protected_rel("GI3F2A~1/hooks/x"));
        assert!(!protected_rel(".obsidian/plugins/p/main.js") && protected_rel(".textdbignore") && protected_rel(".TextDbIgnore") && !protected_rel("sub/.textdbignore"), "plugins are up to .textdbignore, which is the directory's own");
        assert!(eligible("logo.png", &parse_exts("*")));
        assert!(has_markers(b"a\n<<<<<<< textdb\nb\n=======\nc\n>>>>>>> disk\n"));
        assert!(!has_markers(b"<<<<<<< only an opening line\n"));
    }

    #[test]
    fn the_default_textdbignore_leaves_out_obsidian_code_at_any_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults = default_ignore_additions("").unwrap();
        let (gi, notes) = ignore_rules(tmp.path(), &format!("{defaults}!*.css\n"));
        assert!(notes.is_empty(), "{notes:?}");
        let gi = gi.unwrap();
        let ignored = |rel: &str| ignored_by(&gi, rel);
        assert!(ignored(".obsidian/plugins/p/main.js") && ignored("sub/vault/.obsidian/themes/t/theme.css") && ignored(".obsidian/plugins"));
        assert!(ignored(".obsidian/snippets/s.css"), "a ! line does not bring back a file inside a folder left out");
        assert!(!ignored(".obsidian/app.json") && !ignored("plugins/p/main.js") && !ignored("notes/a.md") && !ignored("a.css"));
        if CASE_INSENSITIVE {
            assert!(ignored(".Obsidian/Plugins/p/main.js"));
        }
        // Added once, only for the folders a file does not mention.
        let added = default_ignore_additions("*.tmp\n!**/.obsidian/plugins/\n").unwrap();
        assert!(!added.contains("plugins\n") && added.contains("**/.obsidian/snippets\n") && added.contains("**/.obsidian/themes\n"), "{added}");
        // Comments, the top folder only, or some files in a folder are no say on it at any depth.
        let added = default_ignore_additions("# .obsidian/themes are shared\n.obsidian/plugins/*/data.json\n/.obsidian/snippets/\n.obsidian/plugins\n").unwrap();
        assert_eq!(added.lines().filter(|l| l.starts_with("**/")).count(), 3, "{added}");
        assert!(default_ignore_additions(&defaults).is_none());
        assert!(protected_rel(".textdbignore/readme.md"));
    }
}
