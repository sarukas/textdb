//! `textdb search` and `textdb grep`.
//!
//! The full-text index covers chunks of documents, so on its own it can say which documents
//! match but only guess at the line. `search` takes the documents the index finds, reads them,
//! and lists every line that actually holds one of the query's words, comparing words the way
//! the index does (case and accents aside); a document whose lines do not hold every word is
//! dropped. `grep` reads every file under a folder and matches a regular expression per line.

use textdb_sqlite::normalize_path;

use crate::store::{FileHit, Hit, Store, StoreError};
use crate::{emit_json, out, Result};








/// How a hit listing was asked for: every line, just the paths, or a count per path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Lines,
    /// `-l`: the documents that matched.
    Files,
    /// `-c`: the documents and how many lines matched in each.
    Count,
}

pub struct Options {
    pub mode: Mode,
    /// Rows returned, on both commands. It used to count documents on `search` and lines on
    /// `grep`.
    pub limit: usize,
    /// Most lines to list from any one document; the rest are reported as `more`.
    pub per_file: usize,
    pub ignore_case: bool,
    pub fixed: bool,
}

/// `-l` and `-c` collapse the rows to one per document without changing the row *type*:
/// `{path, version, matches}` either way, so a flag filters rows rather than switching the
/// shape of the JSON under the caller.
fn by_file(hits: &[Hit]) -> Vec<FileHit> {
    let mut out: Vec<FileHit> = Vec::new();
    for h in hits {
        match out.last_mut() {
            Some(f) if f.path == h.path => f.matches += 1,
            _ => out.push(FileHit {
                path: h.path.clone(),
                version: h.version,
                matches: 1,
            }),
        }
    }
    // `more` counts what `--per-file` held back, which a count mode should still include.
    for f in out.iter_mut() {
        if let Some(h) = hits.iter().find(|h| h.path == f.path) {
            f.matches += h.more;
        }
    }
    out
}

fn emit(hits: Vec<Hit>, o: &Options, what: &str, prefix: &str, json: bool) -> Result<()> {
    if o.mode != Mode::Lines {
        let files = by_file(&hits);
        if json {
            return emit_json(&files);
        }
        if files.is_empty() {
            eprintln!("no matches for {what} under {prefix}");
            return Ok(());
        }
        let text: String = match o.mode {
            Mode::Count => files.iter().map(|f| format!("{}:{}\n", f.path, f.matches)).collect(),
            _ => files.iter().map(|f| format!("{}\n", f.path)).collect(),
        };
        return out(text.as_bytes());
    }
    if json {
        return emit_json(&hits);
    }
    if hits.is_empty() {
        eprintln!("no matches for {what} under {prefix}");
        return Ok(());
    }
    let mut text = String::new();
    let mut last: Option<&str> = None;
    for h in &hits {
        text.push_str(&format!("{}:{}: {}\n", h.path, h.line, h.text));
        if h.more > 0 && last != Some(h.path.as_str()) {
            last = Some(&h.path);
        }
    }
    // One line per file that had more, after its rows, so the text says what the JSON does.
    for f in by_file(&hits) {
        if let Some(h) = hits.iter().find(|h| h.path == f.path) {
            if h.more > 0 {
                text.push_str(&format!("{}: {} more matching lines (--per-file)\n", f.path, h.more));
            }
        }
    }
    out(text.as_bytes())
}

/// `textdb search`: the full-text index, then the lines that really hold the terms.
pub fn search(st: &mut dyn Store, query: &str, prefix: &str, o: Options, json: bool) -> Result<()> {
    if textdb_core::terms::parse(textdb_sqlite::db::query_terms(query)).is_empty() {
        return Err(StoreError::invalid("nothing to search for: give at least one word"));
    }
    let prefix = normalize_path(prefix)?;
    let hits = st.search(query, &prefix, o.limit as i64, o.per_file as i64)?;
    emit(hits, &o, query, &prefix, json)
}

