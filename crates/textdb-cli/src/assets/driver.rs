//! Where asset bytes are kept. An asset store is declared in the textdb store (a name, a driver
//! and the store-side root) and bound on each computer to where it is reachable from there: a
//! local folder, or (stage 3) an rclone remote.
//!
//! The layout mirrors the vault: the asset at store path `/accounts/acme/arch.png` is kept at
//! `<root>/accounts/acme/arch.png`, so people can browse the store. What a push replaces goes to
//! `<root>/.textdb-trash/<time>/…`, never deleted outright.

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::pointer::hash_file;
use crate::store::{Result, StoreError};

/// The trash folder at the root of a local asset store.
pub const TRASH: &str = ".textdb-trash";

/// An asset store as the textdb store declares it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetStore {
    pub name: String,
    /// `local` or `rclone`.
    pub driver: String,
    /// The store-side identity: a folder, or an rclone remote path such as `teamdrive:textdb`.
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

pub const DRIVERS: &[&str] = &["local", "rclone"];

pub trait Driver {
    /// Where the asset at store path `path` is kept, for messages.
    fn location(&self, path: &str) -> String;
    /// The stored file's size, `None` when it is not there.
    fn size(&self, path: &str, item: Option<&str>) -> Result<Option<u64>>;
    /// The stored file's SHA-256 and size, `None` when it is not there. Reads the whole file.
    fn hash(&self, path: &str, item: Option<&str>) -> Result<Option<(String, u64)>>;
    /// Keep `src`, whose SHA-256 is `sha256`, at `path`. Bytes there whose SHA-256 is `replaces`
    /// go to the trash first; any other bytes there are kept, and `src` goes next to them. Returns
    /// the item the bytes are at: the provider's id, or the path for a local store.
    fn put(&self, path: &str, src: &Path, sha256: &str, replaces: Option<&str>) -> Result<Option<String>>;
    /// Copy the stored file to `dest`, which must not exist.
    fn get(&self, path: &str, item: Option<&str>, dest: &Path) -> Result<()>;
}

fn io(what: impl std::fmt::Display, e: std::io::Error) -> StoreError {
    StoreError::other(format!("{what}: {e}"))
}

/// A UTC time as `20260915-081500`, for trash folders.
pub fn stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A folder reachable from this computer: a NAS, a USB disk, or a cloud drive synced to a folder.
pub struct LocalDriver {
    pub root: PathBuf,
}

impl LocalDriver {
    /// The file of store path `path`, refusing anything that would leave the root.
    fn file(&self, path: &str) -> Result<PathBuf> {
        let rel = Path::new(path.trim_start_matches('/'));
        if path.trim_start_matches('/').is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(StoreError::invalid(format!("{path} is not a path inside an asset store")));
        }
        Ok(self.root.join(rel))
    }

    /// Where `path` goes when a push replaces it: a folder of its own per replacement, so two in
    /// the same second never meet.
    fn trash_for(&self, path: &str) -> PathBuf {
        let now = SystemTime::now();
        let nanos = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let base = self.root.join(TRASH);
        let mut n = 0;
        loop {
            let slot = base.join(format!("{}-{nanos:09}-{}-{n}", stamp(now), std::process::id())).join(path.trim_start_matches('/'));
            if !slot.exists() {
                return slot;
            }
            n += 1;
        }
    }

    /// Copy `src` to `path` through a checked partial file; with `replace`, what is there moves to
    /// the trash first, else the name must be free.
    fn place(&self, path: &str, src: &Path, sha256: &str, replace: bool) -> Result<()> {
        let dest = self.file(path)?;
        // The new copy first, complete and checked, next to where it goes; only then does the copy
        // it replaces move to the trash. A copy that fails leaves the store as it was.
        let part = partial(&dest);
        let checked = copy_to(src, &part).map_err(|e| io(format!("copying to {}", part.display()), e)).and_then(|_| match hash_file(&part) {
            Ok((sha, _)) if sha == sha256 => Ok(()),
            Ok(_) => Err(StoreError::other(format!("{} changed while it was copied to the asset store; push it again", src.display()))),
            Err(e) => Err(io(part.display(), e)),
        });
        if let Err(e) = checked {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        if replace && dest.exists() {
            let trashed = self.trash_for(path);
            let moved = trashed
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| rename_new(&dest, &trashed));
            if let Err(e) = moved {
                let _ = std::fs::remove_file(&part);
                return Err(io(format!("moving {} to the trash", dest.display()), e));
            }
        }
        if let Err(e) = rename_new(&part, &dest) {
            let _ = std::fs::remove_file(&part);
            return Err(io(format!("putting {} in place", dest.display()), e));
        }
        Ok(())
    }
}

