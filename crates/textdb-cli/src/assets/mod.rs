//! Assets: binaries kept in an asset store, each with a pointer document where it belongs.
//! See `docs/assets.md`.
//!
//! A vault is a directory synced with a store folder. Its assets are the files the rules make
//! assets (`classify`) and the files pointers name, in the store or on disk. What this directory
//! last had of each asset (pushed, pulled, or found matching its pointer) is remembered per vault,
//! which tells the states apart:
//!
//! - `ok`: the file matches its pointer; `new`: no pointer yet; `not-pulled`: a pointer, no file;
//! - `modified`: changed here since this directory had the pointer's bytes (push publishes it);
//! - `outdated`: unchanged here, and the pointer names newer bytes (pull fetches them);
//! - `conflict`: other bytes than the pointer names, and not what this directory last had.
//!
//! `push` uploads, verifies and only then commits the pointer; `pull` downloads to a partial file,
//! checks its hash and only then puts it in place.

pub mod classify;
pub mod driver;
pub mod migrate;
pub mod pairing;
pub mod pointer;
pub mod rclone;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;
use textdb_sqlite::normalize_path;

use classify::{Class, Classifier, IGNORED_DIRS};
use driver::{AssetStore, Driver};
use ignore::gitignore::GitignoreBuilder;
use pointer::{asset_path, hash_file, is_asset_pointer, Pointer, SUFFIX};

use crate::store::{BaseFile, Store, StoreError, SyncBase};
use crate::{emit_json, out, Result};

/// A file modified this recently may change again within its timestamp's resolution, so what is
/// learnt from it is not cached (git's "racy clean" problem).
const RACY_NS: i64 = 2_000_000_000;

/// A directory on this computer and the store folder it is synced with.
pub struct Vault {
    pub prefix: String,
    pub dir: PathBuf,
}

fn under(folder: &str, path: &str) -> bool {
    folder == "/" || path == folder || path.starts_with(&format!("{folder}/"))
}

/// This computer's name, as the environment or the system gives it.
pub fn host() -> String {
    let named = |s: String| Some(s.trim().to_string()).filter(|h| !h.is_empty());
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().and_then(named))
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().and_then(named))
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| named(String::from_utf8_lossy(&o.stdout).into_owned()))
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Refuse store paths outside the vault's folder, which would match nothing.
fn check_scope(v: &Vault, scope: &[String]) -> Result<()> {
    match scope.iter().find(|p| !under(&v.prefix, p) && !under(p, &v.prefix)) {
        Some(p) => Err(StoreError::invalid(format!("{p} is not in {}, the folder {} is synced with", v.prefix, v.dir.display()))),
        None => Ok(()),
    }
}

fn store_path(prefix: &str, rel: &str) -> String {
    if prefix == "/" {
        format!("/{rel}")
    } else {
        format!("{prefix}/{rel}")
    }
}

/// `path` relative to the folder `prefix`, when it is inside it.
fn rel_under(prefix: &str, path: &str) -> Option<String> {
    let rest = if prefix == "/" { path.strip_prefix('/') } else { path.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) };
    rest.filter(|r| !r.is_empty()).map(str::to_string)
}

fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64)
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64)
}

fn io_err(what: impl std::fmt::Display, e: std::io::Error) -> StoreError {
    StoreError::other(format!("{what}: {e}"))
}

/// `1.2 MB`.
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (bytes as f64, 0);
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Whether `rel` can name a file on every system: no empty, `.` or `..` segments, none of
/// `\ : * ? " < > |` or control characters, no trailing dot or space, no reserved device name.
pub fn portable_rel(rel: &str) -> bool {
    let reserved = |seg: &str| {
        let stem = seg.split('.').next().unwrap_or("").to_ascii_uppercase();
        matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit())
    };
    !rel.is_empty()
        && rel.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && !seg.ends_with('.')
                && !seg.ends_with(' ')
                && !seg.chars().any(|c| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
                && !reserved(seg)
        })
}

/// The store folder the directory `dir` was last synced with.
pub fn synced_prefix(st: &mut dyn Store, dir: &Path) -> Result<Option<String>> {
    let key = crate::sync::dir_key(dir);
    Ok(st.all_sync_bases()?.into_iter().find(|b| crate::sync::is_dir_key(&b.dir, &key)).map(|b| b.prefix))
}

/// The vault `path` (a store path) belongs to. With `dir`: that directory and the store folder it
/// was synced with, which must hold `path`; a directory never synced takes `path` as its folder.
/// Without: the directory on this computer last synced with a folder holding `path`.
pub fn vault(st: &mut dyn Store, path: Option<&str>, dir: Option<&Path>) -> Result<Vault> {
    let path = path.map(normalize_path).transpose()?;
    let bases = st.all_sync_bases()?;
    if let Some(dir) = dir {
        if !dir.is_dir() {
            return Err(StoreError::invalid(format!("{} is not a directory", dir.display())));
        }
        let key = crate::sync::dir_key(dir);
        let synced: Vec<&SyncBase> = bases.iter().filter(|b| crate::sync::is_dir_key(&b.dir, &key)).collect();
        if let Some(newest) = synced.first() {
            let Some(p) = path else {
                return Ok(Vault { prefix: newest.prefix.clone(), dir: dir.to_path_buf() });
            };
            return synced
                .iter()
                .find(|b| under(&b.prefix, &p) || under(&p, &b.prefix))
                .map(|b| Vault { prefix: b.prefix.clone(), dir: dir.to_path_buf() })
                .ok_or_else(|| {
                    let folders: Vec<&str> = synced.iter().map(|b| b.prefix.as_str()).collect();
                    StoreError::invalid(format!("{} is synced with {}, and {p} is not in it", dir.display(), folders.join(", ")))
                });
        }
        let Some(p) = path else {
            return Err(StoreError::invalid(format!(
                "{} has not been synced with a store folder: name the folder it holds, as in `textdb assets status /notes --dir {}`",
                dir.display(),
                dir.display()
            )));
        };
        if st.stat(&p).is_ok_and(|s| s.kind == "file") {
            return Err(StoreError::invalid(format!("{p} is a file: name the store folder {} holds", dir.display())));
        }
        return Ok(Vault { prefix: p, dir: dir.to_path_buf() });
    }
    let p = path.unwrap_or_else(|| "/".to_string());
    // The directory the user is standing in comes first. Two directories per folder is the normal
    // state — a person's vault and an agent's checkout — and "whichever was synced with this
    // folder last" picked the wrong one silently: `assets push` run from the original pushed
    // nothing and never said which directory it had looked at.
    if let Ok(here) = std::env::current_dir() {
        if let Some((root, config)) = crate::root::Config::find(&here) {
            if (under(&config.prefix, &p) || under(&p, &config.prefix)) && root.is_dir() {
                return Ok(Vault { prefix: config.prefix, dir: root });
            }
        }
    }
    bases
        .iter()
        .find(|b| under(&b.prefix, &p) && Path::new(&b.dir).is_dir())
        .map(|b| Vault { prefix: b.prefix.clone(), dir: PathBuf::from(&b.dir) })
        .ok_or_else(|| StoreError::invalid(format!("no directory on this computer has been synced with a folder holding {p}: give one with --dir")))
}

/// Files by `/`-separated relative path: size and modification time (nanoseconds).
type Files = BTreeMap<String, (u64, i64)>;

/// Every file below `root` outside the ignored directories; and the `.gitattributes` files below
/// the top.
fn walk(root: &Path) -> Result<(Files, Vec<String>)> {
    let mut files = BTreeMap::new();
    let mut attrs = Vec::new();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel_dir)) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| io_err(dir.display(), e))?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if rel_dir.is_empty() { name.clone() } else { format!("{rel_dir}/{name}") };
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if !IGNORED_DIRS.iter().any(|d| d.eq_ignore_ascii_case(&name)) {
                    pending.push((entry.path(), rel));
                }
            } else if kind.is_file() {
                if classify::is_rules_file(&name) && !rel_dir.is_empty() {
                    attrs.push(rel.clone());
                }
                let meta = entry.metadata()?;
                files.insert(rel, (meta.len(), mtime_ns(&meta)));
            }
        }
    }
    Ok((files, attrs))
}

type Parsed = std::result::Result<Pointer, String>;

struct Scan {
    classifier: Classifier,
    disk: Files,
    /// By the asset's relative path.
    disk_pointers: BTreeMap<String, Parsed>,
    /// By the asset's relative path: the pointer document's version and what it says.
    store_pointers: BTreeMap<String, (i64, Parsed)>,
}

/// `inner` relative to `outer` (`/`-separated), when it is inside it or is it.
fn dir_inside(outer: &Path, inner: &Path) -> Option<String> {
    let (o, i) = (crate::sync::dir_key(outer), crate::sync::dir_key(inner));
    let sep = std::path::MAIN_SEPARATOR;
    let (o, i) = if cfg!(windows) { (o.to_lowercase(), i.to_lowercase()) } else { (o, i) };
    let o = o.trim_end_matches(sep);
    if i == o {
        return Some(String::new());
    }
    i.strip_prefix(o).and_then(|r| r.strip_prefix(sep)).map(|r| r.replace(sep, "/"))
}

/// Leave out of `files` (below the directory `dir`) what an asset store bound on this computer
/// keeps inside it: copies, not assets. A directory inside an asset store's folder is refused:
/// its files would be the store's copies, changed in place.
fn leave_out_asset_stores(st: &mut dyn Store, dir: &Path, files: &mut Files) -> Result<()> {
    // Only a local store has a folder on this computer; other drivers are not even started.
    for s in st.asset_stores()?.into_iter().filter(|s| s.driver == "local") {
        let Ok(d) = driver::open(&s) else { continue };
        let Some(root) = d.local_root() else { continue };
        if dir_inside(root, dir).is_some() {
            return Err(StoreError::invalid(format!(
                "{} is inside the folder of asset store {} ({}): keep a vault and its asset store apart",
                dir.display(),
                s.name,
                root.display()
            )));
        }
        let Some(rel) = dir_inside(dir, root) else { continue };
        eprintln!("warning: asset store {} keeps its files in {rel}, inside {}: they are left out", s.name, dir.display());
        let inside = format!("{}/", rel.to_lowercase());
        files.retain(|r, _| !r.to_lowercase().starts_with(&inside));
    }
    Ok(())
}

fn scan(st: &mut dyn Store, v: &Vault) -> Result<Scan> {
    let (mut disk, nested) = walk(&v.dir)?;
    leave_out_asset_stores(st, &v.dir, &mut disk)?;
    let classifier = Classifier::load(&v.dir, &nested);
    for w in &classifier.warnings {
        eprintln!("warning: {w}");
    }
    let mut disk_pointers = BTreeMap::new();
    for rel in disk.keys().filter(|r| is_asset_pointer(r)) {
        let parsed = std::fs::read(v.dir.join(rel)).map_err(|e| e.to_string()).and_then(|b| Pointer::parse(&b));
        disk_pointers.insert(asset_path(rel).to_string(), parsed);
    }
    let mut store_pointers = BTreeMap::new();
    for head in st.file_heads(&v.prefix)? {
        if let Some(rel) = rel_under(&v.prefix, &head.path).filter(|r| is_asset_pointer(r)) {
            let (bytes, version) = st.read(&head.path, None)?;
            store_pointers.insert(asset_path(&rel).to_string(), (version, Pointer::parse(&bytes)));
        }
    }
    Ok(Scan { classifier, disk, disk_pointers, store_pointers })
}

/// What a vault learnt about its files, kept per computer and directory in the local cache
/// directory: hashes and binary sniffs by size and modification time, and the bytes it last had of
/// each asset. Losing it is safe: files are hashed again, and what the directory last had is
/// unknown, so a file that differs from its pointer is a conflict until pulled or pushed.
#[derive(Default, Serialize, Deserialize)]
struct VaultCache {
    #[serde(default)]
    host: String,
    #[serde(default)]
    dir: String,
    #[serde(default)]
    files: BTreeMap<String, Cached>,
    #[serde(default)]
    sniffed: BTreeMap<String, (u64, i64, bool)>,
    /// By the asset's relative path: the SHA-256 of the bytes this directory last had for it.
    #[serde(default)]
    seen: BTreeMap<String, String>,
    /// The files the last sync found in the directory.
    #[serde(default)]
    present: BTreeSet<String>,
    /// A sync recorded `present`.
    #[serde(default)]
    present_known: bool,
    #[serde(skip)]
    path: Option<PathBuf>,
    /// Where an older build kept this cache, removed once it is saved here.
    #[serde(skip)]
    legacy: Option<PathBuf>,
    /// The entries this process learnt: `(0, file)`, `(1, sniffed)`, `(2, seen)`.
    #[serde(skip)]
    touched: BTreeSet<(u8, String)>,
    #[serde(skip)]
    dirty: bool,
    /// Hash every file afresh.
    #[serde(skip)]
    fresh: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    size: u64,
    mtime: i64,
    sha256: String,
}

