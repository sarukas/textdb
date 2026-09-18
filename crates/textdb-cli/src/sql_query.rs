//! `textdb sql`: one SQL statement against the store, printed as a table, TSV, one value per
//! line or JSON; and `textdb revert-batch`, which undoes what one writing statement changed.

use serde_json::{json, Value};

use crate::store::{RevertOutcome, SqlResult, Store, StoreError};
use crate::{emit_json, out, Result};

/// Longest cell in the table output, in characters, unless `--full`.
const CELL_MAX: usize = 60;

/// Postgres tables behind `kb`; its views (`kb.file`, `kb.entry`, …) and functions stay usable.
const PG_INTERNAL: &[&str] = &[
    "node", "commit", "change", "chunk", "tree_node", "chunk_ref", "section", "link", "frontmatter", "checkpoint", "path_event",
    "setting", "file_author", "folder_delta", "sync", "sync_file",
];

/// How `sql` prints rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SqlFormat {
    /// Padded columns; values cut at 60 characters unless --full.
    Table,
    /// A header line, then one tab-separated line per row; tabs, newlines and backslashes
    /// escaped as \t, \n and \\; NULL empty.
    Tsv,
    /// One value per line, for a single column (escaped as in tsv).
    Lines,
    /// `{ columns, rows, row_count, store_changes, batch }`, as --json.
    Json,
}

pub struct SqlOptions {
    pub write: bool,
    pub dry_run: bool,
    pub full: bool,
    pub format: SqlFormat,
}

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

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// A table cell and whether it was cut.
fn cell(v: &Value, full: bool) -> (String, bool) {
    let s = text_of(v).unwrap_or_else(|| "NULL".to_string());
    let s = s.replace("\r\n", "\\n").replace('\n', "\\n").replace('\t', " ");
    if !full && s.chars().count() > CELL_MAX {
        (format!("{}…", s.chars().take(CELL_MAX - 1).collect::<String>()), true)
    } else {
        (s, false)
    }
}

/// A value for tsv and lines: NULL empty; backslash, tab and line breaks escaped.
fn escaped(v: &Value) -> String {
    text_of(v)
        .unwrap_or_default()
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// Columns padded to their widest value, then `(N rows)`, saying so when values were cut.
pub fn render(r: &SqlResult, full: bool) -> String {
    let mut s = String::new();
    if r.columns.is_empty() {
        return s;
    }
    let mut cut = false;
    let cells: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| {
                    let (text, was_cut) = cell(v, full);
                    cut |= was_cut;
                    text
                })
                .collect()
        })
        .collect();
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
    let n = r.rows.len();
    let rows = format!("{n} {}", if n == 1 { "row" } else { "rows" });
    if cut {
        s.push_str(&format!("({rows}; values cut at {CELL_MAX} characters: --full or --format tsv shows them whole)\n"));
    } else {
        s.push_str(&format!("({rows})\n"));
    }
    s
}

pub fn tsv(r: &SqlResult) -> String {
    let mut s = String::new();
    if r.columns.is_empty() {
        return s;
    }
    s.push_str(&r.columns.join("\t"));
    s.push('\n');
    for row in &r.rows {
        s.push_str(&row.iter().map(escaped).collect::<Vec<_>>().join("\t"));
        s.push('\n');
    }
    s
}

fn lines(r: &SqlResult) -> Result<String> {
    if r.columns.len() > 1 {
        return Err(StoreError::invalid(format!(
            "--format lines prints one column, and the statement returns {}: select one, or use --format tsv",
            r.columns.len()
        )));
    }
    Ok(r.rows.iter().map(|row| format!("{}\n", row.first().map(escaped).unwrap_or_default())).collect())
}

