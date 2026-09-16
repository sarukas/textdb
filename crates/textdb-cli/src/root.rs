//! The synced directory a command is standing in, found the way git finds a repository.
//!
//! A directory that has been synced keeps a `.textdb/config` naming the store and the folder it
//! was synced with. Every command that takes a directory can then find it by walking up from the
//! current directory, so `textdb sync` with no arguments works from anywhere inside the tree and
//! `-s` and a prefix do not have to be spelled the same way every time.
//!
//! Without it the directory remembered nothing: `textdb sync /vault .` after `textdb sync / .`
//! silently imported the whole tree again under a second prefix, and a different `-s` did the
//! same. Both are now refused, naming what the directory is already paired with.

use std::path::{Path, PathBuf};

use crate::store::{Result, StoreError};

/// What a synced directory records about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The store, as `--store` would take it. A relative path is relative to `.textdb/`, so a
    /// tree moved together with its store keeps working.
    pub store: String,
    /// The folder in the store this directory is paired with.
    pub prefix: String,
    /// This directory's own name for itself, so the sync base survives a move.
    pub id: String,
    pub created: String,
    /// The extensions the last sync took in, so `textdb sync` on its own uses the same ones.
    /// `None` for a config written before this was recorded.
    pub ext: Option<String>,
    /// The account this directory was synced as, when it was synced with a token (#12).
    ///
    /// A note to whoever opens the directory later, and nothing more — the layout on disk is that
    /// account's, so a second account syncing the same directory would be reconciling its own
    /// paths against someone else's tree. The **bearer is never written here**: a checkout is
    /// copied, backed up and committed, and a credential in it would go with it.
    pub account: Option<String>,
}

const FILE: &str = "config";

impl Config {
    /// The config of the directory `start` is in, or of one above it.
    ///
    /// Stops at a filesystem boundary, as git does, and at any directory named by
    /// `TEXTDB_CEILING_DIRECTORIES` (`:`-separated). `TEXTDB_DIR` names a root outright.
    pub fn find(start: &Path) -> Option<(PathBuf, Config)> {
        if let Some(dir) = std::env::var_os("TEXTDB_DIR") {
            let dir = PathBuf::from(dir);
            return Config::read(&dir).ok().flatten().map(|c| (dir, c));
        }
        let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
        let ceilings: Vec<PathBuf> = std::env::var("TEXTDB_CEILING_DIRECTORIES")
            .unwrap_or_default()
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        let device = device_of(&start);
        let mut at = start.as_path();
        loop {
            if ceilings.iter().any(|c| c == at) {
                return None;
            }
            if let Ok(Some(config)) = Config::read(at) {
                return Some((at.to_path_buf(), config));
            }
            let Some(parent) = at.parent() else { return None };
            // A tree that spans a mount point is two trees as far as discovery is concerned,
            // which keeps a walk from wandering out of a container or a network share.
            if device.is_some() && device_of(parent) != device {
                return None;
            }
            at = parent;
        }
    }

    /// Read `dir/.textdb/config`, or `None` when the directory has never been synced.
    pub fn read(dir: &Path) -> Result<Option<Config>> {
        let path = dir.join(".textdb").join(FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::other(format!("{}: {e}", path.display()))),
        };
        let get = |key: &str| -> String {
            text.lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| k.trim() == key)
                .map(|(_, v)| v.trim().trim_matches('"').to_string())
                .unwrap_or_default()
        };
        let (store, prefix) = (get("store"), get("prefix"));
        if store.is_empty() || prefix.is_empty() {
            return Err(StoreError::other(format!(
                "{} is missing store or prefix; delete it to pair this directory again",
                path.display()
            )));
        }
        let ext = get("ext");
        Ok(Some(Config {
            store,
            prefix,
            id: get("id"),
            created: get("created"),
            ext: Some(ext).filter(|e| !e.is_empty()),
            account: Some(get("account")).filter(|a| !a.is_empty()),
        }))
    }

    /// Write the config, creating `.textdb/` if it is not there.
    ///
    /// Only sync writes this, and only on a sync that goes ahead: a dry run of a directory that
    /// has never been synced must leave it as it found it.
    pub fn write(&self, dir: &Path) -> Result<()> {
        let path = crate::sync::textdb_dir(dir)
            .map_err(|e| StoreError::other(format!("{}: {e}", dir.join(".textdb").display())))?
            .join(FILE);
        let text = format!(
            "# Written by `textdb sync`. It pairs this directory with a folder in a store, so\n\
             # `textdb sync` from anywhere inside the tree knows what to sync with what.\n\
             store = \"{}\"\nprefix = \"{}\"\nid = \"{}\"\ncreated = \"{}\"\next = \"{}\"\n{}",
            self.store,
            self.prefix,
            self.id,
            self.created,
            self.ext.as_deref().unwrap_or_default(),
            self.account.as_deref().map(|a| format!("account = \"{a}\"\n")).unwrap_or_default()
        );
        std::fs::write(&path, text).map_err(|e| StoreError::other(format!("{}: {e}", path.display())))
    }

    /// The store as a command-line value, resolving a path recorded relative to `.textdb/`.
    pub fn store_for(&self, dir: &Path) -> String {
        match crate::config::parse_store(&self.store) {
            crate::config::StoreUrl::Postgres(url) => url,
            crate::config::StoreUrl::Sqlite(path) if Path::new(&path).is_absolute() => path,
            crate::config::StoreUrl::Sqlite(path) => {
                dir.join(".textdb").join(path).to_string_lossy().into_owned()
            }
        }
    }

    /// How to record `store` for `dir`: relative to `.textdb/` when it is a file inside the tree,
    /// so moving the directory and its store together does not break the pairing.
    pub fn record_store(store: &str, dir: &Path) -> String {
        let crate::config::StoreUrl::Sqlite(path) = crate::config::parse_store(store) else {
            return store.to_string();
        };
        let file = std::fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
        let dot = std::fs::canonicalize(dir.join(".textdb")).unwrap_or_else(|_| dir.join(".textdb"));
        match file.strip_prefix(&dot) {
            Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
            Err(_) => file.to_string_lossy().into_owned(),
        }
    }
}

