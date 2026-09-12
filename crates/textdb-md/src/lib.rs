//! Markdown structure extractor (spec §6.8): sections (heading path, level, line span),
//! links (wikilinks and relative markdown links) and YAML frontmatter.
//!
//! Runs on the full materialized document after each commit. Only this crate knows
//! markdown; core is format-agnostic.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use textdb_core::structure::{Link, Section, Structure, StructureExtractor};

mod yaml;

#[derive(Default, Clone, Copy, Debug)]
pub struct MarkdownExtractor;

impl StructureExtractor for MarkdownExtractor {
    fn extract(&self, bytes: &[u8]) -> Structure {
        extract(bytes)
    }
}

/// Byte offsets at which each line starts (line 0 at 0).
fn line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut v = vec![0];
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

/// 1-based line of a byte offset.
fn line_of(starts: &[usize], off: usize) -> u64 {
    match starts.binary_search(&off) {
        Ok(i) => i as u64 + 1,
        Err(i) => i as u64,
    }
}

fn level_num(l: HeadingLevel) -> u32 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Split off a leading YAML frontmatter block. Returns `(yaml_text, body_offset)`.
pub fn split_frontmatter(bytes: &[u8]) -> Option<(&[u8], usize)> {
    let s = bytes;
    if !(s.starts_with(b"---\n") || s.starts_with(b"---\r\n")) {
        return None;
    }
    let first_nl = s.iter().position(|&b| b == b'\n')?;
    let mut pos = first_nl + 1;
    while pos < s.len() {
        let end = s[pos..].iter().position(|&b| b == b'\n').map(|p| pos + p).unwrap_or(s.len());
        let line = &s[pos..end];
        let trimmed = line.strip_suffix(b"\r").unwrap_or(line);
        if trimmed == b"---" || trimmed == b"..." {
            return Some((&s[first_nl + 1..pos], (end + 1).min(s.len())));
        }
        pos = end + 1;
    }
    None
}

pub fn extract(bytes: &[u8]) -> Structure {
    let text = String::from_utf8_lossy(bytes);
    let starts = line_starts(bytes);
    let total_lines = starts.len() as u64; // a trailing newline yields an empty last line
    let last_line = if bytes.last() == Some(&b'\n') { total_lines - 1 } else { total_lines }.max(1);

    let (frontmatter, body_off) = match split_frontmatter(bytes) {
        Some((yaml, off)) => (yaml::parse(&String::from_utf8_lossy(yaml)), off),
        None => (None, 0),
    };
    let body = &text[body_off.min(text.len())..];
    // Offsets from the parser are relative to `body`; translate with `body_off`.
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    opts.insert(Options::ENABLE_WIKILINKS);
    let parser = Parser::new_ext(body, opts).into_offset_iter();

    struct Heading {
        level: u32,
        text: String,
        line: u64,
    }
    let mut headings: Vec<Heading> = Vec::new();
    let mut links: Vec<Link> = Vec::new();
    let mut cur: Option<(u32, usize, String)> = None; // (level, start_off, text)
    for (ev, range) in parser {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                cur = Some((level_num(level), range.start + body_off, String::new()));
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(c) = cur.as_mut() {
                    c.2.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, off, text)) = cur.take() {
                    headings.push(Heading {
                        level,
                        text: text.trim().to_string(),
                        line: line_of(&starts, off),
                    });
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let d = dest_url.to_string();
                if !d.contains("://") && !d.starts_with('#') && !d.starts_with("mailto:") && !d.is_empty() {
                    links.push(Link {
                        target_path: d,
                        line: line_of(&starts, range.start + body_off),
                    });
                }
            }
            _ => {}
        }
    }
    // Wikilinks `[[target]]` / `[[target|alias]]` scanned on raw text as well, because the
    // parser only reports them when the feature is enabled and the syntax is well formed.
    let mut i = 0;
    let b = bytes;
    while i + 4 <= b.len() {
        if &b[i..i + 2] == b"[[" {
            if let Some(end) = b[i + 2..].windows(2).position(|w| w == b"]]") {
                let inner = &b[i + 2..i + 2 + end];
                if !inner.contains(&b'\n') && !inner.is_empty() {
                    let target = String::from_utf8_lossy(inner);
                    let target = target.split('|').next().unwrap_or("").trim().to_string();
                    let line = line_of(&starts, i);
                    if !links.iter().any(|l| l.target_path == target && l.line == line) {
                        links.push(Link {
                            target_path: target,
                            line,
                        });
                    }
                }
                i += 2 + end + 2;
                continue;
            }
        }
        i += 1;
    }
    links.sort_by_key(|l| l.line);

    // Sections: each heading spans until the next heading of the same or higher level.
    let mut sections = Vec::new();
    let mut stack: Vec<(u32, String)> = Vec::new();
    for (i, h) in headings.iter().enumerate() {
        while let Some((l, _)) = stack.last() {
            if *l >= h.level {
                stack.pop();
            } else {
                break;
            }
        }
        stack.push((h.level, h.text.clone()));
        let heading_path = stack.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" / ");
        let line_to = headings[i + 1..]
            .iter()
            .find(|n| n.level <= h.level)
            .map(|n| n.line - 1)
            .unwrap_or(last_line);
        sections.push(Section {
            heading_path,
            level: h.level,
            line_from: h.line,
            line_to: line_to.max(h.line),
        });
    }
    Structure {
        sections,
        links,
        frontmatter,
    }
}