/// `/img/a.png` as `/img/a (1a2b3c4d).png`, the start of `sha256` in brackets; `-2`, `-3`… after
/// it for further ones.
fn beside(path: &str, sha256: &str, n: u32) -> String {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let short = sha256.get(..8).unwrap_or(sha256);
    let tag = if n <= 1 { short.to_string() } else { format!("{short}-{n}") };
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{dir}/{stem} ({tag}).{ext}"),
        _ => format!("{dir}/{name} ({tag})"),
    }
}

/// A hidden name next to `file` for a copy in progress, unique to this process and moment.
pub fn partial(file: &Path) -> PathBuf {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    file.with_file_name(format!(".{name}.{}-{nanos:09}.tdbpart", std::process::id()))
}

/// Copy `src` to `to` (created or truncated), flushed to disk.
fn copy_to(src: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut from = std::fs::File::open(src)?;
    let mut file = std::fs::File::create(to)?;
    std::io::copy(&mut from, &mut file)?;
    file.flush()?;
    file.sync_all()
}

/// Rename `from` to `to` unless `to` exists: a hard link where the filesystem has them (it fails
/// on an existing name), else a check and a rename.
pub fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => std::fs::remove_file(from),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) if to.exists() => Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{} exists", to.display()))),
        Err(_) => std::fs::rename(from, to),
    }
}

/// Copy `src` to `dest`, which must not exist, through a partial file next to it.
pub fn copy_into_place(src: &Path, dest: &Path) -> std::io::Result<()> {
    let part = partial(dest);
    if let Err(e) = copy_to(src, &part).and_then(|_| rename_new(&part, dest)) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    Ok(())
}

impl Driver for LocalDriver {
    fn location(&self, path: &str) -> String {
        self.file(path).map_or_else(|_| path.to_string(), |f| f.display().to_string())
    }

    fn size(&self, path: &str, item: Option<&str>) -> Result<Option<u64>> {
        let path = item.unwrap_or(path);
        match std::fs::metadata(self.file(path)?) {
            Ok(m) if m.is_file() => Ok(Some(m.len())),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(self.location(path), e)),
        }
    }

    fn hash(&self, path: &str, item: Option<&str>) -> Result<Option<(String, u64)>> {
        let path = item.unwrap_or(path);
        if self.size(path, None)?.is_none() {
            return Ok(None);
        }
        hash_file(&self.file(path)?).map(Some).map_err(|e| io(self.location(path), e))
    }

    fn put(&self, path: &str, src: &Path, sha256: &str, replaces: Option<&str>) -> Result<Option<String>> {
        self.file(path)?;
        if !self.root.is_dir() {
            return Err(StoreError::other(format!("the asset store folder {} is not there", self.root.display())));
        }
        // The item of a local store is the path its bytes were put at, so a pointer moved in the
        // store still finds them.
        match self.hash(path, None)? {
            Some((sha, _)) if sha == sha256 => Ok(Some(path.to_string())),
            None => self.place(path, src, sha256, false).map(|_| Some(path.to_string())),
            Some((sha, _)) if Some(sha.as_str()) == replaces => self.place(path, src, sha256, true).map(|_| Some(path.to_string())),
            Some(_) => {
                // Bytes something else may still name: kept, and these go next to them.
                let mut n = 1;
                loop {
                    let alt = beside(path, sha256, n);
                    match self.hash(&alt, None)? {
                        Some((sha, _)) if sha == sha256 => return Ok(Some(alt)),
                        Some(_) => n += 1,
                        None => return self.place(&alt, src, sha256, false).map(|_| Some(alt)),
                    }
                }
            }
        }
    }

    fn get(&self, path: &str, item: Option<&str>, dest: &Path) -> Result<()> {
        let src = self.file(item.unwrap_or(path))?;
        if dest.exists() {
            return Err(StoreError::invalid(format!("{} exists already", dest.display())));
        }
        copy_into_place(&src, dest).map_err(|e| io(format!("copying {}", src.display()), e))
    }
}

