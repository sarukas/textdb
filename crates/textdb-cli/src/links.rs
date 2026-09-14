//! `textdb links` and `textdb backlinks`.

use std::collections::HashSet;
use std::path::Path;

use textdb_sqlite::db::parent_of;
use textdb_sqlite::links::join;

use crate::store::{LinkRow, Store};
use crate::{emit_json, out, Result};

/// Statuses `links --broken` lists.
pub const BROKEN: &[&str] = &["broken", "anchor-missing", "not-in-store"];

/// How a link is written, from its parts.
pub fn written(l: &LinkRow) -> String {
    let anchor = l.anchor.as_ref().map(|a| format!("#{a}")).unwrap_or_default();
    match l.kind.as_str() {
        "wiki" => format!("[[{}{anchor}]]", l.target),
        "embed" => format!("![[{}{anchor}]]", l.target),
        "image" => format!("![]({}{anchor})", l.target),
        _ => format!("[]({}{anchor})", l.target),
    }
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
    fn read(dir: &Path) -> Result<Disk> {
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
                    disk.paths.insert(format!("/{}", rel.to_string_lossy().replace('\\', "/")).to_lowercase());
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
    let mut rows = st.links(path, if broken { BROKEN } else { &[] })?;
    if let Some(dir) = dir {
        let disk = Disk::read(dir)?;
        rows.retain(|l| l.status.as_deref() != Some("not-in-store") || !disk.has(l));
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
