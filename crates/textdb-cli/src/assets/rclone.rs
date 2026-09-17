//! An asset store reached through rclone: a Google shared drive, SharePoint, S3, or anything else
//! rclone has a backend for. Layout, trash and partial copies are the local driver's, kept on the
//! remote: the asset at `/img/a.png` is `ROOT/img/a.png`, and what a push replaces is copied to
//! `ROOT/.textdb-trash/<time>/…` first. Pushes of the same path take turns through lock files in
//! `ROOT/.textdb-trash/locks/`.
//!
//! rclone skips a copy whose destination has the same size and modification time, and a move then
//! deletes its source and keeps the old bytes: every copy and move here passes `--ignore-times`,
//! and what lands anywhere is hashed before it is trusted. rclone also takes any flag from an
//! `RCLONE_*` environment variable, so only its configuration is passed on from the environment.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::driver::{beside, host_word, lock_name, partial_name, partial_pid, process_running, stamp, Driver, Held, LOCK_WAIT, TRASH};
use crate::store::{Result, StoreError};

/// The rclone to run: `TEXTDB_RCLONE`, else the one next to this program (a vault's
/// `.textdb/bin`), else `rclone` on the PATH.
pub fn executable() -> PathBuf {
    if let Some(exe) = std::env::var_os("TEXTDB_RCLONE").filter(|e| !e.is_empty()) {
        return PathBuf::from(exe);
    }
    let name = if cfg!(windows) { "rclone.exe" } else { "rclone" };
    std::env::current_exe()
        .ok()
        .and_then(|me| me.parent().map(|dir| dir.join(name)))
        .filter(|beside| beside.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Why `root`, as an asset store declares it, is not one rclone may be pointed at on the
/// computers of everyone using the store, if it is not. The declaration is shared, so it may only
/// name a remote of each person's own rclone configuration, `REMOTE:path`: a connection string
/// (`:sftp,ssh=…:`, `remote,option=…:`) sets backend options, some of which run programs, and a
/// leading `-` would read as a flag. Anything else is bound on each computer.
pub fn shared_root_problem(root: &str) -> Option<String> {
    // A one-letter name is a Windows drive: a folder of this computer, not a configured remote.
    let valid = root.split_once(':').is_some_and(|(name, _)| {
        name.chars().count() > 1 && !name.starts_with(['-', ' ']) && !name.ends_with(' ') && name.chars().all(|c| c.is_alphanumeric() || "_-.+@ ".contains(c))
    });
    (!valid).then(|| {
        format!("{root} is not REMOTE:path: an rclone asset store's root names a remote of each person's own rclone configuration (connection strings and options go in that configuration, or in a binding on one computer)")
    })
}

pub struct RcloneDriver {
    exe: PathBuf,
    /// The remote folder the store keeps its files in, such as `teamdrive:textdb`.
    root: String,
    /// Whether the remote is Google Drive, once asked.
    drive: OnceCell<bool>,
    /// The store's files as this command listed them, on Google Drive.
    listing: RefCell<Option<Listing>>,
}

/// An entry as `rclone lsjson` lists it.
#[derive(Deserialize)]
struct Listed {
    #[serde(rename = "Path", default)]
    path: String,
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "IsDir")]
    is_dir: bool,
    #[serde(rename = "Hashes", default)]
    hashes: HashMap<String, String>,
}

/// What rclone said went wrong, without its log prefix.
fn failure(what: &str, out: &Output) -> StoreError {
    let text = String::from_utf8_lossy(&out.stderr);
    let said = text.lines().rev().map(str::trim).find(|l| !l.is_empty()).map(|l| {
        let rest = l.split_once(" : ").or_else(|| l.split_once(": ")).map_or(l, |(_, rest)| rest);
        rest.trim().to_string()
    });
    StoreError::other(match said {
        Some(said) => format!("{what}: {said}"),
        None => format!("{what}: rclone exited with {}", out.status),
    })
}

/// `root` and the path `rel` inside it.
fn join(root: &str, rel: &str) -> String {
    // `E:` alone is the current folder of drive E on Windows, not its top.
    let drive = cfg!(windows) && root.len() == 2 && root.as_bytes()[0].is_ascii_alphabetic() && root.ends_with(':');
    let sep = if !drive && root.ends_with([':', '/', '\\']) { "" } else { "/" };
    format!("{root}{sep}{rel}")
}

/// The remote part of `root`, without the colon that ends it: `gdrive` of `gdrive:textdb`,
/// `:drive,team_drive=0A…` of a connection string; empty for a plain folder.
fn remote_name(root: &str) -> &str {
    let start = usize::from(root.starts_with(':'));
    root[start..].find(':').map_or("", |i| &root[..start + i])
}

/// The type of the remote `name` in `rclone listremotes --long` output (`gdrive:   drive`).
fn remote_type(listed: &str, name: &str) -> Option<String> {
    listed.lines().find_map(|line| {
        let (n, rest) = line.split_once(':')?;
        (n.trim() == name).then(|| rest.split_whitespace().next().unwrap_or("").to_string())
    })
}

/// Whether `id` has the shape of a Google Drive file id: letters, digits, `-` and `_`, nothing
/// that could be a path.
fn is_drive_id(id: &str) -> bool {
    (10..=200).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A local path as an argument rclone cannot take for a remote: absolute, since a relative
/// `notes:2024/a.png` would name the remote `notes`.
fn local_arg(path: &Path) -> Result<String> {
    let absolute = std::path::absolute(path).map_err(|e| StoreError::other(format!("{}: {e}", path.display())))?;
    Ok(absolute.to_string_lossy().into_owned())
}

/// A file of a Google Drive store, as a listing of its root showed it.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    /// Its store path, `/img/a.png`.
    path: String,
    size: u64,
    /// Lower case, when Drive keeps one (not for some old uploads).
    sha256: Option<String>,
    /// In Drive's trash.
    trashed: bool,
}

/// The files under a Google Drive store's root, by file id.
#[derive(Default)]
struct Listing {
    by_id: HashMap<String, Entry>,
    /// Whether the files in Drive's trash under the root are in.
    trash_read: bool,
}

impl Listing {
    /// Take in `rclone lsjson -R` output for the store's root (for Drive's trash under it, with
    /// `trashed`). Folders, shortcuts (whose id is the target's and the shortcut's, a tab between)
    /// and Google documents (no bytes) are no files of the store; a file listed already stays as
    /// first listed.
    fn add(&mut self, json: &[u8], trashed: bool) -> Result<()> {
        let entries: Vec<Listed> = serde_json::from_slice(json).map_err(|e| StoreError::other(format!("unexpected rclone listing ({e})")))?;
        for e in entries {
            if e.is_dir || e.size < 0 || !is_drive_id(&e.id) {
                continue;
            }
            let sha256 = e.hashes.get("sha256").filter(|h| h.len() == 64).map(|h| h.to_ascii_lowercase());
            self.by_id.entry(e.id).or_insert(Entry { path: format!("/{}", e.path.trim_start_matches('/')), size: e.size as u64, sha256, trashed });
        }
        Ok(())
    }
}

/// Where an item is: a store path, a Google Drive file under the store's root, or nowhere in it.
enum Resolved {
    Path(String),
    File(String, Entry),
    Missing,
}

/// The process id in the lock file name `KEY-MILLIS-HOST-PID-N.lock`, when that process is of this
/// computer (its name compared without case) and not running any more.
fn abandoned(name: &str, key: &str) -> Option<u32> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let rest = name.strip_prefix(&format!("{key}-"))?.strip_suffix(".lock")?;
    let (millis, rest) = rest.split_once('-')?;
    let host = host_word();
    let rest = rest.get(host.len()..).filter(|_| digits(millis) && rest.get(..host.len()).is_some_and(|h| h.eq_ignore_ascii_case(&host)))?;
    let (pid, n) = rest.strip_prefix('-')?.split_once('-')?;
    if !digits(n) {
        return None;
    }
    let pid: u32 = pid.parse().ok()?;
    (!process_running(pid)).then_some(pid)
}