/// What a writing statement changed: the count and batch, or for a dry run each change and diff.
fn summary(r: &SqlResult) -> String {
    let mut s = String::new();
    let Some(n) = r.store_changes else { return s };
    let changes = format!("{n} {}", if n == 1 { "change" } else { "changes" });
    if r.dry_run {
        s.push_str(&format!("dry run: {changes}, all undone — nothing was written\n"));
        for c in &r.changes {
            match (c.op.as_str(), &c.old_path) {
                ("move", Some(old)) => s.push_str(&format!("move {old} -> {}\n", c.path)),
                (op, _) => match (c.from_version, c.to_version) {
                    (Some(from), Some(to)) => s.push_str(&format!("{op} {} (v{from} -> v{to})\n", c.path)),
                    _ => s.push_str(&format!("{op} {}\n", c.path)),
                },
            }
            if let Some(diff) = &c.diff {
                s.push_str(diff);
                if !diff.ends_with('\n') {
                    s.push('\n');
                }
            }
        }
        return s;
    }
    s.push_str(&format!("{changes} to the store\n"));
    if let Some(batch) = &r.batch {
        s.push_str(&format!("batch {batch}: `textdb revert-batch {batch}` undoes it\n"));
    }
    s
}

pub fn run(st: &mut dyn Store, query: &str, params: &[String], author: Option<&str>, o: SqlOptions) -> Result<()> {
    let query = query.trim().trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    if query.is_empty() {
        return Err(StoreError::invalid("no SQL statement given"));
    }
    if o.write {
        if let Some(table) = internal_table(query, st.backend() == "sqlite") {
            return Err(StoreError::invalid(format!(
                "--write statements cannot name the internal table {table}: change the store through kb and the textdb \
                 functions, and read through the views files, folders, frontmatter, properties, sections, links, commits and authors"
            )));
        }
    }
    let result = st.sql(query, params, author, o.write, o.dry_run)?;
    match o.format {
        SqlFormat::Json => {
            let rows: Vec<serde_json::Map<String, Value>> = result
                .rows
                .iter()
                .map(|row| result.columns.iter().cloned().zip(row.iter().cloned()).collect())
                .collect();
            let mut obj = json!({
                "columns": result.columns,
                "rows": rows,
                "row_count": result.rows.len(),
                "store_changes": result.store_changes,
                "batch": result.batch,
            });
            if result.dry_run {
                obj["dry_run"] = json!(true);
                obj["changes"] = json!(result.changes);
            }
            emit_json(&obj)
        }
        SqlFormat::Table => out(format!("{}{}", render(&result, o.full), summary(&result)).as_bytes()),
        SqlFormat::Tsv | SqlFormat::Lines => {
            let body = if o.format == SqlFormat::Tsv { tsv(&result) } else { lines(&result)? };
            out(body.as_bytes())?;
            // Rows alone on stdout, so a pipeline reads only them.
            eprint!("{}", summary(&result));
            Ok(())
        }
    }
}

pub fn revert_text(r: &RevertOutcome) -> String {
    let mut s = match (&r.revert_batch, r.dry_run) {
        (_, true) => format!("dry run: reverting batch {} would change this, and nothing was written\n", r.batch),
        (Some(id), false) => format!("reverted batch {} (recorded as batch {id}, which revert-batch can undo in turn)\n", r.batch),
        (None, false) => format!("reverted batch {}\n", r.batch),
    };
    for f in &r.restored {
        s.push_str(&format!("restored {} (now v{})\n", f.path, f.version));
    }
    for path in &r.removed {
        s.push_str(&format!("removed {path} (the batch created it)\n"));
    }
    for m in &r.moved_back {
        s.push_str(&format!("moved back {} -> {}\n", m.from, m.to));
    }
    for path in &r.recreated {
        s.push_str(&format!("recreated {path} (a new file; the deleted one keeps its history in the trash)\n"));
    }
    for why in &r.skipped {
        s.push_str(&format!("skipped: {why}\n"));
    }
    s
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
            ..SqlResult::default()
        };
        let text = render(&r, false);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("path{}  n", " ".repeat(56)));
        assert!(lines[3].ends_with("…  NULL") && lines[3].chars().count() == 66, "{text}");
        assert!(lines[4].starts_with("(2 rows; values cut at 60 characters"), "{text}");
        assert!(render(&r, true).contains(&"x".repeat(80)));
        assert!(render(&r, true).ends_with("(2 rows)\n"));
    }

    #[test]
    fn tsv_escapes_and_keeps_whole_values() {
        let r = SqlResult {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec![json!("x\ty\nz\\"), Value::Null], vec![json!(3), json!("y".repeat(80))]],
            ..SqlResult::default()
        };
        assert_eq!(tsv(&r), format!("a\tb\nx\\ty\\nz\\\\\t\n3\t{}\n", "y".repeat(80)));
        assert!(lines(&r).is_err());
    }
}