impl VaultCache {
    fn open(v: &Vault) -> VaultCache {
        use sha2::Digest;
        let (host, dir) = (host(), crate::sync::dir_key(&v.dir));
        let key = pointer::hex(&sha2::Sha256::digest(format!("{}\n{dir}", host.to_lowercase()).as_bytes()));
        let path = driver::binding::cache_dir().map(|d| d.join(format!("assets-{}.json", &key[..16])));
        let mine = |c: &VaultCache| c.host.eq_ignore_ascii_case(&host) && c.dir == dir;
        let mut c = path.as_deref().and_then(Self::read).filter(mine);
        let mut legacy = None;
        if c.is_none() {
            // Builds before this one kept it in the config directory, keyed by the directory only.
            let old_key = pointer::hex(&sha2::Sha256::digest(dir.as_bytes()));
            let old = driver::binding::config_dir().map(|d| d.join("cache").join(format!("assets-{}.json", &old_key[..16])));
            if let Some(found) = old.as_deref().and_then(Self::read) {
                (c, legacy) = (Some(found), old);
            }
        }
        let mut c = c.unwrap_or_default();
        c.dirty = legacy.is_some();
        (c.path, c.legacy, c.host, c.dir) = (path, legacy, host, dir);
        c
    }

    fn read(path: &Path) -> Option<VaultCache> {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok())
    }

    fn trusted(size: u64, mtime: i64, entry: (u64, i64)) -> bool {
        entry == (size, mtime) && now_ns() - mtime > RACY_NS
    }

    fn sha(&mut self, root: &Path, rel: &str, size: u64, mtime: i64) -> Result<String> {
        if !self.fresh {
            if let Some(c) = self.files.get(rel).filter(|c| Self::trusted(size, mtime, (c.size, c.mtime))) {
                return Ok(c.sha256.clone());
            }
        }
        let (sha, _) = hash_file(&root.join(rel)).map_err(|e| io_err(rel, e))?;
        self.remember(rel, size, mtime, &sha);
        Ok(sha)
    }

    fn remember(&mut self, rel: &str, size: u64, mtime: i64, sha: &str) {
        if now_ns() - mtime > RACY_NS {
            self.files.insert(rel.to_string(), Cached { size, mtime, sha256: sha.to_string() });
            self.touched.insert((0, rel.to_string()));
            self.dirty = true;
        }
    }

    /// This directory has the bytes `sha` for the asset `rel`.
    fn saw(&mut self, rel: &str, sha: &str) {
        if self.seen.get(rel).map(String::as_str) != Some(sha) {
            self.seen.insert(rel.to_string(), sha.to_string());
            self.touched.insert((2, rel.to_string()));
            self.dirty = true;
        }
    }

    /// A file was put at `rel` after the last sync looked: sync counts it as there then.
    fn add_present(&mut self, rel: &str) {
        if self.present_known && self.present.insert(rel.to_string()) {
            // Added to whatever record is there when saved, not over it.
            self.touched.insert((3, rel.to_string()));
            self.dirty = true;
        }
    }

    fn binary(&mut self, root: &Path, rel: &str, size: u64, mtime: i64) -> bool {
        if let Some(&(s, m, b)) = self.sniffed.get(rel) {
            if Self::trusted(size, mtime, (s, m)) {
                return b;
            }
        }
        let b = classify::looks_binary(&root.join(rel));
        if now_ns() - mtime > RACY_NS {
            self.sniffed.insert(rel.to_string(), (size, mtime, b));
            self.touched.insert((1, rel.to_string()));
            self.dirty = true;
        }
        b
    }

    fn save(&self) {
        if let (true, Some(path)) = (self.dirty, &self.path) {
            // Another process may have saved meanwhile: only what this one learnt goes over what
            // is there (all of it when taking over an older build's cache).
            let mut merged = Self::read(path).filter(|c| c.host == self.host && c.dir == self.dir).unwrap_or_default();
            let all = self.legacy.is_some();
            let mine = |kind: u8, k: &String| all || self.touched.contains(&(kind, k.clone()));
            merged.files.extend(self.files.iter().filter(|(k, _)| mine(0, k)).map(|(k, v)| (k.clone(), v.clone())));
            merged.sniffed.extend(self.sniffed.iter().filter(|(k, _)| mine(1, k)).map(|(k, v)| (k.clone(), *v)));
            merged.seen.extend(self.seen.iter().filter(|(k, _)| mine(2, k)).map(|(k, v)| (k.clone(), v.clone())));
            if all || self.touched.contains(&(3, String::new())) {
                (merged.present, merged.present_known) = (self.present.clone(), self.present_known);
            }
            if merged.present_known {
                merged.present.extend(self.touched.iter().filter(|(kind, k)| *kind == 3 && !k.is_empty()).map(|(_, k)| k.clone()));
            }
            // What this process forgot is forgotten there too.
            for (kind, k) in &self.touched {
                match kind {
                    0 if !self.files.contains_key(k) => drop(merged.files.remove(k)),
                    1 if !self.sniffed.contains_key(k) => drop(merged.sniffed.remove(k)),
                    2 if !self.seen.contains_key(k) => drop(merged.seen.remove(k)),
                    _ => {}
                }
            }
            (merged.host, merged.dir) = (self.host.clone(), self.dir.clone());
            let Ok(text) = serde_json::to_string(&merged) else { return };
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Written aside and renamed over, so no one ever reads half of it.
            let part = driver::partial(path);
            if std::fs::write(&part, text).and_then(|_| std::fs::rename(&part, path)).is_err() {
                let _ = std::fs::remove_file(&part);
            } else if let Some(old) = &self.legacy {
                let _ = std::fs::remove_file(old);
            }
        }
    }
}

/// One asset of a vault.
#[derive(Clone, Serialize)]
pub struct Item {
    /// The asset's path in the store.
    pub path: String,
    /// `ok`, `new`, `modified`, `outdated`, `conflict`, `not-pulled`, `conflict-copy`, `orphan`,
    /// `invalid-pointer` or `invalid-path`; and from what the asset store itself holds,
    /// `moved-here` (the asset moved in textdb while its file stayed where it was),
    /// `moved-in-store`, `changed-in-store`, `trashed-in-store`, `ambiguous` (two files of one
    /// name there) or `invalid-item` (no file of that item at all).
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// Where the asset store keeps the file now, when the store can tell and that is not where the
    /// asset's own path says it should be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_store: Option<String>,
    /// The asset store holds other bytes in that very file than its pointer names: the fact itself,
    /// not the state, since an asset waiting to be pulled or one already outdated keeps its own
    /// state and must still follow those bytes.
    #[serde(skip)]
    pub store_changed: bool,
    /// What a store said was added to this asset's note, whatever state it kept.
    #[serde(skip)]
    pub store_told: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The media type: the pointer's, or from the name for a file without one.
    #[serde(rename = "type")]
    media_type: String,
    /// The SHA-256 the pointer names.
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    /// Relative to the vault: the pointer's asset path, or the file's for a new asset.
    #[serde(skip)]
    rel: String,
    /// The file on disk relative to the vault, when there is one (its name may differ in case from
    /// the pointer's).
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip)]
    pointer: Option<Pointer>,
    /// The pointer document's version in the store.
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<i64>,
    /// The pointer on disk says something else than the store's.
    #[serde(skip)]
    disk_differs: bool,
    /// A file whose name differs only in case from this pointer's asset.
    #[serde(skip)]
    case_of: Option<String>,
}

impl Item {
    fn new(v: &Vault, rel: &str) -> Item {
        Item {
            path: store_path(&v.prefix, rel),
            state: "ok",
            size: None,
            store: None,
            in_store: None,
            store_changed: false,
            store_told: false,
            note: None,
            media_type: pointer::media_type(rel).to_string(),
            sha256: None,
            rel: rel.to_string(),
            file: None,
            pointer: None,
            version: None,
            disk_differs: false,
            case_of: None,
        }
    }
}

fn in_scope(scope: &[String], path: &str) -> bool {
    scope.is_empty() || scope.iter().any(|s| under(s, path))
}

