//! Word counts as `wc -w` counts them: maximal runs of bytes that are not ASCII whitespace.
//! Markdown syntax is text like any other, so `## Setup` is two words.
//!
//! A word never spans a newline, so what a commit adds and removes can be counted over the
//! lines it changed alone: everything outside those lines is the same bytes on both sides.
//! That keeps a file's count current at the cost of the edit, not of the document.

use crate::diff::changed_line_regions;
use crate::hash::Hash;
use crate::storage::{Result, Storage};
use crate::tree::{leaves, materialize_range};

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// A running word count over bytes fed in pieces; a word split across two pieces counts once.
#[derive(Clone, Copy, Debug, Default)]
pub struct WordCounter {
    words: u64,
    in_word: bool,
}

impl WordCounter {
    pub fn feed(&mut self, bytes: &[u8]) -> &mut Self {
        for &b in bytes {
            if is_space(b) {
                self.in_word = false;
            } else if !self.in_word {
                self.in_word = true;
                self.words += 1;
            }
        }
        self
    }

    pub fn words(&self) -> u64 {
        self.words
    }
}

/// Words in `bytes`.
pub fn count_words(bytes: &[u8]) -> u64 {
    WordCounter::default().feed(bytes).words()
}

/// Words in the document under `root`, read chunk by chunk.
pub fn words_at<S: Storage + ?Sized>(storage: &S, root: &Hash) -> Result<u64> {
    let mut counter = WordCounter::default();
    for leaf in leaves(storage, root)? {
        counter.feed(&storage.chunk_shared(&leaf.hash)?[..]);
    }
    Ok(counter.words())
}

/// `words_at(b) - words_at(a)`, reading only the lines that differ.
pub fn word_delta<S: Storage + ?Sized>(storage: &S, a: &Hash, b: &Hash) -> Result<i64> {
    let mut delta = 0i64;
    for r in changed_line_regions(storage, a, b)? {
        delta -= count_words(&materialize_range(storage, a, r.a_from, r.a_to)?) as i64;
        delta += count_words(&materialize_range(storage, b, r.b_from, r.b_to)?) as i64;
    }
    Ok(delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::ChunkParams;
    use crate::storage::MemStorage;
    use crate::tree::build;

    #[test]
    fn counts_like_wc() {
        assert_eq!(count_words(b""), 0);
        assert_eq!(count_words(b"   \n\t"), 0);
        assert_eq!(count_words(b"## Setup\n\n- run `make`\n"), 5);
        assert_eq!(count_words("naïve café\r\n".as_bytes()), 2);
        let mut c = WordCounter::default();
        c.feed(b"hel").feed(b"lo wor").feed(b"ld");
        assert_eq!(c.words(), 2);
    }

    fn text(seed: u64, lines: usize) -> Vec<u8> {
        let mut x = seed | 1;
        let mut out = Vec::new();
        for _ in 0..lines {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let words = (x % 12) as usize;
            for w in 0..words {
                if w > 0 {
                    out.extend_from_slice(if x >> (w % 60) & 1 == 1 { b"  " } else { b" " });
                }
                out.extend_from_slice(format!("w{}", (x >> w) % 1000).as_bytes());
            }
            out.push(b'\n');
        }
        out
    }

    #[test]
    fn delta_matches_a_full_count_across_chunk_boundaries() {
        let p = ChunkParams::DEFAULT;
        let mut st = MemStorage::new();
        let base = text(7, 20_000);
        let a = build(&mut st, &p, &base).unwrap();
        assert_eq!(words_at(&st, &a).unwrap(), count_words(&base));
        let edits: Vec<Box<dyn Fn(&[u8]) -> Vec<u8>>> = vec![
            Box::new(|d| [&d[..1000], &b"new words here "[..], &d[1000..]].concat()),
            Box::new(|d| [&d[..50_000], &d[90_000..]].concat()),
            Box::new(|d| [d, &b"tail without newline"[..]].concat()),
            Box::new(|d| d.iter().map(|&b| if b == b' ' { b'\n' } else { b }).collect()),
            Box::new(|d| [&d[..d.len() / 2], &b"x"[..], &d[d.len() / 2..]].concat()),
            Box::new(|_| Vec::new()),
        ];
        for (i, edit) in edits.iter().enumerate() {
            let next = edit(&base);
            let b = build(&mut st, &p, &next).unwrap();
            let expect = count_words(&next) as i64 - count_words(&base) as i64;
            assert_eq!(word_delta(&st, &a, &b).unwrap(), expect, "edit {i}");
        }
    }
}
