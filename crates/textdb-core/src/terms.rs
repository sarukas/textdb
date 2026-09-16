//! Reading a search query as terms, and finding the lines of a document that hold them.
//!
//! One implementation, used by the store to build hit rows and by the CLI to render them, so
//! `line` means the same thing on every surface. It used to live only in the CLI: the SQL
//! functions, and therefore the SDKs and the HTTP API, returned one row per *document* with
//! the best-ranked chunk's best line, so the same column name was a fact on one surface and a
//! guess on three.

use crate::fold::fold;

/// One term of a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Word(String),
    /// `prefix*` — matches a word that starts with it.
    Prefix(String),
    /// Several words that must appear next to each other, in order.
    Phrase(Vec<String>),
}

/// The words of `s`, folded the way the index folds them: runs of letters and digits.
pub fn words(s: &str) -> Vec<String> {
    fold(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// A query as terms: words ANDed, `"a phrase"`, `prefix*`.
///
/// A term that is several words once punctuation is dropped (`2026-02`) is a phrase, as it is
/// to the index. `raw` is the term list the binding's own parser produced, so the two agree on
/// quoting.
pub fn parse(raw: Vec<String>) -> Vec<Term> {
    raw.into_iter()
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

/// The lines of `text` (1-based number, the line) that hold any term, or `None` when some term
/// appears on no line at all.
///
/// `None` is what keeps document-level AND from leaking into the rows: the index says a
/// document holds every term somewhere, and this says whether any *line* does. A document
/// where the words only ever appear apart is dropped rather than reported with a line that
/// holds none of them.
pub fn matching_lines<'t>(terms: &[Term], text: &'t str) -> Option<Vec<(usize, &'t str)>> {
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

/// How much of a line to show. One length for every surface: `search` cut at 200 and `grep`
/// at 300 for the same text under two key names.
pub const LINE_CUT: usize = 300;

/// How much context to keep before the match when a line has to be windowed.
const LEAD: usize = 60;

/// The line as a hit should show it: itself when it fits, else a window around the match.
///
/// A blind prefix would drop the match off the end of any line longer than the cut, which is
/// most lines in a real document. `terms` may be empty — `grep` knows the line matched but
/// not which word — in which case the head of the line is shown.
pub fn show(line: &str, terms: &[Term]) -> String {
    if line.chars().count() <= LINE_CUT {
        return line.to_string();
    }
    let folded = fold(line);
    let at = terms
        .iter()
        .flat_map(|t| match t {
            Term::Word(w) | Term::Prefix(w) => vec![w.clone()],
            Term::Phrase(ws) => ws.clone(),
        })
        .filter_map(|w| folded.find(&w))
        // `fold` is one character in, one out, so a byte offset counts the same characters.
        .map(|b| folded[..b].chars().count())
        .min()
        .unwrap_or(0);
    crate::snippet::window(line, at, LINE_CUT, LEAD)
}

/// `s` cut to `LINE_CUT` characters, for text that has no match to centre on.
pub fn cut(s: &str) -> String {
    s.chars().take(LINE_CUT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(q: &[&str]) -> Vec<Term> {
        parse(q.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn a_term_is_a_word_a_prefix_or_a_phrase() {
        assert_eq!(t(&["rate"]), vec![Term::Word("rate".into())]);
        assert_eq!(t(&["rat*"]), vec![Term::Prefix("rat".into())]);
        // Punctuation makes a phrase, because that is what the index sees too.
        assert_eq!(t(&["2026-02"]), vec![Term::Phrase(vec!["2026".into(), "02".into()])]);
        assert_eq!(t(&["rate limit"]), vec![Term::Phrase(vec!["rate".into(), "limit".into()])]);
    }

    #[test]
    fn every_term_must_appear_on_some_line() {
        let doc = "rate is here\nlimit is there\n";
        // Both words appear, on different lines: both lines are returned.
        let got = matching_lines(&t(&["rate", "limit"]), doc).unwrap();
        assert_eq!(got.len(), 2);
        // A term on no line at all drops the document rather than reporting a wrong line.
        assert!(matching_lines(&t(&["rate", "absent"]), doc).is_none());
    }

    #[test]
    fn matching_folds_the_way_the_index_does() {
        let doc = "the façade is here\n";
        assert_eq!(matching_lines(&t(&["facade"]), doc).unwrap().len(), 1);
        assert_eq!(matching_lines(&t(&["FAÇADE"]), doc).unwrap().len(), 1);
    }

    #[test]
    fn a_phrase_needs_its_words_adjacent_and_in_order() {
        assert!(matching_lines(&t(&["rate limit"]), "rate limit here\n").is_some());
        assert!(matching_lines(&t(&["rate limit"]), "limit rate here\n").is_none());
        assert!(matching_lines(&t(&["rate limit"]), "rate then limit\n").is_none());
    }
}
