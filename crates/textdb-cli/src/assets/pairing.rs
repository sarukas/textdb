//! How sync pairs pointers with the real files next to them (docs/assets.md, "Sync"): the names of
//! the copies a keep-both conflict leaves, and matching things 1:1 by a key such as a pointer id or
//! a file's SHA-256.

use std::collections::HashMap;
use std::hash::Hash;
use std::path::Path;
use std::time::SystemTime;

/// Today's UTC date as `2026-09-15`.
pub fn today() -> String {
    let s = super::driver::stamp(SystemTime::now());
    format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
}

/// `host` as one word of a file name: letters, digits, `-` and `_`.
fn name_word(host: &str) -> String {
    let w: String = host.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    if w.is_empty() {
        "unknown".to_string()
    } else {
        w
    }
}

/// `img/arch.png` as `img/arch (conflict HOST 2026-09-15).png`, with ` 2`, ` 3`… after the date
/// for further copies: where a keep-both conflict puts the bytes this directory had.
pub fn conflict_copy(rel: &str, host: &str, date: &str, n: u32) -> String {
    let (dir, name) = match rel.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, rel),
    };
    let tag = if n <= 1 { format!("(conflict {} {date})", name_word(host)) } else { format!("(conflict {} {date} {n})", name_word(host)) };
    let tagged = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} {tag}.{ext}"),
        _ => format!("{name} {tag}"),
    };
    match dir {
        Some(d) => format!("{d}/{tagged}"),
        None => tagged,
    }
}

/// The first [`conflict_copy`] name for `rel` that nothing below `root` has.
pub fn free_conflict_copy(root: &Path, rel: &str, host: &str) -> String {
    let date = today();
    let mut n = 1;
    loop {
        let name = conflict_copy(rel, host, &date, n);
        if !root.join(&name).exists() {
            return name;
        }
        n += 1;
    }
}

/// Whether `rel` names a copy a keep-both conflict left, which never starts another conflict and
/// is never pushed on its own.
pub fn is_conflict_copy(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if stem.ends_with(')') => stem,
        _ => name,
    };
    let Some(open) = stem.rfind(" (conflict ") else { return false };
    let Some(inner) = stem[open + " (conflict ".len()..].strip_suffix(')') else { return false };
    let words: Vec<&str> = inner.split(' ').collect();
    let date = |w: &str| {
        let b = w.as_bytes();
        b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    };
    match words.as_slice() {
        [host, d] => !host.is_empty() && date(d),
        [host, d, n] => !host.is_empty() && date(d) && !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()),
        _ => false,
    }
}

/// The asset a conflict copy was set aside from: `img/arch (conflict HOST DATE).png` is
/// `img/arch.png`.
pub fn original_of(rel: &str) -> Option<String> {
    if !is_conflict_copy(rel) {
        return None;
    }
    let (dir, name) = match rel.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, rel),
    };
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if stem.ends_with(')') => (stem, Some(ext)),
        _ => (name, None),
    };
    let base = &stem[..stem.rfind(" (conflict ")?];
    let file = ext.map_or_else(|| base.to_string(), |ext| format!("{base}.{ext}"));
    Some(dir.map_or_else(|| file.clone(), |d| format!("{d}/{file}")))
}

/// The pairs of `lost` and `found` that share a key no other item on either side has.
pub fn one_to_one<K: Eq + Hash, A: Clone, B: Clone>(lost: impl IntoIterator<Item = (K, A)>, found: impl IntoIterator<Item = (K, B)>) -> Vec<(A, B)> {
    let mut sides: HashMap<K, (Vec<A>, Vec<B>)> = HashMap::new();
    for (k, a) in lost {
        sides.entry(k).or_default().0.push(a);
    }
    for (k, b) in found {
        sides.entry(k).or_default().1.push(b);
    }
    sides
        .into_values()
        .filter(|(a, b)| a.len() == 1 && b.len() == 1)
        .map(|(a, b)| (a[0].clone(), b[0].clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_copies_are_named_and_recognised() {
        assert_eq!(conflict_copy("img/arch.png", "LAPTOP-7", "2026-09-15", 1), "img/arch (conflict LAPTOP-7 2026-09-15).png");
        assert_eq!(conflict_copy("deck.tar.gz", "my host.local", "2026-09-15", 2), "deck.tar (conflict my-host-local 2026-09-15 2).gz");
        assert_eq!(conflict_copy("a/Makefile", "h", "2026-09-15", 1), "a/Makefile (conflict h 2026-09-15)");
        assert_eq!(conflict_copy(".hidden", "", "2026-09-15", 1), ".hidden (conflict unknown 2026-09-15)");
        for name in ["img/arch (conflict LAPTOP-7 2026-09-15).png", "deck.tar (conflict my-host-local 2026-09-15 2).gz", "a/Makefile (conflict h 2026-09-15)"] {
            assert!(is_conflict_copy(name), "{name}");
        }
        for name in ["img/arch.png", "notes (conflict).png", "x (conflict h 2026-9-15).png", "x (conflict h 2026-09-15 two).png", "x (conflict  2026-09-15).png"] {
            assert!(!is_conflict_copy(name), "{name}");
        }
        assert_eq!(original_of("img/arch (conflict LAPTOP-7 2026-09-15 2).png").as_deref(), Some("img/arch.png"));
        assert_eq!(original_of("a/Makefile (conflict h 2026-09-15)").as_deref(), Some("a/Makefile"));
        assert_eq!(original_of("img/arch.png"), None);
        assert_eq!(today().len(), 10);
        assert!(is_conflict_copy(&conflict_copy("z.pdf", "h", &today(), 1)));
    }

    #[test]
    fn only_keys_unique_on_both_sides_pair() {
        let mut pairs = one_to_one([("s1", "a"), ("s2", "b"), ("s2", "c"), ("s3", "d")], [("s1", "x"), ("s2", "y"), ("s3", "z"), ("s3", "w"), ("s4", "v")]);
        pairs.sort();
        assert_eq!(pairs, vec![("a", "x")]);
    }
}
