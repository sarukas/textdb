//! `fs-git`: files plus one `git commit` per write. The incumbent.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::*;
use crate::backends::fs::{regex_escape, slice_lines, FsBackend};
use crate::backends::{du, first_hit_line_and_text, query_terms};
use crate::reference::splice;

pub struct FsGitBackend {
    fs: FsBackend,
    root: PathBuf,
    base_children: AtomicU64,
}

impl FsGitBackend {
    pub fn new(root: &Path, mode: Mode) -> anyhow::Result<Self> {
        let fs = FsBackend::new(root, mode)?;
        let b = FsGitBackend {
            fs,
            root: root.to_path_buf(),
            base_children: AtomicU64::new(0),
        };
        b.git(&["init", "-q"])?;
        b.git(&["config", "user.email", "bench@textdb"])?;
        b.git(&["config", "user.name", "bench"])?;
        b.git(&["config", "core.fsync", if mode == Mode::Durable { "all" } else { "none" }])?;
        b.git(&["config", "core.fsyncMethod", "fsync"])?;
        b.git(&["config", "gc.auto", "0"])?;
        b.git(&["commit", "-q", "--allow-empty", "-m", "init"])?;
        Ok(b)
    }

    fn git(&self, args: &[&str]) -> R<Vec<u8>> {
        let out = Command::new("git").arg("-C").arg(&self.root).args(args).output()?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).to_string();
            return Err(BackendError::Other(msg));
        }
        Ok(out.stdout)
    }

    fn rel(path: &str) -> &str {
        path.trim_start_matches('/')
    }

    /// `git add && git commit` of one path. `index.lock` failures are reported as contention.
    fn commit_path(&self, path: &str) -> R<Result<(), ()>> {
        let rel = Self::rel(path);
        for args in [vec!["add", "--", rel], vec!["commit", "-q", "-m", "edit", "--", rel]] {
            let out = Command::new("git").arg("-C").arg(&self.root).args(&args).output()?;
            if !out.status.success() {
                let msg = String::from_utf8_lossy(&out.stderr).to_string();
                if msg.contains("index.lock") || msg.contains("Unable to create") || msg.contains("lock") {
                    return Ok(Err(()));
                }
                if msg.contains("nothing to commit") || msg.contains("no changes added") {
                    return Ok(Ok(()));
                }
                return Err(BackendError::Other(msg));
            }
        }
        Ok(Ok(()))
    }

    fn shas(&self, path: &str) -> R<Vec<String>> {
        let out = self.git(&["log", "--follow", "--reverse", "--format=%H", "--", Self::rel(path)])?;
        Ok(String::from_utf8_lossy(&out).lines().map(|s| s.to_string()).collect())
    }
}

