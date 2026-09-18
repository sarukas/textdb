//! Okapi BM25, once, for both bindings.
//!
//! The two engines rank with different functions and always have: SQLite's FTS5 exposes `bm25()`
//! as its `rank` column, Postgres has `ts_rank`, which has no term saturation, no length
//! normalisation and no inverse document frequency at all. Normalising both into `(0, 1]` hid the
//! sign difference from callers but not the fact that the *order* differed — the same query
//! against the same documents came back in a different order depending on which engine answered.
//! For a store whose whole claim is one algorithm over two persistences, that is the wrong kind
//! of difference.
//!
//! So neither engine's ranker decides the order any more. Each is used for what it is good at —
//! finding the documents that match — and the order is decided here.
//!
//! ## What it scores, and on what
//!
//! **Documents, not chunks.** The index is over chunks, so both engines' own rankers compute
//! their statistics over chunks of about a kilobyte: a document's length never enters it, and a
//! term's rarity is measured against chunks rather than documents. This scores the document, from
//! the body the caller is already holding — `resolve_hits` materialises it to find the matching
//! lines, so the text is in hand and the term frequency costs a pass over words that were about
//! to be walked anyway.
//!
//! **Every input is free.** `ndocs` and `total_words` are the folder totals the listing surfaces
//! already carry on every folder row, so the corpus statistics are one indexed lookup rather than
//! an aggregate over the store. `df` comes out of the retrieval query itself — a window count
//! over the matched set, computed before the limit truncates it, so it is the true number of
//! matching documents and not the size of the pool that survived.
//!
//! ## Reranking, not retrieval
//!
//! This orders a pool the engine has already chosen. That is the ordinary shape of a search
//! stack — a cheap retriever, then a good ranker over its best few hundred — and it is what keeps
//! the cost of scoring independent of the size of the store. It also means a document the
//! retriever never returned cannot be rescued here, which is why the pool is `limit * 50` and not
//! `limit`.

use crate::terms::{words, Term};

/// The classic Robertson/Sparck Jones constants, and the ones FTS5 fixes: `k1` sets how quickly a
/// repeated term stops adding, `b` how much a long document is discounted for its length.
pub const K1: f64 = 1.2;
pub const B: f64 = 0.75;

/// What the scored documents are being compared against.
#[derive(Clone, Copy, Debug)]
pub struct Corpus {
    /// Live documents in the searched scope — a folder's `files` total, or the store's.
    pub ndocs: u64,
    /// Words across all of them, for the mean length `b` normalises against.
    pub total_words: u64,
}

impl Corpus {
    /// Mean document length in words. One word for an empty corpus, so the ratio below is finite
    /// and a store with nothing in it scores rather than divides by zero.
    pub fn avg_len(&self) -> f64 {
        if self.ndocs == 0 {
            return 1.0;
        }
        (self.total_words as f64 / self.ndocs as f64).max(1.0)
    }

    /// Inverse document frequency, in the form that cannot go negative.
    ///
    /// The textbook `ln((N - df + 0.5) / (df + 0.5))` turns negative once a term is in more than
    /// half the corpus, which makes a document score *worse* for containing it and can push a
    /// total below zero. Lucene's `ln(1 + …)` is the same curve with that floor removed, and it
    /// is what every modern implementation uses.
    pub fn idf(&self, df: u64) -> f64 {
        let n = self.ndocs.max(df) as f64;
        let df = df as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }
}

/// One term of the query, with how many documents in the scope hold it.
#[derive(Clone, Debug)]
pub struct TermDf<'a> {
    pub term: &'a Term,
    pub df: u64,
}

