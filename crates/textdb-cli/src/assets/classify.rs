//! Which files in a vault are documents, assets, pointers or neither.
//!
//! Built-in defaults by extension come first; then the `textdb` attribute in `.gitattributes`
//! files, with git's rules: a file's patterns match relative to its directory, deeper files are
//! read after shallower ones, and the last matching line wins. A file nothing decides is an asset
//! when a rule marks it `binary`, or when it looks binary (a NUL byte in its first 8000 bytes, as
//! git checks); otherwise it is left alone. Matching ignores case everywhere, so teammates on
//! Windows, macOS and Linux classify a vault the same way.
//!
//! ```text
//! *.md        textdb=document
//! *.png       textdb=asset
//! *.log       textdb=ignore
//! drafts/*.svg !textdb        # back to the default
//! ```

use std::io::Read;
use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Serialize;

use super::pointer::{is_asset_pointer, SUFFIX};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Text textdb holds and versions.
    Document,
    /// A binary kept in an asset store, with a pointer.
    Asset,
    /// Never textdb's: its own files, version control, system clutter, or a rule says so.
    Ignore,
    /// An asset's pointer document (`*.tdbasset`).
    Pointer,
    /// Text no rule mentions (JSON, code, HTML): left to git.
    Other,
}

/// Extensions of documents by default.
pub const DOCUMENT_EXTS: &[&str] = &["md", "markdown", "mdx", "txt", "canvas", "base"];

/// Extensions of assets by default: images, PDF and office files, archives, audio, video, fonts,
/// diagrams.
pub const ASSET_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "tif", "tiff", "ico", "heic", "heif", "avif", "psd", "ai", "eps", "pdf", "doc", "docx",
    "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "pages", "numbers", "epub", "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz",
    "mp3", "wav", "m4a", "aac", "ogg", "opus", "flac", "mp4", "m4v", "mov", "avi", "mkv", "webm", "wmv", "woff", "woff2", "ttf", "otf", "eot",
    "drawio", "excalidraw", "vsdx", "sketch", "fig",
];

/// Directories never looked into: version control, textdb's own, trash folders, dependencies,
/// application settings and operating system folders.
pub const IGNORED_DIRS: &[&str] = &[
    ".git", ".textdb", ".trash", ".textdb-trash", "node_modules", ".obsidian", "__pycache__", "$recycle.bin", "system volume information",
    ".spotlight-v100", ".fseventsd", ".trashes",
];

/// A path segment as Windows resolves it: without an NTFS stream (`name:stream`) and the trailing
/// dots and spaces it drops, in lower case.
fn resolved(seg: &str) -> String {
    seg.split(':').next().unwrap_or(seg).trim_end_matches(['.', ' ']).to_lowercase()
}

/// The stem of the 8.3 short name Windows gives `name`: its base (before a last dot) without
/// leading dots, spaces and other dots, in upper case, six characters at most.
fn short_stem(name: &str) -> String {
    let trimmed = name.trim_start_matches('.');
    let base = match trimmed.rfind('.') {
        Some(i) if i > 0 => &trimmed[..i],
        _ => trimmed,
    };
    base.chars().filter(|c| *c != ' ' && *c != '.').flat_map(char::to_uppercase).take(6).collect()
}

/// Whether `seg` has the shape of an 8.3 short name: up to six characters, `~`, digits, and up to
/// three more after a dot (`NOTES~1.MD`).
pub fn looks_short(seg: &str) -> bool {
    let Some((stem, rest)) = seg.split_once('~') else { return false };
    let (num, ext) = rest.split_once('.').unwrap_or((rest, ""));
    (1..=6).contains(&stem.chars().count()) && !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) && ext.chars().count() <= 3 && !ext.contains('.')
}

/// Whether `seg` is an 8.3 short name Windows may have given one of `dirs`: `GIT~1` for `.git`,
/// `NODE_M~1` for `node_modules`, or the hashed form it uses when many names share a start
/// (`GI3F2A~1`: two letters of the stem and four hex digits). Other names that look like short
/// names (`photos~1`, `report~1.pdf`) are not.
fn short_name_of(seg: &str, dirs: &[&str]) -> bool {
    if !looks_short(seg) {
        return false;
    }
    let stem = seg.split_once('~').map_or(seg, |(stem, _)| stem).to_uppercase();
    dirs.iter().map(|d| short_stem(d)).any(|s| {
        let start: String = s.chars().take(2).collect();
        stem == s || (stem.chars().count() == 6 && start.chars().count() == 2 && stem.starts_with(&start) && stem.chars().skip(2).all(|c| c.is_ascii_hexdigit()))
    })
}

