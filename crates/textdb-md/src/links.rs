//! Links in a markdown document, with where each target is written so a rename can rewrite it:
//! wikilinks `[[target#anchor|alias]]`, embeds `![[…]]`, and markdown links and images
//! `[text](target#anchor)`. Nothing inside code spans or code blocks is a link.

use std::borrow::Cow;
use std::ops::Range;

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSpan {
    /// `wiki`, `embed`, `md` or `image`.
    pub kind: &'static str,
    /// The target without anchor, alias or query; percent-decoded for markdown links.
    pub target: String,
    pub anchor: Option<String>,
    pub alias: Option<String>,
    /// A URL, email address, query or numbered reference rather than a document.
    pub external: bool,
    /// The bytes of the target as written; `None` when it is not written in place (reference
    /// links, autolinks).
    pub range: Option<Range<usize>>,
    /// Where the link starts.
    pub offset: usize,
    /// A markdown destination written in `<…>`.
    pub angle: bool,
}

fn has_scheme(s: &str) -> bool {
    match s.find(':') {
        Some(i) if i > 0 => {
            let scheme = &s[..i];
            !scheme.contains('/') && scheme.starts_with(|c: char| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
        }
        _ => false,
    }
}

fn is_email(s: &str) -> bool {
    s.contains('@') && !s.contains('/')
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() &&b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'%'));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// The destination of the inline link or image spanning `r`, and whether it is in `<…>`.
fn dest_range(text: &str, r: &Range<usize>) -> Option<(Range<usize>, bool)> {
    let src = text.get(r.clone())?;
    if !src.ends_with(')') {
        return None;
    }
    let open = src.rfind("](")? + 2;
    let inner = &src[open..src.len() - 1];
    let start = r.start + open + (inner.len() - inner.trim_start().len());
    let rest = &text[start..r.end - 1];
    match rest.strip_prefix('<') {
        Some(inside) => inside.find('>').map(|end| (start + 1..start + 1 + end, true)),
        None => Some((start..start + rest.find(char::is_whitespace).unwrap_or(rest.len()), false)),
    }
}

fn markdown_link(text: &str, r: &Range<usize>, link_type: LinkType, dest: &str, kind: &'static str) -> Option<LinkSpan> {
    if matches!(link_type, LinkType::WikiLink { .. }) || dest.is_empty() {
        return None;
    }
    let cut = dest.find(['#', '?']).unwrap_or(dest.len());
    let target = percent_decode(&dest[..cut]);
    let anchor = dest[cut..].split_once('#').map(|(_, a)| percent_decode(a)).filter(|a| !a.is_empty());
    let external = matches!(link_type, LinkType::Autolink | LinkType::Email)
        || dest.starts_with('?')
        || has_scheme(dest)
        || is_email(dest)
        || (!target.is_empty() && target.chars().all(|c| c.is_ascii_digit()));
    let (range, angle) = match link_type {
        LinkType::Inline => match dest_range(text, r) {
            Some((range, angle)) => {
                let raw = &text[range.clone()];
                let cut = raw.find(['#', '?']).unwrap_or(raw.len());
                (Some(range.start..range.start + cut), angle)
            }
            None => (None, false),
        },
        _ => (None, false),
    };
    // The text between `[` and `](`: the words a reader clicks. Without it every markdown link
    // rendered as `[](target)` — the same four characters for every link in a document.
    let alias = link_text(text, r);
    Some(LinkSpan { kind, target, anchor, alias, external, range, offset: r.start, angle })
}

/// The display text of a markdown link or image: what sits between `[` (or `![`) and `](`.
///
/// `None` when it is empty, so `[](x.md)` stays aliasless rather than carrying `""`.
fn link_text(text: &str, r: &Range<usize>) -> Option<String> {
    let src = text.get(r.clone())?;
    let open = src.find('[')? + 1;
    let close = src.rfind("](")?;
    (close > open).then(|| src[open..close].trim().to_string()).filter(|t| !t.is_empty())
}

