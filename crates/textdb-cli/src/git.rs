//! The git commands `sync` and `git-status` use, run as `git -C DIR …`. Git is optional: without
//! it, or outside a checkout, sync works on the files alone.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::store::StoreError;
use crate::Result;

/// Run git in `dir`, feeding `stdin`; its stdout on success, the error it printed otherwise.
fn run(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> std::result::Result<Vec<u8>, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("git: {e}"))?;
    // Fed from a thread, so a large input cannot deadlock against git filling its stdout.
    let feeder = match (stdin, child.stdin.take()) {
        (Some(input), Some(mut pipe)) => {
            let input = input.to_vec();
            Some(std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
            }))
        }
        _ => None,
    };
    let out: Output = child.wait_with_output().map_err(|e| format!("git: {e}"))?;
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn text(dir: &Path, args: &[&str]) -> Option<String> {
    let out = run(dir, args, None).ok()?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn nul_separated(items: impl IntoIterator<Item = String>) -> Vec<u8> {
    items.into_iter().flat_map(|s| s.into_bytes().into_iter().chain([0])).collect()
}

/// A remote URL without the user name or token some carry.
fn without_credentials(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if let Some(at) = rest.find('@') {
            if !rest[..at].contains('/') {
                return format!("{scheme}://{}", &rest[at + 1..]);
            }
        }
    }
    url.to_string()
}

/// The checkout a directory is in.
#[derive(Debug, Clone)]
pub struct Repo {
    /// `None` before the first commit.
    pub commit: Option<String>,
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    pub remote: Option<String>,
    /// No uncommitted changes below the directory.
    pub clean: bool,
}

