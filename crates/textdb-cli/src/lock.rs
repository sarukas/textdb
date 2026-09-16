//! The directory lock a sync holds for its whole run.
//!
//! Two syncs of one directory at once corrupt files: each reads the same base, computes both
//! sides from it and commits, so one disk edit lands twice. A turn-end hook in one agent and a
//! turn-start hook in another produce exactly that, which is what this exists to stop.
//!
//! It is an advisory lock rather than git's `index.lock` trick, so the operating system drops it
//! when the process dies: a killed sync leaves nothing to clean up by hand. `File::try_lock` is
//! `flock` on Unix and `LockFileEx` on Windows, so one call covers both; where a filesystem does
//! not support locking at all it reports so, and a sync there runs unlocked rather than pretending.
//!
//! The hand-rolled fallback this replaced was not a lock: two processes could both read an empty
//! record and both go on, because reading and writing it were separate steps.

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
/// A `timeout` of zero — the default — fails at once. Two syncs of one directory are a
/// collision, and one of them failing visibly (exit 4, meaning retry shortly) is better than
/// both reporting success because the second found the first had already done the work.
/// `--lock-timeout` queues instead, for a caller that would rather wait than retry.
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
        let outcome = try_lock(&file);
        if outcome != Outcome::Taken {
            if outcome == Outcome::Unsupported {
                eprintln!(
                    "note: {} does not support file locking, so two syncs of it at once cannot be kept apart",
                    dir.display()
                );
            }
            let mut lock = Lock { file, path };
            lock.record()?;
            // Holding the lock means no other sync of this directory is running, so anything in
            // its staging directory is from a run that is gone — a sync a hook timed out on, say.
            // Nothing else would ever remove them, and they are whole documents, not scraps.
            clear_staging(dir);
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            let holder = read_holder(&path);
            return Err(StoreError::contention(format!(
                "{} is already being synced by another process ({}); wait for it to finish and run again, or pass --lock-timeout to queue behind it",
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

/// Whether we now hold the lock.
///
/// A filesystem that cannot lock at all (some network mounts) reports `Unsupported`; there is no
/// honest lock to take, so the sync goes ahead rather than failing on every run or, worse,
/// believing itself alone. Every other error means someone holds it.
fn try_lock(file: &File) -> Outcome {
    use std::fs::TryLockError;
    match file.try_lock() {
        Ok(()) => Outcome::Held,
        Err(TryLockError::WouldBlock) => Outcome::Taken,
        Err(TryLockError::Error(e)) if e.kind() == std::io::ErrorKind::Unsupported => Outcome::Unsupported,
        Err(TryLockError::Error(_)) => Outcome::Taken,
    }
}

#[derive(PartialEq, Eq)]
enum Outcome {
    Held,
    Taken,
    /// This filesystem does not do locking.
    Unsupported,
}

fn unlock(file: &File) {
    let _ = file.unlock();
}


/// Remove what a dead run left in `.textdb/tmp`. Best effort: a file that cannot be removed is
/// not worth failing a sync over, and the next one will try again.
fn clear_staging(dir: &Path) {
    let staging = dir.join(".textdb").join("tmp");
    let Ok(entries) = std::fs::read_dir(&staging) else { return };
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|e| e == "tdbtmp") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