/// Whether the path segment `seg` names, or on Windows may name, one of `dirs`: in any letter
/// case, with trailing dots or spaces or a stream (`.git.`, `.git::$INDEX_ALLOCATION`), or as an
/// 8.3 short name it may have (`GIT~1`).
pub fn names_dir(seg: &str, dirs: &[&str]) -> bool {
    let name = resolved(seg);
    dirs.iter().any(|d| d.eq_ignore_ascii_case(&name)) || short_name_of(&name, dirs)
}

/// Whether a file named `name` is a `.gitattributes` file: in any letter case where the file system
/// ignores case (`.GitAttributes`), since git there reads it all the same.
pub fn is_rules_file(name: &str) -> bool {
    if cfg!(any(windows, target_os = "macos")) { name.eq_ignore_ascii_case(".gitattributes") } else { name == ".gitattributes" }
}

/// Files that are never assets: the rules files, a store's database and its copies, system
/// clutter, lock files, downloads and copies in progress.
fn ignored_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(lower.as_str(), ".gitattributes" | ".textdbignore" | ".ds_store" | "thumbs.db" | "ehthumbs.db" | "desktop.ini" | "icon\r")
        || lower.starts_with("kb.db")
        || name.starts_with("~$")
        || name.starts_with("._")
        || name.starts_with(".~lock.")
        || [".tdbpart", ".crdownload", ".part", ".partial", ".download", ".tmp"].iter().any(|s| lower.ends_with(s))
}

/// What one attribute on a `.gitattributes` line says.
#[derive(Clone, Copy, Debug)]
enum Attr {
    Set(Class),
    /// `!textdb`: back to the default.
    Unspecified,
    /// `binary`: an asset unless a `textdb` attribute or the default says otherwise.
    Binary,
    /// `text`: not binary after all.
    Text,
}

struct Rule {
    pattern: String,
    matcher: Gitignore,
    attrs: Vec<Attr>,
}

/// The rules of one `.gitattributes` file.
struct Layer {
    /// Its directory relative to the vault, `""` at the top.
    dir: String,
    rules: Vec<Rule>,
}

pub struct Classifier {
    layers: Vec<Layer>,
    /// Lines that could not be used, with where they are.
    pub warnings: Vec<String>,
}

fn default_class(name_or_pattern: &str) -> Class {
    let name = name_or_pattern.rsplit('/').next().unwrap_or(name_or_pattern);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => {
            let ext = ext.to_ascii_lowercase();
            if DOCUMENT_EXTS.contains(&ext.as_str()) {
                Class::Document
            } else if ASSET_EXTS.contains(&ext.as_str()) {
                Class::Asset
            } else {
                Class::Other
            }
        }
        _ => Class::Other,
    }
}

/// The pattern and the rest of a line; a pattern may be quoted, with `\"` and `\\` inside.
fn pattern_of(line: &str) -> Option<(String, &str)> {
    if let Some(rest) = line.strip_prefix('"') {
        let mut pattern = String::new();
        let mut chars = rest.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => pattern.push(chars.next()?.1),
                '"' => return Some((pattern, &rest[i + 1..])),
                c => pattern.push(c),
            }
        }
        None
    } else {
        let end = line.find(char::is_whitespace).unwrap_or(line.len());
        Some((line[..end].to_string(), &line[end..]))
    }
}

/// `png` as `[pP][nN][gG]`, so git ignores it in any case on every system.
fn any_case(ext: &str) -> String {
    ext.chars()
        .map(|c| if c.is_ascii_alphabetic() { format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase()) } else { c.to_string() })
        .collect()
}