impl Backend for FsGitBackend {
    fn id(&self) -> &'static str {
        "fs-git"
    }
    fn capabilities(&self) -> Caps {
        Caps {
            replace: Cap::Emulated,
            read_lines: Cap::Emulated,
            read_version: Cap::Emulated,
            history: Cap::Emulated,
            search: Cap::Emulated,
            rename_folder: Cap::Emulated,
            concurrency_guard: "none at write; index.lock contention counted",
            invalid_utf8: Cap::Native,
        }
    }
    fn create(&self, path: &str, body: &[u8]) -> R<Version> {
        self.fs.write_atomic(path, body)?;
        match self.commit_path(path)? {
            Ok(()) => Ok(1),
            Err(()) => Err(BackendError::Other("index.lock contention on create".into())),
        }
    }
    fn delete(&self, path: &str) -> R<()> {
        self.git(&["rm", "-r", "-q", "--", Self::rel(path)])?;
        self.git(&["commit", "-q", "-m", "delete"])?;
        Ok(())
    }
    fn rename(&self, from: &str, to: &str) -> R<()> {
        let dst = self.fs.full(to);
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        self.git(&["mv", Self::rel(from), Self::rel(to)])?;
        self.git(&["commit", "-q", "-m", "rename"])?;
        Ok(())
    }
    fn list(&self, prefix: &str) -> R<Vec<Entry>> {
        self.fs.list(prefix)
    }
    fn read(&self, path: &str) -> R<Vec<u8>> {
        self.fs.read(path)
    }
    fn read_versioned(&self, path: &str) -> R<(Vec<u8>, Version)> {
        let body = self.fs.read(path)?;
        let v = self.shas(path)?.len() as u64;
        Ok((body, v))
    }
    fn read_lines(&self, path: &str, from: u64, to: u64) -> R<Vec<u8>> {
        Ok(slice_lines(&self.fs.read(path)?, from, to))
    }
    fn read_version(&self, path: &str, v: Version) -> R<Vec<u8>> {
        let shas = self.shas(path)?;
        let sha = shas
            .get((v as usize).saturating_sub(1))
            .ok_or_else(|| BackendError::NotFound(format!("{} v{}", path, v)))?;
        // Path at that commit may differ after renames; resolve via --follow list order.
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["show", &format!("{}:{}", sha, Self::rel(path))])
            .output()?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        // Fallback: find the path in that commit's tree by blob lookup via diff-tree.
        let names = self.git(&["diff-tree", "--no-commit-id", "--name-only", "-r", "--root", sha])?;
        for n in String::from_utf8_lossy(&names).lines() {
            let o = Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(["show", &format!("{}:{}", sha, n)])
                .output()?;
            if o.status.success() {
                return Ok(o.stdout);
            }
        }
        Err(BackendError::NotFound(format!("{} v{}", path, v)))
    }
    fn overwrite(&self, path: &str, body: &[u8]) -> R<Version> {
        self.fs.write_atomic(path, body)?;
        match self.commit_path(path)? {
            Ok(()) => Ok(0),
            Err(()) => Err(BackendError::Other("index.lock contention".into())),
        }
    }
    fn replace(&self, path: &str, old: &[u8], new: &[u8], _base: Option<Version>) -> R<WriteOutcome> {
        let cur = self.fs.read(path)?;
        match splice(&cur, old, new) {
            Some(next) => {
                self.fs.write_atomic(path, &next)?;
                match self.commit_path(path)? {
                    Ok(()) => Ok(WriteOutcome::Committed { version: 0, direct: true }),
                    Err(()) => Ok(WriteOutcome::Contention),
                }
            }
            None => Ok(WriteOutcome::Conflict { current_region: cur }),
        }
    }
    fn append(&self, path: &str, tail: &[u8]) -> R<Version> {
        let mut cur = self.fs.read(path)?;
        cur.extend_from_slice(tail);
        self.overwrite(path, &cur)
    }
    fn search(&self, query: &str, prefix: &str) -> R<Vec<Hit>> {
        let terms = query_terms(query);
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let mut args: Vec<String> = vec!["grep".into(), "-l".into(), "-i".into(), "--all-match".into()];
        for t in &terms {
            args.push("-e".into());
            if let Some(stem) = t.strip_suffix('*') {
                args.push(format!("\\b{}", regex_escape(stem)));
            } else {
                args.push(format!("\\b{}\\b", regex_escape(t)));
            }
        }
        args.push("--".into());
        args.push(Self::rel(prefix).to_string());
        let out = Command::new("git").arg("-C").arg(&self.root).args(&args).output()?;
        let mut hits = Vec::new();
        for f in String::from_utf8_lossy(&out.stdout).lines() {
            let body = std::fs::read(self.root.join(f))?;
            let (line, snippet) = first_hit_line_and_text(&body, &terms);
            hits.push(Hit {
                path: format!("/{}", f),
                line,
                snippet,
            });
        }
        Ok(hits)
    }
    fn history(&self, path: &str) -> R<Vec<Version>> {
        Ok((1..=self.shas(path)?.len() as u64).collect())
    }
    fn storage_bytes(&self) -> R<u64> {
        Ok(du(&self.root))
    }
    fn bytes_written_since_reset(&self) -> R<u64> {
        let own = self.fs.bytes_written_since_reset()?;
        let kids = io_counters::children_write_bytes().saturating_sub(self.base_children.load(Ordering::Relaxed));
        Ok(own + kids)
    }
    fn reset_counters(&self) -> R<()> {
        self.fs.reset_counters()?;
        self.base_children.store(io_counters::children_write_bytes(), Ordering::Relaxed);
        Ok(())
    }
    fn maintenance(&self) -> R<&'static str> {
        self.git(&["gc", "-q", "--aggressive"])?;
        Ok("git gc --aggressive")
    }
}
