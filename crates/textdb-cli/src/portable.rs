//! Whether the paths an export writes can exist side by side on a file system. textdb paths
//! are case-sensitive and may use any character but `/` and NUL; Windows and macOS compare
//! names without regard to letter case, macOS also without regard to Unicode normalization,
//! and Windows refuses some names outright. The web app applies the same rules
//! (`node/apps/web/src/export/names.ts`).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Windows,
    Macos,
    Linux,
}

pub const ALL: [Platform; 3] = [Platform::Windows, Platform::Macos, Platform::Linux];

/// Windows' classic path limit; a relative path this long cannot fit wherever it goes.
pub const LONG_PATH: usize = 260;

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::Macos
        } else {
            Platform::Linux
        }
    }

    /// Whether names that differ only in letter case are one file, as by default on Windows and macOS.
    pub fn case_insensitive(self) -> bool {
        self != Platform::Linux
    }

    pub fn name(self) -> &'static str {
        match self {
            Platform::Windows => "Windows",
            Platform::Macos => "macOS",
            Platform::Linux => "Linux",
        }
    }
}

/// A name as the file system on `p` compares it by default.
pub fn fold(name: &str, p: Platform) -> String {
    match p {
        Platform::Linux => name.to_string(),
        Platform::Windows => name.to_lowercase(),
        Platform::Macos => name.nfc().collect::<String>().to_lowercase(),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Problem {
    /// Relative to the exported folder; a folder ends with `/`.
    pub path: String,
    /// `case`, `unicode`, `reserved`, `character`, `trailing`, `long`, `disk-case` or `disk-kind`.
    pub kind: &'static str,
    pub detail: String,
    /// Where it is a problem.
    pub platforms: Vec<Platform>,
    /// A problem on this computer: nothing is written.
    pub blocking: bool,
}

/// Problems found so far, one per path and kind.
pub struct Problems {
    here: Platform,
    items: BTreeMap<(String, &'static str), Problem>,
}

impl Problems {
    pub fn new(here: Platform) -> Self {
        Problems { here, items: BTreeMap::new() }
    }

    pub fn add(&mut self, path: impl Into<String>, kind: &'static str, detail: String, platforms: &[Platform]) {
        let path = path.into();
        let blocking = platforms.contains(&self.here);
        self.items.entry((path.clone(), kind)).or_insert(Problem {
            path,
            kind,
            detail,
            platforms: platforms.to_vec(),
            blocking,
        });
    }

    /// Blocking problems first, then by path.
    pub fn into_vec(self) -> Vec<Problem> {
        let mut v: Vec<Problem> = self.items.into_values().collect();
        v.sort_by(|a, b| b.blocking.cmp(&a.blocking).then_with(|| a.path.cmp(&b.path)));
        v
    }
}

fn is_reserved(base: &str) -> bool {
    let b = base.to_ascii_uppercase();
    matches!(b.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((b.starts_with("COM") || b.starts_with("LPT")) && b.len() == 4 && matches!(b.as_bytes()[3], b'1'..=b'9'))
}

fn groups<'a>(paths: &'a BTreeSet<String>, key: impl Fn(&str) -> String) -> Vec<Vec<&'a str>> {
    let mut by_key: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for p in paths {
        by_key.entry(key(p.trim_end_matches('/'))).or_default().push(p);
    }
    by_key.into_values().filter(|g| g.len() > 1).collect()
}

fn quoted(paths: &[&str], except: &str) -> String {
    paths
        .iter()
        .filter(|q| **q != except)
        .map(|q| format!("“{q}”"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Record every problem with writing the relative file paths `rels`, on each platform.
pub fn check_names(rels: &[String], problems: &mut Problems) {
    let mut all = BTreeSet::new();
    for rel in rels {
        let segs: Vec<&str> = rel.split('/').collect();
        for (i, seg) in segs.iter().enumerate() {
            let shown = if i + 1 < segs.len() { format!("{}/", segs[..=i].join("/")) } else { rel.clone() };
            all.insert(shown.clone());
            if is_reserved(seg.split('.').next().unwrap_or(seg)) {
                problems.add(shown.clone(), "reserved", format!("“{seg}” is a reserved device name on Windows"), &[Platform::Windows]);
            }
            if seg.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\') || (c as u32) < 0x20) {
                problems.add(
                    shown.clone(),
                    "character",
                    format!("“{seg}” contains a character Windows does not allow (< > : \" | ? * \\ or a control character)"),
                    &[Platform::Windows],
                );
            }
            if seg.ends_with('.') || seg.ends_with(' ') {
                problems.add(shown, "trailing", format!("“{seg}” ends with a dot or a space, which Windows removes"), &[Platform::Windows]);
            }
        }
        let len = rel.encode_utf16().count();
        if len >= LONG_PATH {
            problems.add(
                rel.clone(),
                "long",
                format!("{len} characters, beyond Windows' {LONG_PATH}-character path limit"),
                &[Platform::Windows],
            );
        }
    }
    for g in groups(&all, |p| p.to_lowercase()) {
        for p in &g {
            problems.add(*p, "case", format!("differs only in letter case from {}", quoted(&g, p)), &[Platform::Windows, Platform::Macos]);
        }
    }
    for g in groups(&all, |p| p.nfc().collect::<String>().to_lowercase()) {
        for p in &g {
            let same: Vec<&str> = g.iter().copied().filter(|q| q.to_lowercase() != p.to_lowercase()).collect();
            if !same.is_empty() {
                problems.add(
                    *p,
                    "unicode",
                    format!("is written differently but is the same name as {} once Unicode is normalized", quoted(&same, p)),
                    &[Platform::Macos],
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(rels: &[&str], here: Platform) -> Vec<Problem> {
        let mut p = Problems::new(here);
        check_names(&rels.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &mut p);
        p.into_vec()
    }

    fn at(problems: &[Problem], path: &str) -> Vec<(&'static str, bool)> {
        problems.iter().filter(|p| p.path == path).map(|p| (p.kind, p.blocking)).collect()
    }

    #[test]
    fn collisions_by_case_and_normalization() {
        let p = check(&["Docs/a.md", "docs/b.md", "README.md", "readme.md", "ok.md"], Platform::Linux);
        assert_eq!(at(&p, "Docs/"), [("case", false)]);
        assert_eq!(at(&p, "readme.md"), [("case", false)]);
        assert!(at(&p, "ok.md").is_empty());
        assert!(check(&["A.md", "a.md"], Platform::Windows).iter().all(|x| x.blocking));

        let (nfc, nfd) = ("caf\u{e9}.md", "cafe\u{301}.md");
        assert_eq!(at(&check(&[nfc, nfd], Platform::Macos), nfc), [("unicode", true)]);
        assert_eq!(at(&check(&[nfc, nfd], Platform::Windows), nfd), [("unicode", false)]);
        assert_eq!(fold(nfd, Platform::Macos), fold(nfc, Platform::Macos));
    }

    #[test]
    fn names_windows_refuses() {
        let p = check(&["notes/CON.md", "aux", "com1.txt", "com10.md", "a:b.md", "folder./x.md", "tab\there.md"], Platform::Windows);
        assert_eq!(at(&p, "notes/CON.md"), [("reserved", true)]);
        assert_eq!(at(&p, "aux"), [("reserved", true)]);
        assert_eq!(at(&p, "com1.txt"), [("reserved", true)]);
        assert!(at(&p, "com10.md").is_empty());
        assert_eq!(at(&p, "a:b.md"), [("character", true)]);
        assert_eq!(at(&p, "tab\there.md"), [("character", true)]);
        assert_eq!(at(&p, "folder./"), [("trailing", true)]);
        assert!(check(&["notes/CON.md"], Platform::Linux).iter().all(|x| !x.blocking));
    }
}