/// BM25 for one document, summed over the query's terms.
///
/// `body` is the document's text. Term frequency is counted the way the index tokenises — folded
/// runs of letters and digits — so what is counted here and what the index matched are the same
/// words, diacritics and all.
pub fn score(terms: &[TermDf<'_>], body: &str, corpus: &Corpus) -> f64 {
    let doc = words(body);
    if doc.is_empty() {
        return 0.0;
    }
    let norm = K1 * (1.0 - B + B * (doc.len() as f64 / corpus.avg_len()));
    terms
        .iter()
        .map(|t| {
            let tf = frequency(t.term, &doc) as f64;
            if tf == 0.0 {
                return 0.0;
            }
            corpus.idf(t.df) * (tf * (K1 + 1.0)) / (tf + norm)
        })
        .sum()
}

/// How often a term occurs in an already-tokenised document.
///
/// A phrase counts its occurrences as a phrase, not its words separately: `"churn prediction"`
/// twice is two, not four, which is what makes a document that happens to use both words all over
/// score below one that uses the phrase.
fn frequency(term: &Term, doc: &[String]) -> usize {
    match term {
        Term::Word(w) => doc.iter().filter(|x| *x == w).count(),
        Term::Prefix(p) => doc.iter().filter(|x| x.starts_with(p.as_str())).count(),
        Term::Phrase(ws) if !ws.is_empty() => doc.windows(ws.len()).filter(|win| *win == ws.as_slice()).count(),
        Term::Phrase(_) => 0,
    }
}

/// Scale a set of scores into `(0, 1]` against the best of them.
///
/// Callers have always been given a relative number rather than a raw one, because the raw one
/// meant different things on the two engines. It still is relative — BM25 has no natural upper
/// bound — but now it is relative to the same function on both.
pub fn normalise(scores: &mut [f64]) {
    let best = scores.iter().copied().fold(f64::MIN, f64::max);
    if best <= 0.0 {
        return;
    }
    for s in scores.iter_mut() {
        *s = (*s / best).clamp(0.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terms::parse;

    fn t(q: &str, df: u64) -> (Vec<Term>, u64) {
        (parse(vec![q.to_string()]), df)
    }

    #[test]
    fn a_rarer_term_is_worth_more_than_a_common_one() {
        let c = Corpus { ndocs: 1000, total_words: 100_000 };
        assert!(c.idf(1) > c.idf(500), "{} vs {}", c.idf(1), c.idf(500));
    }

    #[test]
    fn idf_never_goes_negative_for_a_term_in_most_of_the_corpus() {
        let c = Corpus { ndocs: 1000, total_words: 100_000 };
        assert!(c.idf(999) > 0.0, "{}", c.idf(999));
        assert!(c.idf(1000) > 0.0, "{}", c.idf(1000));
    }

    #[test]
    fn a_repeated_term_saturates_rather_than_adding_up() {
        let c = Corpus { ndocs: 1000, total_words: 100_000 };
        let (terms, df) = t("churn", 10);
        let td = [TermDf { term: &terms[0], df }];
        let one = score(&td, "churn", &c);
        let ten = score(&td, "churn churn churn churn churn churn churn churn churn churn", &c);
        assert!(ten > one, "more occurrences score higher");
        assert!(ten < one * 10.0, "but not ten times higher: {ten} vs {one}");
    }

    #[test]
    fn a_longer_document_is_discounted_for_its_length() {
        let c = Corpus { ndocs: 1000, total_words: 10_000 };
        let (terms, df) = t("churn", 10);
        let td = [TermDf { term: &terms[0], df }];
        let short = score(&td, "churn prediction", &c);
        let long = score(&td, &format!("churn {}", "filler ".repeat(500)), &c);
        assert!(short > long, "{short} vs {long}");
    }

    #[test]
    fn a_phrase_counts_as_a_phrase() {
        let c = Corpus { ndocs: 100, total_words: 10_000 };
        let terms = parse(vec!["churn prediction".into()]);
        let td = [TermDf { term: &terms[0], df: 5 }];
        assert_eq!(frequency(&terms[0], &words("churn prediction and churn prediction")), 2);
        assert_eq!(frequency(&terms[0], &words("churn is churn and prediction is prediction")), 0);
        assert!(score(&td, "churn prediction", &c) > 0.0);
    }

    #[test]
    fn a_document_without_the_term_scores_nothing() {
        let c = Corpus { ndocs: 100, total_words: 10_000 };
        let (terms, df) = t("churn", 5);
        assert_eq!(score(&[TermDf { term: &terms[0], df }], "something else entirely", &c), 0.0);
    }

    #[test]
    fn normalise_puts_the_best_at_one_and_leaves_order_alone() {
        let mut s = vec![2.0, 8.0, 4.0];
        normalise(&mut s);
        assert_eq!(s[1], 1.0);
        assert!(s[0] < s[2] && s[2] < s[1]);
    }
}