fn items(v: &Vault, scan: &Scan, cache: &mut VaultCache, scope: &[String]) -> Result<Vec<Item>> {
    let pointer_rels: BTreeSet<&String> = scan.store_pointers.keys().chain(scan.disk_pointers.keys()).collect();
    // Pointers by lower-case path: a file whose name differs only in case is the same asset on
    // Windows and macOS, and must not become a second one on Linux.
    let by_lower: HashMap<String, &String> = pointer_rels.iter().map(|r| (r.to_lowercase(), *r)).collect();
    let mut file_of: HashMap<&String, &String> = HashMap::new();
    let mut case_variants: Vec<(&String, &String)> = Vec::new();
    let mut out = Vec::new();
    for (rel, &(size, mtime)) in &scan.disk {
        match by_lower.get(&rel.to_lowercase()) {
            Some(&p) => {
                if p == rel {
                    if let Some(other) = file_of.insert(p, rel).filter(|o| *o != rel) {
                        case_variants.push((other, p));
                    }
                } else if file_of.contains_key(p) {
                    case_variants.push((rel, p));
                } else {
                    file_of.insert(p, rel);
                }
            }
            None => {
                if !in_scope(scope, &store_path(&v.prefix, rel)) {
                    continue;
                }
                let name = rel.rsplit('/').next().unwrap_or(rel);
                let class = match scan.classifier.rule_class(rel) {
                    Class::Other if !name.starts_with(".git") && cache.binary(&v.dir, rel, size, mtime) => Class::Asset,
                    c => c,
                };
                if class == Class::Asset {
                    let mut item = Item::new(v, rel);
                    item.size = Some(size);
                    item.file = Some(rel.clone());
                    item.state = if !portable_rel(rel) {
                        "invalid-path"
                    } else if pairing::is_conflict_copy(rel) {
                        item.note = Some("kept from a conflict: compare it with the asset, then delete it, or rename it to push it".into());
                        "conflict-copy"
                    } else if seen_key(&cache.seen, rel).is_some() {
                        item.note = Some(
                            "this directory had an asset here whose pointer was moved or deleted in textdb: sync moves the file after it or trashes it (push --force publishes it as a new asset)".into(),
                        );
                        "orphan"
                    } else {
                        "new"
                    };
                    out.push(item);
                }
            }
        }
    }
    for (rel, p) in case_variants {
        let mut item = Item::new(v, rel);
        if in_scope(scope, &item.path) {
            item.state = "conflict";
            item.file = Some(rel.clone());
            item.note = Some(format!("differs only in case from {}, which has a pointer: rename one of them", store_path(&v.prefix, p)));
            item.case_of = Some(p.clone());
            out.push(item);
        }
    }
    for rel in pointer_rels {
        let mut item = Item::new(v, rel);
        if !in_scope(scope, &item.path) {
            continue;
        }
        item.version = scan.store_pointers.get(rel).map(|s| s.0);
        if !portable_rel(rel) {
            item.state = "invalid-path";
            item.note = Some("the name cannot be a file on every system: rename the pointer in the store".into());
            out.push(item);
            continue;
        }
        // Never pulled into .git, .textdb, node_modules and the like, nor onto a name the rules
        // leave out: a pointer anyone wrote to the store must not put bytes there (a git hook, say).
        if scan.classifier.rule_class(rel) == Class::Ignore {
            item.state = "invalid-path";
            item.note = Some("a place textdb never writes assets to (.git, .textdb, node_modules, a name the rules leave out): move the pointer in the store".into());
            out.push(item);
            continue;
        }
        let pointer = match (scan.store_pointers.get(rel), scan.disk_pointers.get(rel)) {
            (Some((_, Err(e))), _) | (None, Some(Err(e))) => {
                item.state = "invalid-pointer";
                item.note = Some(e.clone());
                out.push(item);
                continue;
            }
            (Some((_, Ok(p))), on_disk) => {
                match on_disk {
                    Some(Ok(d)) if d != p => {
                        item.disk_differs = true;
                        item.note = Some("the pointer on disk is not the store's: sync to bring them together".into());
                    }
                    Some(Err(e)) => item.note = Some(format!("the pointer on disk is not valid ({e}): sync writes the store's")),
                    _ => {}
                }
                p.clone()
            }
            (None, Some(Ok(p))) => {
                item.note = Some("the pointer is only on disk: sync adds it to the store".into());
                p.clone()
            }
            (None, None) => continue,
        };
        item.store = Some(pointer.store.clone());
        match file_of.get(rel) {
            Some(&f) => {
                let (size, mtime) = scan.disk[f];
                let name = f.rsplit('/').next().unwrap_or(f);
                let asset_file = match scan.classifier.rule_class(f) {
                    Class::Asset => true,
                    Class::Other => !name.starts_with(".git") && cache.binary(&v.dir, f, size, mtime),
                    _ => false,
                };
                if !asset_file {
                    // A pointer only names this file: nothing of it is read or told (not its size,
                    // not whether its bytes are the pointer's), since anyone can write a pointer.
                    item.state = "conflict";
                    item.note = Some("the file here is not an asset (a document, or a file the rules leave out): rename the file or move the pointer".into());
                    item.pointer = Some(pointer);
                    out.push(item);
                    continue;
                }
                item.size = Some(size);
                item.file = Some(f.clone());
                if f != rel {
                    item.note.get_or_insert_with(|| format!("named {} on disk", store_path(&v.prefix, f)));
                }
                let sha = cache.sha(&v.dir, f, size, mtime)?;
                let seen = cache.seen.get(rel).cloned();
                item.state = if sha == pointer.sha256 {
                    cache.saw(rel, &sha);
                    "ok"
                } else if seen.as_deref() == Some(pointer.sha256.as_str()) {
                    "modified"
                } else if seen.as_deref() == Some(sha.as_str()) {
                    "outdated"
                } else {
                    item.note.get_or_insert_with(|| {
                        "other bytes than the pointer names, and not what this directory last had: move the file aside and pull to compare; push --force replaces the asset store's copy with this one".into()
                    });
                    "conflict"
                };
            }
            None => {
                item.size = Some(pointer.size);
                item.state = "not-pulled";
            }
        }
        (item.media_type, item.sha256) = (pointer.media_type.clone(), Some(pointer.sha256.clone()));
        item.pointer = Some(pointer);
        out.push(item);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn scope_of(paths: &[String]) -> Result<Vec<String>> {
    Ok(paths.iter().map(|p| normalize_path(p)).collect::<std::result::Result<Vec<_>, _>>()?)
}

fn counts(items: &[Item]) -> BTreeMap<&'static str, usize> {
    let mut c = BTreeMap::new();
    for i in items {
        *c.entry(i.state).or_insert(0) += 1;
    }
    c
}

fn counts_text(c: &BTreeMap<&'static str, usize>) -> String {
    if c.is_empty() {
        return "no assets".to_string();
    }
    c.iter().map(|(s, n)| format!("{n} {}", s.replace('-', " "))).collect::<Vec<_>>().join(", ")
}

fn item_line(i: &Item) -> String {
    let size = i.size.map(|s| format!("  {}", size_text(s))).unwrap_or_default();
    let note = i.note.as_deref().map(|n| format!("  ({n})")).unwrap_or_default();
    format!("  {:<16}{}{size}{note}\n", i.state, i.path)
}

/// Add what a store said to an asset's note, keeping any note it had: what the bytes here say is
/// never replaced by what the store says, only added to.
fn also_told(item: &mut Item, told: String) {
    item.store_told = true;
    item.note = Some(match item.note.take() {
        Some(had) => format!("{had}; {told}"),
        None => told,
    });
}

/// Ask the stores that keep their files by an id of their own what they hold of each asset, and mark
/// what they say: a file moved, replaced, trashed or doubled in the store, or an item the store has
/// no file of at all. A store-side state goes over `ok` alone -- what the bytes here say comes
/// first, and an asset waiting to be pulled still says so -- while any other state keeps its own and
/// is told of this as well. A store that cannot be asked leaves its assets as they were and is named
/// in what comes back, so nothing is silently taken for being as it was.
pub(crate) fn mark_store_states(items: &mut [Item], mut ask: impl FnMut(&str, &str) -> Result<Option<driver::Found>>) -> Vec<String> {
    let mut unasked: BTreeMap<String, String> = BTreeMap::new();
    for item in items.iter_mut() {
        let Some(p) = item.pointer.clone() else { continue };
        let Some(named) = p.item.clone() else { continue };
        if named.starts_with('/') {
            continue;
        }
        // What the bytes here say holds its state; the first store-side fact takes an `ok` one, and
        // every fact after it is told all the same.
        let mut claimed = item.state != "ok";
        let plain = !claimed;
        match ask(&p.store, &named) {
            // No file of that item at all: one purged from the store's own trash, or an id naming a
            // file outside the store, which is never read, moved or trashed.
            Ok(None) => {
                if plain {
                    item.state = "invalid-item";
                }
                also_told(item, format!("its asset store has no file of item {named}: purged from the store's trash, or a file outside the store, which textdb never touches"));
            }
            Ok(Some(f)) => {
                // Compared as the store compares its own locations: a path differing only in letter
                // case is the same place, not a move that could never be settled.
                let elsewhere = location_key(&f.path) != location_key(&item.path);
                if elsewhere {
                    item.in_store = Some(f.path.clone());
                }
                // The trash first: which of two files of a path an item means says nothing about a
                // file that is in the store's own trash, and a pull by id fetches it from there.
                if f.trashed {
                    if !claimed {
                        item.state = "trashed-in-store";
                        claimed = true;
                    }
                    also_told(item, format!("its file is in the asset store's own trash (at {}): a pull still fetches it, until the store empties it", f.path));
                } else if f.two_of_a_name {
                    if !claimed {
                        item.state = "ambiguous";
                        claimed = true;
                    }
                    also_told(item, format!("the asset store holds two files at {}: which of them the pointer names cannot be said, so neither is read or written over -- keep one of them there", f.path));
                } else {
                    // Other bytes in that very file: a push keeps a file's id through replacing what
                    // is in it, so the id alone says nothing of whose bytes are there now. Where the
                    // provider keeps no SHA-256 (an old upload), a size that is not the pointer's
                    // still says they are other bytes.
                    let replaced = match &f.sha256 {
                        Some(sha) => *sha != p.sha256,
                        None => f.size != p.size,
                    };
                    if replaced {
                        item.store_changed = true;
                        if item.state == "modified" {
                            // Changed here and there: neither side is taken over the other.
                            item.state = "conflict";
                            claimed = true;
                            also_told(item, "its bytes changed here and in the asset store: pull to take the store's as the pointer's new version (the copy here is set aside), or push --force to replace the store's with this one".to_string());
                        } else {
                            if !claimed {
                                item.state = "changed-in-store";
                                claimed = true;
                            }
                            also_told(item, "the asset store holds other bytes than its pointer names: someone replaced them there, and a pull takes them as the pointer's new version".to_string());
                        }
                    }
                    // Told as well as the change, never instead of it: a file both replaced and
                    // moved is two things that happened, and one hiding the other tells neither.
                    if elsewhere {
                        // Which side moved it: the pointer records where the store last had the
                        // bytes, so a path that is not that one was moved in the store.
                        let recorded = p.item_path.as_deref();
                        let by_store = recorded.is_some_and(|r| location_key(r) != location_key(&f.path));
                        if !claimed {
                            item.state = if by_store { "moved-in-store" } else { "moved-here" };
                            claimed = true;
                        }
                        also_told(
                            item,
                            match (by_store, recorded) {
                                (true, _) => format!(
                                    "someone moved its file in the asset store, to {}: a pull still fetches it from there, and `textdb assets relocate {}` moves it back to the asset",
                                    f.path, item.path
                                ),
                                (false, Some(_)) => format!(
                                    "its file in the asset store is still at {}, where the asset used to be: `textdb assets relocate {}` moves it to the asset, or move the pointer back",
                                    f.path, item.path
                                ),
                                // A pointer written before the store path was recorded: which side
                                // moved cannot be told, so both places are given and neither blamed.
                                (false, None) => format!(
                                    "its file in the asset store is at {}, and this pointer does not say where the store last had it, so which side moved is not known: `textdb assets relocate {}` moves the file to the asset, or move the pointer back",
                                    f.path, item.path
                                ),
                            },
                        );
                    }
                }
            }
            Err(e) => {
                unasked.entry(p.store.clone()).or_insert(e.message);
            }
        }
    }
    unasked
        .into_iter()
        .map(|(store, why)| format!("the asset store {store} could not be asked what it holds ({why}): what was moved, replaced or trashed there is not told of"))
        .collect()
}

pub fn status(st: &mut dyn Store, path: Option<&str>, dir: Option<&Path>, json: bool) -> Result<()> {
    let v = vault(st, path, dir)?;
    let scan = scan(st, &v)?;
    let mut cache = VaultCache::open(&v);
    let scope = scope_of(&path.map(str::to_string).into_iter().collect::<Vec<_>>())?;
    let mut items = items(&v, &scan, &mut cache, &scope)?;
    // The stores are asked what they hold, so a file moved, replaced, trashed or doubled there
    // shows as its own state; a store that cannot be reached says so and changes nothing.
    let unasked = match StoreFiles::new(st) {
        Ok(mut files) => mark_store_states(&mut items, |store, item| files.found(store, item)),
        Err(e) => vec![format!("the asset stores could not be read ({}): assets moved in textdb are not told of", e.message)],
    };
    cache.save();
    let c = counts(&items);
    if json {
        // In the document too: a reader of the JSON must not take every asset for being in its
        // place when a store was never asked.
        return emit_json(&json!({ "prefix": v.prefix, "dir": v.dir.display().to_string(), "assets": items, "counts": c, "notes": unasked }));
    }
    for line in &unasked {
        eprintln!("note: {line}");
    }
    let mut s = format!("assets of {} in {}\n", v.prefix, v.dir.display());
    for i in items.iter().filter(|i| i.state != "ok" || i.note.is_some()) {
        s.push_str(&item_line(i));
    }
    s.push_str(&counts_text(&c));
    s.push('\n');
    out(s.as_bytes())
}

pub struct PushOptions<'a> {
    pub to: Option<&'a str>,
    pub message: Option<&'a str>,
    pub author: Option<&'a str>,
    pub dry_run: bool,
    /// Push conflicting files too, replacing what the asset store has.
    pub force: bool,
}

/// Open drivers, one per asset store, each once.
struct Drivers {
    stores: Vec<AssetStore>,
    open: HashMap<String, std::result::Result<Box<dyn Driver>, String>>,
}

impl Drivers {
    fn new(st: &mut dyn Store) -> Result<Drivers> {
        Ok(Drivers { stores: st.asset_stores()?, open: HashMap::new() })
    }

    fn names(&self) -> Vec<String> {
        self.stores.iter().map(|s| s.name.clone()).collect()
    }

    fn get(&mut self, name: &str) -> std::result::Result<&dyn Driver, String> {
        if !self.open.contains_key(name) {
            let opened = match self.stores.iter().find(|s| s.name == name) {
                Some(s) => driver::open(s).map_err(|e| e.message),
                None => Err(format!("no asset store named {name} is declared (textdb assets stores)")),
            };
            self.open.insert(name.to_string(), opened);
        }
        match &self.open[name] {
            Ok(d) => Ok(d.as_ref()),
            Err(e) => Err(e.clone()),
        }
    }
}

/// What became of a store's own copy of an asset whose path changed.
pub(crate) enum StoreMove {
    /// At the asset's own path now; the item its pointer names (on Google Drive the id, which a
    /// move leaves as it was).
    Moved(Option<String>),
    /// Left where it is, for a reason nobody needs telling.
    Left,
    /// Left where it is, for a reason a command tells of.
    LeftBecause(String),
}

/// What became of a store's own copy of an asset whose pointer went away.
pub(crate) enum StoreCopy {
    /// In the store's own trash now, or there already.
    Trashed,
    /// Left where it is, for a reason nobody needs telling: another pointer names those bytes, or
    /// the store keeps no trash of a provider's own.
    Left,
    /// Left where it is for a reason a sync tells of.
    LeftBecause(String),
}

/// The asset stores' own files as a sync may change them: which pointers still name what, and the
/// drivers to move or trash with. Made once for a command, since knowing what every pointer names
/// reads the store's pointers, and each driver opens once.
pub(crate) struct StoreFiles {
    in_use: InUse,
    drivers: Drivers,
}

impl StoreFiles {
    pub(crate) fn new(st: &mut dyn Store) -> Result<StoreFiles> {
        Ok(StoreFiles { in_use: InUse::new(), drivers: Drivers::new(st)? })
    }

    /// Why the file at `location` in the store `store` stays where it is, if it does: another
    /// pointer than the asset `own`'s names it, or a pointer of the store cannot be read at all and
    /// what its bytes are for is therefore not known. `None` leaves the file to the caller.
    pub(crate) fn keeps(&mut self, st: &mut dyn Store, store: &str, location: &str, own: &str) -> Result<Option<StoreCopy>> {
        self.in_use.refresh(st)?;
        // A pointer textdb cannot read names nothing it can be sure of: what a store's files are
        // for is not guessed at from the pointers that happened to parse. Said out loud, since one
        // such pointer anywhere in the store stops every store of every asset being tidied.
        let mut unreadable: Vec<&str> = self.in_use.pointers.iter().filter(|(_, (_, names))| names.is_none()).map(|(p, _)| p.as_str()).collect();
        unreadable.sort();
        if let Some(first) = unreadable.first() {
            return Ok(Some(StoreCopy::LeftBecause(match unreadable.len() {
                1 => format!("the pointer {first} cannot be read, so what its bytes are for is not known"),
                n => format!("{n} pointers cannot be read ({first} among them), so what their bytes are for is not known"),
            })));
        }
        // An account is answered in its own view, so a pointer of another account naming these
        // bytes is not visible here, and "nothing else names them" would be a guess -- one that ends
        // with somebody else's asset naming bytes on a thirty-day clock in a provider's trash. Until
        // the store can answer what no view hides, an account's session takes nothing away.
        if self.in_use.delegated.unwrap_or(true) {
            // Asked of the store, which holds every pointer, and answered in counts: which asset
            // needs the bytes, and where it is, is no part of what this session may learn.
            return Ok(match st.asset_item_users(store, location, own)? {
                None => Some(StoreCopy::LeftBecause(
                    "this session is an account's, and this store cannot say whether another asset still needs these bytes".to_string(),
                )),
                Some(u) if u.unreadable > 0 => Some(StoreCopy::LeftBecause(match u.unreadable {
                    1 => "a pointer in the store cannot be read, so what its bytes are for is not known".to_string(),
                    n => format!("{n} pointers in the store cannot be read, so what their bytes are for is not known"),
                })),
                Some(u) if u.others > 0 => Some(StoreCopy::Left),
                Some(_) => None,
            });
        }
        Ok(self.in_use.shared(st, store, location, own)?.then_some(StoreCopy::Left))
    }

