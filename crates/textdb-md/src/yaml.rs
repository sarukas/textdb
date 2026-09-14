//! Minimal YAML subset for frontmatter: `key: value` maps, block lists (`- item`),
//! flow lists (`[a, b]`), quoted strings, numbers, booleans, null. Nested maps via
//! indentation. Anything unparseable is kept as a string so no document is rejected.

use serde_json::{Map, Value};

pub fn parse(text: &str) -> Option<Value> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).collect();
    if lines.is_empty() {
        return Some(Value::Object(Map::new()));
    }
    let mut idx = 0;
    let v = parse_block(&lines, &mut idx, indent_of(lines[0]));
    Some(v)
}

fn indent_of(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

fn parse_block(lines: &[&str], idx: &mut usize, indent: usize) -> Value {
    if *idx >= lines.len() {
        return Value::Null;
    }
    if lines[*idx].trim_start().starts_with("- ") || lines[*idx].trim() == "-" {
        let mut arr = Vec::new();
        while *idx < lines.len() && indent_of(lines[*idx]) == indent && (lines[*idx].trim_start().starts_with("- ") || lines[*idx].trim() == "-") {
            let item = strip_comment(&lines[*idx].trim_start()[1..]);
            *idx += 1;
            if item.is_empty() {
                let next_indent = lines.get(*idx).map(|l| indent_of(l)).unwrap_or(0);
                if *idx < lines.len() && next_indent > indent {
                    arr.push(parse_block(lines, idx, next_indent));
                } else {
                    arr.push(Value::Null);
                }
            } else if let Some((k, v)) = split_kv(item) {
                // Inline map inside a list item.
                let mut m = Map::new();
                m.insert(k.to_string(), scalar(v));
                while *idx < lines.len() && indent_of(lines[*idx]) > indent {
                    if let Some((k2, v2)) = split_kv(lines[*idx].trim()) {
                        m.insert(k2.to_string(), scalar(v2));
                    }
                    *idx += 1;
                }
                arr.push(Value::Object(m));
            } else {
                arr.push(scalar(item));
            }
        }
        return Value::Array(arr);
    }
    let mut map = Map::new();
    while *idx < lines.len() {
        let l = lines[*idx];
        let ind = indent_of(l);
        if ind < indent {
            break;
        }
        if ind > indent {
            *idx += 1;
            continue;
        }
        match split_kv(l.trim()) {
            Some((k, v)) => {
                *idx += 1;
                if v.is_empty() {
                    let next_indent = lines.get(*idx).map(|l| indent_of(l)).unwrap_or(0);
                    if *idx < lines.len() && next_indent > indent {
                        let child = parse_block(lines, idx, next_indent);
                        map.insert(k.to_string(), child);
                    } else if *idx < lines.len() && next_indent == indent && lines[*idx].trim_start().starts_with("- ") {
                        let child = parse_block(lines, idx, indent);
                        map.insert(k.to_string(), child);
                    } else {
                        map.insert(k.to_string(), Value::Null);
                    }
                } else {
                    map.insert(k.to_string(), scalar(v));
                }
            }
            None => {
                *idx += 1;
            }
        }
    }
    Value::Object(map)
}

/// `key: value` with the value's trailing comment removed.
fn split_kv(s: &str) -> Option<(&str, &str)> {
    if s.starts_with('"') || s.starts_with('\'') || s.starts_with('[') || s.starts_with('{') {
        return None;
    }
    let pos = s.find(": ").or_else(|| if s.ends_with(':') { Some(s.len() - 1) } else { None })?;
    let k = s[..pos].trim();
    let v = strip_comment(&s[(pos + 1).min(s.len())..]);
    if k.is_empty() || k.contains(' ') && k.contains('"') {
        return None;
    }
    Some((k, v))
}

/// `v` trimmed, without a trailing `# comment`: a `#` that follows whitespace outside quotes
/// starts one. Quotes open only at the start of a value or item, so an apostrophe inside
/// plain text (`don't`) is not one.
fn strip_comment(v: &str) -> &str {
    let v = v.trim();
    if v.starts_with('#') {
        return "";
    }
    let b = v.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < b.len() {
        match (quote, b[i]) {
            (Some(b'"'), b'\\') => i += 1,
            (Some(b'\''), b'\'') if b.get(i + 1) == Some(&b'\'') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, c @ (b'"' | b'\'')) if i == 0 || matches!(b[i - 1], b' ' | b'\t' | b'[' | b',') => quote = Some(c),
            (None, b'#') if i > 0 && matches!(b[i - 1], b' ' | b'\t') => return v[..i].trim_end(),
            (None, _) => {}
        }
        i += 1;
    }
    v
}

