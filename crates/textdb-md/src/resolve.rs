//! Link resolution by Obsidian's rules, shared by every store so they cannot drift apart.
//!
//! A store answers a handful of lookups ([`Lookup`]); [`resolve`] decides what a link written in
//! a file points to and its status, and [`rewritten_target`] how to write a link's target again
//! after the file it points to moved. Statuses: `ok`, `ambiguous` (several files match; the
//! nearest is taken), `anchor-missing` (the file has no such heading), `folder`, `broken`,
//! `not-in-store` (a PDF, image or other file a text store does not hold) and `external` (URLs,
//! email addresses, queries, numbered references).
//!
//! An asset (a binary kept in an asset store, see `docs/assets.md`) is held as a pointer document
//! next to where the file belongs: `deck.pdf` as `deck.pdf.tdbasset`. A link that finds no
//! document resolves to the pointer of that name, as `ok`.

use textdb_core::TextdbError;

/// The suffix of an asset's pointer document: the pointer of `deck.pdf` is `deck.pdf.tdbasset`.
pub const ASSET_POINTER_SUFFIX: &str = ".tdbasset";

/// Whether `path` names an asset pointer.
pub fn is_asset_pointer(path: &str) -> bool {
    path.len() > ASSET_POINTER_SUFFIX.len() && path.is_char_boundary(path.len() - ASSET_POINTER_SUFFIX.len()) && path[path.len() - ASSET_POINTER_SUFFIX.len()..].eq_ignore_ascii_case(ASSET_POINTER_SUFFIX)
}

/// The file a pointer stands for (`/a/deck.pdf.tdbasset` → `/a/deck.pdf`); any other path as it is.
pub fn asset_path(path: &str) -> &str {
    if is_asset_pointer(path) {
        &path[..path.len() - ASSET_POINTER_SUFFIX.len()]
    } else {
        path
    }
}

/// The store setting deciding what a move does to links pointing at what moved.
pub const LINK_UPDATES_SETTING: &str = "link_updates";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkUpdates {
    /// Nothing.
    Off,
    /// List the links the move leaves pointing elsewhere.
    Report,
    /// Rewrite them, one commit per linking file, in the same transaction as the move.
    Rewrite,
}

impl LinkUpdates {
    pub const DEFAULT: LinkUpdates = LinkUpdates::Report;

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "report" => Some(Self::Report),
            "rewrite" => Some(Self::Rewrite),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Report => "report",
            Self::Rewrite => "rewrite",
        }
    }
}

/// What a store answers about its live nodes. Paths are store paths (`/a/b.md`).
pub trait Lookup {
    /// Live files whose path equals `path`, ignoring ASCII case: `(id, path)`.
    fn files_by_path(&self, path: &str) -> Result<Vec<(i64, String)>, TextdbError>;
    /// Live files whose name equals `name`, ignoring ASCII case.
    fn files_by_name(&self, name: &str) -> Result<Vec<(i64, String)>, TextdbError>;
    /// Live files whose lower-case path ends with `suffix` (lower case, starting with `/`).
    fn files_by_suffix(&self, suffix: &str) -> Result<Vec<(i64, String)>, TextdbError>;
    /// Whether a live folder has this path, ignoring ASCII case.
    fn is_folder(&self, path: &str) -> Result<bool, TextdbError>;
    /// The heading paths (`Title / Section`) of a file.
    fn headings(&self, file_id: i64) -> Result<Vec<String>, TextdbError>;
}

/// Extensions of files a store of text documents does not hold.
const NOT_TEXT: &[&str] = &[
    "pdf", "png", "jpg", "jpeg", "gif", "svg", "webp", "bmp", "heic", "tif", "tiff", "ico", "xlsx", "xls", "docx", "doc", "pptx", "ppt",
    "odt", "ods", "csv", "tsv", "zip", "mp4", "mov", "mp3", "wav", "m4a", "canvas", "base", "json", "html", "htm", "drawio", "excalidraw",
];

/// The folder a store path is in: `/a/b.md` → `/a`, `/b.md` → `/`.
pub fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

/// A file name as links find it: lower case, without `.md`; a pointer as the name of its asset.
pub fn name_key(path_or_name: &str) -> String {
    let last = asset_path(path_or_name.trim_end_matches('/')).rsplit('/').next().unwrap_or("").to_lowercase();
    last.strip_suffix(".md").map(str::to_string).unwrap_or(last)
}

