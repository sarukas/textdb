//! Choosing what to show for a search hit: which line matched, and a window of it.
//!
//! One implementation for both bindings. Two copies of this logic drifted apart once already,
//! and the ways they were wrong were invisible until the benchmark started checking snippets
//! against the query — a search that finds the right document and shows the wrong line reads
//! as broken and measures as fast.
/// The line of `bytes` that best matches `terms`, and a snippet window around the match.
///
/// Three things this has to get right, each of which it used to get wrong:
///
/// * **Folding.** The index tokenises with fts5's `unicode61`, which strips diacritics, so a
///   chunk holding `façade` is indexed under `facade`. Comparing raw text found nothing for
///   such a hit and fell through to the top of the chunk. Both sides go through
///   `textdb_core::fold`.
/// * **Phrases.** `query_terms` keeps a quoted phrase as one term with the spaces in it, so
///   `"draft false"` was looked for literally and never matched `draft: false`. A term is
///   matched word by word instead, which is also what makes the document-level AND the rest
///   of search uses consistent with what it shows.
/// * **Where the window sits.** The snippet used to be the first 200 characters of the line,
///   so on a long line — which is most lines in a real document — the match was off the end
///   and the user read text that had nothing to do with the query. It is now a window around
///   the match, elided at whichever end was cut.
pub fn locate_terms(bytes: &[u8], terms: &[String]) -> (usize, String) {
    /// Characters shown. Enough to read, short enough for a list of results.
    const WIDTH: usize = 200;
    /// How much of the window sits before the match, so it reads with a little lead-in.
    const LEAD: usize = 60;

    let raw = String::from_utf8_lossy(bytes);
    // Each query term becomes the words it needs; a phrase needs all of its own.
    let wanted: Vec<Vec<String>> = terms
        .iter()
        .map(|t| {
            t.split_whitespace()
                .map(|w| crate::fold::fold(w.trim_end_matches('*')))
                .filter(|w| !w.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|ws: &Vec<String>| !ws.is_empty())
        .collect();
    if wanted.is_empty() {
        return (0, raw.lines().next().unwrap_or("").chars().take(WIDTH).collect());
    }

    // Scored on two counts, in that order: how many whole terms the line satisfies, then how
    // many individual words it holds. The second is what keeps a phrase whose words fall on
    // different lines from scoring zero everywhere and landing on the top of the chunk — the
    // store's AND is document-level, so a line with one of the words is the honest thing to
    // show when no line has both.
    let (mut best_score, mut best_line, mut best_at) = ((0usize, 0usize), 0usize, 0usize);
    for (i, line) in raw.lines().enumerate() {
        let folded = crate::fold::fold(line);
        let n = wanted.iter().filter(|ws| ws.iter().all(|w| folded.contains(w.as_str()))).count();
        let words = wanted.iter().flatten().filter(|w| folded.contains(w.as_str())).count();
        if (n, words) > best_score {
            // Where to centre the window: the earliest word of any term that this line has.
            let at = wanted
                .iter()
                .flatten()
                .filter_map(|w| folded.find(w.as_str()))
                .min()
                .unwrap_or(0);
            // `fold` is one character in, one character out, so a byte offset in the folded
            // line counts the same number of characters as it does in the original.
            (best_score, best_line, best_at) = ((n, words), i, folded[..at].chars().count());
            if n == wanted.len() {
                break;
            }
        }
    }

    let line = raw.lines().nth(best_line).unwrap_or("");
    let total = line.chars().count();
    let snippet = if total <= WIDTH {
        line.to_string()
    } else {
        let from = best_at.saturating_sub(LEAD).min(total.saturating_sub(WIDTH));
        let body: String = line.chars().skip(from).take(WIDTH).collect();
        let head = if from > 0 { "…" } else { "" };
        let tail = if from + WIDTH < total { "…" } else { "" };
        format!("{head}{body}{tail}")
    };
    (best_line, snippet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(body: &str, terms: &[&str]) -> (usize, String) {
        locate_terms(body.as_bytes(), &terms.iter().map(|t| t.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn finds_the_line_that_matched_through_the_index_fold() {
        // The index strips diacritics, so `facade` is how `façade` is found. Comparing raw
        // text here used to fall through to line 0.
        let body = "# Doc\n\nfiller\nthe word façade is here\n";
        assert_eq!(at(body, &["facade"]), (3, "the word façade is here".into()));
        assert_eq!(at(body, &["façade"]), (3, "the word façade is here".into()));
    }

    #[test]
    fn a_quoted_phrase_is_matched_word_by_word() {
        // `query_terms` keeps a phrase as one term with its spaces, and the words are rarely
        // adjacent in the text: `draft: false` has a colon between them.
        let body = "---\ntitle: T\ndraft: false\n---\n";
        assert_eq!(at(body, &["draft false"]), (2, "draft: false".into()));
    }

    #[test]
    fn a_phrase_split_across_lines_still_lands_on_a_word() {
        // No line holds both, so the fallback picks a line holding one rather than line 0.
        let body = "nothing\nalpha lives here\nand beta lives here\n";
        let (line, snippet) = at(body, &["alpha beta"]);
        assert!(line == 1 || line == 2, "line {line}");
        assert!(snippet.contains("alpha") || snippet.contains("beta"), "{snippet:?}");
    }

    #[test]
    fn the_window_follows_the_match_down_a_long_line() {
        let pad = "filler word here ".repeat(40);
        let body = format!("head\n{pad}NEEDLE{pad}\n");
        let (line, snippet) = at(&body, &["needle"]);
        assert_eq!(line, 1);
        assert!(snippet.contains("NEEDLE"), "{snippet:?}");
        assert!(snippet.starts_with('…') && snippet.ends_with('…'), "{snippet:?}");
        // Bounded: a result list shows many of these.
        assert!(snippet.chars().count() <= 202, "{} chars", snippet.chars().count());
    }

    #[test]
    fn a_line_that_fits_is_shown_whole() {
        assert_eq!(at("alpha\nbeta gamma\n", &["gamma"]), (1, "beta gamma".into()));
    }

    #[test]
    fn a_match_near_the_start_of_a_long_line_keeps_its_head() {
        let pad = "tail word ".repeat(60);
        let body = format!("NEEDLE at the very start {pad}\n");
        let (_, snippet) = at(&body, &["needle"]);
        assert!(snippet.starts_with("NEEDLE"), "{snippet:?}");
        assert!(snippet.ends_with('…'), "{snippet:?}");
    }

    #[test]
    fn nothing_to_match_is_not_a_panic() {
        assert_eq!(at("", &["x"]), (0, String::new()));
        assert_eq!(at("only line\n", &[]).0, 0);
        // A prefix term is matched without its star.
        assert_eq!(at("alpha\nbetaxyz here\n", &["betax*"]).0, 1);
    }
}
