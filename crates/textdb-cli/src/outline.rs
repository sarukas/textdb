//! `textdb outline`: markdown headings, for one document or across a folder or the store.
//!
//! The plain output is indented by heading level, so a single file reads as the document's
//! table of contents, and a wider scope groups under the path it came from. `--json` gives
//! every column, including each document's own size and last change, so a caller does not
//! have to ask a second time per row.

use crate::store::Store;
use crate::{emit_json, out, Result};

#[allow(clippy::too_many_arguments)]
pub fn run(
    st: &mut dyn Store,
    path: &str,
    heading: Option<&str>,
    mode: &str,
    level: Option<i64>,
    names: bool,
    limit: i64,
    json: bool,
) -> Result<()> {
    if names {
        let rows = st.heading_names(path, heading.unwrap_or(""), limit)?;
        if json {
            return emit_json(&rows);
        }
        let mut text = String::new();
        for r in &rows {
            let plural = |n: i64, word: &str| if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") };
            text.push_str(&format!("{}  {}, {}\n", r.heading, plural(r.sections, "section"), plural(r.docs, "doc")));
        }
        return out(text.as_bytes());
    }

    let rows = st.outline(path, heading, mode, level, limit)?;
    if json {
        return emit_json(&rows);
    }
    let mut text = String::new();
    let mut current = String::new();
    // One document's outline needs no path headers; anything wider does.
    let grouped = rows.iter().any(|r| r.path != rows[0].path);
    for r in &rows {
        if grouped && r.path != current {
            if !current.is_empty() {
                text.push('\n');
            }
            current = r.path.clone();
            text.push_str(&format!("{}\n", r.path));
        }
        let indent = "  ".repeat((r.level.max(1) - 1) as usize);
        // The word count is the section's own; a parent that only holds subsections would
        // otherwise look empty, so its total is shown next to it when the two differ.
        let words = match (r.nwords, r.nwords_total) {
            (Some(w), Some(t)) if w != t => format!("  {w} words, {t} with subsections"),
            (Some(w), _) => format!("  {w} words"),
            _ => String::new(),
        };
        text.push_str(&format!("{}{}  L{}{}\n", indent, r.heading, r.line_from, words));
    }
    out(text.as_bytes())
}