/// `rel` below the directory `dir` (ignoring case), relative to it.
fn below<'a>(rel: &'a str, dir: &str) -> Option<&'a str> {
    if dir.is_empty() {
        return Some(rel);
    }
    let n = dir.len();
    (rel.len() > n && rel.as_bytes()[..n].eq_ignore_ascii_case(dir.as_bytes()) && rel.as_bytes()[n] == b'/').then(|| &rel[n + 1..])
}

impl Classifier {
    /// No `.gitattributes`: the defaults only.
    pub fn defaults() -> Classifier {
        Classifier { layers: Vec::new(), warnings: Vec::new() }
    }

    /// The rules of `root/.gitattributes` and of the other `.gitattributes` files named in `nested`
    /// (paths relative to `root`).
    pub fn load(root: &Path, nested: &[String]) -> Classifier {
        let mut c = Classifier::defaults();
        let mut files: Vec<String> = std::iter::once(".gitattributes".to_string()).chain(nested.iter().cloned()).collect();
        files.sort_by_key(|f| (f.matches('/').count(), f.clone()));
        files.dedup();
        for rel in files {
            if let Ok(text) = std::fs::read_to_string(root.join(&rel)) {
                let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
                c.add(dir, &rel, &text);
            }
        }
        c
    }

    /// Add the rules of a `.gitattributes` file in the directory `dir`, read from `source`.
    pub fn add(&mut self, dir: &str, source: &str, text: &str) {
        let mut rules = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("[attr]") {
                continue;
            }
            let at = || format!("{source}:{}", n + 1);
            let Some((pattern, rest)) = pattern_of(line) else {
                self.warnings.push(format!("{}: unterminated quoted pattern", at()));
                continue;
            };
            let mut attrs = Vec::new();
            for a in rest.split_whitespace() {
                if a.starts_with('#') {
                    break;
                }
                attrs.push(match a {
                    "textdb=document" | "textdb" => Attr::Set(Class::Document),
                    "textdb=asset" => Attr::Set(Class::Asset),
                    "textdb=ignore" | "-textdb" => Attr::Set(Class::Ignore),
                    "!textdb" => Attr::Unspecified,
                    "binary" => Attr::Binary,
                    "text" => Attr::Text,
                    other if other.starts_with("textdb=") => {
                        self.warnings.push(format!("{}: {other} is not document, asset or ignore", at()));
                        continue;
                    }
                    _ => continue,
                });
            }
            if attrs.is_empty() {
                continue;
            }
            if pattern.starts_with('!') {
                self.warnings.push(format!("{}: negative patterns are not allowed in .gitattributes", at()));
                continue;
            }
            if pattern.ends_with('/') {
                self.warnings.push(format!("{}: a directory pattern ({pattern}) matches no files in .gitattributes", at()));
                continue;
            }
            let mut b = GitignoreBuilder::new("");
            b.case_insensitive(true).ok();
            let built = b.add_line(None, &pattern).ok().and_then(|b| b.build().ok());
            match built {
                Some(matcher) => rules.push(Rule { pattern, matcher, attrs }),
                None => self.warnings.push(format!("{}: `{pattern}` is not a valid pattern", at())),
            }
        }
        if !rules.is_empty() {
            self.layers.push(Layer { dir: dir.to_string(), rules });
        }
    }

    /// The class rules and defaults give `rel` (a `/`-separated path relative to the vault),
    /// without looking at its content: `Other` when nothing decides.
    pub fn rule_class(&self, rel: &str) -> Class {
        let mut segs: Vec<&str> = rel.split('/').collect();
        let name = segs.pop().unwrap_or("");
        if segs.iter().chain(std::iter::once(&name)).any(|s| names_dir(s, IGNORED_DIRS)) || ignored_name(name) {
            return Class::Ignore;
        }
        if is_asset_pointer(name) {
            return Class::Pointer;
        }
        let default = default_class(name);
        let (mut set, mut binary) = (None, false);
        for layer in &self.layers {
            let Some(sub) = below(rel, &layer.dir) else { continue };
            for rule in &layer.rules {
                if !rule.matcher.matched(sub, false).is_ignore() {
                    continue;
                }
                for attr in &rule.attrs {
                    match attr {
                        Attr::Set(c) => set = Some(*c),
                        Attr::Unspecified => set = None,
                        Attr::Binary => binary = true,
                        Attr::Text => binary = false,
                    }
                }
            }
        }
        let class = set.unwrap_or(if default == Class::Other && binary { Class::Asset } else { default });
        // Git's own files are never assets, whatever a rule says.
        if class == Class::Asset && name.starts_with(".git") {
            return Class::Other;
        }
        class
    }

    /// The class of the file `rel` under `root`: [`rule_class`](Self::rule_class), and for what
    /// nothing decides, an asset when the file looks binary.
    pub fn classify(&self, root: &Path, rel: &str) -> Class {
        match self.rule_class(rel) {
            Class::Other if !rel.rsplit('/').next().unwrap_or(rel).starts_with(".git") && looks_binary(&root.join(rel)) => Class::Asset,
            c => c,
        }
    }

    /// `.gitignore` lines, in order, for what the defaults and the rules make assets: each rule
    /// that makes something an asset adds its pattern (and keeps the directories it matches
    /// visible, so pointers inside them stay in git), each that makes something else a document,
    /// ignored or back to a non-asset default adds a negation, and pointers are never ignored.
    pub fn gitignore_patterns(&self) -> Vec<String> {
        // What pull moved aside in the vault.
        let mut out = Vec::new();
        // Each asset pattern is followed by its negation as a directory: rules match files only,
        // and git never looks inside an ignored directory for the documents and pointers there.
        out.extend(ASSET_EXTS.iter().flat_map(|e| [format!("*.{}", any_case(e)), format!("!*.{}/", any_case(e))]));
        for layer in &self.layers {
            for rule in &layer.rules {
                let (mut set, mut decided, mut binary) = (None, false, false);
                for attr in &rule.attrs {
                    match attr {
                        Attr::Set(c) => (set, decided) = (Some(*c), true),
                        Attr::Unspecified => (set, decided) = (None, true),
                        Attr::Binary => binary = true,
                        Attr::Text => binary = false,
                    }
                }
                let default = default_class(&rule.pattern);
                let asset = match (decided, set) {
                    (true, Some(c)) => c == Class::Asset,
                    (true, None) => default == Class::Asset,
                    (false, _) if binary => default == Class::Other,
                    (false, _) => continue,
                };
                let pattern = if layer.dir.is_empty() {
                    rule.pattern.clone()
                } else if rule.pattern.trim_start_matches('/').contains('/') {
                    format!("/{}/{}", layer.dir, rule.pattern.trim_start_matches('/'))
                } else {
                    format!("/{}/**/{}", layer.dir, rule.pattern)
                };
                if asset {
                    out.push(pattern.clone());
                    out.push(format!("!{pattern}/"));
                } else {
                    out.push(format!("!{pattern}"));
                }
            }
        }
        // After the rules, so none of their directory negations brings them back: what pull moved
        // aside in the vault is ignored, git's own files and pointers never are.
        out.push("/.textdb/trash/".to_string());
        out.push("/.textdb/base/".to_string());
        out.push("!.git*".to_string());
        out.push(format!("!*{SUFFIX}"));
        out
    }
}

