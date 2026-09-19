//! The base text a synced directory keeps (ADR 0008).
//!
//! A sync base row records, per file, the git blob id of the content both sides agreed on. The
//! content itself is the file as it stood before the next edit — and once an editor overwrites
//! the file in place, nothing else has it. Without it, a local change can only be sent whole
//! and a merge has to download the base again. This cache keeps that text under
//! `.textdb/base/<blob>` in the directory, so the local diff needs no remote read.
//!
//! It is only ever an optimisation. Every read is verified against the blob id it is filed
//! under, a miss or a mismatch makes sync take its whole-file path and refile the text, and
//! deleting the directory costs one such sync. It lives under `.textdb/`, which sync never
//! walks, and the managed `.gitignore` block leaves it out of git.

use std::path::{Path, PathBuf};

use crate::sync::blob_id;

pub struct BaseCache {
    root: PathBuf,
    /// A dry run reads the cache and files nothing: it leaves the directory as it found it.
    writable: bool,
}

impl BaseCache {
    pub fn open(dir: &Path, writable: bool) -> Self {
        BaseCache { root: dir.join(".textdb").join("base"), writable }
    }

    fn file(&self, blob: &str) -> Option<PathBuf> {
        // A blob id is 40 hex characters; anything else is not filed, so a bad value cannot
        // name a path outside the cache.
        (blob.len() == 40 && blob.bytes().all(|b| b.is_ascii_hexdigit())).then(|| self.root.join(&blob[..2]).join(&blob[2..]))
    }

    /// Is `blob` filed? A stat, no read.
    pub fn has(&self, blob: &str) -> bool {
        self.file(blob).is_some_and(|p| p.is_file())
    }

    /// The text filed under `blob`, only when it still hashes to it.
    pub fn get(&self, blob: &str) -> Option<Vec<u8>> {
        let bytes = std::fs::read(self.file(blob)?).ok()?;
        (blob_id(&bytes) == blob).then_some(bytes)
    }

    /// File `bytes` under its own blob id. Written whole, then renamed into place, so a reader
    /// never sees a partial file; a failure is silent because the cache is never required.
    pub fn put(&self, bytes: &[u8]) {
        if !self.writable {
            return;
        }
        let blob = blob_id(bytes);
        let Some(path) = self.file(&blob) else { return };
        if path.is_file() {
            return;
        }
        let Some(parent) = path.parent() else { return };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let tmp = parent.join(format!(".{}.{}.tmp", &blob[2..], std::process::id()));
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_by_blob_and_verifies_on_read() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = BaseCache::open(tmp.path(), true);
        let text = b"one\ntwo\n";
        let blob = blob_id(text);
        assert!(!cache.has(&blob) && cache.get(&blob).is_none());
        cache.put(text);
        assert!(cache.has(&blob));
        assert_eq!(cache.get(&blob).as_deref(), Some(&text[..]));
        // A file that no longer hashes to its name is not returned.
        std::fs::write(tmp.path().join(".textdb/base").join(&blob[..2]).join(&blob[2..]), b"other\n").unwrap();
        assert!(cache.get(&blob).is_none());
        // Nothing but a blob id names a file.
        assert!(!cache.has("../../etc/passwd") && cache.get("x").is_none());
    }
}