/// `rel` against the folder `base` (`/a/b`), resolving `.` and `..`; `None` above the root.
pub fn join(base: &str, rel: &str) -> Option<String> {
    let mut segs: Vec<&str> = if rel.starts_with('/') { Vec::new() } else { base.split('/').filter(|s| !s.is_empty()).collect() };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop()?;
            }
            s => segs.push(s),
        }
    }
    Some(format!("/{}", segs.join("/")))
}

/// `target` (`/a/b/c.md`) relative to the folder `folder` (`/a/x`): `../b/c.md`.
pub fn relative(folder: &str, target: &str) -> String {
    let a: Vec<&str> = folder.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count().min(b.len().saturating_sub(1));
    let mut parts: Vec<&str> = vec![".."; a.len() - common];
    parts.extend(&b[common..]);
    parts.join("/")
}

fn variants(path: &str) -> Vec<String> {
    if path.to_ascii_lowercase().ends_with(".md") {
        vec![path.to_string()]
    } else {
        vec![path.to_string(), format!("{path}.md")]
    }
}

/// Lower-case letters and digits only, so `Next steps` and `next-steps` compare equal.
pub fn heading_key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Whether the file has the heading `anchor` names (`#A#B` checks `B`); block ids (`^id`) are
/// not checked.
pub fn anchor_exists(l: &impl Lookup, file_id: i64, anchor: &str) -> Result<bool, TextdbError> {
    let last = anchor.rsplit('#').next().unwrap_or(anchor);
    if last.starts_with('^') {
        return Ok(true);
    }
    let want = heading_key(last);
    Ok(l.headings(file_id)?.iter().any(|h| heading_key(h.rsplit(" / ").next().unwrap_or(h)) == want))
}

/// The file a link written in `source` (id and path) points to, and its status. `kind` is the
/// extractor's (`wiki`, `embed`, `md`, `image`); `target` is without anchor or alias.
pub fn resolve(
    l: &impl Lookup,
    source_id: i64,
    source: &str,
    kind: &str,
    target: &str,
    anchor: Option<&str>,
    external: bool,
) -> Result<(Option<i64>, &'static str), TextdbError> {
    if external {
        return Ok((None, "external"));
    }
    if target.is_empty() {
        return Ok(match anchor {
            None => (None, "broken"),
            Some(a) if anchor_exists(l, source_id, a)? => (Some(source_id), "ok"),
            Some(_) => (Some(source_id), "anchor-missing"),
        });
    }
    let folder = parent(source);
    let mut found = candidates(l, kind, folder, target, variants)?;
    // No document: the pointer of an asset by that name.
    let asset = found.is_empty() && !target.to_ascii_lowercase().ends_with(".md") && {
        found = candidates(l, kind, folder, target, |p| vec![format!("{p}{ASSET_POINTER_SUFFIX}")])?;
        !found.is_empty()
    };
    if found.is_empty() {
        // A link to a folder (`[[accounts/acme/projects/]]`) is not a broken link to a file.
        for f in [join(folder, target), join("/", target)].iter().flatten() {
            if f != "/" && l.is_folder(f)? {
                return Ok((None, "folder"));
            }
        }
        let ext = target.rsplit('/').next().and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase());
        let status = if ext.is_some_and(|e| NOT_TEXT.contains(&e.as_str())) { "not-in-store" } else { "broken" };
        return Ok((None, status));
    }
    let several = found.len() > 1;
    // The exact spelling first, then the linking note's folder, then the shortest path.
    found.sort_by_key(|(_, p)| {
        let p = asset_path(p);
        let exact = p.ends_with(target) || p.ends_with(&format!("{target}.md"));
        (!exact, parent(p) != folder, p.len(), p.to_string())
    });
    let id = found[0].0;
    // An asset's anchor (`#page=3`) is not a heading.
    if let (Some(a), false) = (anchor, asset) {
        if !anchor_exists(l, id, a)? {
            return Ok((Some(id), "anchor-missing"));
        }
    }
    Ok((Some(id), if several { "ambiguous" } else { "ok" }))
}