    /// The file the asset `own`'s pointer names, moved in its store to `to` (the asset's own path
    /// now), once no other pointer names it: the item the pointer should name, or `None` where
    /// nothing moved -- another pointer names those bytes, or the store keeps items by path and the
    /// bytes stay for them.
    pub(crate) fn moved(&mut self, st: &mut dyn Store, store: &str, item: &str, to: &str, own: &str) -> Result<StoreMove> {
        // Items that are paths move with their pointer already: no driver is asked, so a store this
        // computer cannot reach is not waited on for work that could not come to anything.
        if item.starts_with('/') {
            return Ok(StoreMove::Left);
        }
        if let Some(kept) = self.keeps(st, store, item, own)? {
            return Ok(match kept {
                StoreCopy::LeftBecause(why) => StoreMove::LeftBecause(why),
                _ => StoreMove::Left,
            });
        }
        Ok(StoreMove::Moved(self.drivers.get(store).map_err(StoreError::invalid)?.move_to(item, to)?))
    }

    /// What the store `store` knows of the file the item `item` names, when it can tell.
    pub(crate) fn found(&mut self, store: &str, item: &str) -> Result<Option<driver::Found>> {
        self.drivers.get(store).map_err(StoreError::invalid)?.found(item)
    }

    /// The driver of the store `store`, so a command asking what the stores hold and then reading
    /// from them opens each of them once.
    pub(crate) fn driver(&mut self, store: &str) -> std::result::Result<&dyn Driver, String> {
        self.drivers.get(store)
    }

    /// The file the deleted pointer of the asset `own` named, holding the bytes `sha256`, sent to
    /// its store's own trash once no pointer names it.
    pub(crate) fn trashed(&mut self, st: &mut dyn Store, store: &str, item: &str, own: &str, sha256: &str) -> Result<StoreCopy> {
        // A store keeping its files by path keeps no trash of a provider's own, so there is nothing
        // to ask a driver -- and nothing to wait for where the store cannot be reached.
        if item.starts_with('/') {
            return Ok(StoreCopy::Left);
        }
        if let Some(kept) = self.keeps(st, store, item, own)? {
            return Ok(kept);
        }
        let sent = self.drivers.get(store).map_err(StoreError::invalid)?.trash(item, sha256)?;
        Ok(match sent {
            true => StoreCopy::Trashed,
            false => StoreCopy::Left,
        })
    }
}

enum Outcome {
    Done(serde_json::Value),
    Conflict(String),
    Failed(String),
}

/// A pointer committed by push: its store path, version and text.
struct Written {
    path: String,
    version: i64,
    text: String,
}

/// A location as [`InUse`] compares it: a store path without case, since Windows and macOS keep
/// `a.png` and `A.png` in the same file; a provider's file id (Google Drive's) as it is, since ids
/// differing only in case are other files.
/// What a pointer document names: its asset store, and the location in it as locations are
/// compared. `None` when the text is no pointer this build can read. The one place that is worked
/// out, so a binding answering for a whole store agrees with the scan here.
pub(crate) fn pointer_names(pointer_path: &str, text: &str) -> Option<(String, String)> {
    let p = Pointer::parse(text.as_bytes()).ok()?;
    let named = p.item.clone().unwrap_or_else(|| asset_path(pointer_path).to_string());
    Some((p.store.clone(), location_key(&named)))
}

pub(crate) fn location_key(location: &str) -> String {
    if location.starts_with('/') {
        location.to_lowercase()
    } else {
        location.to_string()
    }
}

/// What every pointer in the store names: by pointer path, its version and its asset store and
/// location. Read again, for the pointers that changed, whenever the store changed.
struct InUse {
    seq: i64,
    /// Whether this connection is an account's rather than the owner's, asked once. An account is
    /// answered in its own view, so the pointers below are only the ones it may see: what else
    /// names a store's bytes cannot be known from here at all.
    delegated: Option<bool>,
    pointers: HashMap<String, (i64, Option<(String, String)>)>,
}

impl InUse {
    fn new() -> InUse {
        InUse { seq: -1, pointers: HashMap::new(), delegated: None }
    }

    fn refresh(&mut self, st: &mut dyn Store) -> Result<()> {
        if self.delegated.is_none() {
            // A store that cannot say who is asking is treated as an account's: this answer decides
            // whether bytes in somebody's drive are taken away, and the safe way to be wrong is to
            // keep them.
            self.delegated = Some(st.whoami().map_or(true, |me| me.account.is_some()));
        }
        let seq = st.last_seq()?;
        if seq == self.seq {
            return Ok(());
        }
        let mut live = HashSet::new();
        for head in st.file_heads("/")? {
            if !is_asset_pointer(&head.path) {
                continue;
            }
            live.insert(head.path.clone());
            if self.pointers.get(&head.path).is_some_and(|(v, _)| *v == head.version) {
                continue;
            }
            let names = Pointer::parse(&st.read(&head.path, Some(head.version))?.0)
                .ok()
                .map(|p| (p.store.clone(), location_key(p.item.as_deref().unwrap_or(asset_path(&head.path)))));
            self.pointers.insert(head.path, (head.version, names));
        }
        self.pointers.retain(|path, _| live.contains(path));
        self.seq = seq;
        Ok(())
    }

    /// Whether a pointer other than the asset `own`'s names `location` in `store`.
    fn shared(&self, st: &mut dyn Store, store: &str, location: &str, own: &str) -> Result<bool> {
        // An account sees its own pointers, so the store is asked instead -- and what it cannot
        // answer counts as shared, which leaves a push putting its bytes beside what is there
        // rather than over it, as it already does for bytes another pointer names.
        if self.delegated.unwrap_or(true) {
            return Ok(st.asset_item_users(store, location, own)?.map_or(true, |u| u.others > 0 || u.unreadable > 0));
        }
        let key = (store.to_string(), location_key(location));
        Ok(self
            .pointers
            .iter()
            .any(|(path, (_, names))| names.as_ref() == Some(&key) && location_key(asset_path(path)) != location_key(own)))
    }

    /// Where the upload of `item` goes, and the bytes it may replace there: the asset's own where
    /// they are kept (its path, or the item its pointer names), unless another pointer names them
    /// too or the asset store no longer has them (`gone`); otherwise the asset's path, next to
    /// anything already there.
    fn target<'p>(&self, st: &mut dyn Store, store: &str, item: &'p Item, gone: bool) -> Result<(String, Option<&'p str>)> {
        let own = item
            .pointer
            .as_ref()
            .filter(|p| p.store == store && !gone)
            .map(|p| (p.item.clone().unwrap_or_else(|| item.path.clone()), p.sha256.as_str()));
        let Some((location, sha)) = own else {
            return Ok((item.path.clone(), None));
        };
        match self.shared(st, store, &location, &item.path)? {
            true => Ok((item.path.clone(), None)),
            false => Ok((location, Some(sha))),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_one(
    st: &mut dyn Store,
    v: &Vault,
    drivers: &mut Drivers,
    o: &PushOptions,
    cache: &mut VaultCache,
    in_use: &mut InUse,
    item: &Item,
    written: &mut Vec<Written>,
) -> Outcome {
    let names = drivers.names();
    let store = match (o.to, &item.pointer) {
        (Some(n), _) => n.to_string(),
        (None, Some(p)) => p.store.clone(),
        (None, None) => names.first().cloned().unwrap_or_default(),
    };
    if !names.contains(&store) {
        return Outcome::Failed(format!("{}: no asset store named {store} is declared (textdb assets stores)", item.path));
    }
    if o.dry_run {
        return Outcome::Done(json!({ "path": item.path, "state": item.state, "size": item.size, "store": store }));
    }
    if item.disk_differs {
        return Outcome::Conflict(format!("{}: its pointer changed in the store since this directory was synced; sync first", item.path));
    }
    // Checked before the upload as well as before the commit, so a push that lost the race to
    // another leaves the asset store alone.
    let pointer_path = format!("{}{SUFFIX}", item.path);
    if st.stat(&pointer_path).ok().and_then(|s| s.version) != item.version {
        return Outcome::Conflict(format!("{}: its pointer changed in the store since this directory was scanned; run it again", item.path));
    }
    let d = match drivers.get(&store) {
        Ok(d) => d,
        Err(e) => return Outcome::Failed(format!("{}: {e}", item.path)),
    };
    let file = item.file.as_deref().unwrap_or(&item.rel);
    let src = v.dir.join(file);
    let (sha, size) = match hash_file(&src) {
        Ok(h) => h,
        Err(e) => return Outcome::Failed(format!("{}: {e}", item.path)),
    };
    let old = item.pointer.as_ref();
    // A provider's file id its pointer names that the asset store no longer has (a Drive file
    // purged from Drive's trash, say): the upload goes to the asset's path.
    let gone = old
        .filter(|p| p.store == store)
        .and_then(|p| p.item.as_deref())
        .filter(|i| !i.starts_with('/'))
        .is_some_and(|i| matches!(d.size(&item.path, Some(i)), Ok(None)));
    // The target's lock is held until the pointer is committed, and which pointers name the
    // target is read again once it is held: a push that reuses bytes already there commits its
    // pointer before another push may decide to replace them.
    let mut decided = None;
    for _ in 0..8 {
        if let Err(e) = in_use.refresh(st) {
            return Outcome::Failed(format!("{}: {}", item.path, e.message));
        }
        let (target, _) = match in_use.target(st, &store, item, gone) {
            Ok(t) => t,
            Err(e) => return Outcome::Failed(format!("{}: {}", item.path, e.message)),
        };
        let held = match d.lock(&target) {
            Ok(h) => h,
            Err(e) => return Outcome::Failed(format!("{}: {}", item.path, e.message)),
        };
        // Read every pointer's version again: a change number alone can miss a commit that
        // finished after a later one (Postgres hands them out before commit).
        in_use.seq = -1;
        if let Err(e) = in_use.refresh(st) {
            return Outcome::Failed(format!("{}: {}", item.path, e.message));
        }
        let (again, replaces) = match in_use.target(st, &store, item, gone) {
            Ok(t) => t,
            Err(e) => return Outcome::Failed(format!("{}: {}", item.path, e.message)),
        };
        if again == target {
            decided = Some((target, replaces, held));
            break;
        }
    }
    let Some((target, replaces, _held)) = decided else {
        return Outcome::Conflict(format!("{}: the pointers naming its bytes kept changing during this push; run it again", item.path));
    };
    let (provider_item, _also_held) = match d.put(&target, &src, &sha, replaces) {
        Ok(put) => put,
        Err(e) => return Outcome::Failed(format!("{}: {}", item.path, e.message)),
    };
    let named = provider_item.or_else(|| old.filter(|p| p.store == store).and_then(|p| p.item.clone()));
    // Where the store keeps these bytes now, for a store addressing its files by an id of its own:
    // what tells a file someone moved in the store from an asset moved in textdb. Asked of the
    // listing this push already made -- not taken to be the asset's own path, since a push replaces
    // the bytes in the file where it stands. Kept as it was where the store cannot say.
    let item_path = named
        .as_deref()
        .and_then(|i| d.found(i).ok().flatten())
        .map(|f| f.path)
        .or_else(|| old.filter(|p| p.store == store).and_then(|p| p.item_path.clone()));
    let p = Pointer {
        id: old.map_or_else(pointer::new_id, |p| p.id.clone()),
        sha256: sha.clone(),
        size,
        media_type: pointer::media_type(&item.rel).to_string(),
        store: store.clone(),
        item: named,
        item_path,
        extra: old.map(|p| p.extra.clone()).unwrap_or_default(),
    };
    // The bytes are in the asset store and checked; now the pointer, unless another push got there first.
    if st.stat(&pointer_path).ok().and_then(|s| s.version) != item.version {
        return Outcome::Conflict(format!(
            "{}: its pointer changed in the store during this push; run it again (the uploaded bytes stay in the asset store unused, and anything they replaced is in its trash)",
            item.path
        ));
    }
    let text = p.to_text();
    let w = match st.write(&pointer_path, text.as_bytes(), item.version, o.author, Some(o.message.unwrap_or("assets push"))) {
        Ok(w) => w,
        Err(e) => return Outcome::Failed(format!("{}: the bytes are in the asset store, but the pointer was not committed: {}", item.path, e.message)),
    };
    if let Ok(meta) = std::fs::metadata(&src) {
        cache.remember(file, meta.len(), mtime_ns(&meta), &sha);
    }
    cache.saw(&item.rel, &sha);
    let on_disk = v.dir.join(format!("{}{SUFFIX}", item.rel));
    if let Err(e) = std::fs::write(&on_disk, &text) {
        return Outcome::Failed(format!("{}: the pointer is committed but could not be written to {} ({e}); sync writes it", item.path, on_disk.display()));
    }
    if let Some(location) = p.item.as_deref() {
        in_use.pointers.insert(pointer_path.clone(), (w.version, Some((store.clone(), location_key(location)))));
    }
    // Recorded as synced only now that both sides have it.
    written.push(Written { path: pointer_path, version: w.version, text });
    Outcome::Done(json!({ "path": item.path, "file": file, "state": item.state, "size": size, "store": store, "item": p.item, "version": w.version }))
}

/// This directory's sync base for the vault's folder, without its files, as it was recorded.
fn recorded_base(st: &mut dyn Store, v: &Vault) -> Result<Option<SyncBase>> {
    let key = crate::sync::dir_key(&v.dir);
    Ok(st.all_sync_bases()?.into_iter().find(|b| b.prefix == v.prefix && crate::sync::is_dir_key(&b.dir, &key)))
}

/// Record the pointers push committed and wrote to disk in this directory's sync base for the
/// vault's folder, as a sync would have: the next sync then knows both sides agree on them, and a
/// pointer deleted or moved in the store is deleted or moved on disk rather than taken in again.
/// Only their rows change; the base's other files, and when it was synced, stay as they are.
fn record_in_sync_base(st: &mut dyn Store, v: &Vault, written: &[Written]) -> Result<()> {
    if written.is_empty() {
        return Ok(());
    }
    let Some(base) = recorded_base(st, v)? else { return Ok(()) };
    let rows: Vec<BaseFile> = written
        .iter()
        .filter_map(|w| {
            let rel = rel_under(&v.prefix, &w.path)?;
            Some(crate::sync::base_row(&v.dir, &rel, Some(w.version), crate::sync::blob_id(w.text.as_bytes()), false))
        })
        .collect();
    st.put_sync_files(&base.prefix, &base.dir, &rows)?;
    Ok(())
}

/// The key of `seen` for the asset at `rel`: that path, or on Windows and macOS the one differing
/// only in letter case.
fn seen_key(seen: &BTreeMap<String, String>, rel: &str) -> Option<String> {
    if seen.contains_key(rel) {
        return Some(rel.to_string());
    }
    if cfg!(any(windows, target_os = "macos")) {
        let folded = rel.to_lowercase();
        return seen.keys().find(|k| k.to_lowercase() == folded).cloned();
    }
    None
}

/// What a directory knows of its assets, for sync: the bytes it last had of each, its files'
/// hashes, and the files the last sync found.
pub(crate) struct DirCache {
    cache: VaultCache,
    dir: PathBuf,
    /// `present` in lower case, where letter case does not tell files apart.
    present_folded: HashSet<String>,
}

impl DirCache {
    pub(crate) fn open(dir: &Path) -> DirCache {
        let cache = VaultCache::open(&Vault { prefix: "/".to_string(), dir: dir.to_path_buf() });
        let present_folded = if cfg!(any(windows, target_os = "macos")) { cache.present.iter().map(|r| r.to_lowercase()).collect() } else { HashSet::new() };
        DirCache { cache, dir: dir.to_path_buf(), present_folded }
    }

    /// The SHA-256 of the bytes this directory last had for the asset at `rel`.
    pub(crate) fn had(&self, rel: &str) -> Option<&str> {
        seen_key(&self.cache.seen, rel).and_then(|k| self.cache.seen.get(&k)).map(String::as_str)
    }

    /// The SHA-256 of the file at `rel`, from the cache while its size and time are unchanged.
    pub(crate) fn sha(&mut self, rel: &str, size: u64, mtime: i64) -> Result<String> {
        self.cache.sha(&self.dir.clone(), rel, size, mtime)
    }

    /// Whether the last sync found a file at `rel`; `None` when no sync recorded that.
    pub(crate) fn was_present(&self, rel: &str) -> Option<bool> {
        self.cache
            .present_known
            .then(|| self.cache.present.contains(rel) || self.present_folded.contains(&rel.to_lowercase()))
    }

    /// The asset at `rel` is no longer here.
    pub(crate) fn forget(&mut self, rel: &str) {
        if let Some(k) = seen_key(&self.cache.seen, rel) {
            self.cache.seen.remove(&k);
            self.cache.touched.insert((2, k));
            self.cache.dirty = true;
        }
    }

    /// The asset at `from` is at `to` now.
    pub(crate) fn moved(&mut self, from: &str, to: &str) {
        if let Some(k) = seen_key(&self.cache.seen, from) {
            if let Some(sha) = self.cache.seen.remove(&k) {
                self.cache.touched.insert((2, k));
                self.cache.saw(to, &sha);
            }
        }
    }

    pub(crate) fn record_present(&mut self, rels: BTreeSet<String>) {
        (self.cache.present, self.cache.present_known) = (rels, true);
        self.cache.touched.insert((3, String::new()));
        self.cache.dirty = true;
    }

    pub(crate) fn save(&self) {
        self.cache.save();
    }
}

/// Whether writing `rel` below `root` would go through a symbolic link or junction already on
/// disk: a folder that is some other folder, anywhere (a `.git` included), or a file that is a link.
pub(crate) fn through_link(root: &Path, rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let mut at = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => return true,
            // A file on the way, or the end: nothing further to follow.
            Ok(meta) if !meta.is_dir() || i + 1 == parts.len() => return false,
            Ok(_) => {}
            // Not there: made fresh, not followed.
            Err(_) => return false,
        }
    }
    false
}