/// Where this computer reaches each asset store: `TEXTDB_ASSET_STORE_<NAME>` (the name upper
/// case, other characters `_`), else the bindings file in the config directory.
pub mod binding {
    use super::*;

    pub fn env_var(name: &str) -> String {
        format!("TEXTDB_ASSET_STORE_{}", name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect::<String>())
    }

    /// `TEXTDB_CONFIG_DIR`, else the platform's config directory, with `textdb` in it.
    pub fn config_dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("TEXTDB_CONFIG_DIR").filter(|d| !d.is_empty()) {
            return Some(PathBuf::from(dir));
        }
        let base = if cfg!(windows) {
            std::env::var_os("APPDATA").map(PathBuf::from)
        } else if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
            Some(PathBuf::from(x))
        } else {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        };
        base.map(|b| b.join("textdb"))
    }

    /// Where what this computer learns about its files is kept: `TEXTDB_CONFIG_DIR/cache`, else
    /// the platform's local (not roaming) cache directory, with `textdb` in it.
    pub fn cache_dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("TEXTDB_CONFIG_DIR").filter(|d| !d.is_empty()) {
            return Some(PathBuf::from(dir).join("cache"));
        }
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else if let Some(x) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
            Some(PathBuf::from(x))
        } else {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache"))
        };
        base.map(|b| b.join("textdb"))
    }

    fn file() -> Option<PathBuf> {
        config_dir().map(|d| d.join("asset-stores.json"))
    }

    #[derive(Default, Serialize, Deserialize)]
    struct Bindings {
        #[serde(default)]
        bindings: std::collections::BTreeMap<String, String>,
    }

    fn read() -> Bindings {
        file().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// This computer's location for the asset store `name`, and where that came from.
    pub fn get(name: &str) -> Option<(String, String)> {
        let var = env_var(name);
        if let Some(v) = std::env::var(&var).ok().filter(|v| !v.is_empty()) {
            return Some((v, var));
        }
        let path = file()?;
        read().bindings.get(name).map(|v| (v.clone(), path.display().to_string()))
    }

    /// Record `location` for `name` in the bindings file (`None` removes it); returns the file.
    pub fn set(name: &str, location: Option<&str>) -> Result<PathBuf> {
        let path = file().ok_or_else(|| StoreError::other("no config directory: set TEXTDB_CONFIG_DIR"))?;
        let mut b = read();
        match location {
            Some(l) => b.bindings.insert(name.to_string(), l.to_string()),
            None => b.bindings.remove(name),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent.display(), e))?;
        }
        let text = serde_json::to_string_pretty(&b).map_err(StoreError::other)?;
        std::fs::write(&path, text + "\n").map_err(|e| io(path.display(), e))?;
        Ok(path)
    }
}