/// Find the section whose heading matches `heading`: exact heading-path match first, then
/// the last path component, case-insensitively. Returns the 1-based inclusive line span.
pub fn find_section(structure: &Structure, heading: &str) -> Option<(u64, u64)> {
    let h = heading.trim();
    if let Some(s) = structure.sections.iter().find(|s| s.heading_path == h) {
        return Some((s.line_from, s.line_to));
    }
    let hl = h.to_lowercase();
    structure
        .sections
        .iter()
        .find(|s| s.heading_path.rsplit(" / ").next().map(|t| t.to_lowercase()) == Some(hl.clone()))
        .map(|s| (s.line_from, s.line_to))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\ntitle: Test doc\ntags: [a, b]\ndraft: false\n---\n# Intro\n\nSee [[Other Note]] and [local](./x.md) and [web](https://example.com).\n\n## Goals\nline\n\n### Detail\nmore\n\n## Scope\nend\n# Second\ntail";

    #[test]
    fn sections_links_frontmatter() {
        let s = extract(DOC.as_bytes());
        let paths: Vec<_> = s.sections.iter().map(|x| (x.heading_path.clone(), x.level, x.line_from, x.line_to)).collect();
        assert_eq!(
            paths,
            vec![
                ("Intro".to_string(), 1, 6, 17),
                ("Intro / Goals".to_string(), 2, 10, 15),
                ("Intro / Goals / Detail".to_string(), 3, 13, 15),
                ("Intro / Scope".to_string(), 2, 16, 17),
                ("Second".to_string(), 1, 18, 19),
            ]
        );
        let links: Vec<_> = s.links.iter().map(|l| (l.target_path.clone(), l.line)).collect();
        assert_eq!(links, vec![("Other Note".to_string(), 8), ("./x.md".to_string(), 8)]);
        let fm = s.frontmatter.clone().unwrap();
        assert_eq!(fm["title"], "Test doc");
        assert_eq!(fm["tags"], serde_json::json!(["a", "b"]));
        assert_eq!(fm["draft"], false);
        assert_eq!(find_section(&s, "Goals"), Some((10, 15)));
        assert_eq!(find_section(&s, "Intro / Scope"), Some((16, 17)));
    }

    #[test]
    fn empty_and_plain() {
        assert_eq!(extract(b""), Structure::default());
        let s = extract(b"just text\nno headings\n");
        assert!(s.sections.is_empty());
    }
}