/// The folders below `dir` (relative to it, in lower case) that asset stores bound on this
/// computer keep their files in.
pub(crate) fn store_folders_inside(st: &mut dyn Store, dir: &Path) -> Vec<String> {
    let Ok(stores) = st.asset_stores() else { return Vec::new() };
    stores
        .iter()
        .filter(|s| s.driver == "local")
        .filter_map(|s| driver::open(s).ok())
        .filter_map(|d| d.local_root().and_then(|root| dir_inside(dir, root)))
        .map(|rel| rel.to_lowercase())
        .collect()
}

/// The vault's assets that are not pulled and have a conflict copy next to them: the store's bytes
/// a sync set a file aside for and could not pull then.
pub(crate) fn not_pulled_after_conflict(st: &mut dyn Store, v: &Vault) -> Result<BTreeSet<String>> {
    let scan = scan(st, v)?;
    let mut cache = VaultCache::open(v);
    let found = items(v, &scan, &mut cache, &[])?;
    cache.save();
    let originals: HashSet<String> = found.iter().filter(|i| i.state == "conflict-copy").filter_map(|i| pairing::original_of(&i.rel)).collect();
    Ok(found.iter().filter(|i| i.state == "not-pulled" && originals.contains(&i.rel)).map(|i| i.path.clone()).collect())
}

/// How many of the vault's assets are in each state, and what to tell of the ones that moved in
/// textdb while their store kept their file where it was. The stores are asked once: for a store
/// keeping its files by path there is nothing to ask, so only a store with ids of its own (Google
/// Drive) is reached at all, and one that cannot be reached leaves its assets as they are and says
/// so. The move itself is never made here: `textdb assets relocate` makes it, `textdb mv` puts the
/// pointer back.
pub(crate) fn counts_and_moves(st: &mut dyn Store, v: &Vault, save: bool, stores: Option<&mut StoreFiles>) -> Result<(BTreeMap<String, usize>, Vec<String>)> {
    let scan = scan(st, v)?;
    let mut cache = VaultCache::open(v);
    let mut found = items(v, &scan, &mut cache, &[])?;
    // Whose stores to ask is the caller's to say, so a sync asks one set of them for its pull, its
    // states and its deletions alike. A dry run asks nothing of anyone's provider -- it says what a
    // sync would do here, and reaching a drive to tell of a move is not that -- so it passes none.
    let mut notes = match stores {
        None => Vec::new(),
        Some(files) => mark_store_states(&mut found, |store, item| files.found(store, item)),
    };
    // One line each while they are few, one line for a folderful: a folder moved in textdb is
    // hundreds of assets, and a sync saying so hundreds of times says nothing at all.
    // Everything a store said, whatever state the asset kept: a `modified` file replaced in the
    // store, an asset waiting to be pulled whose file was purged there -- the ones a state list
    // would have hidden are exactly the ones worth saying.
    let differ: Vec<&Item> = found.iter().filter(|i| i.store_told).collect();
    match differ.len() {
        0 => {}
        1..=3 => notes.extend(differ.iter().map(|i| format!("{}: {}", i.path, i.note.as_deref().unwrap_or("it differs from what its asset store holds")))),
        n => notes.push(format!(
            "{n} assets differ from what their asset stores hold (the first is {}): `textdb assets status` says of each, and `textdb assets relocate` settles the ones whose file only stayed where the asset used to be",
            differ[0].path
        )),
    }
    if save {
        cache.save();
    }
    Ok((counts(&found).into_iter().map(|(state, n)| (state.to_string(), n)).collect(), notes))
}

/// What a push did: the assets pushed (as JSON rows), their bytes, and what was left for a
/// conflict or failed.
#[derive(Default)]
pub(crate) struct PushReport {
    pub pushed: Vec<serde_json::Value>,
    pub bytes: u64,
    pub conflicts: Vec<String>,
    pub failed: Vec<String>,
}

pub fn push(st: &mut dyn Store, paths: &[String], dir: Option<&Path>, o: PushOptions, json: bool) -> Result<()> {
    let v = vault(st, paths.first().map(String::as_str), dir)?;
    let scope = scope_of(paths)?;
    let PushReport { pushed, bytes, conflicts, failed } = push_run(st, &v, &scope, &o)?;
    if !o.dry_run && !pushed.is_empty() && crate::git::repo(&v.dir).is_some() {
        let files: Vec<String> = pushed.iter().filter_map(|p| p["file"].as_str().map(str::to_string)).collect();
        let ignored = crate::git::ignored(&v.dir, &files);
        let loose: Vec<&String> = files.iter().filter(|f| !ignored.contains(*f)).collect();
        if let Some(first) = loose.first() {
            eprintln!(
                "note: git does not ignore {} of the pushed assets ({first}{}): `textdb assets gitignore` covers them, and git keeps tracking what it tracks until `git rm --cached` (or `textdb assets migrate-from-git`)",
                loose.len(),
                if loose.len() > 1 { ", …" } else { "" }
            );
        }
    }
    if json {
        emit_json(&json!({
            "prefix": v.prefix,
            "dir": v.dir.display().to_string(),
            "dry_run": o.dry_run,
            "pushed": pushed,
            "bytes": bytes,
            "conflicts": conflicts,
            "failed": failed
        }))?;
    } else {
        let verb = if o.dry_run { "would push" } else { "pushed" };
        // Which directory, always: two per folder is the normal state, and "pushed 0 assets" from
        // the wrong one looked exactly like "pushed 0 assets" from the right one.
        let mut s = format!("{verb} {} assets ({}) from {} ({})\n", pushed.len(), size_text(bytes), v.dir.display(), v.prefix);
        for p in &pushed {
            s.push_str(&format!("  {:<10} {}\n", p["state"].as_str().unwrap_or(""), p["path"].as_str().unwrap_or("")));
        }
        for c in &conflicts {
            s.push_str(&format!("  conflict   {c}\n"));
        }
        for f in &failed {
            s.push_str(&format!("  failed     {f}\n"));
        }
        out(s.as_bytes())?;
    }
    if !failed.is_empty() {
        return Err(StoreError::other(format!("{} assets could not be pushed", failed.len())));
    }
    if !conflicts.is_empty() {
        return Err(StoreError::conflict(format!("{} assets were not pushed because of conflicts", conflicts.len())));
    }
    Ok(())
}

/// Push the vault's `new` and `modified` assets within `scope` (all of them when it is empty), and
/// with `force` its `conflict` ones.
pub(crate) fn push_run(st: &mut dyn Store, v: &Vault, scope: &[String], o: &PushOptions) -> Result<PushReport> {
    check_scope(v, scope)?;
    let scan = scan(st, v)?;
    let mut cache = VaultCache::open(v);
    let found = items(v, &scan, &mut cache, scope)?;
    let mut drivers = Drivers::new(st)?;
    let names = drivers.names();
    // The pointers this directory's last sync had: one only on disk now was deleted in the store.
    let synced: HashSet<String> = match recorded_base(st, &v)? {
        Some(r) => st
            .sync_base(&r.prefix, &r.dir)?
            .map(|b| b.files.into_iter().filter(|f| f.version.is_some()).map(|f| f.rel).collect())
            .unwrap_or_default(),
        None => HashSet::new(),
    };
    let (mut pushed, mut conflicts, mut failed, mut bytes) = (Vec::new(), Vec::new(), Vec::new(), 0u64);
    let mut todo = Vec::new();
    for item in found {
        let deleted = item.pointer.is_some() && item.version.is_none() && synced.contains(&format!("{}{SUFFIX}", item.rel));
        if deleted && matches!(item.state, "new" | "modified" | "conflict") {
            conflicts.push(format!(
                "{}: its pointer was deleted in the store since this directory was synced; sync (which deletes it here), then push the file as a new asset",
                item.path
            ));
            continue;
        }
        match item.state {
            "new" | "modified" => todo.push(item),
            // Only a conflict with an asset file here: a pointer that only names another file (a
            // .env, say) never sends it to the asset store.
            "conflict" if o.force && item.case_of.is_none() && item.file.is_some() => todo.push(item),
            "orphan" if o.force => todo.push(item),
            "conflict" => conflicts.push(format!("{}: {}", item.path, item.note.as_deref().unwrap_or("conflict"))),
            _ => {}
        }
    }
    if o.to.is_none() && todo.iter().any(|i| i.pointer.is_none()) {
        if names.is_empty() {
            return Err(StoreError::invalid("no asset store is declared: textdb assets stores --add NAME --root FOLDER"));
        }
        if names.len() > 1 {
            return Err(StoreError::invalid(format!("several asset stores ({}): choose one for new assets with --to", names.join(", "))));
        }
    }
    let mut in_use = InUse::new();
    let mut written = Vec::new();
    for item in &todo {
        match push_one(st, &v, &mut drivers, &o, &mut cache, &mut in_use, item, &mut written) {
            Outcome::Done(j) => {
                bytes += j["size"].as_u64().unwrap_or(0);
                pushed.push(j);
            }
            Outcome::Conflict(c) => conflicts.push(c),
            Outcome::Failed(f) => failed.push(f),
        }
    }
    cache.save();
    if let Err(e) = record_in_sync_base(st, v, &written) {
        failed.push(format!("the pushed pointers could not be recorded in the sync base ({}); the next sync compares them itself", e.message));
    }
    Ok(PushReport { pushed, bytes, conflicts, failed })
}

