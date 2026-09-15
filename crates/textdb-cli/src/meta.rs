//! `textdb meta`: read and change one top-level front matter key. Setting a key replaces only
//! the lines of that key (or adds it before the closing `---`), so comments, key order, quoting
//! and line endings of everything else stay as they were.

use serde_json::{json, Value};

use crate::store::{LineRange, Store, StoreError};
use crate::{emit_json, emit_written, line, out, Result};

/// The value `meta set` writes.
#[derive(Debug, Clone, PartialEq)]
pub enum NewValue {
    /// A string, quoted only when YAML would otherwise read it differently.
    Text(String),
    /// A block list, indented like the lists already in the front matter.
    List(Vec<String>),
    /// YAML written as given (`[a, b]`, `"x"`).
    Raw(String),
}

fn bare(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

/// The `---` lines (0-based) around the front matter, when the file starts with one.
fn fences(lines: &[&str]) -> Option<(usize, usize)> {
    if bare(lines.first()?).trim_start_matches('\u{feff}') != "---" {
        return None;
    }
    let close = lines.iter().skip(1).position(|l| matches!(bare(l), "---" | "..."))? + 1;
    Some((0, close))
}

fn eol_of(lines: &[&str]) -> &'static str {
    if lines.first().is_some_and(|l| l.ends_with("\r\n")) {
        "\r\n"
    } else {
        "\n"
    }
}

/// Lines `start..end` (0-based) of top-level `key` and its value: the `key:` line and the
/// indented or `- ` lines below it.
fn key_span(lines: &[&str], (open, close): (usize, usize), key: &str) -> Option<(usize, usize)> {
    let names = [key.to_string(), format!("\"{key}\""), format!("'{key}'")];
    let start = (open + 1..close).find(|&i| {
        let l = bare(lines[i]);
        names.iter().any(|n| l.strip_prefix(n.as_str()).is_some_and(|rest| rest == ":" || rest.starts_with(": ") || rest.starts_with(":\t")))
    })?;
    let mut end = start + 1;
    while end < close {
        let l = bare(lines[end]);
        let continues = if l.trim().is_empty() {
            // A blank line inside a block scalar or list belongs to the key when more follows.
            (end + 1..close).map(|j| bare(lines[j])).find(|n| !n.trim().is_empty()).is_some_and(|n| n.starts_with([' ', '\t']))
        } else {
            l.starts_with([' ', '\t']) || l == "-" || l.starts_with("- ")
        };
        if !continues {
            break;
        }
        end += 1;
    }
    Some((start, end))
}

/// The indent of list items: the key's own, else the first list in the front matter, else two
/// spaces.
fn list_indent(lines: &[&str], (open, close): (usize, usize), span: Option<(usize, usize)>) -> String {
    let item = |i: usize| {
        let l = bare(lines[i]);
        let t = l.trim_start();
        (t.starts_with("- ") || t == "-").then(|| l[..l.len() - t.len()].to_string())
    };
    span.and_then(|(s, e)| (s + 1..e).find_map(item))
        .or_else(|| (open + 1..close).find_map(item))
        .unwrap_or_else(|| "  ".to_string())
}

/// `s` as a YAML scalar: plain when that reads back as the same text (dates and numbers stay
/// unquoted, as people write them), double-quoted otherwise.
fn scalar(s: &str) -> String {
    let indicator = s.starts_with(['[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`', ','])
        || ["-", "?", ":"].iter().any(|c| s == *c || s.starts_with(&format!("{c} ")));
    let plain = !s.is_empty()
        && s == s.trim()
        && !indicator
        && !s.contains(['\n', '\r', '\t'])
        && !s.contains(": ")
        && !s.contains(" #")
        && !s.ends_with(':');
    if plain {
        return s.to_string();
    }
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t");
    format!("\"{escaped}\"")
}

fn render(key: &str, value: &NewValue, indent: &str, eol: &str) -> String {
    match value {
        NewValue::Text(s) => format!("{key}: {}{eol}", scalar(s)),
        NewValue::Raw(s) => format!("{key}: {s}{eol}"),
        NewValue::List(items) if items.is_empty() => format!("{key}: []{eol}"),
        NewValue::List(items) => {
            let mut text = format!("{key}:{eol}");
            for item in items {
                text.push_str(&format!("{indent}- {}{eol}", scalar(item)));
            }
            text
        }
    }
}

