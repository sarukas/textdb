//! `textdb sql`: one SQL statement against the store, printed as a table or JSON.

use serde_json::{json, Value};

use crate::store::{SqlResult, Store, StoreError};
use crate::{emit_json, out, Result};

/// Longest cell in the table output, in characters, unless `--full`.
const CELL_MAX: usize = 60;

/// Postgres tables behind `kb`; its views (`kb.file`, `kb.entry`, …) and functions stay usable.
const PG_INTERNAL: &[&str] = &[
    "node", "commit", "change", "chunk", "tree_node", "chunk_ref", "section", "link", "frontmatter", "checkpoint", "path_event",
    "setting", "file_author", "folder_delta", "sync", "sync_file",
];

/// An internal table the statement names, if any. A `--write` statement must change the store
/// through `kb` and the textdb functions, which keep versions, totals and history consistent.
pub fn internal_table(query: &str, sqlite: bool) -> Option<String> {
    let lower = query.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let needle = if sqlite { "kb_" } else { "kb." };
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut at = 0;
    while let Some(i) = lower[at..].find(needle) {
        let start = at + i;
        at = start + needle.len();
        if start > 0 && ident(bytes[start - 1]) {
            continue;
        }
        let end = lower[at..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(lower.len(), |j| at + j);
        let name = &lower[at..end];
        if (sqlite && !name.is_empty()) || (!sqlite && PG_INTERNAL.contains(&name)) {
            return Some(format!("{needle}{name}"));
        }
    }
    None
}

fn cell(v: &Value, full: bool) -> String {
    let s = match v {
        Value::Null => "NULL".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let s = s.replace("\r\n", "\\n").replace('\n', "\\n").replace('\t', " ");
    if !full && s.chars().count() > CELL_MAX {
        format!("{}…", s.chars().take(CELL_MAX - 1).collect::<String>())
    } else {
        s
    }
}

/// Columns padded to their widest value, then `(N rows)`.
pub fn render(r: &SqlResult, full: bool) -> String {
    let mut s = String::new();
    if !r.columns.is_empty() {
        let cells: Vec<Vec<String>> = r.rows.iter().map(|row| row.iter().map(|v| cell(v, full)).collect()).collect();
        let widths: Vec<usize> = r
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| cells.iter().map(|row| row[i].chars().count()).chain([c.chars().count()]).max().unwrap_or(0))
            .collect();
        let line = |values: &[String]| {
            let padded: Vec<String> = values
                .iter()
                .zip(&widths)
                .map(|(v, w)| format!("{v}{}", " ".repeat(w.saturating_sub(v.chars().count()))))
                .collect();
            format!("{}\n", padded.join("  ").trim_end())
        };
        s.push_str(&line(&r.columns));
        s.push_str(&line(&widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>()));
        for row in &cells {
            s.push_str(&line(row));
        }
        s.push_str(&format!("({} {})\n", r.rows.len(), if r.rows.len() == 1 { "row" } else { "rows" }));
    }
    if let Some(n) = r.store_changes {
        s.push_str(&format!("{n} {} to the store\n", if n == 1 { "change" } else { "changes" }));
    }
    s
}

pub fn run(st: &mut dyn Store, query: &str, params: &[String], author: Option<&str>, write: bool, full: bool, json: bool) -> Result<()> {
    let query = query.trim().trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    if query.is_empty() {
        return Err(StoreError::invalid("no SQL statement given"));
    }
    if write {
        if let Some(table) = internal_table(query, st.backend() == "sqlite") {
            return Err(StoreError::invalid(format!(
                "--write statements cannot name the internal table {table}: change the store through kb and the textdb \
                 functions, and read through the views files, folders, frontmatter, sections, links, commits and authors"
            )));
        }
    }
    let result = st.sql(query, params, author, write)?;
    if json {
        let rows: Vec<serde_json::Map<String, Value>> = result
            .rows
            .iter()
            .map(|row| result.columns.iter().cloned().zip(row.iter().cloned()).collect())
            .collect();
        return emit_json(&json!({
            "columns": result.columns,
            "rows": rows,
            "row_count": result.rows.len(),
            "store_changes": result.store_changes,
        }));
    }
    out(render(&result, full).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_may_not_name_internal_tables() {
        assert_eq!(internal_table("DELETE FROM kb_node", true).as_deref(), Some("kb_node"));
        assert_eq!(internal_table("update \"KB_COMMIT\" set x = 1", true).as_deref(), Some("kb_commit"));
        assert_eq!(internal_table("SELECT textdb_edit(path, 'a', 'b', :author) FROM files", true), None);
        assert_eq!(internal_table("UPDATE kb SET content = 'x' WHERE path = '/a.md'", true), None);
        assert_eq!(internal_table("SELECT kb.edit(f, 'a', 'b') FROM kb.file f", false), None);
        assert_eq!(internal_table("delete from kb.node", false).as_deref(), Some("kb.node"));
    }

    #[test]
    fn tables_pad_columns_and_cut_long_values() {
        let r = SqlResult {
            columns: vec!["path".into(), "n".into()],
            rows: vec![vec![json!("/a.md"), json!(12)], vec![json!("x".repeat(80)), Value::Null]],
            store_changes: None,
        };
        let text = render(&r, false);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("path{}  n", " ".repeat(56)));
        assert!(lines[3].ends_with("…  NULL") && lines[3].chars().count() == 66, "{text}");
        assert_eq!(lines[4], "(2 rows)");
        assert!(render(&r, true).contains(&"x".repeat(80)));
    }
}