/// A lock file held on the remote, removed when dropped.
struct RemoteLock {
    driver: RcloneDriver,
    path: String,
}

impl Drop for RemoteLock {
    fn drop(&mut self) {
        if let Err(e) = self.driver.delete(&self.path) {
            eprintln!("warning: the lock {} was not removed ({}); pushes of that path wait for it until it is removed", self.driver.location(&self.path), e.message);
        }
    }
}

impl RcloneDriver {
    pub fn new(exe: PathBuf, root: String) -> RcloneDriver {
        RcloneDriver { exe, root, drive: OnceCell::new(), listing: RefCell::new(None) }
    }

    /// Whether the store is on Google Drive: the type of its remote in the rclone configuration,
    /// or a `:drive` connection string.
    fn is_drive(&self) -> bool {
        *self.drive.get_or_init(|| {
            let name = remote_name(&self.root);
            if let Some(backend) = name.strip_prefix(':') {
                return backend.split(',').next() == Some("drive");
            }
            let name = name.split(',').next().unwrap_or("");
            // Through the bare command: the flags for a drive are what this answer decides.
            let listed = self.bare_command().args(["listremotes", "--long"]).stdin(Stdio::null()).output();
            !name.is_empty()
                && listed
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| remote_type(&String::from_utf8_lossy(&o.stdout), name))
                    .as_deref()
                    == Some("drive")
        })
    }

    /// Every file under the store's root, recursively (those in Drive's trash, with `trashed`), as
    /// `rclone lsjson` gives them.
    fn list_all(&self, trashed: bool) -> Result<Vec<u8>> {
        let mut args = vec!["lsjson", "-R", "--files-only", "--no-mimetype", "--hash", "--hash-type", "SHA256", "--drive-skip-shortcuts", "--drive-skip-gdocs"];
        if trashed {
            args.push("--drive-trashed-only");
        }
        args.extend(["--", self.root.as_str()]);
        Ok(self.ok(&format!("listing {}", self.root), &args)?.stdout)
    }

    /// The file with Drive id `id` under the store's root, live or in Drive's trash. The store is
    /// listed once per command (each rclone run costs seconds), Drive's trash only when an id is
    /// not among the live files. An id anywhere else, in another drive the person running textdb
    /// can reach, say, is `None`: nothing outside the store is read, moved or trashed by id.
    fn find_id(&self, id: &str) -> Result<Option<Entry>> {
        if !is_drive_id(id) {
            return Ok(None);
        }
        let mut cached = self.listing.borrow_mut();
        if cached.is_none() {
            let mut l = Listing::default();
            l.add(&self.list_all(false)?, false)?;
            *cached = Some(l);
        }
        let l = cached.as_mut().expect("listed above");
        if !l.by_id.contains_key(id) && !l.trash_read {
            l.add(&self.list_all(true)?, true)?;
            l.trash_read = true;
        }
        Ok(l.by_id.get(id).cloned())
    }

    /// Where the item `item` is: a store path (`/img/a.png`) as it is, or on Google Drive a file
    /// id, found under the store's root.
    fn resolve(&self, item: &str) -> Result<Resolved> {
        if item.starts_with('/') {
            self.remote(item)?;
            return Ok(Resolved::Path(item.to_string()));
        }
        if !is_drive_id(item) {
            return Err(StoreError::invalid(format!("{item} is not a path inside an asset store")));
        }
        if !self.is_drive() {
            return Err(StoreError::invalid(format!(
                "{item} is a Google Drive file id, but {} is not reached through a Google Drive remote on this computer (rclone listremotes --long shows each remote's type; an alias or other remote wrapping a drive is not followed): bind the asset store to the drive remote itself",
                self.root
            )));
        }
        Ok(match self.find_id(item)? {
            Some(e) => Resolved::File(item.to_string(), e),
            None => Resolved::Missing,
        })
    }

    /// The store path where `location` (a path, or a Drive file id) is now, for a push to lock and
    /// replace.
    fn live_path(&self, location: &str) -> Result<String> {
        match self.resolve(location)? {
            Resolved::Path(p) => Ok(p),
            Resolved::File(_, e) if !e.trashed => Ok(e.path),
            Resolved::File(id, e) => Err(StoreError::invalid(format!(
                "its file (Drive id {id}, {}) went to Drive's trash meanwhile: push again",
                self.remote(&e.path).unwrap_or_default()
            ))),
            Resolved::Missing => Err(StoreError::not_found(format!("Drive id {location} names no file of the asset store {}", self.root))),
        }
    }

    /// The remote the store is on, with its colon, for `rclone backend` commands.
    fn backend_remote(&self) -> String {
        format!("{}:", remote_name(&self.root))
    }

    /// The SHA-256 of the Drive file `id`, downloaded by its id to a temporary file.
    fn hash_by_id(&self, id: &str) -> Result<String> {
        static N: AtomicU64 = AtomicU64::new(0);
        let tmp = std::env::temp_dir().join(format!("textdb-hash-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let dest = local_arg(&tmp)?;
        let hashed = self
            .ok(&format!("downloading Drive id {id} to hash it"), &["--inplace", "backend", "copyid", "--", &self.backend_remote(), id, &dest])
            .and_then(|_| crate::assets::pointer::hash_file(&tmp).map(|(sha, _)| sha).map_err(|e| StoreError::other(format!("{}: {e}", tmp.display()))));
        let _ = std::fs::remove_file(&tmp);
        hashed
    }

    /// Whether the root is a folder rclone reaches, giving up on a remote that does not answer.
    pub fn check(&self) -> Result<()> {
        // Known before anything else runs, so every command on a Google Drive store skips shortcuts.
        self.is_drive();
        let mut last = None;
        for attempt in 0..2 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(2));
            }
            let out = self.run(&["--contimeout", "15s", "--timeout", "30s", "--retries", "1", "--low-level-retries", "2", "lsjson", "--stat", "--no-mimetype", "--", &self.root])?;
            match out.status.code() {
                Some(0) if serde_json::from_slice::<Listed>(&out.stdout).is_ok_and(|l| l.is_dir) => return Ok(()),
                Some(0) => return Err(StoreError::invalid(format!("{} is a file, not a folder", self.root))),
                Some(3 | 4) => return Err(StoreError::invalid(format!("the folder {} is not there (rclone mkdir {} makes it)", self.root, self.root))),
                // A moment's trouble reaching the provider (one busy with this computer's own
                // other commands, say) is looked at again before a store counts as unreachable.
                _ => last = Some(failure(&format!("reaching {}", self.root), &out)),
            }
        }
        Err(last.unwrap_or_else(|| StoreError::other(format!("reaching {}", self.root))))
    }

    /// The remote path of store path `path`, refusing anything that would leave the root.
    fn remote(&self, path: &str) -> Result<String> {
        let rel = path.trim_start_matches('/');
        if rel.is_empty() || rel.contains('\\') || rel.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
            return Err(StoreError::invalid(format!("{path} is not a path inside an asset store")));
        }
        Ok(join(&self.root, rel))
    }

    /// The remote path of the folder `dir`; the root for `/` or ``.
    fn folder(&self, dir: &str) -> Result<String> {
        if dir.trim_matches('/').is_empty() {
            Ok(self.root.clone())
        } else {
            self.remote(dir)
        }
    }

    /// rclone with nothing from the environment that would change what a command does
    /// (`RCLONE_IGNORE_EXISTING`, `RCLONE_DRY_RUN`…); its configuration still comes through. Names
    /// compared in upper case: Windows, and rclone there, do not tell them apart.
    fn bare_command(&self) -> Command {
        let mut c = Command::new(&self.exe);
        for (key, _) in std::env::vars_os() {
            let Some(key) = key.to_str() else { continue };
            let upper = key.to_ascii_uppercase();
            if upper.starts_with("RCLONE_") && !upper.starts_with("RCLONE_CONFIG") && upper != "RCLONE_PASSWORD_COMMAND" {
                c.env_remove(key);
            }
        }
        c.arg("-q");
        c
    }

    fn command(&self) -> Command {
        let mut c = self.bare_command();
        // On Google Drive nothing is reached through a shortcut (to a folder of another drive, say),
        // or taken for a file when it is a Google document: by path as by listing. Asked here, not
        // read from what was asked before, so no command can go without these.
        if self.is_drive() {
            c.args(["--drive-skip-shortcuts", "--drive-skip-gdocs"]);
        }
        c
    }

    /// Another driver for the same store, knowing what this one knows of its remote.
    fn sibling(&self) -> RcloneDriver {
        let d = RcloneDriver::new(self.exe.clone(), self.root.clone());
        if let Some(drive) = self.drive.get() {
            let _ = d.drive.set(*drive);
        }
        d
    }

    fn not_run(&self, e: std::io::Error) -> StoreError {
        StoreError::other(format!("could not run {} ({e}): install rclone, put it next to textdb, or set TEXTDB_RCLONE", self.exe.display()))
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        self.command().args(args).stdin(Stdio::null()).output().map_err(|e| self.not_run(e))
    }

    fn ok(&self, what: &str, args: &[&str]) -> Result<Output> {
        let out = self.run(args)?;
        if out.status.success() {
            Ok(out)
        } else {
            Err(failure(what, &out))
        }
    }

    /// The file at `path`, with its SHA-256 when `hash` and the provider keeps one; `None` when
    /// no file is there.
    fn stat(&self, path: &str, hash: bool) -> Result<Option<Listed>> {
        let remote = self.remote(path)?;
        let mut args = vec!["lsjson", "--stat", "--no-mimetype"];
        if hash {
            args.extend(["--hash", "--hash-type", "SHA256"]);
        }
        args.extend(["--", remote.as_str()]);
        let out = self.run(&args)?;
        match out.status.code() {
            Some(0) => {
                let l: Listed = serde_json::from_slice(&out.stdout).map_err(|e| StoreError::other(format!("{remote}: unexpected rclone output ({e})")))?;
                if l.is_dir {
                    return Ok(None);
                }
                if l.size < 0 {
                    return Err(StoreError::invalid(format!("{remote} is not a file of bytes (a Google document, say), so it cannot be an asset")));
                }
                Ok(Some(l))
            }
            Some(3 | 4) => Ok(None),
            _ => Err(failure(&format!("looking at {remote}"), &out)),
        }
    }

    /// The names of the files in the folder `dir`; none when it is not there.
    fn names_in(&self, dir: &str) -> Result<Vec<String>> {
        let remote = self.folder(dir)?;
        let out = self.run(&["lsjson", "--files-only", "--no-modtime", "--no-mimetype", "--", &remote])?;
        match out.status.code() {
            Some(0) => Ok(serde_json::from_slice::<Vec<Listed>>(&out.stdout)
                .map_err(|e| StoreError::other(format!("{remote}: unexpected rclone output ({e})")))?
                .into_iter()
                .map(|l| l.name)
                .collect()),
            Some(3 | 4) => Ok(Vec::new()),
            _ => Err(failure(&format!("listing {remote}"), &out)),
        }
    }

    /// Remove textdb's own file at `path` (a lock, a partial copy, a trash copy it made), not to
    /// the provider's trash; one not there is fine. Tried three times.
    fn delete(&self, path: &str) -> Result<()> {
        let remote = self.remote(path)?;
        let mut last = None;
        for _ in 0..3 {
            let out = self.run(&["--retries", "1", "--drive-use-trash=false", "deletefile", "--", &remote])?;
            match out.status.code() {
                Some(0 | 3 | 4) => return Ok(()),
                _ => last = Some(failure(&format!("removing {remote}"), &out)),
            }
        }
        Err(last.unwrap_or_else(|| StoreError::other(format!("removing {remote}"))))
    }

    /// Write `body` to a small file at `path`.
    fn write_small(&self, path: &str, body: &str) -> Result<()> {
        let remote = self.remote(path)?;
        let mut child = self
            .command()
            .args(["rcat", "--", &remote])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| self.not_run(e))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body.as_bytes());
        }
        let out = child.wait_with_output().map_err(|e| self.not_run(e))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(failure(&format!("writing {remote}"), &out))
        }
    }

    /// Where `path` goes when a push replaces it: a folder of its own per replacement.
    fn trash_for(&self, path: &str) -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        let now = SystemTime::now();
        let nanos = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let n = N.fetch_add(1, Ordering::Relaxed);
        format!("/{TRASH}/{}-{nanos:09}-{}-{n}/{}", stamp(now), std::process::id(), path.trim_start_matches('/'))
    }

    /// Remove the partial copies of `path` a process on this computer that is not running any
    /// more left next to it.
    fn remove_abandoned_partials(&self, path: &str) {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let Ok(names) = self.names_in(dir) else { return };
        for found in names {
            if partial_pid(&found, name).is_some_and(|pid| !process_running(pid)) {
                let _ = self.delete(&format!("{dir}/{found}"));
            }
        }
    }

    /// The lock files of the path whose [`lock_name`] is `key`.
    fn lock_files(&self, folder: &str, key: &str) -> Result<Vec<String>> {
        let start = format!("{key}-");
        let mut files: Vec<String> = self.names_in(folder)?.into_iter().filter(|n| n.starts_with(&start) && n.ends_with(".lock")).collect();
        // A provider that keeps two files of one name (Google Drive) lists it twice.
        files.sort();
        files.dedup();
        Ok(files)
    }

    /// Hold the lock of `path` (its case ignored). rclone cannot create a file only when none is
    /// there, so a push lists the lock folder first and writes a lock file of its own
    /// (`KEY-MILLIS-HOST-PID-N.lock`, N counting this process's locks) only when no other push's
    /// file of that path is listed; it holds the lock when two listings a moment apart show its
    /// file and no other. Where several wrote at once, the file whose name sorts first (the push
    /// that started first) stays and the others are removed; while another file is listed a push
    /// waits, longer each time. Of two pushes that wrote at once, the one that lists later sees the
    /// other's file, as long as the provider lists what was written, so both never hold the lock.
    fn lock_remote(&self, path: &str) -> Result<RemoteLock> {
        static N: AtomicU64 = AtomicU64::new(0);
        let folder = format!("/{TRASH}/locks");
        let key = lock_name(path);
        let now = SystemTime::now();
        let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
        let mine = format!("{key}-{:020}-{}-{}-{}.lock", since.as_millis(), host_word(), std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        let mine_path = format!("{folder}/{mine}");
        let body = format!("{path}\nheld by process {} on {} since {}\n", std::process::id(), host_word(), stamp(now));
        let started = Instant::now();
        let (mut told, mut checked, mut wait, mut unlisted) = (false, None::<Instant>, Duration::from_millis(200), 0);
        let own = || RemoteLock { driver: self.sibling(), path: mine_path.clone() };
        // This push's lock file, once written; removed when dropped.
        let mut written: Option<RemoteLock> = None;
        loop {
            let files = self.lock_files(&folder, &key)?;
            let others: Vec<String> = files.iter().filter(|f| **f != mine).cloned().collect();
            if files.contains(&mine) {
                unlisted = 0;
                // Its own file, still there after a removal that failed: taken back, for the
                // rules below to keep or remove.
                if written.is_none() {
                    written = Some(own());
                }
            }
            if started.elapsed() > LOCK_WAIT {
                return Err(StoreError::other(format!(
                    "{path} is locked ({}) and was not released in {} minutes; if that push is not running any more, remove its file from {}",
                    if others.is_empty() { "this push's lock file was not listed".to_string() } else { others.join(", ") },
                    LOCK_WAIT.as_secs() / 60,
                    self.folder(&folder).unwrap_or_default()
                )));
            }
            if others.is_empty() {
                if written.is_none() {
                    self.write_small(&mine_path, &body).inspect_err(|_| drop(self.delete(&mine_path)))?;
                    written = Some(own());
                } else if files.len() == 1 {
                    std::thread::sleep(Duration::from_millis(300));
                    let again = self.lock_files(&folder, &key)?;
                    if again.len() == 1 && again[0] == mine {
                        if let Some(lock) = written.take() {
                            return Ok(lock);
                        }
                    }
                } else {
                    // Written and not listed: not yet, or gone (removed by hand, say), when it is
                    // written again.
                    unlisted += 1;
                    if unlisted >= 3 {
                        (written, unlisted) = (None, 0);
                    } else {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
                continue;
            }
            // Of pushes that wrote at once, the one that started first keeps its file.
            if written.is_some() && others.iter().any(|o| *o < mine) {
                written = None;
            }
            // A lock left by a process of this computer that is not running any more (a push that
            // was killed) is removed, now and then looked at again.
            if checked.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
                checked = Some(Instant::now());
                for other in &others {
                    if let Some(pid) = abandoned(other, &key) {
                        eprintln!("removing the lock of {path} left by process {pid}, which is not running any more");
                        let _ = self.delete(&format!("{folder}/{other}"));
                    }
                }
            }
            if !told {
                eprintln!("waiting for another push of {path}");
                told = true;
            }
            // Apart from one another, so pushes that keep meeting stop doing so.
            let spread = u64::from(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos()) ^ std::process::id().wrapping_mul(2_654_435_761));
            std::thread::sleep(wait + Duration::from_millis(spread % (wait.as_millis() as u64 + 1)));
            wait = (wait * 3 / 2).min(Duration::from_secs(2));
        }
    }

    /// [`Driver::put`] at the store path `path`; the item returned is the path the bytes are at.
    fn put_at(&self, path: &str, src: &Path, sha256: &str, replaces: Option<&str>) -> Result<(Option<String>, Held)> {
        self.remote(path)?;
        if self.is_drive() {
            self.refuse_two_of_a_name(path)?;
        }
        let here = || (Some(path.to_string()), Held::default());
        match self.hash(path, None)? {
            Some((sha, _)) if sha == sha256 => Ok(here()),
            None => self.place(path, src, sha256, None).map(|_| here()),
            Some((sha, _)) if Some(sha.as_str()) == replaces => {
                let expected = sha.as_str();
                // On Google Drive the bytes are replaced in the file that is there, so its id, and
                // every link people made to it, stay; elsewhere a checked copy is moved into place.
                let replaced = match self.is_drive() {
                    true => self.place_over(path, src, sha256, expected)?,
                    false => self.place(path, src, sha256, replaces)?,
                };
                match replaced {
                    true => Ok(here()),
                    false => self.put_beside(path, src, sha256),
                }
            }
            // Bytes something else may still name: kept, and these go next to them.
            Some(_) => self.put_beside(path, src, sha256),
        }
    }

    /// Keep the cached listing right after a push, so the assets after it need no new listing: the
    /// file at `path` is now `id`, holding these bytes. What became of another file listed at that
    /// path is not textdb's to guess (Drive keeps two files of one name apart, and either may be
    /// live still), so the listing goes and the next look lists the store again.
    fn remember(&self, id: &str, path: &str, size: u64, sha256: &str) {
        let mut cached = self.listing.borrow_mut();
        let Some(l) = cached.as_mut() else { return };
        if l.by_id.iter().any(|(other, e)| other != id && e.path == path && !e.trashed) {
            *cached = None;
            return;
        }
        l.by_id.insert(id.to_string(), Entry { path: path.to_string(), size, sha256: Some(sha256.to_string()), trashed: false });
    }

    /// A push leaves a path Drive holds two files of alone: which of them it would replace, and
    /// which a pointer names, is not textdb's to guess.
    fn refuse_two_of_a_name(&self, path: &str) -> Result<()> {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        if self.names_in(dir)?.iter().filter(|n| *n == name).count() > 1 {
            return Err(StoreError::invalid(format!(
                "{} is two files of one name in the drive: keep one of them there, then push again",
                self.remote(path)?
            )));
        }
        Ok(())
    }

    /// Replace the bytes at `path`, keeping the provider's file and so its id, for the links people
    /// made to it in the drive. A server-side copy of what is there goes to the store's trash first
    /// and must hash to `replace` (not bytes put there some other way); then `src` is uploaded onto
    /// the same file, which readers see whole or not at all, and what is there is hashed. `false`
    /// when what was there turned out to be other bytes: nothing is uploaded.
    fn place_over(&self, path: &str, src: &Path, sha256: &str, replace: &str) -> Result<bool> {
        let dest = self.remote(path)?;
        // The file this push replaces, to be the same file afterwards: its id is what the pointer
        // names and what the drive's links point at.
        let Some(before) = self.stat(path, false)? else {
            return Err(StoreError::other(format!("putting {dest} in place: nothing is there any more")));
        };
        let copy = self.trash_for(path);
        let copied = self
            .ok(&format!("copying {dest} to the trash"), &["copyto", "--ignore-times", "--", &dest, &self.remote(&copy)?])
            .and_then(|_| self.hash(&copy, None));
        match copied {
            // Checked to be what this push replaces, not bytes put there some other way.
            Ok(Some((sha, _))) if sha == replace => {}
            Ok(Some(_)) => {
                let _ = self.delete(&copy);
                return Ok(false);
            }
            Ok(None) => {
                let _ = self.delete(&copy);
                return Err(StoreError::other(format!("copying {dest} to the trash: nothing is there afterwards")));
            }
            Err(e) => {
                let _ = self.delete(&copy);
                return Err(e);
            }
        }
        let source = local_arg(src)?;
        let uploaded = self.ok(&format!("uploading {} onto {dest}", src.display()), &["copyto", "--ignore-times", "--", &source, &dest]);
        // Looked at again after a failure to look, so a moment's trouble reaching the provider is
        // not taken for a failed upload.
        let mut found = self.hashed_id(path);
        for _ in 0..2 {
            if found.is_err() {
                std::thread::sleep(Duration::from_secs(1));
                found = self.hashed_id(path);
            }
        }
        match &found {
            // The same file, holding what was uploaded.
            Ok(Some((id, sha))) if *sha == sha256 && *id == before.id => return Ok(true),
            // The upload went through and what it left cannot be looked at: it was checked as it went.
            Err(_) if uploaded.is_ok() => return Ok(true),
            _ => {}
        }
        let why = match (uploaded, &found) {
            (Err(e), _) => e.message,
            (Ok(_), Ok(Some((id, _)))) if *id != before.id => "another file of that name is there".to_string(),
            (Ok(_), Ok(Some(_))) => "other bytes are there".to_string(),
            (Ok(_), Ok(None)) => "nothing is there afterwards".to_string(),
            (Ok(_), Err(e)) => e.message.clone(),
        };
        // What was there goes back only onto its own file, empty now or holding those bytes still:
        // bytes textdb did not write are not its to overwrite, and the copy is named instead.
        let ours = matches!(&found, Ok(None)) || matches!(&found, Ok(Some((id, sha))) if *id == before.id && sha == replace);
        let back = match ours {
            true => match self.ok(&format!("putting what was at {dest} back"), &["copyto", "--ignore-times", "--", &self.remote(&copy)?, &dest]) {
                Ok(_) => "; what was there is back".to_string(),
                Err(_) => format!("; a copy of what was there is at {}", self.location(&copy)),
            },
            false => format!("; a copy of what was there is at {}", self.location(&copy)),
        };
        Err(StoreError::other(format!("putting {dest} in place: {why}{back}")))
    }

    /// The file at the store path `path`: its provider id, and its SHA-256, read through by id
    /// where Drive keeps none of it.
    fn hashed_id(&self, path: &str) -> Result<Option<(String, String)>> {
        let Some(l) = self.stat(path, true)? else { return Ok(None) };
        let sha = match l.hashes.get("sha256").filter(|h| h.len() == 64) {
            Some(sha) => sha.to_ascii_lowercase(),
            None if is_drive_id(&l.id) => self.hash_by_id(&l.id)?,
            None => match self.hash(path, None)? {
                Some((sha, _)) => sha,
                None => return Ok(None),
            },
        };
        Ok(Some((l.id, sha)))
    }

    /// Keep `src` next to `path`, under the first [`beside`] name that is free or holds these
    /// bytes, whose lock is returned held.
    fn put_beside(&self, path: &str, src: &Path, sha256: &str) -> Result<(Option<String>, Held)> {
        let mut n = 1;
        loop {
            let alt = beside(path, sha256, n);
            let lock = self.lock_remote(&alt)?;
            match self.hash(&alt, None)? {
                Some((sha, _)) if sha == sha256 => return Ok((Some(alt), Held::of(lock))),
                Some(_) => n += 1,
                None => {
                    self.place(&alt, src, sha256, None)?;
                    return Ok((Some(alt), Held::of(lock)));
                }
            }
        }
    }

    /// Upload `src` to `path` through a checked partial copy next to it. With `replace`, what is
    /// there is copied to the trash first and must hash to `replace`; else the name must be free.
    /// `false` when what was there turned out to be other bytes: nothing is placed.
    fn place(&self, path: &str, src: &Path, sha256: &str, replace: Option<&str>) -> Result<bool> {
        let dest = self.remote(path)?;
        // The caller holds the path's lock: copies a killed push left here are nobody's.
        self.remove_abandoned_partials(path);
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let part_path = format!("{dir}/{}", partial_name(name));
        let part = self.remote(&part_path)?;
        let discard = |e: StoreError| {
            let _ = self.delete(&part_path);
            e
        };
        let source = local_arg(src).map_err(discard)?;
        // Straight to the partial name (already this push's own), so rclone leaves no partial of its own.
        self.ok(&format!("uploading {} to {part}", src.display()), &["copyto", "--ignore-times", "--inplace", "--", &source, &part]).map_err(discard)?;
        match self.hash(&part_path, None).map_err(discard)? {
            Some((sha, _)) if sha == sha256 => {}
            _ => return Err(discard(StoreError::other(format!("{} changed while it was copied to the asset store; push it again", src.display())))),
        }
        let mut trashed = None;
        match (replace, self.stat(path, false).map_err(discard)?.is_some()) {
            (Some(expected), true) => {
                // A copy in the trash first, checked to be what this push replaces (not bytes put
                // there some other way).
                let copy = self.trash_for(path);
                let copied = self.ok(&format!("copying {dest} to the trash"), &["copyto", "--ignore-times", "--", &dest, &self.remote(&copy)?]).and_then(|_| self.hash(&copy, None));
                match copied {
                    Ok(Some((sha, _))) if sha == expected => trashed = Some(copy),
                    Ok(_) => {
                        let _ = self.delete(&copy);
                        let _ = self.delete(&part_path);
                        return Ok(false);
                    }
                    Err(e) => {
                        let _ = self.delete(&copy);
                        return Err(discard(e));
                    }
                }
            }
            (None, true) => return Err(discard(StoreError::other(format!("putting {dest} in place: something is there already")))),
            _ => {}
        }
        // rclone removes what is at the destination before a server-side move, so the path is
        // empty for a moment: what is there afterwards is checked, and when it is not the new
        // bytes, what was there is put back from the trash.
        let moved = self.ok(&format!("putting {dest} in place"), &["moveto", "--ignore-times", "--", &part, &dest]);
        // Looked at again after a failure to look, so a moment's trouble reaching the provider is
        // not taken for a failed move.
        let mut found = self.hash(path, None);
        for _ in 0..2 {
            if found.is_err() {
                std::thread::sleep(Duration::from_secs(1));
                found = self.hash(path, None);
            }
        }
        if matches!(&found, Ok(Some((sha, _))) if sha == sha256) {
            if moved.is_err() {
                let _ = self.delete(&part_path);
            }
            return Ok(true);
        }
        // The move went through and what it put there cannot be looked at: the copy it moved was
        // checked, so the bytes are placed.
        if moved.is_ok() && found.is_err() {
            return Ok(true);
        }
        let why = match (moved, &found) {
            (Err(e), _) => e.message,
            (Ok(_), Ok(Some(_))) => "rclone left other bytes there".to_string(),
            (Ok(_), Ok(None)) => "nothing is there after the move".to_string(),
            (Ok(_), Err(e)) => e.message.clone(),
        };
        let kept = match (&trashed, &found) {
            (Some(copy), Ok(None)) => match self.ok("", &["copyto", "--ignore-times", "--", &self.remote(copy)?, &dest]) {
                Ok(_) => "; what was there is back".to_string(),
                Err(_) => format!("; what was there is kept at {}", self.location(copy)),
            },
            (Some(copy), _) => format!("; a copy of what was there is at {}", self.location(copy)),
            (None, _) => String::new(),
        };
        let _ = self.delete(&part_path);
        Err(StoreError::other(format!("putting {dest} in place: {why}{kept}")))
    }
}