/// `textdb grep`: a regular expression over every file under a folder.
///
/// The rows are the same seven keys `search` returns, with `score` null — `grep` ranks
/// nothing — so a caller can treat the two alike.
pub fn grep(st: &mut dyn Store, pattern: &str, prefix: &str, o: Options, json: bool) -> Result<()> {
    let source = if o.fixed { regex::escape(pattern) } else { pattern.to_string() };
    let re = regex::RegexBuilder::new(&source)
        .case_insensitive(o.ignore_case)
        .build()
        .map_err(|e| StoreError::invalid(format!("not a valid regular expression: {e}")))?;
    let prefix = normalize_path(prefix)?;
    let mut paths: Vec<String> = st.file_heads(&prefix)?.into_iter().map(|h| h.path).collect();
    if paths.is_empty() && prefix != "/" && st.stat(&prefix).is_ok_and(|s| s.kind == "file") {
        paths.push(prefix.clone());
    }
    paths.sort();
    let mut hits: Vec<Hit> = Vec::new();
    for path in paths {
        if hits.len() >= o.limit {
            break;
        }
        let (body, version) = match st.read(&path, None) {
            Ok(x) => x,
            Err(e) if e.code == "TX003" => continue,
            Err(e) => return Err(e),
        };
        let body = String::from_utf8_lossy(&body);
        let found: Vec<(usize, &str)> =
            body.lines().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, l)| (i + 1, l)).collect();
        if found.is_empty() {
            continue;
        }
        let more = found.len().saturating_sub(o.per_file.max(1)) as i64;
        let sections = st.sections_of(&path).unwrap_or_default();
        for (n, line) in found.into_iter().take(o.per_file.max(1)) {
            if hits.len() >= o.limit {
                break;
            }
            hits.push(Hit {
                path: path.clone(),
                version,
                line: n as i64,
                text: textdb_core::terms::show(line, &[]),
                section: section_at(&sections, n as i64),
                score: None,
                more,
            });
        }
    }
    emit(hits, &o, pattern, &prefix, json)
}

/// The deepest heading span containing `line`.
fn section_at(spans: &[(i64, i64, String)], line: i64) -> Option<String> {
    spans
        .iter()
        .filter(|(from, to, _)| *from <= line && line <= *to)
        .next_back()
        .map(|(_, _, h)| h.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use textdb_core::terms::{matching_lines, parse, Term};

    /// The CLI's query parser feeding core's term reader: the two used to be one function
    /// living only here, which is why the SQL surfaces could not match lines at all.
    fn terms(q: &str) -> Vec<Term> {
        parse(textdb_sqlite::db::query_terms(q))
    }

    #[test]
    fn queries_become_words_prefixes_and_phrases() {
        assert_eq!(
            terms(r#"RFI tracker* "Response Plan" 2026-02 AND Café"#),
            vec![
                Term::Word("rfi".into()),
                Term::Prefix("tracker".into()),
                Term::Phrase(vec!["response".into(), "plan".into()]),
                Term::Phrase(vec!["2026".into(), "02".into()]),
                Term::Word("cafe".into()),
            ]
        );
    }

    #[test]
    fn lines_are_those_holding_a_word_and_every_word_must_occur() {
        let text = "---\nentity_name: RFI Response Tracker\n---\n# Tracker\n\n- 2026-02-11: renamed the trackers\n";
        let lines: Vec<usize> = matching_lines(&terms("rfi tracker"), text).unwrap().iter().map(|(n, _)| *n).collect();
        assert_eq!(lines, [2, 4]);
        assert_eq!(matching_lines(&terms("tracker*"), text).unwrap().len(), 3);
        assert_eq!(matching_lines(&terms("\"response tracker\""), text).unwrap()[0].0, 2);
        assert!(matching_lines(&terms("rfi changelog"), text).is_none());
        assert_eq!(matching_lines(&terms("café"), "a cafe\n").unwrap()[0].0, 1);
    }

    #[test]
    fn files_mode_collapses_lines_without_changing_the_row_type() {
        // `-l` and `-c` filter rows; they do not swap `[{…}]` for `["path"]` the way `grep -l`
        // used to, so one consumer handles every mode.
        let hit = |path: &str, line: i64, more: i64| Hit {
            path: path.into(),
            version: 1,
            line,
            text: "x".into(),
            section: None,
            score: None,
            more,
        };
        let files = by_file(&[hit("/a.md", 1, 0), hit("/a.md", 4, 0), hit("/b.md", 2, 0)]);
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].path.as_str(), files[0].matches), ("/a.md", 2));
        assert_eq!((files[1].path.as_str(), files[1].matches), ("/b.md", 1));
        // A count includes what `--per-file` held back, so it is the file's real total.
        let capped = by_file(&[hit("/c.md", 1, 7)]);
        assert_eq!(capped[0].matches, 8);
    }
}