/// What a pull did: the assets pulled (as JSON rows), their bytes, and the files kept or failed.
#[derive(Default)]
pub(crate) struct PullReport {
    pub pulled: Vec<serde_json::Value>,
    pub bytes: u64,
    pub kept: Vec<String>,
    pub failed: Vec<String>,
}

/// The assets the notes at or below `path` link to.
pub(crate) fn linked_assets(st: &mut dyn Store, path: &str) -> Result<BTreeSet<String>> {
    Ok(st.links(path, &[])?.into_iter().filter(|l| l.asset).filter_map(|l| l.resolved).collect())
}

pub fn pull(st: &mut dyn Store, paths: &[String], dir: Option<&Path>, linked_from: Option<&str>, author: Option<&str>, dry_run: bool, json: bool) -> Result<()> {
    let v = vault(st, paths.first().map(String::as_str).or(linked_from), dir)?;
    let scope = scope_of(paths)?;
    let linked = linked_from.map(|p| linked_assets(st, p)).transpose()?;
    let PullReport { pulled, bytes, kept, failed } = pull_run(st, &v, &scope, linked.as_ref(), author, dry_run, None)?;
    if json {
        emit_json(&json!({ "dry_run": dry_run, "pulled": pulled, "bytes": bytes, "kept": kept, "failed": failed }))?;
    } else {
        let mut s = format!("{} {} assets ({})\n", if dry_run { "would pull" } else { "pulled" }, pulled.len(), size_text(bytes));
        for p in &pulled {
            s.push_str(&format!("  {:<10} {}\n", p["state"].as_str().unwrap_or(""), p["path"].as_str().unwrap_or("")));
        }
        for k in &kept {
            s.push_str(&format!("  kept       {k}\n"));
        }
        for f in &failed {
            s.push_str(&format!("  failed     {f}\n"));
        }
        out(s.as_bytes())?;
    }
    if !failed.is_empty() {
        return Err(StoreError::other(format!("{} assets could not be pulled", failed.len())));
    }
    Ok(())
}

/// Pull the vault's `not-pulled` and `outdated` assets within `scope` (all of them when it is
/// empty), only those in `linked` when it is given.
pub(crate) fn pull_run(
    st: &mut dyn Store,
    v: &Vault,
    scope: &[String],
    linked: Option<&BTreeSet<String>>,
    author: Option<&str>,
    dry_run: bool,
    stores: Option<&mut StoreFiles>,
) -> Result<PullReport> {
    check_scope(v, scope)?;
    let scan = scan(st, v)?;
    let mut cache = VaultCache::open(v);
    let mut found = items(v, &scan, &mut cache, scope)?;
    // The caller's stores when it has them -- a sync's pull, states and deletions then list a drive
    // and open its drivers once between them -- and this command's own otherwise.
    let mut owned = None;
    let files = match stores {
        Some(files) => files,
        None => owned.insert(StoreFiles::new(st)?),
    };
    // What the stores hold is asked before anything is fetched, so bytes someone replaced in a store
    // are followed rather than refused as the wrong ones. A dry run asks nothing of anyone's
    // provider: it says what a pull would do here, and reaching a drive to look is not that.
    let mut kept: Vec<String> = match dry_run {
        true => Vec::new(),
        false => mark_store_states(&mut found, |store, item| files.found(store, item)),
    };
    let (mut pulled, mut failed, mut bytes) = (Vec::new(), Vec::new(), 0u64);
    let mut written: Vec<Written> = Vec::new();
    for item in found {
        if linked.is_some_and(|l| !l.contains(&item.path)) {
            continue;
        }
        match item.state {
            // `changed-in-store`: someone put other bytes in that very file, and a pull takes them
            // as the pointer's new version, the copy here going to the vault's trash first.
            "not-pulled" | "outdated" | "changed-in-store" => {}
            "modified" => {
                kept.push(format!("{}: changed here, so not replaced (push it, or delete it and pull)", item.path));
                continue;
            }
            "conflict" => {
                kept.push(format!("{}: {}", item.path, item.note.as_deref().unwrap_or("conflict")));
                continue;
            }
            _ => continue,
        }
        let Some(p) = &item.pointer else { continue };
        if dry_run {
            bytes += p.size;
            pulled.push(json!({ "path": item.path, "state": item.state, "size": p.size, "store": p.store }));
            continue;
        }
        let d = match files.driver(&p.store) {
            Ok(d) => d,
            Err(e) => {
                failed.push(format!("{}: {e}", item.path));
                continue;
            }
        };
        let file = item.file.clone().unwrap_or_else(|| item.rel.clone());
        // Never through a link or junction already in the directory: its folder may be anywhere.
        if through_link(&v.dir, &file) || through_link(&v.dir, ".textdb/trash/x") {
            failed.push(format!("{}: {file} is reached through a link or junction on disk, which pull never writes through", item.path));
            continue;
        }
        let dest = v.dir.join(&file);
        let part = driver::partial(&dest);
        // The bytes must be the ones the pointer names -- except for `changed-in-store`, where the
        // point is that they are not: someone replaced them in the store, and these are taken as
        // the pointer's new version, so what arrived is what the pointer will say.
        // The fact, not the state: an asset waiting to be pulled, or one already outdated here, has
        // its own state and must follow the store's bytes all the same -- keying this on the state
        // would refuse them the very bytes they came for.
        let follows_store = item.store_changed;
        // A pointer this directory has not caught up with, or one the store has not got at all, is
        // not one to commit over: a pull writes no pointer the store never agreed to.
        if follows_store && (item.version.is_none() || item.disk_differs) {
            kept.push(format!(
                "{}: the asset store holds other bytes than its pointer names, and this directory's pointer is not the store's yet: sync first",
                item.path
            ));
            continue;
        }
        let fetched = dest
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(|e| e.to_string())
            .and_then(|_| d.get(&item.path, p.item.as_deref(), &part).map_err(|e| e.message))
            .and_then(|_| hash_file(&part).map_err(|e| e.to_string()))
            .and_then(|(sha, size)| match (follows_store, sha == p.sha256 && size == p.size) {
                (false, false) => Err("the asset store holds other bytes than its pointer names (textdb assets verify)".to_string()),
                _ => Ok((sha, size)),
            });
        let (sha, size) = match fetched {
            Ok(got) => got,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                failed.push(format!("{}: {e}", item.path));
                continue;
            }
        };
        // The pointer first, then the bytes. A commit that fails must leave the vault exactly as it
        // was: once the file is in place, the asset reads as changed here, which a pull skips, so
        // the store's bytes could never be taken again.
        let mut committed = None;
        if follows_store {
            let mut told = p.clone();
            (told.sha256, told.size) = (sha.clone(), size);
            let pointer_path = format!("{}{SUFFIX}", item.path);
            // Read again right before writing, as a push does: a write against the version this scan
            // saw would otherwise be merged line by line with a pointer someone else committed
            // meanwhile, leaving one that names bytes no file holds.
            let wrote = match st.stat(&pointer_path).ok().and_then(|s| s.version) == item.version {
                false => Err(StoreError::conflict("its pointer changed in the store during this pull; run it again".to_string())),
                true => st
                    .write(&pointer_path, told.to_text().as_bytes(), item.version, author, Some("assets pull: the asset store's bytes"))
                    .and_then(|w| match w.kind.as_str() {
                        "direct" | "noop" => Ok(w),
                        other => Err(StoreError::conflict(format!("its pointer was {other} against a commit made during this pull; run it again"))),
                    }),
            };
            match wrote {
                Ok(w) => committed = Some(Written { path: pointer_path, version: w.version, text: told.to_text() }),
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    failed.push(format!("{}: the asset store's bytes were not taken as its new version: {}", item.path, e.message));
                    continue;
                }
            }
        }
        if item.state == "outdated" || follows_store {
            // What is here is what this directory last had: kept in the vault's trash, not lost.
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |t| t.subsec_nanos());
            let trashed = v.dir.join(".textdb").join("trash").join(format!("{}-{nanos:09}", driver::stamp(SystemTime::now()))).join(&file);
            let moved = trashed.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| driver::rename_new(&dest, &trashed));
            if let Err(e) = moved {
                let _ = std::fs::remove_file(&part);
                failed.push(format!("{}: could not move the old file aside ({e})", item.path));
                continue;
            }
        }
        if let Err(e) = driver::rename_new(&part, &dest) {
            let _ = std::fs::remove_file(&part);
            failed.push(format!("{}: not put in place ({e})", item.path));
            continue;
        }
        // The pointer was committed before the bytes were placed, so what is left is to write it
        // where a push writes it: to disk, and into this directory's sync base. Without that, every
        // followed asset reads as "the pointer on disk is not the store's" and push refuses it.
        if let Some(w) = committed {
            let on_disk = v.dir.join(format!("{}{SUFFIX}", item.rel));
            if let Err(e) = std::fs::write(&on_disk, w.text.as_bytes()) {
                failed.push(format!("{}: its pointer is committed but could not be written to {} ({e}); the next sync writes it", item.path, on_disk.display()));
                continue;
            }
            written.push(w);
        }
        if let Ok(meta) = std::fs::metadata(&dest) {
            cache.remember(&file, meta.len(), mtime_ns(&meta), &sha);
        }
        cache.saw(&item.rel, &sha);
        cache.add_present(&file);
        bytes += size;
        pulled.push(json!({ "path": item.path, "state": item.state, "size": size, "store": p.store }));
    }
    cache.save();
    // Recorded as synced only now that both sides have them, as a push records what it wrote: a
    // base that does not know of these pointers would have the next sync take them in again.
    if let Err(e) = record_in_sync_base(st, v, &written) {
        kept.push(format!("the pointers this pull committed were not recorded in this directory's sync base ({}): the next sync compares them", e.message));
    }
    Ok(PullReport { pulled, bytes, kept, failed })
}

/// What a relocate did: the files moved in their stores, what it left and why, and what failed.
#[derive(Default)]
pub(crate) struct RelocateReport {
    pub moved: Vec<serde_json::Value>,
    pub kept: Vec<String>,
    pub failed: Vec<String>,
}

pub fn relocate(st: &mut dyn Store, paths: &[String], dir: Option<&Path>, author: Option<&str>, dry_run: bool, json: bool) -> Result<()> {
    let v = vault(st, paths.first().map(String::as_str), dir)?;
    let scope = scope_of(paths)?;
    let RelocateReport { moved, kept, failed } = relocate_run(st, &v, &scope, author, dry_run)?;
    if json {
        emit_json(&json!({ "dry_run": dry_run, "moved": moved, "kept": kept, "failed": failed }))?;
    } else {
        let mut s = format!("{} {} assets in their stores\n", if dry_run { "would move" } else { "moved" }, moved.len());
        for m in &moved {
            s.push_str(&format!("  {} -> {}\n", m["from"].as_str().unwrap_or(""), m["to"].as_str().unwrap_or("")));
        }
        for k in &kept {
            s.push_str(&format!("  kept       {k}\n"));
        }
        for f in &failed {
            s.push_str(&format!("  failed     {f}\n"));
        }
        out(s.as_bytes())?;
    }
    if !failed.is_empty() {
        return Err(StoreError::other(format!("{} assets could not be moved in their stores", failed.len())));
    }
    Ok(())
}