/// A NUL byte in the first 8000 bytes, as git decides a file is binary.
pub fn looks_binary(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else { return false };
    let mut head = Vec::with_capacity(8000);
    file.take(8000).read_to_end(&mut head).is_ok() && head.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_rules_and_nested_files() {
        let mut c = Classifier::defaults();
        assert_eq!(c.rule_class("notes/a.md"), Class::Document);
        assert_eq!(c.rule_class("img/Arch.PNG"), Class::Asset);
        assert_eq!(c.rule_class("img/arch.png.tdbasset"), Class::Pointer);
        assert_eq!(c.rule_class("data/x.json"), Class::Other);
        for junk in [".git/objects/ab", "docs/~$report.docx", "docs/.DS_Store", "img/._photo.jpg", "kb.db.bak", "dl/big.zip.crdownload", "~WRL0001.tmp", ".obsidian/plugins/p/main.wasm", "$RECYCLE.BIN/x.png"] {
            assert_eq!(c.rule_class(junk), Class::Ignore, "{junk}");
        }
        // Other names Windows gives those folders: 8.3 short names, trailing dots and spaces, streams.
        for alias in ["GIT~1/hooks/post-checkout", ".git./hooks/x", ".GIT /config", ".git::$INDEX_ALLOCATION/hooks/x", "NODE_M~1/x.png", "sub/.git"] {
            assert_eq!(c.rule_class(alias), Class::Ignore, "{alias}");
        }
        for alias in ["GIT~1", "git~1.", "NODE_M~1", "$RECYC~1.BIN", "TEXTDB~2", "GI3F2A~1", "SYSTEM~1"] {
            assert!(names_dir(alias, IGNORED_DIRS), "{alias}");
        }
        for name in ["report~1.pdf", "photos~1", "a~b", "GIT~", "GITHUB~1", "GIZZZZ~1"] {
            assert!(!names_dir(name, IGNORED_DIRS), "{name}");
        }
        assert_eq!(c.rule_class("scans/report~1.pdf"), Class::Asset, "a file may be named like a short name");
        assert_eq!(c.rule_class("photos~1/p.png"), Class::Asset, "and a folder too");
        assert_eq!(c.rule_class(".png"), Class::Other);
        assert_eq!(c.rule_class("certs/server.key"), Class::Other);

        c.add("", ".gitattributes", "# comment\n* -text\n*.log textdb=ignore\n*.svg textdb=document\n/exports/** textdb=asset\n*.dat binary\n*.dat text\n*.bin binary\n\"with space/*.txt\" textdb=asset\n!x textdb\nweird textdb=maybe\n.gitkeep textdb=asset\n");
        c.add("Sub", "Sub/.gitattributes", "*.svg !textdb\n*.MD -textdb\n");
        assert_eq!(c.rule_class("a/run.log"), Class::Ignore);
        assert_eq!(c.rule_class("a/icon.svg"), Class::Document);
        assert_eq!(c.rule_class("sub/deep/icon.SVG"), Class::Asset, "rules and their directories ignore case");
        assert_eq!(c.rule_class("sub/x.md"), Class::Ignore);
        assert_eq!(c.rule_class("exports/report.csv"), Class::Asset);
        assert_eq!(c.rule_class("a/exports/report.csv"), Class::Other);
        assert_eq!(c.rule_class("a/blob.dat"), Class::Other, "a later `text` undoes `binary`");
        assert_eq!(c.rule_class("a/blob.bin"), Class::Asset);
        assert_eq!(c.rule_class("a/data.json"), Class::Other, "`-text` is about line ends, not binaries");
        assert_eq!(c.rule_class("with space/a.txt"), Class::Asset);
        assert_eq!(c.rule_class(".gitkeep"), Class::Other);
        assert_eq!(c.warnings.len(), 2, "{:?}", c.warnings);

        let p = c.gitignore_patterns();
        let at = |s: &str| p.iter().position(|x| x == s).unwrap_or_else(|| panic!("{s} not in {p:?}"));
        assert!(at("*.[pP][nN][gG]") < at("!*.svg") && at("!*.svg") < at("/Sub/**/*.svg"), "{p:?}");
        assert!(at("/exports/**") < at("!/exports/**/") && at("*.bin") < at("!*.bin/"));
        assert_eq!(at("!*.[pP][nN][gG]/"), at("*.[pP][nN][gG]") + 1);
        assert!(at("!*.log") > 0 && at("*.bin") > 0 && at("!/Sub/**/*.MD") > 0);
        assert_eq!(p.last().map(String::as_str), Some("!*.tdbasset"));
    }

    #[test]
    fn files_nothing_decides_are_sniffed() {
        let dir = std::env::temp_dir().join(format!("textdb-classify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("blob"), b"abc\0def").unwrap();
        std::fs::write(dir.join("notes.json"), b"{}").unwrap();
        std::fs::write(dir.join(".gitattributes"), "*.json textdb=document\n").unwrap();
        std::fs::write(dir.join(".gitmodules"), b"\0").unwrap();
        let c = Classifier::load(&dir, &[]);
        assert_eq!(c.classify(&dir, "blob"), Class::Asset);
        assert_eq!(c.classify(&dir, "notes.json"), Class::Document);
        assert_eq!(c.classify(&dir, ".gitmodules"), Class::Other);
        assert_eq!(Classifier::defaults().classify(&dir, "notes.json"), Class::Other);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