/// The live files a link of `kind` written in `folder` to `target` could mean, each spelling of a
/// path given by `variants`.
fn candidates(l: &impl Lookup, kind: &str, folder: &str, target: &str, variants: impl Fn(&str) -> Vec<String>) -> Result<Vec<(i64, String)>, TextdbError> {
    let mut found: Vec<(i64, String)> = Vec::new();
    let by_path = |found: &mut Vec<(i64, String)>, p: Option<String>| -> Result<(), TextdbError> {
        for v in p.map(|p| variants(&p)).unwrap_or_default() {
            found.extend(l.files_by_path(&v)?);
        }
        Ok(())
    };
    if kind == "md" || kind == "image" {
        by_path(&mut found, join(folder, target))?;
        if found.is_empty() && !target.starts_with('/') {
            by_path(&mut found, join("/", target))?;
        }
    } else if target.contains('/') {
        by_path(&mut found, join("/", target))?;
        if found.is_empty() {
            by_path(&mut found, join(folder, target))?;
        }
        if found.is_empty() {
            for v in variants(target.trim_start_matches('/')) {
                found.extend(l.files_by_suffix(&format!("/{}", v.to_lowercase()))?);
            }
        }
    } else {
        for v in variants(target) {
            found.extend(l.files_by_name(&v)?);
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// How to write a link's target again, in the style `raw` was written, for a link in `source` to
/// the file now at `target`: bare names while they stay unique, vault paths, relative markdown
/// paths, `.md` kept or left out as before, `%20` in markdown links unless in `<…>`.
pub fn rewritten_target(l: &impl Lookup, kind: &str, raw: &str, angle: bool, source: &str, target: &str) -> Result<String, TextdbError> {
    // A link to an asset names the file, not its pointer.
    let target = asset_path(target);
    let keep_md = raw.to_ascii_lowercase().ends_with(".md");
    let strip = |p: &str| if keep_md { p.to_string() } else { p.strip_suffix(".md").unwrap_or(p).to_string() };
    let folder = parent(source);
    Ok(match kind {
        "wiki" | "embed" => {
            if !raw.contains('/') {
                let name = target.rsplit('/').next().unwrap_or(target);
                if l.files_by_name(name)?.len() + l.files_by_name(&format!("{name}{ASSET_POINTER_SUFFIX}"))?.len() <= 1 {
                    strip(name)
                } else {
                    strip(&target[1..])
                }
            } else if raw.starts_with("./") || raw.starts_with("../") {
                strip(&relative(folder, target))
            } else {
                strip(&target[1..])
            }
        }
        _ => {
            let path = if raw.starts_with('/') {
                target.to_string()
            } else {
                let rel = relative(folder, target);
                if raw.starts_with("./") && !rel.starts_with("../") { format!("./{rel}") } else { rel }
            };
            let path = strip(&path);
            if angle {
                path
            } else {
                path.replace('%', "%25").replace(' ', "%20").replace('(', "%28").replace(')', "%29")
            }
        }
    })
}

/// The 1-based line of each byte offset in `doc`, for pairing scanned spans with stored rows.
pub fn line_of_offset(doc: &[u8]) -> impl Fn(usize) -> i64 {
    let starts: Vec<usize> = std::iter::once(0).chain(doc.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i + 1)).collect();
    move |offset| match starts.binary_search(&offset) {
        Ok(i) => i as i64 + 1,
        Err(i) => i as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Files(Vec<(i64, &'static str)>, Vec<(i64, &'static str)>);

    impl Lookup for Files {
        fn files_by_path(&self, path: &str) -> Result<Vec<(i64, String)>, TextdbError> {
            Ok(self.0.iter().filter(|(_, p)| p.eq_ignore_ascii_case(path)).map(|(i, p)| (*i, p.to_string())).collect())
        }
        fn files_by_name(&self, name: &str) -> Result<Vec<(i64, String)>, TextdbError> {
            Ok(self.0.iter().filter(|(_, p)| p.rsplit('/').next().unwrap().eq_ignore_ascii_case(name)).map(|(i, p)| (*i, p.to_string())).collect())
        }
        fn files_by_suffix(&self, suffix: &str) -> Result<Vec<(i64, String)>, TextdbError> {
            Ok(self.0.iter().filter(|(_, p)| p.to_lowercase().ends_with(suffix)).map(|(i, p)| (*i, p.to_string())).collect())
        }
        fn is_folder(&self, path: &str) -> Result<bool, TextdbError> {
            Ok(self.0.iter().any(|(_, p)| p.to_lowercase().starts_with(&format!("{}/", path.to_lowercase()))))
        }
        fn headings(&self, file_id: i64) -> Result<Vec<String>, TextdbError> {
            Ok(self.1.iter().filter(|(i, _)| *i == file_id).map(|(_, h)| h.to_string()).collect())
        }
    }

    #[test]
    fn obsidian_rules() {
        let files = Files(vec![(1, "/notes/Plan.md"), (2, "/acc/acme.md"), (3, "/other/Plan.md"), (4, "/solo/Deck.md")], vec![(1, "Plan / Next steps")]);
        let r = |kind, target, anchor| resolve(&files, 2, "/acc/acme.md", kind, target, anchor, false).unwrap();
        assert_eq!(r("wiki", "Deck", None), (Some(4), "ok"));
        assert_eq!(r("wiki", "Plan", None), (Some(1), "ambiguous"));
        assert_eq!(r("wiki", "notes/Plan", Some("next-steps")), (Some(1), "ok"));
        assert_eq!(r("md", "../notes/Plan.md", Some("Nope")), (Some(1), "anchor-missing"));
        assert_eq!(r("wiki", "Missing", None), (None, "broken"));
        assert_eq!(r("embed", "deck.pdf", None), (None, "not-in-store"));
        assert_eq!(r("wiki", "notes/", None), (None, "folder"));
        assert_eq!(resolve(&files, 2, "/acc/acme.md", "md", "https://x", None, true).unwrap(), (None, "external"));
        assert_eq!(rewritten_target(&files, "wiki", "Deck", false, "/acc/acme.md", "/solo/Deck.md").unwrap(), "Deck");
        assert_eq!(rewritten_target(&files, "wiki", "Plan", false, "/acc/acme.md", "/notes/Plan.md").unwrap(), "notes/Plan");
        assert_eq!(rewritten_target(&files, "md", "../x.md", false, "/acc/acme.md", "/a b/Plan.md").unwrap(), "../a%20b/Plan.md");
    }

    #[test]
    fn links_to_assets_resolve_to_their_pointers() {
        let files = Files(vec![(1, "/acc/acme.md"), (2, "/acc/deck.pdf.tdbasset"), (3, "/img/arch.png.tdbasset"), (4, "/other/arch.png.tdbasset")], vec![]);
        let r = |kind, target, anchor| resolve(&files, 1, "/acc/acme.md", kind, target, anchor, false).unwrap();
        assert_eq!(r("embed", "deck.pdf", None), (Some(2), "ok"));
        assert_eq!(r("wiki", "deck.pdf", Some("page=3")), (Some(2), "ok"));
        assert_eq!(r("md", "deck.pdf", None), (Some(2), "ok"));
        assert_eq!(r("image", "../img/arch.png", None), (Some(3), "ok"));
        assert_eq!(r("embed", "arch.png", None), (Some(3), "ambiguous"));
        assert_eq!(r("embed", "img/arch.png", None), (Some(3), "ok"));
        assert_eq!(r("embed", "missing.png", None), (None, "not-in-store"));
        assert_eq!(r("wiki", "deck.pdf.md", None), (None, "broken"));
        assert_eq!(name_key("/acc/Deck.PDF.tdbasset"), "deck.pdf");
        assert_eq!(asset_path("/acc/deck.pdf.TDBASSET"), "/acc/deck.pdf");
        assert_eq!(asset_path("/acc/deck.pdf"), "/acc/deck.pdf");
        assert_eq!(rewritten_target(&files, "embed", "deck.pdf", false, "/acc/acme.md", "/arch/deck.pdf.tdbasset").unwrap(), "deck.pdf");
        assert_eq!(rewritten_target(&files, "image", "deck.pdf", false, "/acc/acme.md", "/arch/deck.pdf.tdbasset").unwrap(), "../arch/deck.pdf");
        assert_eq!(rewritten_target(&files, "embed", "arch.png", false, "/acc/acme.md", "/img/arch.png.tdbasset").unwrap(), "img/arch.png");
    }

    #[test]
    fn paths_join_and_names_key() {
        assert_eq!(join("/a/b", "../c/d.md").as_deref(), Some("/a/c/d.md"));
        assert_eq!(join("/a", "./x/./y").as_deref(), Some("/a/x/y"));
        assert_eq!(join("/a", "/z.md").as_deref(), Some("/z.md"));
        assert_eq!(join("/a", "../../x"), None);
        assert_eq!(name_key("Notes/Plan.md"), "plan");
        assert_eq!(heading_key("Next steps"), heading_key("next-steps"));
        assert_eq!(relative("/acc", "/archive/2026/Plan.md"), "../archive/2026/Plan.md");
        assert_eq!(relative("/notes", "/notes/Plan.md"), "Plan.md");
        assert_eq!(relative("/", "/Plan.md"), "Plan.md");
        assert_eq!(parent("/a/b.md"), "/a");
        assert_eq!(parent("/b.md"), "/");
        let line = line_of_offset(b"a\nb\nc");
        assert_eq!((line(0), line(2), line(4)), (1, 2, 3));
    }
}