/// Move the file of every `moved-here` asset within `scope` to the asset's own path in its store,
/// keeping the provider's file and so its id. Nothing is moved onto bytes already there, and a file
/// another pointer also names stays where it is.
pub(crate) fn relocate_run(st: &mut dyn Store, v: &Vault, scope: &[String], author: Option<&str>, dry_run: bool) -> Result<RelocateReport> {
    check_scope(v, scope)?;
    let scan = scan(st, v)?;
    let mut cache = VaultCache::open(v);
    let mut found = items(v, &scan, &mut cache, scope)?;
    let mut files = StoreFiles::new(st)?;
    let mut r = RelocateReport::default();
    r.kept.extend(mark_store_states(&mut found, |store, item| files.found(store, item)));
    for item in &found {
        let (Some(p), Some(at)) = (&item.pointer, &item.in_store) else { continue };
        let Some(named) = p.item.as_deref() else { continue };
        let row = json!({ "path": item.path, "from": at, "to": item.path, "store": p.store });
        // A file the store cannot place is not one to move: two of a name, in the store's own trash,
        // or none of the store's at all. Nor one whose bytes there are not the pointer's: moving it
        // would commit a pointer still naming the old ones -- pull or push it first.
        if matches!(item.state, "ambiguous" | "trashed-in-store" | "invalid-item") || item.store_changed {
            r.kept.push(format!("{}: {}", item.path, item.note.as_deref().unwrap_or("its file in the asset store cannot be moved")));
            continue;
        }
        // A pointer this directory has not caught up with is not the one to move a store's file by.
        if item.disk_differs {
            r.kept.push(format!("{}: its pointer changed in the store since this directory was synced; sync first", item.path));
            continue;
        }
        if dry_run {
            // What would keep the file where it is is known without moving anything, so a dry run
            // says `kept` for those rather than promising a move it would not make.
            match files.keeps(st, &p.store, named, &item.path)? {
                Some(StoreCopy::LeftBecause(why)) => r.kept.push(format!("{}: {why}", item.path)),
                Some(_) => r.kept.push(format!("{}: another pointer names those bytes, so they stay where they are", item.path)),
                None => r.moved.push(row),
            }
            continue;
        }
        match files.moved(st, &p.store, named, &item.path, &item.path) {
            // The item the pointer names after the move: on Google Drive the file's id, which a move
            // leaves alone. The pointer is written all the same, because it records where the store
            // keeps the bytes, and that is what just changed -- against the version this scan saw,
            // so a pointer someone else committed meanwhile is not written over.
            Ok(StoreMove::Moved(now)) if now.as_deref().is_none_or(|now| now == named) => {
                let mut settled = p.clone();
                settled.item_path = Some(item.path.clone());
                let pointer_path = format!("{}{SUFFIX}", item.path);
                // Read again right before writing, as a push does: a write against a version this
                // scan saw would otherwise be merged line by line with a pointer someone else
                // committed meanwhile, leaving one that names bytes no file holds.
                if st.stat(&pointer_path).ok().and_then(|s| s.version) != item.version {
                    r.failed.push(format!(
                        "{}: its file is at the asset's own path in the store now, but its pointer changed in the store meanwhile; run it again",
                        item.path
                    ));
                    continue;
                }
                match st.write(&pointer_path, settled.to_text().as_bytes(), item.version, author, Some("assets relocate")).and_then(|w| match w.kind.as_str() {
                    "direct" | "noop" => Ok(w),
                    other => Err(StoreError::conflict(format!("its pointer was {other} against another commit"))),
                }) {
                    Ok(_) => r.moved.push(row),
                    Err(e) => r.failed.push(format!(
                        "{}: its file is at the asset's own path in the store now, but the pointer saying so was not committed ({}); run it again",
                        item.path, e.message
                    )),
                }
            }
            Ok(StoreMove::Moved(now)) => r.failed.push(format!(
                "{}: its file moved in the asset store, but the store now names it {} and not {named}: push it, so its pointer names what the store does",
                item.path,
                now.as_deref().unwrap_or("")
            )),
            Ok(StoreMove::LeftBecause(why)) => r.kept.push(format!("{}: {why}", item.path)),
            Ok(StoreMove::Left) => r.kept.push(format!("{}: another pointer names those bytes, so they stay where they are", item.path)),
            Err(e) => r.failed.push(format!("{}: {}", item.path, e.message)),
        }
    }
    // A move whose pointer commit failed leaves the file at the asset's own path with the pointer
    // still recording where the store used to keep it: nothing to move, only the record to settle.
    // Until it is settled, the next status blames the store for a move nobody made. The version is
    // read again first, so an asset this run has just written is left alone.
    for item in found.iter().filter(|i| !dry_run && i.in_store.is_none() && !i.disk_differs) {
        let Some(p) = &item.pointer else { continue };
        let Some(named) = p.item.as_deref() else { continue };
        if named.starts_with('/') || p.item_path.as_deref().is_none_or(|r| location_key(r) == location_key(&item.path)) {
            continue;
        }
        let mut settled = p.clone();
        settled.item_path = Some(item.path.clone());
        let pointer_path = format!("{}{SUFFIX}", item.path);
        if st.stat(&pointer_path).ok().and_then(|s| s.version) != item.version {
            continue;
        }
        match st.write(&pointer_path, settled.to_text().as_bytes(), item.version, author, Some("assets relocate: where its store keeps it")) {
            Ok(_) => r.moved.push(json!({ "path": item.path, "from": item.path, "to": item.path, "store": p.store, "record_only": true })),
            Err(e) => r.failed.push(format!(
                "{}: its file is at the asset's own path in the store, but the pointer saying so was not committed ({}); run it again",
                item.path, e.message
            )),
        }
    }
    cache.save();
    Ok(r)
}

pub fn verify(st: &mut dyn Store, path: Option<&str>, dir: Option<&Path>, json: bool) -> Result<()> {
    let v = vault(st, path, dir)?;
    let scan = scan(st, &v)?;
    let scope = scope_of(&path.map(str::to_string).into_iter().collect::<Vec<_>>())?;
    let mut cache = VaultCache::open(&v);
    cache.fresh = true;
    let found = items(&v, &scan, &mut cache, &scope)?;
    cache.save();
    let mut drivers = Drivers::new(st)?;
    let mut rows = Vec::new();
    let mut problems = 0;
    for item in &found {
        let in_store = match (&item.pointer, item.state) {
            (_, "invalid-pointer" | "invalid-path") => {
                problems += 1;
                "-".to_string()
            }
            (None, _) => "not pushed".to_string(),
            (Some(p), _) => {
                let checked = match drivers.get(&p.store) {
                    Err(e) => Err(e),
                    Ok(d) => d.hash(&item.path, p.item.as_deref()).map_err(|e| e.message),
                };
                match checked {
                    Ok(Some((sha, _))) if sha == p.sha256 => "ok".to_string(),
                    Ok(Some(_)) => {
                        problems += 1;
                        "differs".to_string()
                    }
                    Ok(None) => {
                        problems += 1;
                        "missing".to_string()
                    }
                    Err(e) => {
                        problems += 1;
                        format!("unchecked: {e}")
                    }
                }
            }
        };
        rows.push(json!({ "path": item.path, "here": item.state, "asset_store": in_store, "note": item.note }));
    }
    // Files in a store that no pointer names: an upload someone made by hand, a leftover of a push
    // that never committed, the file of a pointer since deleted. Compared with what every pointer in
    // the store names, not only this vault's, since other folders' assets live in the same store.
    // Told of, never counted as problems: whose files those are is not for textdb to decide. Only
    // for a whole vault -- a check of one asset has no business listing every file of a drive.
    let mut unnamed = Vec::new();
    if scope.is_empty() {
        let mut in_use = InUse::new();
        // What a store's files are for is never guessed at from the pointers that happened to parse.
        // One pointer textdb cannot read and it names none of them, saying why instead: this list is
        // read as "nothing needs these bytes", and someone acts on it by hand in their own drive.
        let named = match in_use.refresh(st) {
            Err(e) => Err(format!("the store's pointers could not be read ({})", e.message)),
            // The reason `keeps` gives, for the same cause: an account sees its own pointers, so
            // files another account's pointers name would be listed here as named by nothing -- and
            // this list is acted on by hand, in somebody's drive.
            Ok(()) if in_use.delegated.unwrap_or(true) => Err(
                "this session is an account's, which sees only its own pointers, so what no pointer names cannot be told from here".to_string(),
            ),
            Ok(()) => {
                let mut unreadable: Vec<&str> = in_use.pointers.iter().filter(|(_, (_, names))| names.is_none()).map(|(p, _)| p.as_str()).collect();
                unreadable.sort();
                match unreadable.first() {
                    Some(first) => Err(match unreadable.len() {
                        1 => format!("the pointer {first} cannot be read, so what the store's files are for is not known"),
                        n => format!("{n} pointers cannot be read ({first} among them), so what the store's files are for is not known"),
                    }),
                    // A pointer an earlier build wrote names its file by the path it sits at, one
                    // pushed since by the id the drive keeps it under: a file is named when either
                    // says so, or every asset not pushed again since would read as named by nothing.
                    None => Ok(in_use
                        .pointers
                        .iter()
                        .filter_map(|(pointer, (_, names))| names.as_ref().map(|(store, key)| (pointer, store, key)))
                        .flat_map(|(pointer, store, key)| [(store.clone(), key.clone()), (store.clone(), location_key(asset_path(pointer)))])
                        .collect::<HashSet<(String, String)>>()),
                }
            }
        };
        let stores: BTreeSet<String> = found.iter().filter_map(|i| i.pointer.as_ref().map(|p| p.store.clone())).collect();
        for store in stores {
            let listed = match &named {
                // Said of the store itself: it is why none of its files are spoken for here.
                Err(why) => Err(StoreError::other(why.clone())),
                Ok(_) => match drivers.get(&store) {
                    Ok(d) => d.files(),
                    Err(e) => Err(StoreError::other(e)),
                },
            };
            match (listed, &named) {
                (Ok(Some(files)), Ok(named)) => unnamed.extend(
                    files
                        .into_iter()
                        .filter(|(item, at)| {
                            !named.contains(&(store.clone(), location_key(item))) && !named.contains(&(store.clone(), location_key(at)))
                        })
                        .map(|(_, at)| json!({ "store": store, "at": at })),
                ),
                (Ok(_), _) => {}
                (Err(e), _) => unnamed.push(json!({ "store": store, "unchecked": e.message })),
            }
        }
    }
    if json {
        emit_json(&json!({ "prefix": v.prefix, "dir": v.dir.display().to_string(), "assets": rows, "unnamed": unnamed, "problems": problems }))?;
    } else {
        let mut s = format!("verified {} assets of {} in {}\n", rows.len(), v.prefix, v.dir.display());
        for r in rows.iter().filter(|r| r["here"] != "ok" || r["asset_store"] != "ok") {
            s.push_str(&format!(
                "  {}  here: {}, asset store: {}\n",
                r["path"].as_str().unwrap_or(""),
                r["here"].as_str().unwrap_or(""),
                r["asset_store"].as_str().unwrap_or("")
            ));
        }
        for u in &unnamed {
            match u["unchecked"].as_str() {
                Some(why) => s.push_str(&format!("  the asset store {} could not be listed: {why}\n", u["store"].as_str().unwrap_or(""))),
                None => s.push_str(&format!("  {} in the asset store {}: no pointer names it\n", u["at"].as_str().unwrap_or(""), u["store"].as_str().unwrap_or(""))),
            }
        }
        s.push_str(&format!("{problems} problems\n"));
        out(s.as_bytes())?;
    }
    if problems > 0 {
        return Err(StoreError::other(format!("verify found {problems} problems")));
    }
    Ok(())
}

const GITIGNORE_BEGIN: &str = "# BEGIN textdb assets (written by `textdb assets gitignore`: binaries live in the asset store, their .tdbasset pointers in git)";
const GITIGNORE_END: &str = "# END textdb assets";

/// `rel` as a `.gitignore` pattern for that one file: from the top, glob characters escaped.
fn exact_pattern(rel: &str) -> String {
    let mut s = String::from("/");
    for c in rel.chars() {
        if matches!(c, '\\' | '*' | '?' | '[') {
            s.push('\\');
        }
        s.push(c);
    }
    if s.ends_with(' ') {
        s.pop();
        s.push_str("\\ ");
    }
    s
}

/// Lines for the single files `patterns` get wrong: one that is not an asset they would ignore (a
/// document under a `binary` rule, say), and an asset they would not (one only its bytes show).
fn single_file_lines(root: &Path, files: &Files, classifier: &Classifier, cache: &mut VaultCache, patterns: &[String]) -> Vec<String> {
    // Checked as git on Linux matches (case matters) and on Windows and macOS (it does not).
    let matcher = |insensitive: bool| {
        let mut b = GitignoreBuilder::new("");
        b.case_insensitive(insensitive).ok();
        for p in patterns {
            let _ = b.add_line(None, p);
        }
        b.build().ok()
    };
    let (Some(exact), Some(folded)) = (matcher(false), matcher(true)) else { return Vec::new() };
    let mut out = Vec::new();
    for (rel, &(size, mtime)) in files {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let asset = match classifier.rule_class(rel) {
            Class::Asset => true,
            Class::Other => !name.starts_with(".git") && cache.binary(root, rel, size, mtime),
            Class::Document => false,
            Class::Ignore | Class::Pointer => continue,
        };
        let ignored = [&exact, &folded].map(|m| m.matched_path_or_any_parents(rel, false).is_ignore());
        if asset && !(ignored[0] && ignored[1]) {
            out.push(exact_pattern(rel));
        } else if !asset && (ignored[0] || ignored[1]) {
            out.push(format!("!{}", exact_pattern(rel)));
        }
    }
    out
}

/// `existing` with the managed block replaced by `block`, or `block` put first, so the lines
/// written after it (the user's own) take precedence over it.
fn with_block(existing: &str, block: &str, nl: &str) -> String {
    if let (Some(a), Some(b)) = (existing.find(GITIGNORE_BEGIN), existing.find(GITIGNORE_END)) {
        if b > a {
            let end = existing[b..].find('\n').map_or(existing.len(), |i| b + i + 1);
            return format!("{}{block}{}", &existing[..a], &existing[end..]);
        }
    }
    if existing.is_empty() {
        block.to_string()
    } else {
        format!("{block}{nl}{existing}")
    }
}

pub fn gitignore(st: &mut dyn Store, path: Option<&str>, dir: Option<&Path>, dry_run: bool, json: bool) -> Result<()> {
    let root = match dir {
        Some(d) => d.to_path_buf(),
        None => vault(st, path, None)?.dir,
    };
    let GitignoreBlock { file, changed, patterns, single } = write_gitignore_block(st, &root, dry_run)?;
    if json {
        return emit_json(&json!({ "file": file.display().to_string(), "changed": changed, "dry_run": dry_run, "patterns": patterns, "single_files": single }));
    }
    let what = match (changed, dry_run) {
        (false, _) => "up to date",
        (true, true) => "would be updated",
        (true, false) => "updated",
    };
    out(format!(
        "{}: {what}, {} patterns ({single} of them for single files the others get wrong; run it again as files come and go). Files git already tracks stay tracked until they are removed from its index (git rm --cached, or textdb assets migrate-from-git).\n",
        file.display(),
        patterns.len(),
    )
    .as_bytes())
}

/// What writing the managed `.gitignore` block did.
pub(crate) struct GitignoreBlock {
    pub file: PathBuf,
    pub changed: bool,
    pub patterns: Vec<String>,
    /// How many of the patterns name single files.
    pub single: usize,
}

