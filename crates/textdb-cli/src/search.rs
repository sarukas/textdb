//! `textdb search` and `textdb grep`.
//!
//! The full-text index covers chunks of documents, so on its own it can say which documents
//! match but only guess at the line. `search` takes the documents the index finds, reads them,
//! and lists every line that actually holds one of the query's words, comparing words the way
//! the index does (case and accents aside); a document whose lines do not hold every word is
//! dropped. `grep` reads every file under a folder and matches a regular expression per line.

use serde::Serialize;
use textdb_sqlite::normalize_path;
use unicode_normalization::UnicodeNormalization;

use crate::store::{Hit, Store, StoreError};
use crate::{emit_json, out, Result};

#[derive(Debug, Clone, PartialEq)]
enum Term {
    Word(String),
    Prefix(String),
    Phrase(Vec<String>),
}

/// Lower case without accents, as the index compares words.
fn fold(s: &str) -> String {
    s.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// The words of `s`, folded: runs of letters and digits.
fn words(s: &str) -> Vec<String> {
    fold(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// A query as `search` reads it: words ANDed, `"a phrase"`, `prefix*`. A term that is several
/// words once punctuation is dropped (`2026-02`) is a phrase, as it is to the index.
fn parse(query: &str) -> Vec<Term> {
    textdb_sqlite::db::query_terms(query)
        .into_iter()
        .filter_map(|t| {
            let (stem, prefix) = match t.strip_suffix('*') {
                Some(stem) => (stem.to_string(), true),
                None => (t, false),
            };
            let mut w = words(&stem);
            match (w.len(), prefix) {
                (0, _) => None,
                (1, true) => w.pop().map(Term::Prefix),
                (1, false) => w.pop().map(Term::Word),
                _ => Some(Term::Phrase(w)),
            }
        })
        .collect()
}

fn holds(term: &Term, line_words: &[String]) -> bool {
    match term {
        Term::Word(w) => line_words.iter().any(|x| x == w),
        Term::Prefix(p) => line_words.iter().any(|x| x.starts_with(p.as_str())),
        Term::Phrase(ws) => line_words.windows(ws.len()).any(|win| win == ws.as_slice()),
    }
}

/// The lines of `text` (1-based number, text) holding any term, or `None` when some term is on
/// no line at all.
fn matching_lines<'t>(terms: &[Term], text: &'t str) -> Option<Vec<(usize, &'t str)>> {
    let mut seen = vec![false; terms.len()];
    let mut lines = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let ws = words(line);
        let mut any = false;
        for (k, term) in terms.iter().enumerate() {
            if holds(term, &ws) {
                seen[k] = true;
                any = true;
            }
        }
        if any {
            lines.push((i + 1, line));
        }
    }
    seen.iter().all(|s| *s).then_some(lines)
}

fn cut(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

pub fn search(st: &mut dyn Store, query: &str, prefix: &str, limit: i64, per_file: usize, json: bool) -> Result<()> {
    let terms = parse(query);
    if terms.is_empty() {
        return Err(StoreError::invalid("nothing to search for: give at least one word"));
    }
    let mut hits = Vec::new();
    let mut text = String::new();
    let mut seen = std::collections::HashSet::new();
    for doc in st.search(query, prefix, limit)? {
        if !seen.insert(doc.path.clone()) {
            continue;
        }
        let body = match st.read(&doc.path, None) {
            Ok((body, _)) => body,
            // Deleted or moved since the index answered.
            Err(e) if e.code == "TX003" => continue,
            Err(e) => return Err(e),
        };
        let body = String::from_utf8_lossy(&body);
        let Some(lines) = matching_lines(&terms, &body) else {
            continue;
        };
        for (n, line) in lines.iter().take(per_file.max(1)) {
            text.push_str(&format!("{}:{}: {}\n", doc.path, n, cut(line, 200)));
            hits.push(Hit {
                path: doc.path.clone(),
                line: *n as i64,
                snippet: cut(line, 200),
                rank: doc.rank,
            });
        }
        if lines.len() > per_file.max(1) {
            text.push_str(&format!("{}: {} more matching lines (--per-file)\n", doc.path, lines.len() - per_file.max(1)));
        }
    }
    if json {
        return emit_json(&hits);
    }
    if hits.is_empty() {
        eprintln!("no matches for {query} under {prefix}");
        return Ok(());
    }
    out(text.as_bytes())
}

#[derive(Serialize)]
struct GrepHit {
    path: String,
    line: i64,
    text: String,
}

pub struct GrepOptions {
    pub ignore_case: bool,
    pub fixed: bool,
    pub files_only: bool,
    pub limit: usize,
}

pub fn grep(st: &mut dyn Store, pattern: &str, prefix: &str, o: GrepOptions, json: bool) -> Result<()> {
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
    let (mut hits, mut files, mut stopped) = (Vec::new(), Vec::new(), false);
    'files: for path in paths {
        let body = match st.read(&path, None) {
            Ok((body, _)) => body,
            Err(e) if e.code == "TX003" => continue,
            Err(e) => return Err(e),
        };
        let body = String::from_utf8_lossy(&body);
        let mut matched = false;
        for (i, line) in body.lines().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            matched = true;
            if o.files_only {
                break;
            }
            if hits.len() >= o.limit {
                stopped = true;
                break 'files;
            }
            hits.push(GrepHit {
                path: path.clone(),
                line: i as i64 + 1,
                text: cut(line, 300),
            });
        }
        if matched && o.files_only {
            files.push(path);
        }
    }
    if stopped {
        eprintln!("stopped after {} matching lines; narrow the folder with -p or raise --limit", o.limit);
    }
    if json {
        return if o.files_only { emit_json(&files) } else { emit_json(&hits) };
    }
    if hits.is_empty() && files.is_empty() {
        eprintln!("no matches for {pattern} under {prefix}");
        return Ok(());
    }
    let text: String = if o.files_only {
        files.iter().map(|f| format!("{f}\n")).collect()
    } else {
        hits.iter().map(|h| format!("{}:{}: {}\n", h.path, h.line, h.text)).collect()
    };
    out(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_become_words_prefixes_and_phrases() {
        assert_eq!(
            parse(r#"RFI tracker* "Response Plan" 2026-02 AND Café"#),
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
        let terms = parse("rfi tracker");
        let lines: Vec<usize> = matching_lines(&terms, text).unwrap().iter().map(|(n, _)| *n).collect();
        assert_eq!(lines, [2, 4]);
        assert_eq!(matching_lines(&parse("tracker*"), text).unwrap().len(), 3);
        assert_eq!(matching_lines(&parse("\"response tracker\""), text).unwrap()[0].0, 2);
        assert!(matching_lines(&parse("rfi changelog"), text).is_none());
        assert_eq!(matching_lines(&parse("café"), "a cafe\n").unwrap()[0].0, 1);
    }
}
