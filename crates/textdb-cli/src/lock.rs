//! The directory lock a sync holds for its whole run.
//!
//! Two syncs of one directory at once corrupt files: each reads the same base, computes both
//! sides from it and commits, so one disk edit lands twice. A turn-end hook in one agent and a
//! turn-start hook in another produce exactly that, which is what this exists to stop.
//!
//! It is an advisory lock (`flock`) rather than git's `index.lock` trick, so the kernel drops it
//! when the process dies: a killed sync leaves nothing to clean up by hand. Where advisory locks
//! are unreliable — some network shares, and Windows, which has no `flock` — the fallback is an
//! `O_EXCL` file plus a liveness check of the pid it records, which is only meaningful for a
//! holder on this host.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::store::{Result, StoreError};

/// Where the lock lives, next to the trash and the staging directory.
pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join(".textdb").join("lock")
}

/// Who holds the lock, for the message the loser prints.
#[derive(Debug, Clone, Default)]
pub struct Holder {
    pub pid: u32,
    pub host: String,
    pub started: String,
}

impl Holder {
    fn parse(text: &str) -> Holder {
        let mut h = Holder::default();
        for line in text.lines() {
            match line.split_once(char::is_whitespace) {
                Some(("pid", v)) => h.pid = v.trim().parse().unwrap_or(0),
                Some(("host", v)) => h.host = v.trim().to_string(),
                Some(("started", v)) => h.started = v.trim().to_string(),
                _ => {}
            }
        }
        h
    }

    fn describe(&self) -> String {
        // Anything may hold this lock — another sync, or `flock` from a script — and only our own
        // syncs leave a record. Without one there is nothing true to say beyond that it is held.
        let who = match (self.pid, self.host.is_empty()) {
            (0, _) => return "another process".to_string(),
            (pid, true) => format!("process {pid}"),
            (pid, false) => format!("process {pid} on {}", self.host),
        };
        if self.started.is_empty() { who } else { format!("{who}, since {}", self.started) }
    }
}

/// The lock, released when dropped — by `Drop` on a clean exit, and by the kernel otherwise.
pub struct Lock {
    file: File,
    path: PathBuf,
}

impl Drop for Lock {
    fn drop(&mut self) {
        // The record is cleared but the file stays: unlinking it would let a waiter that already
        // opened it lock a path nothing else will see, and two syncs would run after all.
        let _ = self.file.set_len(0);
        unlock(&self.file);
        let _ = &self.path;
    }
}

/// Take the lock for `dir`, waiting up to `timeout` for whoever holds it.
///
/// Waiting rather than failing outright is what a hook needs: two agents whose turns overlap
/// should queue, not error. `timeout` of zero fails at once, which is `--no-wait`.
pub fn acquire(dir: &Path, timeout: Duration) -> Result<Lock> {
    let path = lock_path(dir);
    crate::sync::textdb_dir(dir).map_err(|e| StoreError::other(format!("{}: {e}", dir.join(".textdb").display())))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| StoreError::other(format!("{}: {e}", path.display())))?;

    let deadline = Instant::now() + timeout;
    // Backoff rather than a tight loop: a first sync of a large vault takes seconds, and the
    // waiter has nothing to do until it finishes.
    let mut wait = Duration::from_millis(20);
    loop {
        if try_lock(&file) {
            let mut lock = Lock { file, path };
            lock.record()?;
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            let holder = read_holder(&path);
            return Err(StoreError::contention(format!(
                "{} is being synced by another process ({}); retry when it finishes, or raise --lock-timeout",
                dir.display(),
                holder.describe()
            )));
        }
        std::thread::sleep(wait.min(deadline.saturating_duration_since(Instant::now())));
        wait = (wait * 2).min(Duration::from_millis(250));
    }
}

impl Lock {
    /// Write who we are, so a waiter that times out can name us.
    fn record(&mut self) -> Result<()> {
        let text = format!(
            "pid {}\nhost {}\nstarted {}\n",
            std::process::id(),
            crate::assets::driver::host_word(),
            crate::assets::driver::stamp(std::time::SystemTime::now())
        );
        self.file.set_len(0).and_then(|()| self.file.rewind()).and_then(|()| self.file.write_all(text.as_bytes())).map_err(|e| {
            StoreError::other(format!("{}: {e}", self.path.display()))
        })?;
        Ok(())
    }
}

fn read_holder(path: &Path) -> Holder {
    let mut text = String::new();
    let _ = File::open(path).and_then(|mut f| f.read_to_string(&mut text));
    Holder::parse(&text)
}

#[cfg(unix)]
fn try_lock(file: &File) -> bool {
    use std::os::unix::io::AsRawFd;
    // Non-blocking, so the wait and its message are ours rather than the kernel's.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(unix)]
fn unlock(file: &File) {
    use std::os::unix::io::AsRawFd;
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

#[cfg(not(unix))]
fn try_lock(file: &File) -> bool {
    // No `flock` here. A zero-length record means nobody holds it; a record naming a pid that is
    // gone is stale and can be taken over. `Lock::record` writes ours immediately after.
    let mut text = String::new();
    let mut f = file;
    if f.rewind().and_then(|()| f.read_to_string(&mut text)).is_err() {
        return false;
    }
    text.trim().is_empty() || !alive(Holder::parse(&text).pid)
}

#[cfg(not(unix))]
fn unlock(_file: &File) {}

#[cfg(not(unix))]
fn alive(pid: u32) -> bool {
    pid != 0 && crate::assets::driver::process_running(pid)
}