/// Write the managed block of the `.gitignore` in the directory `root` (not with `dry_run`).
pub(crate) fn write_gitignore_block(st: &mut dyn Store, root: &Path, dry_run: bool) -> Result<GitignoreBlock> {
    let (mut files, nested) = walk(root)?;
    leave_out_asset_stores(st, root, &mut files)?;
    let classifier = Classifier::load(root, &nested);
    for w in &classifier.warnings {
        eprintln!("warning: {w}");
    }
    let mut patterns = classifier.gitignore_patterns();
    let mut cache = VaultCache::open(&Vault { prefix: "/".to_string(), dir: root.to_path_buf() });
    let single = single_file_lines(root, &files, &classifier, &mut cache, &patterns);
    cache.save();
    // Before the last line, which keeps every pointer in git.
    let last = patterns.pop();
    patterns.extend(single.iter().cloned());
    patterns.extend(last);
    let file = root.join(".gitignore");
    let existing = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_err(file.display(), e)),
    };
    let nl = if existing.contains("\r\n") { "\r\n" } else { "\n" };
    let block: String = std::iter::once(GITIGNORE_BEGIN.to_string())
        .chain(patterns.iter().cloned())
        .chain(std::iter::once(GITIGNORE_END.to_string()))
        .map(|l| format!("{l}{nl}"))
        .collect();
    let updated = with_block(&existing, &block, nl);
    let changed = updated != existing;
    if changed && !dry_run {
        std::fs::write(&file, &updated).map_err(|e| io_err(file.display(), e))?;
    }
    Ok(GitignoreBlock { file, changed, patterns, single: single.len() })
}

pub struct StoresOptions {
    pub add: Option<String>,
    pub driver: String,
    pub root: Option<String>,
    pub remove: Option<String>,
    pub bind: Option<String>,
}

pub fn stores(st: &mut dyn Store, o: StoresOptions, json: bool) -> Result<()> {
    if let Some(name) = &o.add {
        if !pointer::valid_store_name(name) {
            return Err(StoreError::invalid(format!("{name}: an asset store name is letters, digits, _, - and .")));
        }
        if !driver::DRIVERS.contains(&o.driver.as_str()) {
            return Err(StoreError::invalid(format!("unknown driver {}: {}", o.driver, driver::DRIVERS.join(" or "))));
        }
        let root = o
            .root
            .clone()
            .filter(|r| !r.is_empty())
            .ok_or_else(|| StoreError::invalid("--add needs --root: the folder (or rclone remote path) the asset store keeps its files in"))?;
        if let Some(problem) = (o.driver == "rclone").then(|| rclone::shared_root_problem(&root)).flatten() {
            return Err(StoreError::invalid(problem));
        }
        // A local root is created here rather than left for the first push to fail on: `--add`
        // then `push` said "the folder is not there", with nothing in between to have made it.
        if o.driver == "local" && !std::path::Path::new(&root).exists() {
            std::fs::create_dir_all(&root).map_err(|e| StoreError::other(format!("{root}: {e}")))?;
        }
        st.put_asset_store(&AssetStore { name: name.clone(), driver: o.driver.clone(), root, options: None, created_at: None })?;
    }
    if let Some(name) = &o.remove {
        if !st.remove_asset_store(name)? {
            return Err(StoreError::not_found(format!("no asset store named {name}")));
        }
    }
    if let Some(spec) = &o.bind {
        let (name, location) = spec.split_once('=').ok_or_else(|| StoreError::invalid("--bind NAME=FOLDER (NAME= removes the binding)"))?;
        let file = driver::binding::set(name, Some(location).filter(|l| !l.is_empty()))?;
        eprintln!("{name}: {} in {}", if location.is_empty() { "binding removed".to_string() } else { format!("bound to {location}") }, file.display());
    }
    let rows: Vec<serde_json::Value> = st
        .asset_stores()?
        .iter()
        .map(|s| {
            let bound = driver::binding::get(&s.name);
            let problem = driver::open(s).err().map(|e| e.message);
            json!({
                "name": s.name,
                "driver": s.driver,
                "root": s.root,
                "bound_to": bound.as_ref().map(|b| &b.0),
                "bound_by": bound.as_ref().map(|b| &b.1),
                "reachable": problem.is_none(),
                "problem": problem,
            })
        })
        .collect();
    if json {
        return emit_json(&rows);
    }
    if rows.is_empty() {
        return out(b"no asset stores: textdb assets stores --add NAME --root FOLDER\n");
    }
    let mut s = String::new();
    for r in &rows {
        let reach = match (&r["bound_to"], &r["problem"]) {
            (_, serde_json::Value::String(p)) => format!("not reachable: {p}"),
            (serde_json::Value::String(b), _) => format!("bound to {b} ({})", r["bound_by"].as_str().unwrap_or("")),
            _ => "reachable at its root".to_string(),
        };
        s.push_str(&format!("{}  {}  {}  {reach}\n", r["name"].as_str().unwrap_or(""), r["driver"].as_str().unwrap_or(""), r["root"].as_str().unwrap_or("")));
    }
    out(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gitignore_block_goes_first_and_is_replaced_in_place() {
        let block = format!("{GITIGNORE_BEGIN}\n*.png\n{GITIGNORE_END}\n");
        assert_eq!(with_block("", &block, "\n"), block);
        assert_eq!(with_block("target/\n", &block, "\n"), format!("{block}\ntarget/\n"));
        let placed = with_block("a\n", &block, "\n");
        let other = format!("{GITIGNORE_BEGIN}\n*.pdf\n{GITIGNORE_END}\n");
        assert_eq!(with_block(&placed, &other, "\n"), format!("{other}\na\n"));
        assert_eq!(size_text(1536), "1.5 KB");
        assert!(under("/", "/a") && under("/a", "/a/b") && !under("/a", "/ab"));
        assert_eq!(rel_under("/notes", "/notes/img/a.png").as_deref(), Some("img/a.png"));
        assert_eq!(rel_under("/notes", "/notesx/a.png"), None);
    }

    #[test]
    fn portable_names() {
        for ok in ["img/a.png", "a b/c.pdf", "ünï/ø.jpg", "icons/x.png", "com10.png"] {
            assert!(portable_rel(ok), "{ok}");
        }
        for bad in ["", "a//b.png", "../x.png", "a/./b", "C:x.png", "a\\b.png", "x.png.", "x ", "CON.png", "con.d/x.png", "lpt1", "a/nul.txt", "q?.png"] {
            assert!(!portable_rel(bad), "{bad}");
        }
    }

    /// An asset at `/img/a.png` whose pointer names a Drive id, in the state `state`, with
    /// `item_path` recording where the store last had the bytes.
    fn asset(state: &'static str, item_path: Option<&str>) -> Item {
        let v = Vault { prefix: "/".to_string(), dir: PathBuf::from("/vault") };
        let mut item = Item::new(&v, "img/a.png");
        item.state = state;
        item.pointer = Some(Pointer {
            id: "01926d1e-7b4a-7c1e-9d2f-3a5b6c7d8e9f".into(),
            sha256: "9f".repeat(32),
            size: 11,
            media_type: "image/png".into(),
            store: "drive".into(),
            item: Some("1AbCdEfGhIjKlMnO".into()),
            item_path: item_path.map(str::to_string),
            extra: vec![],
        });
        item
    }

    /// What a store says of it: at `path`, those bytes, live and alone unless said otherwise.
    fn holds(path: &str, sha256: Option<&str>, size: u64) -> driver::Found {
        driver::Found { path: path.to_string(), sha256: sha256.map(str::to_string), size, trashed: false, two_of_a_name: false }
    }

    /// Ask the pass one question and give it one answer.
    fn state_of(mut item: Item, found: Result<Option<driver::Found>>) -> Item {
        let mut answer = Some(found);
        let items = std::slice::from_mut(&mut item);
        mark_store_states(items, |_, _| answer.take().unwrap_or(Ok(None)));
        item
    }

    #[test]
    fn what_a_store_holds_gives_an_asset_its_state_and_never_hides_what_the_bytes_here_say() {
        let same = "9f".repeat(32);
        let other = "ab".repeat(32);

        // Where it is and what it holds, all as the pointer says: nothing to tell.
        let plain = state_of(asset("ok", Some("/img/a.png")), Ok(Some(holds("/img/a.png", Some(&same), 11))));
        assert_eq!(plain.state, "ok");
        assert!(plain.note.is_none() && !plain.store_told && !plain.store_changed);

        // No file of that item at all: purged from the store's trash, or outside the store.
        let gone = state_of(asset("ok", None), Ok(None));
        assert_eq!(gone.state, "invalid-item");
        assert!(gone.note.as_deref().unwrap_or("").contains("no file of item"));

        // Two files of one name, and its own trash: the trash first, since which of two files an
        // item means says nothing about one that is in the trash and still fetched by id.
        let two = state_of(asset("ok", Some("/img/a.png")), Ok(Some(driver::Found { two_of_a_name: true, ..holds("/img/a.png", Some(&same), 11) })));
        assert_eq!(two.state, "ambiguous");
        let binned = state_of(
            asset("ok", Some("/img/a.png")),
            Ok(Some(driver::Found { trashed: true, two_of_a_name: true, ..holds("/img/a.png", Some(&same), 11) })),
        );
        assert_eq!(binned.state, "trashed-in-store", "a file in the store's trash was called ambiguous");

        // Other bytes in that very file, told as a fact and not only as a state, so an asset
        // waiting to be pulled follows them too.
        let changed = state_of(asset("ok", Some("/img/a.png")), Ok(Some(holds("/img/a.png", Some(&other), 11))));
        assert_eq!(changed.state, "changed-in-store");
        assert!(changed.store_changed);
        let unpulled = state_of(asset("not-pulled", Some("/img/a.png")), Ok(Some(holds("/img/a.png", Some(&other), 11))));
        assert_eq!(unpulled.state, "not-pulled", "an asset waiting to be pulled lost that state");
        assert!(unpulled.store_changed && unpulled.store_told);

        // No SHA-256 from the provider (an old upload): a size that is not the pointer's still says
        // the bytes are other ones.
        let by_size = state_of(asset("ok", Some("/img/a.png")), Ok(Some(holds("/img/a.png", None, 12))));
        assert!(by_size.store_changed && by_size.state == "changed-in-store");
        let same_size = state_of(asset("ok", Some("/img/a.png")), Ok(Some(holds("/img/a.png", None, 11))));
        assert_eq!(same_size.state, "ok");

        // Changed here and there: neither side is taken over the other.
        let both = state_of(asset("modified", Some("/img/a.png")), Ok(Some(holds("/img/a.png", Some(&other), 11))));
        assert_eq!(both.state, "conflict");
        assert!(both.note.as_deref().unwrap_or("").contains("changed here and in the asset store"));

        // Which side moved it: the record says where the store last had the bytes.
        let by_us = state_of(asset("ok", Some("/old/a.png")), Ok(Some(holds("/old/a.png", Some(&same), 11))));
        assert_eq!(by_us.state, "moved-here");
        assert_eq!(by_us.in_store.as_deref(), Some("/old/a.png"));
        let by_them = state_of(asset("ok", Some("/img/a.png")), Ok(Some(holds("/moved/a.png", Some(&same), 11))));
        assert_eq!(by_them.state, "moved-in-store");
        // A pointer written before the record existed: moved, but not which side did it.
        let unrecorded = state_of(asset("ok", None), Ok(Some(holds("/old/a.png", Some(&same), 11))));
        assert_eq!(unrecorded.state, "moved-here");
        assert!(unrecorded.note.as_deref().unwrap_or("").contains("does not say where the store last had it"));

        // Replaced *and* moved: both facts told, the change taking the state.
        let twice = state_of(asset("ok", Some("/img/a.png")), Ok(Some(holds("/moved/a.png", Some(&other), 11))));
        assert_eq!(twice.state, "changed-in-store");
        let told = twice.note.as_deref().unwrap_or("");
        assert!(told.contains("other bytes") && told.contains("/moved/a.png"), "{told}");

        // A state the bytes here gave keeps it, and is told of the store's answer as well.
        for state in ["modified", "outdated", "conflict", "invalid-pointer"] {
            let kept = state_of(asset(state, Some("/img/a.png")), Ok(Some(driver::Found { trashed: true, ..holds("/img/a.png", Some(&same), 11) })));
            assert_eq!(kept.state, state, "the store's answer took the state {state}");
            assert!(kept.store_told && kept.note.is_some());
        }

        // A store that cannot be asked changes nothing and is named.
        let mut unasked = asset("ok", Some("/img/a.png"));
        let items = std::slice::from_mut(&mut unasked);
        let said = mark_store_states(items, |_, _| Err(StoreError::other("gdrive: not reached".to_string())));
        assert_eq!(unasked.state, "ok");
        assert_eq!(said.len(), 1);
        assert!(said[0].contains("drive") && said[0].contains("not reached"), "{:?}", said);

        // An item that is a path is nothing to ask about: a local store answers none of this.
        let mut local = asset("ok", None);
        local.pointer.as_mut().unwrap().item = Some("/img/a.png".to_string());
        let items = std::slice::from_mut(&mut local);
        let quiet = mark_store_states(items, |_, _| panic!("a store keeping its files by path was asked"));
        assert!(quiet.is_empty() && local.state == "ok" && !local.store_told);
    }
}