/// Every link in `bytes`, in document order.
pub fn scan(bytes: &[u8]) -> Vec<LinkSpan> {
    let text: Cow<str> = String::from_utf8_lossy(bytes);
    let body_off = crate::split_frontmatter(text.as_bytes()).map_or(0, |(_, off)| off).min(text.len());
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    opts.insert(Options::ENABLE_WIKILINKS);
    let mut code: Vec<Range<usize>> = Vec::new();
    let mut links = Vec::new();
    for (ev, r) in Parser::new_ext(&text[body_off..], opts).into_offset_iter() {
        let r = r.start + body_off..r.end + body_off;
        match ev {
            Event::Code(_) | Event::Start(Tag::CodeBlock(_)) => code.push(r),
            Event::Start(Tag::Link { link_type, dest_url, .. }) => links.extend(markdown_link(&text, &r, link_type, &dest_url, "md")),
            Event::Start(Tag::Image { link_type, dest_url, .. }) => links.extend(markdown_link(&text, &r, link_type, &dest_url, "image")),
            _ => {}
        }
    }
    let b = text.as_bytes();
    let mut i = 0;
    while i + 4 <= b.len() {
        if &b[i..i + 2] != b"[[" || code.iter().any(|c| c.contains(&i)) {
            i += 1;
            continue;
        }
        let Some(len) = b[i + 2..].windows(2).position(|w| w == b"]]") else {
            break;
        };
        let start = i + 2;
        let inner = &text[start..start + len];
        if inner.is_empty() || inner.contains('\n') {
            i += 2;
            continue;
        }
        let (before, alias) = match inner.find('|') {
            Some(p) => (&inner[..p], Some(inner[p + 1..].trim().to_string()).filter(|a| !a.is_empty())),
            None => (inner, None),
        };
        // In a table the pipe is written `\|`.
        let before = before.strip_suffix('\\').unwrap_or(before);
        let hash = before.find('#');
        let raw = &before[..hash.unwrap_or(before.len())];
        let lead = raw.len() - raw.trim_start().len();
        let target = raw.trim();
        links.push(LinkSpan {
            kind: if i > 0 && b[i - 1] == b'!' { "embed" } else { "wiki" },
            target: target.to_string(),
            anchor: hash.map(|h| before[h + 1..].trim().to_string()).filter(|a| !a.is_empty()),
            alias,
            external: has_scheme(target) || is_email(target),
            range: Some(start + lead..start + lead + target.len()),
            offset: if i > 0 && b[i - 1] == b'!' { i - 1 } else { i },
            angle: false,
        });
        i = start + len + 2;
    }
    links.sort_by_key(|l| l.offset);
    links
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_anchors_aliases_and_what_is_not_a_document() {
        let doc = "---\nrelated: \"[[Acme Corp]]\"\n---\n# T\n\n| a | [[meetings/x.md\\|x]] |\n|---|---|\n\nSee [[Note#Next steps|steps]], ![[diagram.png]] and [[#Local]].\n\n[p](../proposals/My%20Plan.md#goals \"t\") [q](<a b.md>) [g](?tab=t.1) [m](mailto:a@b.c) [e](a@b.c) [n](1) [w](https://x.y/z) ![i](./img/p.png)\n\n`[[not a link]]`\n\n```\n[[nor this]]\n```\n";
        let got: Vec<_> = scan(doc.as_bytes())
            .into_iter()
            .map(|l| (l.kind, l.target, l.anchor, l.alias, l.external, l.range.map(|r| doc[r].to_string())))
            .collect();
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            got,
            vec![
                ("wiki", s("Acme Corp").unwrap(), None, None, false, s("Acme Corp")),
                ("wiki", s("meetings/x.md").unwrap(), None, s("x"), false, s("meetings/x.md")),
                ("wiki", s("Note").unwrap(), s("Next steps"), s("steps"), false, s("Note")),
                ("embed", s("diagram.png").unwrap(), None, None, false, s("diagram.png")),
                ("wiki", String::new(), s("Local"), None, false, s("")),
                // A markdown link's display text is its alias, the way a wiki link's is: without
                // it every one of these rendered as `[](target)` in `textdb links`.
                ("md", s("../proposals/My Plan.md").unwrap(), s("goals"), s("p"), false, s("../proposals/My%20Plan.md")),
                ("md", s("a b.md").unwrap(), None, s("q"), false, s("a b.md")),
                ("md", String::new(), None, s("g"), true, s("")),
                ("md", s("mailto:a@b.c").unwrap(), None, s("m"), true, s("mailto:a@b.c")),
                ("md", s("a@b.c").unwrap(), None, s("e"), true, s("a@b.c")),
                ("md", s("1").unwrap(), None, s("n"), true, s("1")),
                ("md", s("https://x.y/z").unwrap(), None, s("w"), true, s("https://x.y/z")),
                ("image", s("./img/p.png").unwrap(), None, s("i"), false, s("./img/p.png")),
            ]
        );
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("My%20Plan%2Fx%zz%2"), "My Plan/x%zz%2");
    }
}