/// The checkout `dir` is in; `None` when it is in none, or git is not installed.
pub fn repo(dir: &Path) -> Option<Repo> {
    if !dir.is_dir() || text(dir, &["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
        return None;
    }
    Some(Repo {
        commit: text(dir, &["rev-parse", "--verify", "-q", "HEAD"]),
        branch: text(dir, &["symbolic-ref", "--short", "-q", "HEAD"]),
        remote: text(dir, &["config", "--get", "remote.origin.url"]).map(|u| without_credentials(&u)),
        clean: run(dir, &["status", "--porcelain", "--", "."], None).is_ok_and(|o| o.is_empty()),
    })
}

/// The full id of the commit `rev` names.
pub fn resolve(dir: &Path, rev: &str) -> Result<String> {
    text(dir, &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")])
        .ok_or_else(|| StoreError::invalid(format!("'{rev}' is not a commit in {}", dir.display())))
}

/// The regular files of `commit` below `dir`, by path relative to it, with their blob ids.
pub fn tree(dir: &Path, commit: &str) -> Result<BTreeMap<String, String>> {
    // In a subdirectory, ls-tree lists only what is below it, relative to it.
    let out = run(dir, &["ls-tree", "-r", "-z", commit], None).map_err(StoreError::other)?;
    let mut files = BTreeMap::new();
    for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let record = String::from_utf8_lossy(record);
        let Some((meta, path)) = record.split_once('\t') else { continue };
        let mut parts = meta.split(' ');
        if let (Some(mode), Some("blob"), Some(oid)) = (parts.next(), parts.next(), parts.next()) {
            if mode != "120000" {
                files.insert(path.to_string(), oid.to_string());
            }
        }
    }
    Ok(files)
}

/// `rel` as `commit` has it, the way a checkout writes it: line-ending conversion and other
/// filters applied.
pub fn show(dir: &Path, commit: &str, rel: &str) -> Result<Vec<u8>> {
    run(dir, &["cat-file", "--filters", &format!("{commit}:./{rel}")], None).map_err(StoreError::other)
}

/// The newest commit that changed a file, and how many did.
#[derive(Debug, Clone)]
pub struct Change {
    pub commit: String,
    pub author: String,
    pub subject: String,
    pub commits: usize,
}

impl Change {
    /// `git 1a2b3c4: Fix the intro (and 2 earlier commits)`: the message of a store commit that
    /// takes in this change.
    pub fn message(&self) -> String {
        let more = match self.commits {
            1 => String::new(),
            2 => " (and 1 earlier commit)".to_string(),
            n => format!(" (and {} earlier commits)", n - 1),
        };
        format!("git {}: {}{more}", short(&self.commit), self.subject)
    }
}

pub fn short(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
}

/// For each file below `dir` that `from..to` changed, by path relative to `dir`, the newest
/// commit that changed it. Empty when git cannot say (for example `from` is gone).
pub fn changes(dir: &Path, from: &str, to: &str) -> HashMap<String, Change> {
    let range = format!("{from}..{to}");
    let args = ["log", "--no-renames", "--format=%x01%H%x09%an%x09%s", "--name-only", "--relative", &range, "--", "."];
    let Ok(out) = run(dir, &args, None) else {
        return HashMap::new();
    };
    let mut found: HashMap<String, Change> = HashMap::new();
    let mut current: Option<(String, String, String)> = None;
    for line in String::from_utf8_lossy(&out).lines() {
        if let Some(head) = line.strip_prefix('\u{1}') {
            let mut parts = head.splitn(3, '\t');
            let mut next = || parts.next().unwrap_or("").to_string();
            current = Some((next(), next(), next()));
        } else if let (false, Some((commit, author, subject))) = (line.is_empty(), &current) {
            // Newest first: the first commit seen for a file is its latest change.
            found
                .entry(line.to_string())
                .and_modify(|c| c.commits += 1)
                .or_insert_with(|| Change {
                    commit: commit.clone(),
                    author: author.clone(),
                    subject: subject.clone(),
                    commits: 1,
                });
        }
    }
    found
}

/// Which of `rels` (relative to `dir`) git ignores. Tracked files are never ignored.
pub fn ignored(dir: &Path, rels: &[String]) -> HashSet<String> {
    if rels.is_empty() {
        return HashSet::new();
    }
    // Exit status 1 means none is ignored.
    let out = run(dir, &["check-ignore", "-z", "--stdin"], Some(&nul_separated(rels.iter().cloned()))).unwrap_or_default();
    out.split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

/// Commit exactly `paths` (relative to `dir`; created, changed or deleted), leaving anything else
/// in the checkout as it is. The new commit, or `None` when they match HEAD already.
pub fn commit(dir: &Path, paths: &[String], message: &str) -> Result<Option<String>> {
    let spec = |paths: &[String]| nul_separated(paths.iter().map(|p| format!(":(literal){p}")));
    let from_stdin = ["--pathspec-from-file=-", "--pathspec-file-nul"];
    // A deleted file git never tracked, or an ignored new one, has nothing to stage.
    let ignored = ignored(dir, paths);
    let tracked: HashSet<String> = run(dir, &[&["ls-files", "-z"][..], &from_stdin[..]].concat(), Some(&spec(paths)))
        .unwrap_or_default()
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    let stage: Vec<String> = paths
        .iter()
        .filter(|p| !ignored.contains(*p) && (tracked.contains(*p) || dir.join(p).exists()))
        .cloned()
        .collect();
    if stage.is_empty() {
        return Ok(None);
    }
    run(dir, &[&["add", "-A"][..], &from_stdin[..]].concat(), Some(&spec(&stage))).map_err(StoreError::other)?;
    // `git diff` takes no pathspec file: list everything staged below `dir` and look for ours.
    let staged = run(dir, &["diff", "--cached", "--name-only", "-z", "--relative"], None).map_err(StoreError::other)?;
    let wanted: HashSet<&str> = stage.iter().map(String::as_str).collect();
    if !staged
        .split(|&b| b == 0)
        .any(|p| wanted.contains(String::from_utf8_lossy(p).as_ref()))
    {
        return Ok(None);
    }
    run(dir, &[&["commit", "-q", "-m", message][..], &from_stdin[..]].concat(), Some(&spec(&stage))).map_err(StoreError::other)?;
    resolve(dir, "HEAD").map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_lose_their_credentials() {
        assert_eq!(without_credentials("https://user:token@github.com/a/b.git"), "https://github.com/a/b.git");
        assert_eq!(without_credentials("https://github.com/a/b@2.git"), "https://github.com/a/b@2.git");
        assert_eq!(without_credentials("git@github.com:a/b.git"), "git@github.com:a/b.git");
    }
}
