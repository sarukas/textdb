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
            let item = lines[*idx].trim_start()[1..].trim();
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

fn split_kv(s: &str) -> Option<(&str, &str)> {
    if s.starts_with('"') || s.starts_with('\'') || s.starts_with('[') || s.starts_with('{') {
        return None;
    }
    let pos = s.find(": ").or_else(|| if s.ends_with(':') { Some(s.len() - 1) } else { None })?;
    let k = s[..pos].trim();
    let v = s[(pos + 1).min(s.len())..].trim();
    if k.is_empty() || k.contains(' ') && k.contains('"') {
        return None;
    }
    Some((k, v))
}

fn scalar(v: &str) -> Value {
    let v = v.trim();
    if v.is_empty() || v == "~" || v == "null" {
        return Value::Null;
    }
    if v == "true" {
        return Value::Bool(true);
    }
    if v == "false" {
        return Value::Bool(false);
    }
    if let Ok(i) = v.parse::<i64>() {
        return Value::Number(i.into());
    }
    if let Ok(f) = v.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Value::Number(n);
        }
    }
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2) || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2) {
        return Value::String(v[1..v.len() - 1].to_string());
    }
    if v.starts_with('[') && v.ends_with(']') {
        let inner = &v[1..v.len() - 1];
        if inner.trim().is_empty() {
            return Value::Array(vec![]);
        }
        return Value::Array(inner.split(',').map(scalar).collect());
    }
    Value::String(v.to_string())
}
