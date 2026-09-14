//! `textdb links` and `textdb backlinks`.

use std::collections::HashSet;
use std::path::Path;

use textdb_sqlite::db::parent_of;
use textdb_sqlite::links::join;

use crate::store::{LinkRow, MovedLink, Store};
use crate::{emit_json, out, Result};

/// Statuses `links --broken` lists.
pub const BROKEN: &[&str] = &["broken", "anchor-missing", "not-in-store"];

/// How a link is written, from its parts.
fn shape(kind: &str, target: &str, anchor: Option<&str>) -> String {
    let anchor = anchor.map(|a| format!("#{a}")).unwrap_or_default();
    match kind {
        "wiki" => format!("[[{target}{anchor}]]"),
        "embed" => format!("![[{target}{anchor}]]"),
        "image" => format!("![]({target}{anchor})"),
        _ => format!("[]({target}{anchor})"),
    }
}

pub fn written(l: &LinkRow) -> String {
    shape(&l.kind, &l.target, l.anchor.as_deref())
}

/// What `mv` says about the links that pointed at what moved.
pub fn moved_text(from: &str, links: &[MovedLink]) -> String {
    let files = |ls: &[&MovedLink]| ls.iter().map(|l| l.path.as_str()).collect::<HashSet<_>>().len();
    let (done, left): (Vec<&MovedLink>, Vec<&MovedLink>) = links.iter().partition(|l| l.version.is_some());
    let mut s = String::new();
    if !done.is_empty() {
        s.push_str(&format!("rewrote {} links in {} files\n", done.len(), files(&done)));
    }
    if !left.is_empty() {
        s.push_str(&format!(
            "{} links in {} files pointed to what moved from {from} and no longer do (--update-links rewrites them):\n",
            left.len(),
            files(&left)
        ));
        for l in left {
            s.push_str(&format!("  {}:{}: {} (now {})\n", l.path, l.line, shape(&l.kind, &l.target, None), l.now_at));
        }
    }
    s
}

/// What `rm` says about the links that pointed into what it deleted.
pub fn deleted_text(links: &[LinkRow]) -> String {
    if links.is_empty() {
        return String::new();
    }
    let files = links.iter().map(|l| l.path.as_str()).collect::<HashSet<_>>().len();
    let mut s = format!("{} links in {files} files pointed here and are now broken:\n", links.len());
    for l in links {
        s.push_str(&format!("  {}:{}: {}\n", l.path, l.line, written(l)));
    }
    s
}

fn line_of(l: &LinkRow) -> String {
    let status = l.status.as_deref().unwrap_or("unresolved");
    let to = match (&l.resolved, status) {
        (Some(r), "ok") => format!("-> {r}"),
        (Some(r), s) => format!("-> {r} ({s})"),
        (None, s) => format!("({s})"),
    };
    format!("{}:{}: {} {to}\n", l.path, l.line, written(l))
}

/// Files below `dir` (relative, lower case) and their names, for links to files a store does
/// not hold.
struct Disk {
    paths: HashSet<String>,
    names: HashSet<String>,
}

impl Disk {
    /// The files below `dir`, by the store path they have in the folder `prefix` it holds.
    fn read(dir: &Path, prefix: &str) -> Result<Disk> {
        let base = if prefix == "/" { String::new() } else { prefix.to_lowercase() };
        let mut disk = Disk { paths: HashSet::new(), names: HashSet::new() };
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if entry.file_type()?.is_dir() {
                    if !crate::sync::SKIP_DIRS.contains(&name.as_str()) {
                        stack.push(entry.path());
                    }
                } else if let Ok(rel) = entry.path().strip_prefix(dir) {
                    disk.paths.insert(format!("{base}/{}", rel.to_string_lossy().replace('\\', "/")).to_lowercase());
                    disk.names.insert(name.to_lowercase());
                }
            }
        }
        Ok(disk)
    }

    fn has(&self, l: &LinkRow) -> bool {
        let target = l.target.to_lowercase();
        match l.kind.as_str() {
            "md" | "image" => {
                join(&parent_of(&l.path).to_lowercase(), &target).is_some_and(|p| self.paths.contains(&p))
                    || join("/", &target).is_some_and(|p| self.paths.contains(&p))
            }
            _ if target.contains('/') => {
                let suffix = format!("/{}", target.trim_start_matches('/'));
                self.paths.iter().any(|p| p.ends_with(&suffix))
            }
            _ => self.names.contains(&target),
        }
    }
}

pub fn links(st: &mut dyn Store, path: &str, broken: bool, dir: Option<&Path>, json: bool) -> Result<()> {
    // With a directory, links to assets whose files are not there are broken too.
    let statuses: Vec<&str> = match (broken, dir.is_some()) {
        (true, true) => BROKEN.iter().copied().chain(["ok", "ambiguous"]).collect(),
        (true, false) => BROKEN.to_vec(),
        (false, _) => Vec::new(),
    };
    let mut rows = st.links(path, &statuses)?;
    if let Some(dir) = dir {
        // The directory holds the store folder it was last synced with, else the whole store.
        let prefix = crate::assets::synced_prefix(st, dir)?.unwrap_or_else(|| "/".to_string());
        let disk = Disk::read(dir, &prefix)?;
        rows.retain_mut(|l| match l.status.as_deref() {
            Some("not-in-store") => !disk.has(l),
            Some("ok" | "ambiguous") if l.asset => {
                let missing = l.resolved.as_ref().is_some_and(|r| !disk.paths.contains(&r.to_lowercase()));
                if missing {
                    l.status = Some("not-pulled".to_string());
                }
                missing || !broken
            }
            Some("ok" | "ambiguous") => !broken,
            _ => true,
        });
    }
    if json {
        return emit_json(&rows);
    }
    if rows.is_empty() {
        eprintln!("{} under {path}", if broken { "no broken links" } else { "no links" });
        return Ok(());
    }
    out(rows.iter().map(line_of).collect::<String>().as_bytes())
}

pub fn backlinks(st: &mut dyn Store, path: &str, json: bool) -> Result<()> {
    let rows = st.backlinks(path)?;
    if json {
        return emit_json(&rows);
    }
    if rows.is_empty() {
        eprintln!("no links to {path}");
        return Ok(());
    }
    out(rows.iter().map(line_of).collect::<String>().as_bytes())
}
