//! An asset store reached through rclone: a Google shared drive, SharePoint, S3, or anything else
//! rclone has a backend for. Layout, trash and locks are the local driver's, kept on the remote:
//! the asset at `/img/a.png` is `ROOT/img/a.png`, what a push replaces is copied to
//! `ROOT/.textdb-trash/<time>/…` first, and pushes of the same path take turns through lock files
//! in `ROOT/.textdb-trash/locks/<hash of the path>/`.
//!
//! rclone skips a copy whose destination has the same size and modification time, and a move then
//! deletes its source and keeps the old bytes: every copy and move here passes `--ignore-times`,
//! and what is uploaded is hashed where it landed before it is used.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::driver::{beside, host_word, lock_name, partial_name, process_running, rename_new, stamp, Driver, Held, LOCK_WAIT, TRASH};
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

/// The process id in a lock file name `HOST-PID-NANOS.lock`, when that process is of this computer
/// and not running any more.
fn abandoned(name: &str) -> Option<u32> {
    let mut parts = name.strip_suffix(".lock")?.rsplitn(3, '-');
    let (_nanos, pid, host) = (parts.next()?, parts.next()?, parts.next()?);
    let pid: u32 = pid.parse().ok()?;
    (host.eq_ignore_ascii_case(&host_word()) && !process_running(pid)).then_some(pid)
}

/// A lock file held on the remote, removed when dropped.
struct RemoteLock {
    exe: PathBuf,
    remote: String,
}

