//! Bidirectional text (Phase M3): Unicode Bidirectional Algorithm line
//! runs over the `unicode-bidi` crate (pure Rust, policy-clean). Layout
//! feeds each line's text and gets visual runs back; `direction: rtl`
//! (the `ComputedStyle.direction` property) selects the paragraph level and
//! flips `text-align: start/end` at style time.

use unicode_bidi::{BidiInfo, Level};

/// One visual run: byte range in the source line plus its direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualRun {
    pub byte_start: usize,
    pub byte_end: usize,
    /// True for right-to-left runs (levels are odd).
    pub rtl: bool,
}

/// Split `line` into visual runs in VISUAL order (the order glyphs paint).
/// `base_rtl` is the paragraph direction (`direction: rtl` or an RTL-first
/// line under the default rule).
pub fn visual_runs(line: &str, base_rtl: bool) -> Vec<VisualRun> {
    let default_level = if base_rtl { Level::rtl() } else { Level::ltr() };
    let info = BidiInfo::new(line, Some(default_level));
    if info.paragraphs.is_empty() {
        return Vec::new();
    }
    let para = &info.paragraphs[0];
    let (levels, runs) = info.visual_runs(para, para.range.clone());
    runs.iter()
        .map(|range| {
            let rtl = levels[range.start].is_rtl();
            VisualRun {
                byte_start: range.start,
                byte_end: range.end,
                rtl,
            }
        })
        .collect()
}

/// Levels for each byte (debugging / tests).
pub fn levels(line: &str, base_rtl: bool) -> Vec<u8> {
    let default_level = if base_rtl { Level::rtl() } else { Level::ltr() };
    let info = BidiInfo::new(line, Some(default_level));
    if info.paragraphs.is_empty() {
        return vec![0; line.len()];
    }
    let para = &info.paragraphs[0];
    let (levels, _) = info.visual_runs(para, para.range.clone());
    levels.iter().map(|l| l.number()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_ltr_is_one_run() {
        let runs = visual_runs("hello world", false);
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].rtl);
        assert_eq!((runs[0].byte_start, runs[0].byte_end), (0, 11));
    }

    #[test]
    fn mixed_text_splits_with_rtl_middle() {
        // "abc אבג def": LTR paragraph → [L, R, L] in visual order.
        let runs = visual_runs("abc אבג def", false);
        assert_eq!(runs.len(), 3, "{runs:?}");
        assert!(!runs[0].rtl && runs[1].rtl && !runs[2].rtl);
        assert_eq!(&"abc אבג def"[runs[1].byte_start..runs[1].byte_end], "אבג");
    }

    #[test]
    fn rtl_paragraph_starts_rtl() {
        // RTL paragraph "אבג abc" displays with Hebrew on the right: visual
        // (left-to-right) order is [latin, hebrew].
        let runs = visual_runs("אבג abc", true);
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert!(!runs[0].rtl && runs[1].rtl);
        assert_eq!(
            "אבג abc"[runs[1].byte_start..runs[1].byte_end].trim(),
            "אבג"
        );
    }

    #[test]
    fn empty_line_has_no_runs() {
        assert!(visual_runs("", false).is_empty());
        assert!(visual_runs("", true).is_empty());
    }
}