/// The text of a quoted scalar: `''` is a quote in single quotes, backslash escapes work in
/// double quotes. `None` when `v` is not quoted.
fn unquote(v: &str) -> Option<String> {
    if v.len() < 2 {
        return None;
    }
    if v.starts_with('\'') && v.ends_with('\'') {
        return Some(v[1..v.len() - 1].replace("''", "'"));
    }
    if !(v.starts_with('"') && v.ends_with('"')) {
        return None;
    }
    let mut out = String::new();
    let mut chars = v[1..v.len() - 1].chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => {
                        out.push_str("\\u");
                        out.push_str(&hex);
                    }
                }
            }
            Some(other @ ('"' | '\\' | '/' | ' ')) => out.push(other),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    Some(out)
}

/// The items of a flow list's inside (`a, "b, c", [d]`): commas inside quotes or nested
/// brackets do not separate.
fn flow_items(inner: &str) -> Vec<&str> {
    let b = inner.as_bytes();
    let (mut items, mut start, mut depth, mut quote) = (Vec::new(), 0, 0i32, None::<u8>);
    let mut i = 0;
    while i < b.len() {
        match (quote, b[i]) {
            (Some(b'"'), b'\\') => i += 1,
            (Some(b'\''), b'\'') if b.get(i + 1) == Some(&b'\'') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, c @ (b'"' | b'\'')) if inner[start..i].trim().is_empty() => quote = Some(c),
            (None, b'[' | b'{') => depth += 1,
            (None, b']' | b'}') => depth -= 1,
            (None, b',') if depth == 0 => {
                items.push(&inner[start..i]);
                start = i + 1;
            }
            (None, _) => {}
        }
        i += 1;
    }
    items.push(&inner[start..]);
    items
}

fn scalar(v: &str) -> Value {
    let v = strip_comment(v);
    if v.is_empty() || v == "~" || v == "null" {
        return Value::Null;
    }
    if v == "true" {
        return Value::Bool(true);
    }
    if v == "false" {
        return Value::Bool(false);
    }
    if let Some(s) = unquote(v) {
        return Value::String(s);
    }
    if let Ok(i) = v.parse::<i64>() {
        return Value::Number(i.into());
    }
    if let Ok(f) = v.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Value::Number(n);
        }
    }
    if v.starts_with('[') && v.ends_with(']') {
        let inner = &v[1..v.len() - 1];
        if inner.trim().is_empty() {
            return Value::Array(vec![]);
        }
        return Value::Array(flow_items(inner).into_iter().map(scalar).collect());
    }
    Value::String(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn comments_after_values_are_not_part_of_them() {
        let v = parse("meeting_date: \"2025-11-[DD]\"  # UPDATE when the date is set\nstatus: draft # was review\nurl: https://x.test/#top\ntags: [a, b] # two\nempty: # nothing yet\n").unwrap();
        assert_eq!(v["meeting_date"], "2025-11-[DD]");
        assert_eq!(v["status"], "draft");
        assert_eq!(v["url"], "https://x.test/#top");
        assert_eq!(v["tags"], json!(["a", "b"]));
        assert_eq!(v["empty"], Value::Null);
        let list = parse("owners:\n  - ana # lead\n  - \"#1 fan\"\n").unwrap();
        assert_eq!(list["owners"], json!(["ana", "#1 fan"]));
    }

    #[test]
    fn quoted_strings_are_unescaped() {
        let v = parse("entity_name: '[''Account Name'']'\nsay: \"a \\\"quote\\\", a \\\\ and\\na newline\"\nplain: don't # mind\n").unwrap();
        assert_eq!(v["entity_name"], "['Account Name']");
        assert_eq!(v["say"], "a \"quote\", a \\ and\na newline");
        assert_eq!(v["plain"], "don't");
    }

    #[test]
    fn flow_lists_split_only_outside_quotes_and_brackets() {
        let v = parse("related: [\"[[Acme, Inc]]\", 'O''Brien', plain, 3, [x, y]]\n").unwrap();
        assert_eq!(v["related"], json!(["[[Acme, Inc]]", "O'Brien", "plain", 3, ["x", "y"]]));
    }
}
