//! D5: dictionary hyphenation for `hyphens: auto`.
//!
//! Wraps the `hyphenation` crate (Knuth-Liang patterns from the Hyphenation
//! dictionary collection) with an embedded English-US dictionary. The API
//! returns byte-offset break points inside a word; layout inserts a visible
//! `-` there exactly like the existing soft-hyphen (B13) path.

use hyphenation::{Hyphenator, Language, Load, Standard};

/// An embedded-en-US dictionary behind a global (dictionaries are read-only
/// after load; the first request pays the one-time deserialize).
fn en_us() -> Option<&'static Standard> {
    static DICT: std::sync::OnceLock<Option<Standard>> = std::sync::OnceLock::new();
    DICT.get_or_init(|| Standard::from_embedded(Language::EnglishUS).ok())
        .as_ref()
}

/// Byte offsets inside `word` where a hyphen may be inserted, per the
/// dictionary. Returns an empty vector when the word is short (fewer than
/// 5 bytes — dictionaries do not break 2–3-letter heads/tails), when no
/// dictionary is available, or when the word contains uppercase (the
/// dictionary is case-insensitive internally, but acronyms like "HTTP"
/// conventionally don't hyphenate — browsers behave alike via lang
/// dictionaries; keeping them whole is the safer choice for layout).
pub fn en_us_breaks(word: &str) -> Vec<usize> {
    if word.len() < 5 {
        return Vec::new();
    }
    // Acronyms / CamelCase: don't hyphenate. (CSS `hyphenate-limit-*`
    // would refine this; not parsed yet.)
    if word.chars().any(|c| c.is_ascii_uppercase()) {
        return Vec::new();
    }
    let Some(dict) = en_us() else {
        return Vec::new();
    };
    // Only trailing punctuation is stripped, and words with leading
    // punctuation are left whole: the returned offsets must stay byte
    // offsets into a *prefix* of the original word so layout can split it.
    if !word.starts_with(|c: char| c.is_alphabetic()) {
        return Vec::new();
    }
    let word = word.trim_end_matches(|c: char| !c.is_alphabetic());
    dict.hyphenate(word)
        .breaks
        .into_iter()
        .filter(|&b| b > 0 && b < word.len())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d5_en_us_dictionary_breaks() {
        let breaks = en_us_breaks("hyphenation");
        assert!(!breaks.is_empty(), "dictionary finds breaks in hyphenation");
        // Every break is a char boundary in the word.
        for b in &breaks {
            assert!(std::str::from_utf8(&"hyphenation".as_bytes()[*b..*b]).is_ok());
        }
        // Recombine segments reproduces the word.
        let mut s = String::new();
        let mut prev = 0;
        for b in &breaks {
            s.push_str(&"hyphenation"[prev..*b]);
            prev = *b;
        }
        s.push_str(&"hyphenation"[prev..]);
        assert_eq!(s, "hyphenation");
        // Known-correct Knuth-Liang result: hy-phen-ation.
        assert!(breaks.contains(&2), "break after hy: {breaks:?}");
    }

    #[test]
    fn d5_short_and_acronyms_stay_whole() {
        assert!(en_us_breaks("hi").is_empty(), "too short to hyphenate");
        assert!(en_us_breaks("HTTP").is_empty(), "acronyms don't hyphenate");
        assert!(
            en_us_breaks("HelloWorld").is_empty(),
            "CamelCase stays whole"
        );
        // Punctuation trimmed so "hyphenation," still hyphenates.
        let breaks = en_us_breaks("hyphenation,");
        assert!(
            breaks.contains(&2),
            "trailing punctuation does not block: {breaks:?}"
        );
        // Leading punctuation is left whole (offsets must index the word).
        assert!(en_us_breaks("\"hyphenation").is_empty());
    }
}