#[cfg(unix)]
fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.dev())
}

#[cfg(not(unix))]
fn device_of(_path: &Path) -> Option<u64> {
    // No cheap equivalent here, so the walk is bounded by the filesystem root and the ceilings.
    None
}

/// Fill in what `textdb sync` was not told, from the directory's own `.textdb/config`.
///
/// The directory is the one given, or the synced directory found by walking up from here, or the
/// current directory. Its config then supplies the folder and the store, so the pairing is stated
/// once rather than on every command. A folder or store that contradicts the config is refused
/// rather than acted on: `textdb sync /vault .` after `textdb sync / .` used to import the whole
/// tree again under a second prefix without a word.
pub fn resolve(
    prefix: &mut Option<String>,
    dir: &mut Option<PathBuf>,
    force: bool,
    store: &mut String,
    store_given: bool,
    ext: &mut String,
    ext_given: bool,
) -> Result<()> {
    let here = std::env::current_dir().map_err(|e| StoreError::other(format!("current directory: {e}")))?;
    // Both arguments are optional now, so a lone one is the folder in the store. A lone one that
    // names a directory on disk is almost certainly meant as the directory, and taking it as a
    // folder would sync the current directory instead — with whatever is in it.
    if dir.is_none() {
        if let Some(p) = prefix.as_deref().filter(|p| *p != "/") {
            let on_disk = Path::new(p);
            if on_disk.is_dir() && on_disk != here {
                return Err(StoreError::invalid(format!(
                    "{p} is a directory on this computer, and one argument is the folder in the store.                      Give both (`textdb sync FOLDER {p}`), or run `textdb sync` from inside {p}"
                )));
            }
        }
    }
    let found = dir.is_none().then(|| Config::find(&here)).flatten();
    let root = match (dir.take(), found.as_ref()) {
        (Some(d), _) => d,
        (None, Some((r, _))) => r.clone(),
        (None, None) => here,
    };
    let config = match found {
        Some((_, c)) => Some(c),
        None => Config::read(&root)?,
    };
    if let Some(c) = &config {
        match prefix.as_deref() {
            None => *prefix = Some(c.prefix.clone()),
            Some(p) if textdb_sqlite::normalize_path(p)? != c.prefix && !force => {
                return Err(StoreError::invalid(format!(
                    "{} is paired with {} in the store, not {p}; sync it as {}, or pass --force to pair it with {p} instead",
                    root.display(),
                    c.prefix,
                    c.prefix
                )));
            }
            Some(_) => {}
        }
        let paired = c.store_for(&root);
        if store_given && !same_store(store, &paired) && !force {
            return Err(StoreError::invalid(format!(
                "{} is paired with the store {paired}, not {store}; pass --force to pair it with {store} instead",
                root.display()
            )));
        }
        if !store_given {
            *store = paired;
        }
        // The include rules are part of the pairing. Without this, `textdb sync` on its own fell
        // back to the default extensions, so a directory taken in with `--ext rs,ts,toml` stopped
        // seeing its own source files — and the hook form is exactly the one with no arguments.
        if !ext_given {
            if let Some(recorded) = c.ext.as_deref().filter(|e| !e.is_empty()) {
                *ext = recorded.to_string();
            }
        }
    } else if prefix.is_none() {
        // A directory nobody has synced: the whole store with the directory as it stands, which
        // is what `textdb sync` on its own should mean the first time too.
        *prefix = Some("/".to_string());
    }
    *dir = Some(root);
    Ok(())
}

/// Two spellings of one store. Paths are compared as canonical paths where they exist, so
/// `kb.db` and `/abs/kb.db` are the same store; a Postgres URL is compared as written.
fn same_store(a: &str, b: &str) -> bool {
    use crate::config::StoreUrl;
    match (crate::config::parse_store(a), crate::config::parse_store(b)) {
        (StoreUrl::Postgres(x), StoreUrl::Postgres(y)) => x == y,
        (StoreUrl::Sqlite(x), StoreUrl::Sqlite(y)) => {
            let real = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
            real(&x) == real(&y)
        }
        _ => false,
    }
}
