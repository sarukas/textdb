//! Folding text the way the full-text index tokenises it, so that what a search matches and
//! what it shows a user agree.
//!
//! The SQLite index uses fts5's `unicode61` tokenizer, which strips diacritics: a chunk
//! holding `façade` is indexed under `facade`, so searching `facade` finds it. Picking the
//! line to show, though, used to compare raw text, find nothing, and fall back to the first
//! line of the chunk — the document was right and the line was wrong. Folding both sides with
//! the same function is what keeps them in step.
//!
//! This covers Latin-1 Supplement, Latin Extended-A and the Latin Extended-B letters that are
//! a base letter plus a mark. It is deliberately not a full Unicode normalisation: the
//! tokenizer's own table is finite too, and anything outside this range (CJK, Greek,
//! Cyrillic, emoji) carries no diacritics to strip. A codepoint this misses degrades a
//! snippet; it can never change which documents a search returns, because the index decides
//! that on its own.

/// Base letters for U+0100..U+017F, indexed by `cp - 0x100`.
///
/// A NUL means "leave it alone": the ligatures (Ĳ, Œ) and the letters that are not a base
/// letter with a mark (ŉ, ŋ) are not diacritical forms of anything.
const LATIN_EXT_A: &[u8; 128] =
    b"aaaaaaccccccccddddeeeeeeeeeegggggggghhhhiiiiiiiiii\0\0jjkkkllllllllllnnnnnn\0\0\0oooooo\0\0rrrrrrssssssssttttttuuuuuuuuuuuuwwyyyzzzzzzs";

/// The base letter for one character, or the character itself when it has no diacritic.
///
/// Uppercase is handled by the caller lowercasing first, which is why only lowercase bases
/// appear here.
fn base(c: char) -> char {
    match c as u32 {
        // Latin-1 Supplement. The gaps are the characters unicode61 also leaves alone: ×, ÷,
        // ß, þ, ð and the æ ligature, none of which is an accented letter.
        0xE0..=0xE5 => 'a',
        0xE7 => 'c',
        0xE8..=0xEB => 'e',
        0xEC..=0xEF => 'i',
        0xF1 => 'n',
        0xF2..=0xF6 | 0xF8 => 'o',
        0xF9..=0xFC => 'u',
        0xFD | 0xFF => 'y',
        cp @ 0x100..=0x17F => match LATIN_EXT_A[(cp - 0x100) as usize] {
            0 => c,
            b => b as char,
        },
        // Latin Extended-B, the handful that are a base letter with a mark.
        0x1CE..=0x1DC => match (c as u32 - 0x1CE) / 2 {
            0 => 'a',
            1 => 'i',
            2 => 'o',
            _ => 'u',
        },
        0x1E3 => 'a',
        0x1E7 => 'g',
        0x1E9 => 'k',
        0x1EB => 'o',
        0x1F5 => 'g',
        0x1F9 => 'n',
        _ => c,
    }
}

/// Lowercase `s` and strip the diacritics the index strips.
///
/// Order-preserving and one output character per input character, so counting newlines in a
/// prefix of the result gives the same answer as counting them in the corresponding prefix of
/// the input — which is what lets a caller turn a match position back into a line number.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        // `to_lowercase` is the one place a character can become several; taking the first
        // keeps the one-for-one property, and the rest are combining marks we would drop.
        let lower = c.to_lowercase().next().unwrap_or(c);
        out.push(base(lower));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_what_the_tokenizer_folds() {
        assert_eq!(fold("Façade"), "facade");
        // Lithuanian, which the benchmark corpus is full of.
        assert_eq!(fold("ąžuolas čiurlys ėglė įrašas šviesa ūkas žemė"), "azuolas ciurlys egle irasas sviesa ukas zeme");
        assert_eq!(fold("NAÏVE"), "naive");
        assert_eq!(fold("Ñandú"), "nandu");
    }

    #[test]
    fn leaves_alone_what_has_no_diacritic() {
        // Scripts with no marks to strip pass through untouched.
        assert_eq!(fold("数据 Москва Ελλάδα 🚀"), "数据 москва ελλάδα 🚀");
        // Ligatures are not accented letters.
        assert_eq!(fold("Æon œuvre straße"), "æon œuvre straße");
        assert_eq!(fold("plain ascii 123"), "plain ascii 123");
    }

    #[test]
    fn one_character_in_one_character_out() {
        // The property a caller relies on to turn a match position into a line number.
        for s in ["façade", "ĄŽUOLAS", "a\nb\nç\n", "🚀x", "İstanbul"] {
            assert_eq!(fold(s).chars().count(), s.chars().count(), "{s:?}");
            assert_eq!(fold(s).matches('\n').count(), s.matches('\n').count(), "{s:?}");
        }
    }
}
