//! `fs`: plain files, one per document. No versioning, no concurrency control.
//! `replace` = read → splice → write temp → rename(2) (emulation table §5).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::*;
use crate::backends::{du, first_hit_line, query_terms};
use crate::reference::splice;

pub struct FsBackend {
    root: PathBuf,
    mode: Mode,
    base_wchar: AtomicU64,
    tmp_ctr: AtomicU64,
}

impl FsBackend {
    pub fn new(root: &Path, mode: Mode) -> anyhow::Result<Self> {
        fs::create_dir_all(root)?;
        Ok(FsBackend {
            root: root.to_path_buf(),
            mode,
            base_wchar: AtomicU64::new(io_counters::self_wchar()),
            tmp_ctr: AtomicU64::new(0),
        })
    }

    pub fn full(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    pub fn write_atomic(&self, path: &str, body: &[u8]) -> R<()> {
        let full = self.full(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = full.with_extension(format!(
            "tmp{}-{}",
            std::process::id(),
            self.tmp_ctr.fetch_add(1, Ordering::Relaxed)
        ));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(body)?;
            if self.mode == Mode::Durable {
                f.sync_all()?;
            }
        }
        fs::rename(&tmp, &full)?;
        if self.mode == Mode::Durable {
            if let Some(parent) = full.parent() {
                if let Ok(d) = fs::File::open(parent) {
                    let _ = d.sync_all();
                }
            }
        }
        Ok(())
    }

    fn rg(&self, query: &str, prefix: &str) -> R<Vec<Hit>> {
        let terms = query_terms(query);
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let dir = self.full(prefix);
        if !dir.exists() {
            return Ok(vec![]);
        }
        // Document-level AND: files containing every term (rg -l per term, intersected),
        // then the first matching line of the first term.
        let mut files: Option<std::collections::BTreeSet<String>> = None;
        for t in &terms {
            let mut cmd = Command::new("rg");
            cmd.arg("-l").arg("-i").arg("--no-messages");
            if let Some(stem) = t.strip_suffix('*') {
                cmd.arg("-e").arg(format!("(?i)\\b{}", regex_escape(stem)));
            } else {
                cmd.arg("-F").arg("-w").arg("-e").arg(t);
            }
            cmd.arg(&dir);
            let out = cmd.output()?;
            let set: std::collections::BTreeSet<String> =
                String::from_utf8_lossy(&out.stdout).lines().map(|s| s.to_string()).collect();
            files = Some(match files {
                None => set,
                Some(f) => f.intersection(&set).cloned().collect(),
            });
        }
        let mut hits = Vec::new();
        for f in files.unwrap_or_default() {
            let body = fs::read(&f)?;
            let rel = format!("/{}", Path::new(&f).strip_prefix(&self.root).unwrap().to_string_lossy());
            hits.push(Hit {
                path: rel,
                line: first_hit_line(&body, &terms),
            });
        }
        Ok(hits)
    }
}

pub fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<Entry>) -> R<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let md = e.metadata()?;
        let rel = format!("/{}", p.strip_prefix(root).unwrap().to_string_lossy());
        if rel.starts_with("/.git") {
            continue;
        }
        if md.is_dir() {
            out.push(Entry {
                path: rel,
                is_dir: true,
                nbytes: None,
            });
            walk(&p, root, out)?;
        } else {
            out.push(Entry {
                path: rel,
                is_dir: false,
                nbytes: Some(md.len()),
            });
        }
    }
    Ok(())
}

impl Backend for FsBackend {
    fn id(&self) -> &'static str {
        "fs"
    }
    fn capabilities(&self) -> Caps {
        Caps {
            replace: Cap::Emulated,
            read_lines: Cap::Emulated,
            read_version: Cap::NA,
            history: Cap::NA,
            search: Cap::Emulated,
            rename_folder: Cap::Native,
            concurrency_guard: "none (lost updates expected and counted)",
            invalid_utf8: Cap::Native,
        }
    }
    fn create(&self, path: &str, body: &[u8]) -> R<Version> {
        self.write_atomic(path, body)?;
        Ok(1)
    }
    fn delete(&self, path: &str) -> R<()> {
        let full = self.full(path);
        if full.is_dir() {
            fs::remove_dir_all(full)?;
        } else {
            fs::remove_file(full)?;
        }
        Ok(())
    }
    fn rename(&self, from: &str, to: &str) -> R<()> {
        let dst = self.full(to);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        fs::rename(self.full(from), dst)?;
        Ok(())
    }
    fn list(&self, prefix: &str) -> R<Vec<Entry>> {
        let mut out = Vec::new();
        let dir = self.full(prefix);
        if dir.exists() {
            walk(&dir, &self.root, &mut out)?;
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
    fn read(&self, path: &str) -> R<Vec<u8>> {
        fs::read(self.full(path)).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BackendError::NotFound(path.to_string()),
            _ => e.into(),
        })
    }
    fn read_lines(&self, path: &str, from: u64, to: u64) -> R<Vec<u8>> {
        Ok(slice_lines(&self.read(path)?, from, to))
    }
    fn read_version(&self, _path: &str, _v: Version) -> R<Vec<u8>> {
        Err(BackendError::NotSupported("fs has no versions"))
    }
    fn overwrite(&self, path: &str, body: &[u8]) -> R<Version> {
        self.write_atomic(path, body)?;
        Ok(0)
    }
    fn replace(&self, path: &str, old: &[u8], new: &[u8], _base: Option<Version>) -> R<WriteOutcome> {
        let cur = self.read(path)?;
        match splice(&cur, old, new) {
            Some(next) => {
                self.write_atomic(path, &next)?;
                Ok(WriteOutcome::Committed { version: 0, direct: true })
            }
            // Old text vanished (someone else's write): report as conflict with current text.
            None => Ok(WriteOutcome::Conflict { current_region: cur }),
        }
    }
    fn append(&self, path: &str, tail: &[u8]) -> R<Version> {
        let mut cur = self.read(path)?;
        cur.extend_from_slice(tail);
        self.write_atomic(path, &cur)?;
        Ok(0)
    }
    fn search(&self, query: &str, prefix: &str) -> R<Vec<Hit>> {
        self.rg(query, prefix)
    }
    fn history(&self, _path: &str) -> R<Vec<Version>> {
        Err(BackendError::NotSupported("fs has no history"))
    }
    fn storage_bytes(&self) -> R<u64> {
        Ok(du(&self.root))
    }
    fn bytes_written_since_reset(&self) -> R<u64> {
        Ok(io_counters::self_wchar().saturating_sub(self.base_wchar.load(Ordering::Relaxed)))
    }
    fn reset_counters(&self) -> R<()> {
        self.base_wchar.store(io_counters::self_wchar(), Ordering::Relaxed);
        Ok(())
    }
}

/// Lines `[from, to]` 1-based inclusive of a byte string.
pub fn slice_lines(body: &[u8], from: u64, to: u64) -> Vec<u8> {
    if from == 0 || to < from {
        return Vec::new();
    }
    let mut line = 1u64;
    let mut start = None;
    let mut i = 0;
    if from == 1 {
        start = Some(0);
    }
    while i < body.len() {
        if body[i] == b'\n' {
            line += 1;
            if line == from {
                start = Some(i + 1);
            }
            if line == to + 1 {
                return body[start.unwrap_or(i + 1)..i + 1].to_vec();
            }
        }
        i += 1;
    }
    match start {
        Some(s) => body[s..].to_vec(),
        None => Vec::new(),
    }
}
