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

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
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
}

/// An entry as `rclone lsjson` lists it.
#[derive(Deserialize)]
struct Listed {
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

/// The process id in the lock file name `KEY-MILLIS-HOST-PID-NANOS.lock`, when that process is of
/// this computer and not running any more.
fn abandoned(name: &str, key: &str) -> Option<u32> {
    let rest = name.strip_prefix(&format!("{key}-"))?.strip_suffix(".lock")?;
    let (millis, rest) = rest.split_once('-')?;
    if millis.is_empty() || !millis.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest = rest.strip_prefix(&format!("{}-", host_word()))?;
    let (pid, nanos) = rest.split_once('-')?;
    if nanos.is_empty() || !nanos.bytes().all(|b| b.is_ascii_digit()) {
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
        RcloneDriver { exe, root }
    }

    /// Whether the root is a folder rclone reaches, giving up on a remote that does not answer.
    pub fn check(&self) -> Result<()> {
        let out = self.run(&["--contimeout", "15s", "--timeout", "30s", "--retries", "1", "--low-level-retries", "2", "lsjson", "--stat", "--no-mimetype", "--", &self.root])?;
        match out.status.code() {
            Some(0) if serde_json::from_slice::<Listed>(&out.stdout).is_ok_and(|l| l.is_dir) => Ok(()),
            Some(0) => Err(StoreError::invalid(format!("{} is a file, not a folder", self.root))),
            Some(3 | 4) => Err(StoreError::invalid(format!("the folder {} is not there (rclone mkdir {} makes it)", self.root, self.root))),
            _ => Err(failure(&format!("reaching {}", self.root), &out)),
        }
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

    fn command(&self) -> Command {
        let mut c = Command::new(&self.exe);
        // A flag set through the environment (RCLONE_IGNORE_EXISTING, RCLONE_DRY_RUN…) would
        // change what these commands do; rclone's configuration still comes through.
        // Names compared in upper case: Windows, and rclone there, do not tell them apart.
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
        Ok(self.names_in(folder)?.into_iter().filter(|n| n.starts_with(&start) && n.ends_with(".lock")).collect())
    }

    /// Hold the lock of `path` (its case ignored). rclone cannot create a file only when none is
    /// there, so a push lists the lock folder first and writes a lock file of its own
    /// (`KEY-MILLIS-HOST-PID-NANOS.lock`) only when no other push's file of that path is listed; it
    /// holds the lock when two listings a moment apart show its file and no other. Where several
    /// wrote at once, the file whose name sorts first (the push that started first) stays and the
    /// others are removed; while another file is listed a push waits, longer each time. Of two
    /// pushes that wrote at once, the one that lists later sees the other's file, as long as the
    /// provider lists what was written, so both never hold the lock.
    fn lock_remote(&self, path: &str) -> Result<RemoteLock> {
        let folder = format!("/{TRASH}/locks");
        let key = lock_name(path);
        let now = SystemTime::now();
        let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
        let mine = format!("{key}-{:020}-{}-{}-{:09}.lock", since.as_millis(), host_word(), std::process::id(), since.subsec_nanos());
        let mine_path = format!("{folder}/{mine}");
        let body = format!("{path}\nheld by process {} on {} since {}\n", std::process::id(), host_word(), stamp(now));
        let started = Instant::now();
        let (mut told, mut checked, mut wait) = (false, None::<Instant>, Duration::from_millis(200));
        // This push's lock file, once written; removed when dropped.
        let mut written: Option<RemoteLock> = None;
        loop {
            let files = self.lock_files(&folder, &key)?;
            let others: Vec<String> = files.iter().filter(|f| **f != mine).cloned().collect();
            if started.elapsed() > LOCK_WAIT {
                return Err(StoreError::other(format!(
                    "{path} is locked ({}) and was not released in {} minutes; if that push is not running any more, remove its file from {}",
                    if others.is_empty() { "this push's lock file was not listed".to_string() } else { others.join(", ") },
                    LOCK_WAIT.as_secs() / 60,
                    self.folder(&folder).unwrap_or_default()
                )));
            }
            if others.is_empty() {
                match &written {
                    None => {
                        self.write_small(&mine_path, &body).inspect_err(|_| drop(self.delete(&mine_path)))?;
                        written = Some(RemoteLock { driver: RcloneDriver::new(self.exe.clone(), self.root.clone()), path: mine_path.clone() });
                    }
                    Some(_) if files.len() == 1 => {
                        std::thread::sleep(Duration::from_millis(300));
                        let again = self.lock_files(&folder, &key)?;
                        if again.len() == 1 && again[0] == mine {
                            if let Some(lock) = written.take() {
                                return Ok(lock);
                            }
                        }
                    }
                    // Written, not listed yet.
                    Some(_) => std::thread::sleep(Duration::from_millis(200)),
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
            wait = (wait * 3 / 2).min(Duration::from_secs(4));
        }
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
        let source = src.to_string_lossy();
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
                let copied = self.ok(&format!("copying {dest} to the trash"), &["copyto", "--ignore-times", "--inplace", "--", &dest, &self.remote(&copy)?]).and_then(|_| self.hash(&copy, None));
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
        self.remote(path).unwrap_or_else(|_| path.to_string())
    }

    fn size(&self, path: &str, item: Option<&str>) -> Result<Option<u64>> {
        Ok(self.stat(item.unwrap_or(path), false)?.map(|l| l.size as u64))
    }

    fn hash(&self, path: &str, item: Option<&str>) -> Result<Option<(String, u64)>> {
        let path = item.unwrap_or(path);
        let Some(l) = self.stat(path, true)? else { return Ok(None) };
        let size = l.size as u64;
        if let Some(sha) = l.hashes.get("sha256").filter(|h| h.len() == 64) {
            return Ok(Some((sha.to_ascii_lowercase(), size)));
        }
        // The provider keeps no SHA-256 of this file: rclone reads it through.
        let remote = self.remote(path)?;
        let out = self.ok(&format!("hashing {remote}"), &["hashsum", "sha256", "--download", "--", &remote])?;
        let text = String::from_utf8_lossy(&out.stdout);
        match text.split_whitespace().next().filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            Some(sha) => Ok(Some((sha.to_ascii_lowercase(), size))),
            None => Err(StoreError::other(format!("hashing {remote}: unexpected rclone output"))),
        }
    }

    fn lock(&self, path: &str) -> Result<Held> {
        self.remote(path)?;
        Ok(Held::of(self.lock_remote(path)?))
    }

    fn put(&self, path: &str, src: &Path, sha256: &str, replaces: Option<&str>) -> Result<(Option<String>, Held)> {
        self.remote(path)?;
        // The item is the path the bytes were put at, as for a local store.
        let here = || (Some(path.to_string()), Held::default());
        match self.hash(path, None)? {
            Some((sha, _)) if sha == sha256 => Ok(here()),
            None => self.place(path, src, sha256, None).map(|_| here()),
            Some((sha, _)) if Some(sha.as_str()) == replaces => match self.place(path, src, sha256, replaces)? {
                true => Ok(here()),
                false => self.put_beside(path, src, sha256),
            },
            // Bytes something else may still name: kept, and these go next to them.
            Some(_) => self.put_beside(path, src, sha256),
        }
    }

    /// `dest` is the caller's own partial name, so rclone downloads straight to it.
    fn get(&self, path: &str, item: Option<&str>, dest: &Path) -> Result<()> {
        let at = item.unwrap_or(path);
        let from = self.remote(at)?;
        if dest.exists() {
            return Err(StoreError::invalid(format!("{} exists already", dest.display())));
        }
        // A folder there would be copied whole.
        if self.stat(at, false)?.is_none() {
            return Err(StoreError::not_found(format!("{from} is not a file in the asset store")));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::other(format!("{}: {e}", parent.display())))?;
        }
        let fetched = self.ok(&format!("downloading {from}"), &["copyto", "--ignore-times", "--inplace", "--", &from, &dest.to_string_lossy()]).map(|_| ());
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

    /// rclone for tests: `TEXTDB_RCLONE`, else `rclone` on the PATH. Without one the test is
    /// skipped, unless `TEXTDB_REQUIRE_RCLONE` is set (as in CI).
    fn test_rclone() -> Option<PathBuf> {
        let exe = executable();
        let runs = Command::new(&exe).arg("version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        if !runs && std::env::var_os("TEXTDB_REQUIRE_RCLONE").is_some() {
            panic!("TEXTDB_REQUIRE_RCLONE is set, but {} does not run", exe.display());
        }
        runs.then_some(exe)
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
