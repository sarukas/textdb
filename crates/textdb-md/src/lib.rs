//! Markdown structure extractor (spec §6.8): sections (heading path, level, line span),
//! links (wikilinks and relative markdown links) and YAML frontmatter.
//!
//! Runs on the full materialized document after each commit. Only this crate knows
//! markdown; core is format-agnostic.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use textdb_core::structure::{Link, Section, Structure, StructureExtractor};

pub mod links;
pub mod query;
pub mod resolve;
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

/// Front matter YAML (without its `---` lines) as JSON, as it is kept for the `frontmatter` table.
pub fn parse_yaml(text: &str) -> Option<serde_json::Value> {
    yaml::parse(text)
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
            _ => {}
        }
    }
    let links: Vec<Link> = links::scan(bytes)
        .into_iter()
        .map(|l| Link {
            target_path: l.target,
            line: line_of(&starts, l.offset),
            kind: l.kind.to_string(),
            anchor: l.anchor,
            alias: l.alias,
            external: l.external,
            span: l.range.as_ref().map(|r| (r.start as u64, r.end as u64)),
        })
        .collect();

    // Words per line, accumulated once, so every section's two counts are a subtraction
    // rather than a re-scan. `cum[i]` is the words in lines 1..=i.
    let cum = word_prefix(bytes, &starts);

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
        let line_to = line_to.max(h.line);
        // The section's own lines run to the next heading of any level, since that heading
        // starts a section of its own; everything after it inside `line_to` is nested.
        let own_to = headings[i + 1..]
            .first()
            .map(|n| (n.line - 1).max(h.line))
            .unwrap_or(line_to)
            .min(line_to);
        sections.push(Section {
            heading_path,
            level: h.level,
            line_from: h.line,
            line_to,
            heading: h.text.clone(),
            nwords: words_in(&cum, h.line, own_to),
            nwords_total: words_in(&cum, h.line, line_to),
        });
    }
    Structure {
        sections,
        links,
        frontmatter,
    }
}

/// Words in lines 1..=i for every i, so a line range costs one subtraction.
///
/// Index 0 is the empty prefix. A word never spans a newline, so counting each line
/// independently and adding is the same number as counting the whole range at once — which
/// is what makes a section's two figures exact and lets a parent's total be built from its
/// children's.
fn word_prefix(bytes: &[u8], starts: &[usize]) -> Vec<u64> {
    let mut cum = Vec::with_capacity(starts.len() + 1);
    cum.push(0u64);
    let mut running = 0u64;
    for (i, &from) in starts.iter().enumerate() {
        let to = starts.get(i + 1).copied().unwrap_or(bytes.len());
        running += textdb_core::words::count_words(&bytes[from.min(bytes.len())..to.min(bytes.len())]);
        cum.push(running);
    }
    cum
}

/// Words in the 1-based inclusive line range `[from, to]`.
fn words_in(cum: &[u64], from: u64, to: u64) -> u64 {
    if cum.len() <= 1 || to < from {
        return 0;
    }
    let last = cum.len() as u64 - 1;
    let hi = to.min(last) as usize;
    let lo = from.saturating_sub(1).min(last) as usize;
    cum[hi].saturating_sub(cum[lo])
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
    fn section_word_counts_are_exact_and_compose() {
        let s = extract(DOC.as_bytes());
        let by = |h: &str| s.sections.iter().find(|x| x.heading_path == h).unwrap();

        // Own lines only: the heading line plus what sits under it before the next heading.
        // "## Goals" is two words, "line" one.
        assert_eq!(by("Intro / Goals").nwords, 3);
        // Totals include everything nested: "### Detail" (2) + "more" (1) on top of the 3.
        assert_eq!(by("Intro / Goals").nwords_total, 6);
        // A leaf section's two figures are the same number.
        let detail = by("Intro / Goals / Detail");
        assert_eq!((detail.nwords, detail.nwords_total), (3, 3));

        // The parent's total is its own words plus every descendant's own words. This is the
        // property that makes the two columns worth storing rather than one.
        for parent in &s.sections {
            let nested: u64 = s
                .sections
                .iter()
                .filter(|c| c.line_from > parent.line_from && c.line_to <= parent.line_to)
                .map(|c| c.nwords)
                .sum();
            assert_eq!(
                parent.nwords_total,
                parent.nwords + nested,
                "{} total {} != own {} + nested {}",
                parent.heading_path,
                parent.nwords_total,
                parent.nwords,
                nested
            );
        }

        // Every top-level section's total, plus anything before the first heading, accounts
        // for the whole document — nothing double counted, nothing dropped.
        let top: u64 = s.sections.iter().filter(|x| x.level == 1).map(|x| x.nwords_total).sum();
        let first = s.sections.first().unwrap().line_from;
        let starts = line_starts(DOC.as_bytes());
        let preamble = starts
            .get(..(first as usize).saturating_sub(1))
            .map(|_| {
                let end = starts[(first as usize) - 1];
                textdb_core::words::count_words(&DOC.as_bytes()[..end])
            })
            .unwrap_or(0);
        assert_eq!(top + preamble, textdb_core::words::count_words(DOC.as_bytes()));
    }

    #[test]
    fn heading_is_the_last_component_of_the_path() {
        let s = extract(DOC.as_bytes());
        for x in &s.sections {
            assert_eq!(x.heading, x.heading_path.rsplit(" / ").next().unwrap());
        }
        assert_eq!(s.sections.iter().find(|x| x.level == 3).unwrap().heading, "Detail");
    }

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
        assert_eq!(links, vec![("Other Note".to_string(), 8), ("./x.md".to_string(), 8), ("https://example.com".to_string(), 8)]);
        assert!(s.links[2].external && !s.links[1].external);
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
