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
pub mod pointer;

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
                if name == ".gitattributes" && !rel_dir.is_empty() {
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

fn scan(st: &mut dyn Store, v: &Vault) -> Result<Scan> {
    let (mut disk, nested) = walk(&v.dir)?;
    // An asset store kept inside the vault holds copies, not assets of it.
    for s in st.asset_stores()? {
        let Ok(d) = driver::open(&s) else { continue };
        let Some(rel) = d.local_root().and_then(|root| dir_inside(&v.dir, root)) else { continue };
        eprintln!("warning: asset store {} keeps its files in {}, inside {}: they are left out", s.name, rel, v.dir.display());
        if rel.is_empty() {
            disk.clear();
        } else {
            let inside = format!("{}/", rel.to_lowercase());
            disk.retain(|r, _| !r.to_lowercase().starts_with(&inside));
        }
    }
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
    #[serde(skip)]
    path: Option<PathBuf>,
    /// Where an older build kept this cache, removed once it is saved here.
    #[serde(skip)]
    legacy: Option<PathBuf>,
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
            self.dirty = true;
        }
    }

    /// This directory has the bytes `sha` for the asset `rel`.
    fn saw(&mut self, rel: &str, sha: &str) {
        if self.seen.get(rel).map(String::as_str) != Some(sha) {
            self.seen.insert(rel.to_string(), sha.to_string());
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
            self.dirty = true;
        }
        b
    }

    fn save(&self) {
        if let (true, Some(path)) = (self.dirty, &self.path) {
            // Another process may have saved meanwhile: what it learnt and this one did not is kept.
            let mut merged = Self::read(path).filter(|c| c.host == self.host && c.dir == self.dir).unwrap_or_default();
            merged.files.extend(self.files.clone());
            merged.sniffed.extend(self.sniffed.clone());
            merged.seen.extend(self.seen.clone());
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
    /// `ok`, `new`, `modified`, `outdated`, `conflict`, `not-pulled`, `invalid-pointer` or
    /// `invalid-path`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Relative to the vault: the pointer's asset path, or the file's for a new asset.
    #[serde(skip)]
    rel: String,
    /// The file on disk, when there is one (its name may differ in case from the pointer's).
    #[serde(skip)]
    file: Option<String>,
    #[serde(skip)]
    pointer: Option<Pointer>,
    /// The pointer document's version in the store.
    #[serde(skip)]
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
            note: None,
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
                    item.state = if portable_rel(rel) { "new" } else { "invalid-path" };
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

pub fn status(st: &mut dyn Store, path: Option<&str>, dir: Option<&Path>, json: bool) -> Result<()> {
    let v = vault(st, path, dir)?;
    let scan = scan(st, &v)?;
    let mut cache = VaultCache::open(&v);
    let scope = scope_of(&path.map(str::to_string).into_iter().collect::<Vec<_>>())?;
    let items = items(&v, &scan, &mut cache, &scope)?;
    cache.save();
    let c = counts(&items);
    if json {
        return emit_json(&json!({ "prefix": v.prefix, "dir": v.dir.display().to_string(), "assets": items, "counts": c }));
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

/// Every pointer in the store by what it names, its asset store and the item there: the paths of
/// the assets naming it.
type Locations = HashMap<(String, String), BTreeSet<String>>;

/// A location as [`Locations`] keys it: without case, since Windows and macOS keep `a.png` and
/// `A.png` in the same file.
fn location_key(location: &str) -> String {
    location.to_lowercase()
}

fn pointer_locations(st: &mut dyn Store) -> Result<Locations> {
    let mut found: Locations = HashMap::new();
    for head in st.file_heads("/")? {
        if !is_asset_pointer(&head.path) {
            continue;
        }
        if let Ok(p) = Pointer::parse(&st.read(&head.path, None)?.0) {
            let path = asset_path(&head.path).to_string();
            found.entry((p.store.clone(), location_key(p.item.as_deref().unwrap_or(&path)))).or_default().insert(path);
        }
    }
    Ok(found)
}

#[allow(clippy::too_many_arguments)]
fn push_one(
    st: &mut dyn Store,
    v: &Vault,
    drivers: &mut Drivers,
    o: &PushOptions,
    cache: &mut VaultCache,
    in_use: &Locations,
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
    if st.stat(&pointer_path).ok().map(|s| s.version) != item.version {
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
    // The upload replaces the asset's own bytes where they are kept (its path, or the item its
    // pointer names), unless another pointer names them too; otherwise it goes to the asset's
    // path, next to anything already there.
    let shared = |location: &str| {
        in_use
            .get(&(store.clone(), location_key(location)))
            .is_some_and(|users| users.iter().any(|u| !u.eq_ignore_ascii_case(&item.path)))
    };
    let own = old
        .filter(|p| p.store == store)
        .map(|p| (p.item.clone().unwrap_or_else(|| item.path.clone()), p.sha256.as_str()))
        .filter(|(location, _)| !shared(location));
    let (target, replaces) = match &own {
        Some((location, sha)) => (location.clone(), Some(*sha)),
        None => (item.path.clone(), None),
    };
    let provider_item = match d.put(&target, &src, &sha, replaces) {
        Ok(id) => id,
        Err(e) => return Outcome::Failed(format!("{}: {}", item.path, e.message)),
    };
    let p = Pointer {
        id: old.map_or_else(pointer::new_id, |p| p.id.clone()),
        sha256: sha.clone(),
        size,
        media_type: pointer::media_type(&item.rel).to_string(),
        store: store.clone(),
        item: provider_item.or_else(|| old.filter(|p| p.store == store).and_then(|p| p.item.clone())),
        extra: old.map(|p| p.extra.clone()).unwrap_or_default(),
    };
    // The bytes are in the asset store and checked; now the pointer, unless another push got there first.
    if st.stat(&pointer_path).ok().map(|s| s.version) != item.version {
        return Outcome::Conflict(format!(
            "{}: its pointer changed in the store during this push; run it again (anything the upload replaced is in the asset store's trash)",
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
    // Recorded as synced only now that both sides have it.
    written.push(Written { path: pointer_path, version: w.version, text });
    Outcome::Done(json!({ "path": item.path, "file": file, "state": item.state, "size": size, "store": store, "version": w.version }))
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

pub fn push(st: &mut dyn Store, paths: &[String], dir: Option<&Path>, o: PushOptions, json: bool) -> Result<()> {
    let v = vault(st, paths.first().map(String::as_str), dir)?;
    let scope = scope_of(paths)?;
    check_scope(&v, &scope)?;
    let scan = scan(st, &v)?;
    let mut cache = VaultCache::open(&v);
    let found = items(&v, &scan, &mut cache, &scope)?;
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
            "conflict" if o.force && item.case_of.is_none() => todo.push(item),
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
    let in_use = if todo.is_empty() || o.dry_run { HashMap::new() } else { pointer_locations(st)? };
    let mut written = Vec::new();
    for item in &todo {
        match push_one(st, &v, &mut drivers, &o, &mut cache, &in_use, item, &mut written) {
            Outcome::Done(j) => {
                bytes += j["size"].as_u64().unwrap_or(0);
                pushed.push(j);
            }
            Outcome::Conflict(c) => conflicts.push(c),
            Outcome::Failed(f) => failed.push(f),
        }
    }
    cache.save();
    if let Err(e) = record_in_sync_base(st, &v, &written) {
        failed.push(format!("the pushed pointers could not be recorded in the sync base ({}); the next sync compares them itself", e.message));
    }
    if !o.dry_run && !pushed.is_empty() && crate::git::repo(&v.dir).is_some() {
        let files: Vec<String> = pushed.iter().filter_map(|p| p["file"].as_str().map(str::to_string)).collect();
        let ignored = crate::git::ignored(&v.dir, &files);
        let loose: Vec<&String> = files.iter().filter(|f| !ignored.contains(*f)).collect();
        if let Some(first) = loose.first() {
            eprintln!(
                "note: git does not ignore {} of the pushed assets ({first}{}): `textdb assets gitignore` covers them, and git keeps tracking what it tracks until `git rm --cached`",
                loose.len(),
                if loose.len() > 1 { ", …" } else { "" }
            );
        }
    }
    if json {
        emit_json(&json!({ "dry_run": o.dry_run, "pushed": pushed, "bytes": bytes, "conflicts": conflicts, "failed": failed }))?;
    } else {
        let verb = if o.dry_run { "would push" } else { "pushed" };
        let mut s = format!("{verb} {} assets ({})\n", pushed.len(), size_text(bytes));
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

pub fn pull(st: &mut dyn Store, paths: &[String], dir: Option<&Path>, linked_from: Option<&str>, dry_run: bool, json: bool) -> Result<()> {
    let v = vault(st, paths.first().map(String::as_str).or(linked_from), dir)?;
    let scope = scope_of(paths)?;
    check_scope(&v, &scope)?;
    let scan = scan(st, &v)?;
    let mut cache = VaultCache::open(&v);
    let found = items(&v, &scan, &mut cache, &scope)?;
    let linked: Option<BTreeSet<String>> = match linked_from {
        Some(p) => Some(st.links(p, &[])?.into_iter().filter(|l| l.asset).filter_map(|l| l.resolved).collect()),
        None => None,
    };
    let mut drivers = Drivers::new(st)?;
    let (mut pulled, mut kept, mut failed, mut bytes) = (Vec::new(), Vec::new(), Vec::new(), 0u64);
    for item in found {
        if linked.as_ref().is_some_and(|l| !l.contains(&item.path)) {
            continue;
        }
        match item.state {
            "not-pulled" | "outdated" => {}
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
        let d = match drivers.get(&p.store) {
            Ok(d) => d,
            Err(e) => {
                failed.push(format!("{}: {e}", item.path));
                continue;
            }
        };
        let file = item.file.clone().unwrap_or_else(|| item.rel.clone());
        let dest = v.dir.join(&file);
        let part = driver::partial(&dest);
        let fetched = dest
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(|e| e.to_string())
            .and_then(|_| d.get(&item.path, p.item.as_deref(), &part).map_err(|e| e.message))
            .and_then(|_| match hash_file(&part) {
                Ok((sha, size)) if sha == p.sha256 && size == p.size => Ok(()),
                Ok(_) => Err("the asset store holds other bytes than its pointer names (textdb assets verify)".to_string()),
                Err(e) => Err(e.to_string()),
            });
        if let Err(e) = fetched {
            let _ = std::fs::remove_file(&part);
            failed.push(format!("{}: {e}", item.path));
            continue;
        }
        if item.state == "outdated" {
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
        if let Ok(meta) = std::fs::metadata(&dest) {
            cache.remember(&file, meta.len(), mtime_ns(&meta), &p.sha256);
        }
        cache.saw(&item.rel, &p.sha256);
        bytes += p.size;
        pulled.push(json!({ "path": item.path, "state": item.state, "size": p.size, "store": p.store }));
    }
    cache.save();
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
    if json {
        emit_json(&json!({ "prefix": v.prefix, "dir": v.dir.display().to_string(), "assets": rows, "problems": problems }))?;
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
    let (files, nested) = walk(&root)?;
    let classifier = Classifier::load(&root, &nested);
    for w in &classifier.warnings {
        eprintln!("warning: {w}");
    }
    let mut patterns = classifier.gitignore_patterns();
    let mut cache = VaultCache::open(&Vault { prefix: "/".to_string(), dir: root.clone() });
    let single = single_file_lines(&root, &files, &classifier, &mut cache, &patterns);
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
    if json {
        return emit_json(&json!({ "file": file.display().to_string(), "changed": changed, "dry_run": dry_run, "patterns": patterns, "single_files": single.len() }));
    }
    let what = match (changed, dry_run) {
        (false, _) => "up to date",
        (true, true) => "would be updated",
        (true, false) => "updated",
    };
    out(format!(
        "{}: {what}, {} patterns ({} of them for single files the others get wrong; run it again as files come and go). Files git already tracks stay tracked until they are removed from its index (git rm --cached).\n",
        file.display(),
        patterns.len(),
        single.len()
    )
    .as_bytes())
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
}