impl Drop for RemoteLock {
    fn drop(&mut self) {
        let _ = Command::new(&self.exe)
            .args(["-q", "--retries", "1", "deletefile", &self.remote])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl RcloneDriver {
    pub fn new(exe: PathBuf, root: String) -> RcloneDriver {
        RcloneDriver { exe, root }
    }

    /// Whether the root is a folder rclone reaches.
    pub fn check(&self) -> Result<()> {
        let out = self.run(&["lsjson", "--stat", "--no-mimetype", &self.root])?;
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
        let sep = if self.root.ends_with([':', '/']) { "" } else { "/" };
        Ok(format!("{}{sep}{rel}", self.root))
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
        args.push(remote.as_str());
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
        let out = self.run(&["lsjson", "--files-only", "--no-modtime", "--no-mimetype", &remote])?;
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

    /// Remove the file at `path`; one not there is fine.
    fn delete(&self, path: &str) -> Result<()> {
        let remote = self.remote(path)?;
        let out = self.run(&["--retries", "1", "deletefile", &remote])?;
        match out.status.code() {
            Some(0 | 3 | 4) => Ok(()),
            _ => Err(failure(&format!("removing {remote}"), &out)),
        }
    }

    /// Write `body` to a small file at `path`.
    fn write_small(&self, path: &str, body: &str) -> Result<()> {
        let remote = self.remote(path)?;
        let mut child = self
            .command()
            .args(["rcat", &remote])
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
        let start = format!(".{name}.{}-", host_word());
        let Ok(names) = self.names_in(dir) else { return };
        for found in names {
            let Some(rest) = found.strip_prefix(&start).and_then(|r| r.strip_suffix(".tdbpart")) else { continue };
            if rest.split('-').next().and_then(|p| p.parse::<u32>().ok()).is_some_and(|pid| !process_running(pid)) {
                let _ = self.delete(&format!("{dir}/{found}"));
            }
        }
    }

    /// Hold the lock of `path` (its case ignored). rclone cannot create a file only when none is
    /// there, so each push writes a lock file of its own into the path's lock folder and then lists
    /// the folder: it holds the lock when its file is the only one, and otherwise removes its file
    /// and tries again a moment later. Of two pushes that write at once, the one that lists later
    /// sees the other's file, so both never hold it.
    fn lock_remote(&self, path: &str) -> Result<RemoteLock> {
        let folder = format!("/{TRASH}/locks/{}", lock_name(path));
        let started = Instant::now();
        let (mut told, mut checked) = (false, None::<Instant>);
        loop {
            let now = SystemTime::now();
            let nanos = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
            let mine = format!("{}-{}-{nanos:09}.lock", host_word(), std::process::id());
            let lock = RemoteLock { exe: self.exe.clone(), remote: self.remote(&format!("{folder}/{mine}"))? };
            self.write_small(&format!("{folder}/{mine}"), &format!("{path}\nheld by process {} on {} since {}\n", std::process::id(), host_word(), stamp(now)))?;
            let others: Vec<String> = self.names_in(&folder)?.into_iter().filter(|n| *n != mine).collect();
            if others.is_empty() {
                return Ok(lock);
            }
            drop(lock);
            // A lock left by a process of this computer that is not running any more (a push that
            // was killed) is removed, now and then looked at again.
            if checked.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
                checked = Some(Instant::now());
                for other in &others {
                    if let Some(pid) = abandoned(other) {
                        eprintln!("removing the lock of {path} left by process {pid}, which is not running any more");
                        let _ = self.delete(&format!("{folder}/{other}"));
                    }
                }
            }
            if started.elapsed() > LOCK_WAIT {
                return Err(StoreError::other(format!(
                    "{path} is locked (by {}) and was not released in {} minutes; if that push is not running any more, remove {}",
                    others.join(", "),
                    LOCK_WAIT.as_secs() / 60,
                    self.folder(&folder).unwrap_or_default()
                )));
            }
            if !told {
                eprintln!("waiting for another push of {path}");
                told = true;
            }
            // Apart from one another, so two pushes that keep meeting stop doing so.
            let jitter = (u64::from(nanos) ^ u64::from(std::process::id()).wrapping_mul(2_654_435_761)) % 800;
            std::thread::sleep(Duration::from_millis(200 + jitter));
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
        self.ok(&format!("uploading {} to {part}", src.display()), &["copyto", "--ignore-times", &source, &part]).map_err(discard)?;
        match self.hash(&part_path, None).map_err(discard)? {
            Some((sha, _)) if sha == sha256 => {}
            _ => return Err(discard(StoreError::other(format!("{} changed while it was copied to the asset store; push it again", src.display())))),
        }
        match replace {
            Some(expected) if self.stat(path, false).map_err(discard)?.is_some() => {
                // A copy in the trash first, checked to be what this push replaces (not bytes put
                // there some other way); then the new copy replaces it in one move, so the asset
                // is never missing from its path.
                let trashed_path = self.trash_for(path);
                let trashed = self.remote(&trashed_path)?;
                self.ok(&format!("copying {dest} to the trash"), &["copyto", "--ignore-times", &dest, &trashed]).map_err(|e| {
                    let _ = self.delete(&trashed_path);
                    discard(e)
                })?;
                if !matches!(self.hash(&trashed_path, None), Ok(Some((sha, _))) if sha == expected) {
                    let _ = self.delete(&trashed_path);
                    let _ = self.delete(&part_path);
                    return Ok(false);
                }
            }
            _ if self.stat(path, false).map_err(discard)?.is_some() => {
                return Err(discard(StoreError::other(format!("putting {dest} in place: something is there already"))));
            }
            _ => {}
        }
        self.ok(&format!("putting {dest} in place"), &["moveto", "--ignore-times", &part, &dest]).map_err(discard)?;
        Ok(true)
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
        let out = self.ok(&format!("hashing {remote}"), &["hashsum", "sha256", "--download", &remote])?;
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

    fn get(&self, path: &str, item: Option<&str>, dest: &Path) -> Result<()> {
        let from = self.remote(item.unwrap_or(path))?;
        if dest.exists() {
            return Err(StoreError::invalid(format!("{} exists already", dest.display())));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::other(format!("{}: {e}", parent.display())))?;
        }
        let part = super::driver::partial(dest);
        let fetched = self
            .ok(&format!("downloading {from}"), &["copyto", "--ignore-times", &from, &part.to_string_lossy()])
            .and_then(|_| rename_new(&part, dest).map_err(|e| StoreError::other(format!("putting {} in place: {e}", dest.display()))));
        if fetched.is_err() {
            let _ = std::fs::remove_file(&part);
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
    fn lock_file_names_tell_abandoned_locks_of_this_computer() {
        assert_eq!(abandoned(&format!("{}-4000000000-000000001.lock", host_word())), Some(4_000_000_000));
        assert_eq!(abandoned(&format!("{}-{}-000000001.lock", host_word(), std::process::id())), None);
        assert_eq!(abandoned("another-host-4000000000-000000001.lock"), None);
        assert_eq!(abandoned("junk.lock"), None);
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

        // Pushes of the same path take turns, whatever its letter case.
        let held = d.lock("/ACC/A.png").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let other = RcloneDriver::new(exe.clone(), root_text.clone());
        let waiter = std::thread::spawn(move || {
            let h = other.lock("/acc/a.png").unwrap();
            tx.send(Instant::now()).unwrap();
            drop(h);
        });
        std::thread::sleep(Duration::from_millis(1500));
        let released = Instant::now();
        drop(held);
        let got = rx.recv().unwrap();
        waiter.join().unwrap();
        assert!(got >= released, "the second push waited for the first");
        assert_eq!(lock_files().len(), 0);

        // What a killed push of this computer left is cleared: its lock, and its partial copy.
        let locks = root.join(TRASH).join("locks").join(lock_name("/acc/c.png"));
        std::fs::create_dir_all(&locks).unwrap();
        std::fs::write(locks.join(format!("{}-4000000000-000000001.lock", host_word())), "/acc/c.png\n").unwrap();
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
        assert!(d.size("/../escape", None).is_err());
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