/// The driver for `store` on this computer, or why there is none.
pub fn open(store: &AssetStore) -> Result<Box<dyn Driver>> {
    match store.driver.as_str() {
        "local" => {
            let location = binding::get(&store.name).map(|(l, _)| l).or_else(|| Path::new(&store.root).is_absolute().then(|| store.root.clone()));
            let Some(location) = location else {
                return Err(StoreError::invalid(format!(
                    "asset store {} is not bound on this computer: textdb assets stores --bind {}=FOLDER, or set {}",
                    store.name,
                    store.name,
                    binding::env_var(&store.name)
                )));
            };
            let root = PathBuf::from(&location);
            if !root.is_dir() {
                return Err(StoreError::invalid(format!("asset store {}: the folder {location} is not there", store.name)));
            }
            Ok(Box::new(LocalDriver { root }))
        }
        "rclone" => Err(StoreError::invalid(format!("asset store {}: the rclone driver is not in this build yet", store.name))),
        other => Err(StoreError::invalid(format!("asset store {}: unknown driver {other}", store.name))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_utc_calendar_times() {
        assert_eq!(stamp(UNIX_EPOCH), "19700101-000000");
        assert_eq!(stamp(UNIX_EPOCH + std::time::Duration::from_secs(1_789_460_100)), "20260915-081500");
        assert_eq!(stamp(UNIX_EPOCH + std::time::Duration::from_secs(951_782_400)), "20000229-000000");
    }

    #[test]
    fn local_store_keeps_replaced_bytes_in_its_trash() {
        let tmp = std::env::temp_dir().join(format!("textdb-driver-{}", std::process::id()));
        let root = tmp.join("store");
        std::fs::create_dir_all(&root).unwrap();
        let d = LocalDriver { root: root.clone() };
        let src = tmp.join("a.png");
        std::fs::write(&src, b"one").unwrap();
        let (sha1, _) = hash_file(&src).unwrap();
        d.put("/acc/a.png", &src, &sha1, None).unwrap();
        assert_eq!(d.size("/acc/a.png", None).unwrap(), Some(3));
        assert_eq!(d.put("/acc/a.png", &src, &sha1, None).unwrap().as_deref(), Some("/acc/a.png"), "the same bytes again copy nothing");
        assert_eq!(walk(&root).len(), 1);
        std::fs::write(&src, b"two!").unwrap();
        let (sha2, _) = hash_file(&src).unwrap();
        d.put("/acc/a.png", &src, &sha2, Some(&sha1)).unwrap();
        assert_eq!(d.hash("/acc/a.png", None).unwrap().unwrap().0, sha2);
        let trashed: Vec<_> = walk(&root.join(TRASH));
        assert_eq!(trashed.len(), 1, "{trashed:?}");
        assert_eq!(std::fs::read(&trashed[0]).unwrap(), b"one");
        // Replacements within the same second each keep their own copy.
        let mut last = sha2.clone();
        for bytes in [&b"three"[..], b"four!"] {
            std::fs::write(&src, bytes).unwrap();
            let sha = hash_file(&src).unwrap().0;
            d.put("/acc/a.png", &src, &sha, Some(&last)).unwrap();
            last = sha;
        }
        assert_eq!(walk(&root.join(TRASH)).len(), 3);
        std::fs::write(&src, b"two!").unwrap();
        d.put("/acc/a.png", &src, &sha2, Some(&last)).unwrap();
        assert!(d.put("/acc/a.png", &src, &sha1, Some(&sha2)).is_err(), "bytes that do not match the hash are refused");
        // Bytes the caller does not replace are kept; the new ones go next to them.
        std::fs::write(&src, b"five").unwrap();
        let sha5 = hash_file(&src).unwrap().0;
        let beside = d.put("/acc/a.png", &src, &sha5, None).unwrap().unwrap();
        assert_eq!(beside, format!("/acc/a ({}).png", &sha5[..8]));
        assert_eq!(d.hash("/acc/a.png", None).unwrap().unwrap().0, sha2);
        assert_eq!(d.put("/acc/a.png", &src, &sha5, Some(&sha1)).unwrap().as_deref(), Some(beside.as_str()), "found there, not copied again");
        assert_eq!(walk(&root.join(TRASH)).len(), 4, "one, two!, three and four! were each replaced once; five replaced nothing");
        std::fs::write(&src, b"two!").unwrap();
        let out = tmp.join("vault/acc/a.png");
        d.get("/acc/a.png", None, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"two!");
        assert!(d.get("/acc/a.png", None, &out).is_err());
        assert!(d.size("/../escape", None).is_err());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                out.extend(walk(&e.path()));
            } else {
                out.push(e.path());
            }
        }
        out
    }
}