/// The line change that sets `key` (or removes it, for `None`); `None` when nothing changes.
fn plan(text: &str, key: &str, value: Option<&NewValue>) -> Option<LineRange> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let eol = eol_of(&lines);
    let Some(f) = fences(&lines) else {
        let block = format!("---{eol}{}---{eol}", render(key, value?, "  ", eol));
        return Some(match lines.first().and_then(|l| l.strip_prefix('\u{feff}')) {
            Some(rest) => LineRange { from: 1, to: 1, text: format!("\u{feff}{block}{rest}") },
            None => LineRange { from: 1, to: 0, text: block },
        });
    };
    let span = key_span(&lines, f, key);
    let text = |v: &NewValue| render(key, v, &list_indent(&lines, f, span), eol);
    Some(match (span, value) {
        (Some((s, e)), Some(v)) => LineRange { from: s as i64 + 1, to: e as i64, text: text(v) },
        (Some((s, e)), None) => LineRange { from: s as i64 + 1, to: e as i64, text: String::new() },
        (None, Some(v)) => LineRange { from: f.1 as i64 + 1, to: f.1 as i64, text: text(v) },
        (None, None) => return None,
    })
}

fn check_key(key: &str) -> Result<()> {
    let bad = key.is_empty()
        || key != key.trim()
        || key.contains([':', '\n', '\r', '#'])
        || key.starts_with(['-', '[', '{', '"', '\'']);
    if bad {
        return Err(StoreError::invalid(format!("{key:?} is not a front matter key meta can set: give a top-level key such as status")));
    }
    Ok(())
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn get(st: &mut dyn Store, path: &str, key: Option<&str>, json: bool) -> Result<()> {
    let (body, version) = st.read(path, None)?;
    let text = String::from_utf8_lossy(&body);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let yaml = fences(&lines).map(|(o, c)| lines[o + 1..c].concat()).unwrap_or_default();
    let data = textdb_md::parse_yaml(&yaml).unwrap_or_else(|| json!({}));
    let Some(key) = key else {
        return if json { emit_json(&data) } else { out(yaml.as_bytes()) };
    };
    let Some(value) = data.get(key) else {
        return Err(StoreError::not_found(format!("no {key} in the front matter of {path} (v{version})")));
    };
    if json {
        return emit_json(value);
    }
    let text: String = match value {
        Value::Array(items) => items.iter().map(|i| format!("{}\n", plain(i))).collect(),
        other => format!("{}\n", plain(other)),
    };
    out(text.as_bytes())
}

/// Set `key` to `value`, or remove it for `None`, against the version just read: a concurrent
/// commit elsewhere in the file is rebased, one to the same lines is a conflict (exit 3).
pub fn set(
    st: &mut dyn Store,
    path: &str,
    key: &str,
    value: Option<NewValue>,
    message: Option<&str>,
    author: Option<&str>,
    json: bool,
) -> Result<()> {
    check_key(key)?;
    let (body, version) = st.read(path, None)?;
    let text = std::str::from_utf8(&body).map_err(|_| StoreError::invalid(format!("{path} is not UTF-8 text")))?;
    let Some(range) = plan(text, key, value.as_ref()) else {
        if json {
            return emit_json(&json!({ "path": path, "version": version, "kind": "noop" }));
        }
        return line(format!("{path}: no {key} in the front matter, unchanged at v{version}"));
    };
    let default = format!("meta {} {key}", if value.is_some() { "set" } else { "unset" });
    let w = st.replace_ranges(path, &[range], Some(version), author, Some(message.unwrap_or(&default)))?;
    emit_written(path, &w, json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::splice_lines;

    fn apply(text: &str, key: &str, value: Option<NewValue>) -> String {
        match plan(text, key, value.as_ref()) {
            Some(r) => String::from_utf8(splice_lines(text.as_bytes(), &[r]).unwrap()).unwrap(),
            None => text.to_string(),
        }
    }

    const DOC: &str = "---\ntitle: Acme # the account\ntags:\n    - telco\n\n    - cvm\nnotes: |\n  line one\n\n  line two\nstatus: draft\n---\n# Acme\nstatus: not front matter\n";

    #[test]
    fn set_replaces_only_the_key_lines() {
        assert_eq!(
            apply(DOC, "status", Some(NewValue::Text("published".into()))),
            DOC.replace("status: draft\n", "status: published\n")
        );
        assert_eq!(
            apply(DOC, "tags", Some(NewValue::List(vec!["rfi".into(), "[[XLSMART]]".into()]))),
            DOC.replace("tags:\n    - telco\n\n    - cvm\n", "tags:\n    - rfi\n    - \"[[XLSMART]]\"\n")
        );
        assert_eq!(apply(DOC, "notes", None), DOC.replace("notes: |\n  line one\n\n  line two\n", ""));
        assert_eq!(apply(DOC, "missing", None), DOC);
    }

    #[test]
    fn a_new_key_goes_before_the_closing_line_or_into_new_front_matter() {
        assert_eq!(
            apply(DOC, "owner", Some(NewValue::Raw("[a, b]".into()))),
            DOC.replace("status: draft\n---\n", "status: draft\nowner: [a, b]\n---\n")
        );
        assert_eq!(apply("# Plain\r\n", "x", Some(NewValue::Text("1".into()))), "---\r\nx: 1\r\n---\r\n# Plain\r\n");
        assert_eq!(apply("\u{feff}# Plain\n", "x", Some(NewValue::Text("y".into()))), "\u{feff}---\nx: y\n---\n# Plain\n");
        assert_eq!(
            apply("---\r\na: 1\r\n---\r\n", "b", Some(NewValue::List(vec!["x".into()]))),
            "---\r\na: 1\r\nb:\r\n  - x\r\n---\r\n"
        );
    }

    #[test]
    fn scalars_are_quoted_only_when_needed() {
        for s in ["2026-02-10", "published", "RFI Response Tracker", "-5", "a-b", "C#"] {
            assert_eq!(scalar(s), s);
        }
        assert_eq!(scalar("[[Jonas]]"), "\"[[Jonas]]\"");
        assert_eq!(scalar("note: see"), "\"note: see\"");
        assert_eq!(scalar("say \"hi\"\n"), "\"say \\\"hi\\\"\\n\"");
        assert_eq!(scalar(""), "\"\"");
        assert_eq!(scalar("- x"), "\"- x\"");
    }
}

/// Property names used anywhere in the store, most-used first.
pub fn keys(st: &mut dyn Store, prefix: &str, limit: i64, json: bool) -> Result<()> {
    let rows = st.property_keys(prefix, limit)?;
    if json {
        return emit_json(&rows);
    }
    let mut text = String::new();
    for k in &rows {
        // The counts are what make a listing useful for deciding what to filter on, and the
        // kind is what tells a reader whether `>` will mean anything on this property.
        text.push_str(&format!("{}  {} docs, {} values, {}\n", k.key, k.docs, k.values, k.kind));
    }
    out(text.as_bytes())
}

/// The values one property takes, most-used first.
pub fn values(st: &mut dyn Store, key: &str, prefix: &str, limit: i64, json: bool) -> Result<()> {
    let rows = st.property_values(key, prefix, limit)?;
    if json {
        return emit_json(&rows);
    }
    let mut text = String::new();
    for v in &rows {
        // A property present but empty is a real state in a vault, so it is listed rather
        // than dropped, and named rather than shown as a blank line.
        let shown = v.value.as_deref().unwrap_or("(empty)");
        text.push_str(&format!("{}  {} docs\n", shown, v.docs));
    }
    out(text.as_bytes())
}

/// Documents matching a property query.
pub fn find(st: &mut dyn Store, query: &str, folder: &str, limit: i64, show: Option<&str>, json: bool) -> Result<()> {
    let rows = st.property_find(query, folder, limit)?;
    if json {
        return emit_json(&rows);
    }
    let columns: Vec<&str> = show.map(|s| s.split(',').map(str::trim).filter(|c| !c.is_empty()).collect()).unwrap_or_default();
    let mut text = String::new();
    for hit in &rows {
        if columns.is_empty() {
            text.push_str(&hit.path);
            text.push('\n');
            continue;
        }
        let shown: Vec<String> = columns
            .iter()
            .map(|c| {
                let v = hit.frontmatter.as_ref().and_then(|f| f.get(*c));
                format!("{}={}", c, v.map(plain_or_list).unwrap_or_else(|| "-".into()))
            })
            .collect();
        text.push_str(&format!("{}  {}\n", hit.path, shown.join("  ")));
    }
    out(text.as_bytes())
}

/// A property value for a table cell: a list joins with commas rather than printing as JSON.
fn plain_or_list(v: &Value) -> String {
    match v {
        Value::Array(items) => items.iter().map(plain).collect::<Vec<_>>().join(","),
        other => plain(other),
    }
}