impl Driver for RcloneDriver {
    fn location(&self, path: &str) -> String {
        if !path.starts_with('/') && is_drive_id(path) {
            return format!("the file with Drive id {path} in {}", self.root);
        }
        self.remote(path).unwrap_or_else(|_| path.to_string())
    }

    fn size(&self, path: &str, item: Option<&str>) -> Result<Option<u64>> {
        match self.resolve(item.unwrap_or(path))? {
            Resolved::Path(p) => Ok(self.stat(&p, false)?.map(|l| l.size as u64)),
            // Only in Drive's trash: not there (a pull still finds it by id).
            Resolved::File(_, e) => Ok((!e.trashed).then_some(e.size)),
            Resolved::Missing => Ok(None),
        }
    }

    fn hash(&self, path: &str, item: Option<&str>) -> Result<Option<(String, u64)>> {
        let (path, size) = match self.resolve(item.unwrap_or(path))? {
            Resolved::Missing => return Ok(None),
            // Only in Drive's trash: not there (a pull still finds it by id).
            Resolved::File(_, e) if e.trashed => return Ok(None),
            Resolved::File(id, e) => {
                return match e.sha256 {
                    Some(sha) => Ok(Some((sha, e.size))),
                    // Drive keeps no SHA-256 of it (an old upload): downloaded by its id and
                    // hashed, never read by a path other files may share.
                    None => self.hash_by_id(&id).map(|sha| Some((sha, e.size))),
                };
            }
            Resolved::Path(p) => {
                let Some(l) = self.stat(&p, true)? else { return Ok(None) };
                if let Some(sha) = l.hashes.get("sha256").filter(|h| h.len() == 64) {
                    return Ok(Some((sha.to_ascii_lowercase(), l.size as u64)));
                }
                (p, l.size as u64)
            }
        };
        // The provider keeps no SHA-256 of this file: rclone reads it through.
        let remote = self.remote(&path)?;
        let args = ["hashsum", "sha256", "--download", "--", remote.as_str()];
        let out = self.ok(&format!("hashing {remote}"), &args)?;
        let text = String::from_utf8_lossy(&out.stdout);
        match text.split_whitespace().next().filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            Some(sha) => Ok(Some((sha.to_ascii_lowercase(), size))),
            None => Err(StoreError::other(format!("hashing {remote}: unexpected rclone output"))),
        }
    }

    fn lock(&self, path: &str) -> Result<Held> {
        let path = self.live_path(path)?;
        Ok(Held::of(self.lock_remote(&path)?))
    }

    fn put(&self, path: &str, src: &Path, sha256: &str, replaces: Option<&str>) -> Result<(Option<String>, Held)> {
        let path = self.live_path(path)?;
        let (at, held) = self.put_at(&path, src, sha256, replaces)?;
        if !self.is_drive() {
            return Ok((at, held));
        }
        let Some(at) = at else { return Ok((None, held)) };
        // On Google Drive the item is the file's id, which stays with it through renames, moves and
        // pushes. The id and the bytes come from one look, so a pointer never names a file of other
        // bytes where two files share a name, as Drive allows.
        let Some(l) = self.stat(&at, true)? else {
            return Err(StoreError::other(format!("{}: placed, but nothing is there now", self.location(&at))));
        };
        if !is_drive_id(&l.id) {
            return Err(StoreError::other(format!("{}: placed, but Drive gave no file id for it", self.location(&at))));
        }
        // The bytes are confirmed for the file the id names, never taken on trust: where Drive
        // keeps no SHA-256 of it (an old upload), they are read through by that id.
        let sha = match l.hashes.get("sha256").filter(|h| h.len() == 64) {
            Some(sha) => sha.to_ascii_lowercase(),
            None => self.hash_by_id(&l.id)?,
        };
        if sha != sha256 {
            return Err(StoreError::other(format!("{}: placed, but the file of that name holds other bytes now; push it again", self.location(&at))));
        }
        self.remember(&l.id, &at, l.size as u64, &sha);
        Ok((Some(l.id), held))
    }

    /// `dest` is the caller's own partial name, so rclone downloads straight to it. On Google
    /// Drive an item that is a file id is downloaded by that id, wherever under the store's root it
    /// is now, in Drive's trash too.
    fn get(&self, path: &str, item: Option<&str>, dest: &Path) -> Result<()> {
        let at = item.unwrap_or(path);
        if dest.exists() {
            return Err(StoreError::invalid(format!("{} exists already", dest.display())));
        }
        let resolved = self.resolve(at)?;
        let from = match &resolved {
            Resolved::Path(p) => {
                let from = self.remote(p)?;
                // A folder there would be copied whole.
                if self.stat(p, false)?.is_none() {
                    return Err(StoreError::not_found(format!("{from} is not a file in the asset store")));
                }
                from
            }
            Resolved::File(id, e) => format!("{} (Drive id {id}{})", self.remote(&e.path)?, if e.trashed { ", in Drive's trash" } else { "" }),
            Resolved::Missing => return Err(StoreError::not_found(format!("Drive id {at} names no file of the asset store {}", self.root))),
        };
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::other(format!("{}: {e}", parent.display())))?;
        }
        let dest_text = local_arg(dest)?;
        let fetched = match &resolved {
            Resolved::File(id, _) => self.ok(&format!("downloading {from}"), &["--inplace", "backend", "copyid", "--", &self.backend_remote(), id, &dest_text]),
            _ => self.ok(&format!("downloading {from}"), &["copyto", "--ignore-times", "--inplace", "--", &from, &dest_text]),
        }
        .map(|_| ());
        let fetched = fetched.and_then(|_| match dest.is_file() {
            true => Ok(()),
            false => Err(StoreError::other(format!("downloading {from}: not a file"))),
        });
        if fetched.is_err() {
            let _ = if dest.is_dir() { std::fs::remove_dir_all(dest) } else { std::fs::remove_file(dest) };
        }
        fetched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::pointer::hash_file;

    /// rclone for tests: `TEXTDB_RCLONE` only, as CI sets it, never one found on the PATH. These
    /// tests rewrite and rename files quickly, which security software on a person's own computer
    /// may take for ransomware. Without it the test is skipped, unless `TEXTDB_REQUIRE_RCLONE` is set.
    fn test_rclone() -> Option<PathBuf> {
        let exe = std::env::var_os("TEXTDB_RCLONE").filter(|e| !e.is_empty()).map(PathBuf::from);
        let runs = exe.as_ref().is_some_and(|exe| Command::new(exe).arg("version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()));
        if !runs && std::env::var_os("TEXTDB_REQUIRE_RCLONE").is_some() {
            panic!("TEXTDB_REQUIRE_RCLONE is set, but TEXTDB_RCLONE does not name an rclone that runs");
        }
        exe.filter(|_| runs)
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

    #[test]
    fn drive_listings_hold_the_files_of_the_store_and_nothing_that_only_looks_like_one() {
        let sha = "AB".repeat(32);
        let live = serde_json::json!([
            { "Path": "img/a.png", "Name": "a.png", "Size": 3, "IsDir": false, "Hashes": { "sha256": sha }, "ID": "1AbCdEfGhIjKlMnOp" },
            { "Path": "img", "Name": "img", "Size": -1, "IsDir": true, "ID": "1FolderIdAbCdEfG" },
            { "Path": "doc.docx", "Name": "doc.docx", "Size": -1, "IsDir": false, "ID": "1-jYdDUUfEkVxOTBHpMz" },
            { "Path": "shortcut", "Name": "shortcut", "Size": 3, "IsDir": false, "Hashes": { "sha256": sha }, "ID": "1AbCdEfGhIjKlMnOp\t18DE605eCvttHBN7y" },
        ]);
        let trashed = serde_json::json!([
            { "Path": "old/b.png", "Name": "b.png", "Size": 5, "IsDir": false, "ID": "1TrashedIdAbCdEf" },
            { "Path": "img/a.png", "Name": "a.png", "Size": 9, "IsDir": false, "ID": "1AbCdEfGhIjKlMnOp" },
        ]);
        let mut l = Listing::default();
        l.add(&serde_json::to_vec(&live).unwrap(), false).unwrap();
        l.add(&serde_json::to_vec(&trashed).unwrap(), true).unwrap();
        assert_eq!(l.by_id.len(), 2, "no folder, Google document or shortcut: {:?}", l.by_id);
        assert_eq!(l.by_id["1AbCdEfGhIjKlMnOp"], Entry { path: "/img/a.png".into(), size: 3, sha256: Some("ab".repeat(32)), trashed: false });
        assert_eq!(l.by_id["1TrashedIdAbCdEf"], Entry { path: "/old/b.png".into(), size: 5, sha256: None, trashed: true });
        assert!(Listing::default().add(b"not json", false).is_err());

        assert!(is_drive_id("1-7rj9nEmLs5ziRcXEV-ueu_2lr_BjQK4") && is_drive_id("0ADEgOn1dRANgUk9PVA"));
        for not in ["/img/a.png", "a\tb-cdefghijkl", "../../etc/passwd", "short", "gdrive:textdb-test"] {
            assert!(!is_drive_id(not), "{not}");
        }
        assert_eq!(remote_name("gdrive:textdb"), "gdrive");
        assert_eq!(remote_name(":drive,team_drive=0AB:textdb"), ":drive,team_drive=0AB");
        assert_eq!(remote_name("/tmp/store"), "");
        let remotes = "gdrive:     drive\nbucket:     s3\n";
        assert_eq!(remote_type(remotes, "gdrive").as_deref(), Some("drive"));
        assert_eq!(remote_type(remotes, "bucket").as_deref(), Some("s3"));
        assert_eq!(remote_type(remotes, "nowhere"), None);
    }

    #[test]
    fn a_push_keeps_the_listing_right_for_the_assets_after_it() {
        let listed = |d: &RcloneDriver, json: &serde_json::Value| {
            let mut l = Listing::default();
            l.add(&serde_json::to_vec(json).unwrap(), false).unwrap();
            *d.listing.borrow_mut() = Some(l);
        };
        let one = serde_json::json!([
            { "Path": "img/a.png", "Name": "a.png", "Size": 3, "IsDir": false, "ID": "1OldIdAbCdEfGhI" },
            { "Path": "img/b.png", "Name": "b.png", "Size": 4, "IsDir": false, "ID": "1OtherIdAbCdEfG" },
        ]);
        let d = RcloneDriver::new(PathBuf::from("rclone"), "gdrive:textdb".to_string());
        listed(&d, &one);

        // A push replaced the bytes in the file that was there: the same id, its new bytes, and
        // every other file of the store as it was.
        d.remember("1OldIdAbCdEfGhI", "/img/a.png", 9, &"cd".repeat(32));
        let listing = d.listing.borrow();
        let by_id = &listing.as_ref().unwrap().by_id;
        assert_eq!(by_id["1OldIdAbCdEfGhI"], Entry { path: "/img/a.png".into(), size: 9, sha256: Some("cd".repeat(32)), trashed: false });
        assert_eq!(by_id["1OtherIdAbCdEfG"], Entry { path: "/img/b.png".into(), size: 4, sha256: None, trashed: false });
        drop(listing);

        // A file of that name textdb did not place: what became of it is not guessed, and the next
        // look lists the store again.
        let two = serde_json::json!([
            { "Path": "img/a.png", "Name": "a.png", "Size": 3, "IsDir": false, "ID": "1OldIdAbCdEfGhI" },
            { "Path": "img/a.png", "Name": "a.png", "Size": 7, "IsDir": false, "ID": "1TwinIdAbCdEfGh" },
        ]);
        listed(&d, &two);
        d.remember("1OldIdAbCdEfGhI", "/img/a.png", 9, &"cd".repeat(32));
        assert!(d.listing.borrow().is_none(), "the listing was kept although another file held that name");
    }

    /// A Google Drive folder called `textdb-test` to test against (`TEXTDB_TEST_GDRIVE`, such as
    /// `gdrive:textdb-test`), and rclone; the test is skipped without one. Tests write only into a
    /// new folder of their own in it, removed at the end.
    fn test_gdrive() -> Option<(PathBuf, String)> {
        let base = std::env::var("TEXTDB_TEST_GDRIVE").ok().filter(|b| !b.is_empty())?;
        let base = base.trim_end_matches('/').to_string();
        assert_eq!(base.rsplit(['/', ':']).next(), Some("textdb-test"), "TEXTDB_TEST_GDRIVE must name a folder called textdb-test, not {base}");
        Some((test_rclone().expect("TEXTDB_TEST_GDRIVE is set, but rclone does not run"), base))
    }

    /// A folder on the remote, removed for good when dropped.
    struct Purge(PathBuf, String);

    impl Drop for Purge {
        fn drop(&mut self) {
            let _ = RcloneDriver::new(self.0.clone(), String::new()).command().args(["purge", "--drive-use-trash=false", &self.1]).output();
        }
    }

    #[test]
    fn google_drive_items_are_file_ids_found_after_renames_and_in_the_trash_and_only_inside_the_store() {
        let Some((exe, base)) = test_gdrive() else { return };
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        let run_dir = format!("{base}/driver-{}-{nanos}", std::process::id());
        let (root, outside) = (format!("{run_dir}/store"), format!("{run_dir}/outside"));
        // rclone as the driver runs it: no flags from RCLONE_* variables, which could send these
        // commands somewhere the driver does not go.
        let rc = |args: &[&str]| {
            let out = RcloneDriver::new(exe.clone(), base.clone()).command().args(args).output().unwrap();
            assert!(out.status.success(), "rclone {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        assert!(RcloneDriver::new(exe.clone(), base.clone()).is_drive(), "TEXTDB_TEST_GDRIVE must be on a Google Drive remote");
        let id_at = |path: &str| serde_json::from_str::<serde_json::Value>(&rc(&["lsjson", "--stat", path])).unwrap()["ID"].as_str().unwrap().to_string();
        rc(&["mkdir", &root]);
        let _purge = Purge(exe.clone(), run_dir.clone());
        let tmp = std::env::temp_dir().join(format!("textdb-gdrive-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let src = tmp.join("a.png");
        std::fs::write(&src, b"drive bytes").unwrap();
        let sha = hash_file(&src).unwrap().0;

        let d = RcloneDriver::new(exe.clone(), root.clone());
        d.check().unwrap();
        assert!(d.is_drive());
        let id = d.put("/img/a.png", &src, &sha, None).unwrap().0.unwrap();
        assert!(is_drive_id(&id), "the item is the file's Drive id: {id}");
        assert_eq!(d.hash("/img/a.png", Some(&id)).unwrap(), Some((sha.clone(), 11)));

        // A second push replaces the bytes in the same file, so its id and the drive's links to it
        // stay, and the bytes it replaced are in the store's trash for pointers that name them.
        std::fs::write(&src, b"drive bytes two").unwrap();
        let sha2 = hash_file(&src).unwrap().0;
        let again = d.put("/img/a.png", &src, &sha2, Some(&sha)).unwrap().0.unwrap();
        assert_eq!(again, id, "a push gave the file a new id");
        // Read from the store, not from what this driver remembered of its push.
        let after = RcloneDriver::new(exe.clone(), root.clone());
        after.check().unwrap();
        assert_eq!(after.hash("/img/a.png", Some(&id)).unwrap(), Some((sha2.clone(), 15)));
        let trashed = rc(&["lsjson", "-R", "--hash", "--hash-type", "SHA256", &format!("{root}/{TRASH}")]);
        assert!(trashed.contains(&sha), "the replaced bytes are not in the store's trash: {trashed}");

        // Renamed and moved in the drive: found by its id (a new command lists the store afresh).
        rc(&["moveto", &format!("{root}/img/a.png"), &format!("{root}/elsewhere/renamed.png")]);
        let d = RcloneDriver::new(exe.clone(), root.clone());
        assert_eq!(d.size("/img/a.png", Some(&id)).unwrap(), Some(15));
        d.get("/img/a.png", Some(&id), &tmp.join("moved.png")).unwrap();
        assert_eq!(std::fs::read(tmp.join("moved.png")).unwrap(), b"drive bytes two");

        // In Drive's trash: still fetched by id, but a push onto it is refused rather than written elsewhere.
        rc(&["deletefile", &format!("{root}/elsewhere/renamed.png")]);
        let d = RcloneDriver::new(exe.clone(), root.clone());
        assert_eq!(d.size("/img/a.png", Some(&id)).unwrap(), None, "a file only in Drive's trash is not there");
        d.get("/img/a.png", Some(&id), &tmp.join("trashed.png")).unwrap();
        assert_eq!(std::fs::read(tmp.join("trashed.png")).unwrap(), b"drive bytes two");
        match d.lock(&id) {
            Err(e) => assert!(e.message.contains("trash"), "{}", e.message),
            Ok(_) => panic!("a push onto a file in Drive's trash was let through"),
        }

        // A file outside the store is never read, whatever its id; nor is an item that is no id.
        rc(&["copyto", &src.to_string_lossy(), &format!("{outside}/x.png")]);
        let outside_id = id_at(&format!("{outside}/x.png"));
        let d = RcloneDriver::new(exe.clone(), root.clone());
        assert!(d.get("/x.png", Some(&outside_id), &tmp.join("outside.png")).is_err());
        assert!(!tmp.join("outside.png").exists());
        assert_eq!(d.hash("/x.png", Some(&outside_id)).unwrap(), None);
        assert!(d.get("/x.png", Some("../outside/x.png"), &tmp.join("up.png")).is_err());

        // Nor anything through a shortcut under the root to a folder outside it.
        let inside_remote = |p: &str| p.split_once(':').map_or(p, |(_, rest)| rest).to_string();
        rc(&["backend", "shortcut", &format!("{}:", remote_name(&base)), &inside_remote(&outside), &format!("{}/short", inside_remote(&root))]);
        let d = RcloneDriver::new(exe.clone(), root.clone());
        d.check().unwrap();
        assert!(!matches!(d.hash("/short/x.png", None), Ok(Some(_))), "hashed through a shortcut");
        assert!(d.get("/short/x.png", None, &tmp.join("short.png")).is_err());
        assert!(!tmp.join("short.png").exists());

        // A path the drive holds two files of is left to be sorted out there, never half replaced.
        rc(&["backend", "copyid", &format!("{}:", remote_name(&base)), &id, &format!("{}:{}/img/", remote_name(&base), inside_remote(&root))]);
        let d = RcloneDriver::new(exe.clone(), root.clone());
        d.check().unwrap();
        match d.put("/img/a.png", &src, &sha2, Some(&sha2)) {
            Err(e) => assert!(e.message.contains("two files of one name"), "{}", e.message),
            Ok(_) => panic!("a push went ahead with two files of one name in the drive"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn shared_roots_name_a_configured_remote_and_nothing_more() {
        for ok in ["teamdrive:textdb", "team drive:Shared/textdb", "s3.eu:bucket/prefix", "remote:", "my_remote-2@x+y:a,b"] {
            assert_eq!(shared_root_problem(ok), None, "{ok}");
        }
        for bad in [":sftp,host=x,ssh='cmd /c calc':x", "remote,ssh=x:path", "-x:path", "--config=x:y", " a:b", "no-colon", ":local:/tmp", "a/b:c", "C:/Users/x", "C:"] {
            assert!(shared_root_problem(bad).is_some(), "{bad}");
        }
        assert_eq!(join("teamdrive:", "img/a.png"), "teamdrive:img/a.png");
        assert_eq!(join("teamdrive:textdb/", "img/a.png"), "teamdrive:textdb/img/a.png");
        assert_eq!(join("teamdrive:textdb", "img/a.png"), "teamdrive:textdb/img/a.png");
        if cfg!(windows) {
            assert_eq!(join("E:", "img/a.png"), "E:/img/a.png");
        }
    }

    #[test]
    fn lock_file_names_tell_abandoned_locks_of_this_computer() {
        let key = lock_name("/a.png");
        let millis = "00000001789460100000";
        assert_eq!(abandoned(&format!("{key}-{millis}-{}-4000000000-000000001.lock", host_word()), &key), Some(4_000_000_000));
        assert_eq!(abandoned(&format!("{key}-{millis}-{}-{}-000000001.lock", host_word(), std::process::id()), &key), None);
        assert_eq!(abandoned(&format!("{key}-{millis}-another-host-4000000000-000000001.lock"), &key), None);
        assert_eq!(abandoned(&format!("{}-{millis}-{}-4000000000-000000001.lock", lock_name("/b.png"), host_word()), &key), None);
        assert_eq!(abandoned(&format!("{key}-{millis}-{}-2-4000000000-1.lock", host_word()), &key), None, "host {}-2 is another computer", host_word());
        assert_eq!(abandoned(&format!("{key}-{}-4000000000-000000001.lock", host_word()), &key), None, "no time in the name");
    }

    #[test]
    fn rclone_store_keeps_replaced_bytes_in_its_trash_and_takes_turns() {
        let Some(exe) = test_rclone() else { return };
        let tmp = std::env::temp_dir().join(format!("textdb-rclone-{}", std::process::id()));
        let root = tmp.join("asset store ä");
        std::fs::create_dir_all(&root).unwrap();
        let root_text = root.to_string_lossy().replace('\\', "/");
        let d = RcloneDriver::new(exe.clone(), root_text.clone());
        d.check().unwrap();
        assert!(RcloneDriver::new(exe.clone(), format!("{root_text}/missing")).check().is_err());
        let trash_files = || walk(&root.join(TRASH)).into_iter().filter(|p| p.extension().is_none_or(|e| e != "lock")).collect::<Vec<_>>();
        let lock_files = || walk(&root.join(TRASH).join("locks"));

        let src = tmp.join("a.png");
        std::fs::write(&src, b"one").unwrap();
        let sha1 = hash_file(&src).unwrap().0;
        assert_eq!(d.size("/acc/a.png", None).unwrap(), None);
        assert_eq!(d.put("/acc/a.png", &src, &sha1, None).unwrap().0.as_deref(), Some("/acc/a.png"));
        assert_eq!(d.hash("/acc/a.png", None).unwrap(), Some((sha1.clone(), 3)));
        assert_eq!(d.put("/acc/a.png", &src, &sha1, None).unwrap().0.as_deref(), Some("/acc/a.png"), "the same bytes again copy nothing");

        // Other bytes of the same size and modification time replace them all the same.
        let mtime = std::fs::metadata(&src).unwrap().modified().unwrap();
        std::fs::write(&src, b"two").unwrap();
        std::fs::File::options().write(true).open(&src).unwrap().set_modified(mtime).unwrap();
        let sha2 = hash_file(&src).unwrap().0;
        d.put("/acc/a.png", &src, &sha2, Some(&sha1)).unwrap();
        assert_eq!(d.hash("/acc/a.png", None).unwrap().unwrap().0, sha2);
        assert_eq!(std::fs::read(root.join("acc/a.png")).unwrap(), b"two");
        let trashed = trash_files();
        assert_eq!(trashed.len(), 1, "{trashed:?}");
        assert_eq!(std::fs::read(&trashed[0]).unwrap(), b"one");
        assert!(!d.place("/acc/a.png", &src, &sha2, Some(&sha1)).unwrap(), "bytes other than those it replaces are kept");
        assert_eq!(trash_files().len(), 1);
        assert!(walk(&root.join("acc")).iter().all(|p| !p.to_string_lossy().ends_with(".tdbpart")));

        // Bytes the caller does not replace are kept; the new ones go next to them.
        std::fs::write(&src, b"three").unwrap();
        let sha3 = hash_file(&src).unwrap().0;
        let (placed, held) = d.put("/acc/a.png", &src, &sha3, None).unwrap();
        assert_eq!(placed.unwrap(), format!("/acc/a ({}).png", &sha3[..8]));
        assert_eq!(lock_files().len(), 1, "the lock of the name beside is held until dropped");
        drop(held);
        assert_eq!(lock_files().len(), 0);
        assert_eq!(d.hash("/acc/a.png", None).unwrap().unwrap().0, sha2);

        // Pushes of the same path take turns, whatever its letter case; another path's lock is
        // no obstacle.
        let held = d.lock("/ACC/A.png").unwrap();
        drop(d.lock("/acc/other.png").unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let other = RcloneDriver::new(exe.clone(), root_text.clone());
        let waiter = std::thread::spawn(move || {
            let h = other.lock("/acc/a.png").unwrap();
            tx.send(Instant::now()).unwrap();
            drop(h);
        });
        std::thread::sleep(Duration::from_millis(2000));
        let released = Instant::now();
        drop(held);
        let got = rx.recv().unwrap();
        waiter.join().unwrap();
        assert!(got >= released, "the second push waited for the first");
        assert_eq!(lock_files().len(), 0);

        // What a killed push of this computer left is cleared: its lock, and its partial copy.
        let locks = root.join(TRASH).join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        std::fs::write(locks.join(format!("{}-00000000000000000001-{}-4000000000-000000001.lock", lock_name("/acc/c.png"), host_word())), "/acc/c.png\n").unwrap();
        let left = root.join(format!("acc/.c.png.{}-4000000000-1.tdbpart", host_word()));
        std::fs::write(&left, b"half").unwrap();
        let held = d.lock("/acc/c.png").unwrap();
        std::fs::write(&src, b"six").unwrap();
        d.put("/acc/c.png", &src, &hash_file(&src).unwrap().0, None).unwrap();
        drop(held);
        assert!(!left.exists());
        assert_eq!(lock_files().len(), 0);

        let out = tmp.join("vault/acc/a.png");
        d.get("/acc/a.png", None, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"two");
        assert!(d.get("/acc/a.png", None, &out).is_err());
        assert!(d.get("/acc/missing.png", None, &tmp.join("vault/missing.png")).is_err());
        assert!(!tmp.join("vault/missing.png").exists());
        assert!(d.get("/acc", None, &tmp.join("vault/folder.png")).is_err(), "a folder is not an asset's bytes");
        assert!(!tmp.join("vault/folder.png").exists());
        assert!(d.size("/../escape", None).is_err());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn rclone_locks_let_several_pushes_through_one_at_a_time() {
        let Some(exe) = test_rclone() else { return };
        let tmp = std::env::temp_dir().join(format!("textdb-rclone-locks-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let root = tmp.to_string_lossy().replace('\\', "/");
        let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started = Instant::now();
        let pushes: Vec<_> = (0..5)
            .map(|_| {
                let (exe, root, busy) = (exe.clone(), root.clone(), busy.clone());
                std::thread::spawn(move || {
                    let held = RcloneDriver::new(exe, root).lock("/img/a.png").unwrap();
                    assert!(!busy.swap(true, Ordering::SeqCst), "two pushes held the lock at once");
                    std::thread::sleep(Duration::from_millis(300));
                    busy.store(false, Ordering::SeqCst);
                    drop(held);
                })
            })
            .collect();
        for push in pushes {
            push.join().unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(120), "five pushes took {:?}", started.elapsed());
        assert!(walk(&tmp.join(TRASH).join("locks")).is_empty());
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
