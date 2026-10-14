//! kitae_shape — text shaping for the Kitae engine (Phase M).
//!
//! The seam (`Shaper::shape`) is designed so HarfBuzz-behind-a-C-ABI can
//! replace the pure-Rust backend without touching callers (per `CPP_ROLE.md`
//! the C side would only ever be a link-in library). Until linking drags,
//! shaping is pure Rust:
//!
//! * [`FallbackShaper`] — per-character advances (kern/shaping-free, exactly
//!   what the [`TextMeasurer`]-based layout does today).
//! * [`RustybuzzShaper`] — real OpenType shaping (ligatures, kerning, marks,
//!   complex scripts) over any font file.
//! * [`bidi`] — Unicode Bidirectional Algorithm line runs.
//! * [`font`] — script inference + per-script fallback chains.
//!
//! No JS, no WASM, no threads: shaping is a pure function of text + font.

pub mod bidi;
pub mod font;
pub mod hyphenate;

use std::collections::HashMap;

/// One shaped glyph: an (opaque-for-now) id, its advance along the run,
/// an optional mark offset, and the source cluster (byte index).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    /// Glyph id in the shaping font (0 for the fallback shaper's codepoint).
    pub glyph_id: u32,
    /// Advance width in px.
    pub advance: f32,
    /// Mark/kerning offset in px (y-down page space).
    pub dx: f32,
    pub dy: f32,
    /// Byte index of the source cluster.
    pub cluster: u32,
}

/// A shaped text run: glyphs in visual order plus the total advance.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedRun {
    pub glyphs: Vec<ShapedGlyph>,
    pub width: f32,
    /// True when the run itself flows right-to-left.
    pub rtl: bool,
}

impl ShapedRun {
    pub fn width_of(glyphs: &[ShapedGlyph]) -> f32 {
        glyphs.iter().map(|g| g.advance).sum()
    }
}

/// The shaping seam: HarfBuzz-behind-C-ABI implements this trait when (and
/// if) linking ever pays; everything here is pure Rust until then.
pub trait Shaper {
    /// Shape `text` at `size_px` (already-resolved CSS px). `rtl` selects
    /// the paragraph direction for mark placement; glyphs come back in
    /// visual order.
    fn shape(
        &self,
        text: &str,
        size_px: f32,
        bold: bool,
        italic: bool,
        mono: bool,
        rtl: bool,
    ) -> ShapedRun;

    /// D4: shape with explicit OpenType features — `(tag, value)` pairs
    /// from `font-feature-settings` / `font-variant*`, applied in order
    /// (later entries win on a tag conflict). The default ignores them:
    /// the fallback shaper has no features to toggle.
    fn shape_with_features(
        &self,
        text: &str,
        size_px: f32,
        bold: bool,
        italic: bool,
        mono: bool,
        rtl: bool,
        features: &[(String, u32)],
    ) -> ShapedRun {
        let _ = features;
        self.shape(text, size_px, bold, italic, mono, rtl)
    }
}

/// Shaping-free shaper: one glyph per character, caller-supplied advances.
/// This is exactly the measurement model layout uses today, so switching a
/// shell from `measure` to `measure_run` changes nothing until a real
/// shaper is plugged in.
pub struct FallbackShaper {
    advance: Box<dyn Fn(char, f32, bool) -> f32 + Send + Sync>,
}

impl FallbackShaper {
    /// Fixed advance ratios (fraction of `size_px`; `mono_ratio` for
    /// `monospace`) — the same arithmetic the layout `Mock` uses in tests.
    pub fn fixed(ratio: f32, mono_ratio: f32) -> Self {
        FallbackShaper {
            advance: Box::new(move |_: char, size: f32, mono: bool| {
                size * if mono { mono_ratio } else { ratio }
            }),
        }
    }

    pub fn with_advance<F>(advance: F) -> Self
    where
        F: Fn(char, f32, bool) -> f32 + Send + Sync + 'static,
    {
        FallbackShaper {
            advance: Box::new(advance),
        }
    }
}

impl Shaper for FallbackShaper {
    fn shape(
        &self,
        text: &str,
        size_px: f32,
        _bold: bool,
        _italic: bool,
        mono: bool,
        rtl: bool,
    ) -> ShapedRun {
        let mut glyphs = Vec::new();
        let mut i = 0usize;
        while i < text.len() {
            let ch = text[i..].chars().next().unwrap();
            let ch_len = ch.len_utf8();
            // D2: emoji grapheme clusters (ZWJ families, skin tones, flags,
            // keycaps) collapse to ONE glyph carrying the base's advance —
            // the cluster's layout width matches what a real font paints.
            if let Some(end) = emoji_cluster_end(text, i, ch) {
                let adv = (self.advance)(ch, size_px, mono).max(0.0);
                glyphs.push(ShapedGlyph {
                    glyph_id: ch as u32,
                    advance: adv,
                    dx: 0.0,
                    dy: 0.0,
                    cluster: i as u32,
                });
                i = end;
                continue;
            }
            // Grapheme-ish: attach combining marks (and stray variation
            // selectors / ZWJ outside a cluster) to the flow with zero
            // advance (cheap approximation; real marks need a font).
            let combining = is_combining_mark(ch)
                || matches!(ch, '\u{FE0E}' | '\u{FE0F}' | '\u{20E3}' | '\u{200D}');
            let adv = if combining {
                0.0
            } else {
                (self.advance)(ch, size_px, mono).max(0.0)
            };
            glyphs.push(ShapedGlyph {
                glyph_id: ch as u32,
                advance: adv,
                dx: 0.0,
                dy: 0.0,
                cluster: i as u32,
            });
            i += ch_len;
        }
        let width = ShapedRun::width_of(&glyphs);
        ShapedRun { glyphs, width, rtl }
    }
}

/// D1: nonspacing marks (Unicode Mn) the fallback shaper attaches to the
/// base cluster with zero advance — Latin combining diacritics plus the
/// Arabic, Indic, Hebrew, Thai/Lao sign blocks. Spacing vowel signs (Mc,
/// e.g. Devanagari ा or the i-matra ि) keep their advance: real fonts draw
/// them as full-width glyphs. Practical subset, not the full UCD.
/// D2: byte length of the emoji grapheme cluster starting at `i` (first
/// char `ch`), or `None` when `ch` does not start a multi-char cluster.
/// Covers ZWJ families (👨‍👩‍👧), skin-tone modifiers, VS15/16, keycaps
/// and regional-indicator flag pairs — the subset layout must measure as
/// one glyph.
fn emoji_cluster_end(text: &str, i: usize, ch: char) -> Option<usize> {
    let start = i + ch.len_utf8();
    // Keycap: digit/#/* (+ optional VS16) + U+20E3.
    if ch.is_ascii_digit() || ch == '#' || ch == '*' {
        let mut e = start;
        if text[e..].starts_with('\u{FE0F}') {
            e += '\u{FE0F}'.len_utf8();
        }
        if text[e..].starts_with('\u{20E3}') {
            return Some(e + '\u{20E3}'.len_utf8());
        }
    }
    // Regional-indicator pair (flags): exactly two RIs form one cluster.
    if matches!(ch as u32, 0x1F1E6..=0x1F1FF) {
        return text[start..]
            .chars()
            .next()
            .filter(|n| matches!(*n as u32, 0x1F1E6..=0x1F1FF))
            .map(|n| start + n.len_utf8());
    }
    if !font::is_emoji_presentation(ch) || ch == '\u{FE0F}' {
        return None;
    }
    let mut end = start;
    let mut merged = false;
    while let Some(next) = text[end..].chars().next() {
        let nlen = next.len_utf8();
        if matches!(next, '\u{FE0E}' | '\u{FE0F}') || (0x1F3FB..=0x1F3FF).contains(&(next as u32)) {
            end += nlen;
            merged = true;
        } else if next == '\u{200D}' {
            end += nlen;
            // The ZWJ binds the following character into the cluster too.
            if let Some(after) = text[end..].chars().next() {
                end += after.len_utf8();
            }
            merged = true;
        } else {
            break;
        }
    }
    merged.then_some(end)
}

fn is_combining_mark(ch: char) -> bool {
    matches!(ch as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2
        | 0x05C4..=0x05C5 | 0x05C7
        | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4
        | 0x06E7..=0x06E8 | 0x06EA..=0x06ED
        | 0x0900..=0x0902 | 0x093A | 0x093C | 0x0941..=0x0948 | 0x094D | 0x0951..=0x0957
        | 0x0962..=0x0963
        | 0x0981..=0x0982 | 0x09BC | 0x09C1..=0x09C4 | 0x09CD | 0x09E2..=0x09E3
        | 0x0A01..=0x0A02 | 0x0A3C | 0x0A41..=0x0A42 | 0x0A47..=0x0A48 | 0x0A4B..=0x0A4D
        | 0x0A70..=0x0A71 | 0x0A81..=0x0A82 | 0x0ABC | 0x0AC1..=0x0AC5 | 0x0AC7..=0x0AC8
        | 0x0ACD | 0x0AE2..=0x0AE3
        | 0x0B01..=0x0B02 | 0x0B3C | 0x0B41..=0x0B44 | 0x0B4D | 0x0B62..=0x0B63
        | 0x0B82 | 0x0BC0 | 0x0BC1..=0x0BC2 | 0x0BCD
        | 0x0C00 | 0x0C3E..=0x0C40 | 0x0C46..=0x0C4D | 0x0C55..=0x0C56 | 0x0C62..=0x0C63
        | 0x0CBC | 0x0CCC..=0x0CCD
        | 0x0D41..=0x0D44 | 0x0D4D | 0x0D62..=0x0D63 | 0x0DCA
        | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E
        | 0x0EB1 | 0x0EB4..=0x0EBC | 0x0EC8..=0x0ECD
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// Real OpenType shaping over an in-memory font file (the pure-Rust
/// escape hatch for HarfBuzz: ligatures, kerning, mark positioning,
/// complex-script substitution — everything the fallback cannot do).
pub struct RustybuzzShaper {
    font: rustybuzz::Face<'static>,
    /// Backing store for the font slice: leaked on load, freed exactly once
    /// in [`Drop`]. The font only ever reads through it.
    ptr: *mut u8,
    len: usize,
}

impl Drop for RustybuzzShaper {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                drop(Vec::from_raw_parts(self.ptr, self.len, self.len));
            }
            self.ptr = std::ptr::null_mut();
        }
    }
}

// SAFETY: the backing slice outlives the font (freed in Drop after the font
// drops — field order matters: `font` first), and all access is read-only.
unsafe impl Send for RustybuzzShaper {}
unsafe impl Sync for RustybuzzShaper {}

impl RustybuzzShaper {
    /// Load face `face_index` from a font file's bytes (`.ttf`/`.ttc`/`.otf`).
    pub fn from_data(data: Vec<u8>, face_index: u32) -> Option<RustybuzzShaper> {
        if data.is_empty() {
            return None;
        }
        // Ownership hand-off through a raw slice pointer: the font borrows
        // shared for 'static; Drop reconstructs the Box exactly once. No
        // mutable borrow ever aliases the shared one.
        let raw: *mut [u8] = Box::into_raw(data.into_boxed_slice());
        let leaked: &'static [u8] = unsafe { &*raw };
        match rustybuzz::Face::from_slice(leaked, face_index) {
            Some(font) => Some(RustybuzzShaper {
                font,
                ptr: raw as *mut u8,
                len: leaked.len(),
            }),
            None => {
                // Parse failed: give the allocation back instead of leaking.
                unsafe {
                    drop(Box::from_raw(raw));
                }
                None
            }
        }
    }

    /// Load a face from a font file on disk.
    pub fn from_file(path: &std::path::Path, face_index: u32) -> Option<RustybuzzShaper> {
        let data = std::fs::read(path).ok()?;
        RustybuzzShaper::from_data(data, face_index)
    }
}

impl Shaper for RustybuzzShaper {
    fn shape(
        &self,
        text: &str,
        size_px: f32,
        bold: bool,
        italic: bool,
        mono: bool,
        rtl: bool,
    ) -> ShapedRun {
        self.shape_with_features(text, size_px, bold, italic, mono, rtl, &[])
    }

    fn shape_with_features(
        &self,
        text: &str,
        size_px: f32,
        _bold: bool,
        _italic: bool,
        _mono: bool,
        rtl: bool,
        features: &[(String, u32)],
    ) -> ShapedRun {
        let feats: Vec<rustybuzz::Feature> = features
            .iter()
            .map(|(tag, value)| {
                rustybuzz::Feature::new(
                    rustybuzz::ttf_parser::Tag::from_bytes_lossy(tag.as_bytes()),
                    *value,
                    ..,
                )
            })
            .collect();
        let mut buf = rustybuzz::UnicodeBuffer::new();
        buf.push_str(text);
        buf.set_direction(if rtl {
            rustybuzz::Direction::RightToLeft
        } else {
            rustybuzz::Direction::LeftToRight
        });
        let output = rustybuzz::shape(&self.font, &feats, buf);
        let upem = self.font.units_per_em() as f32;
        let scale = if upem > 0.0 { size_px / upem } else { 0.0 };
        let infos = output.glyph_infos();
        let positions = output.glyph_positions();
        let mut glyphs = Vec::with_capacity(infos.len());
        for (info, pos) in infos.iter().zip(positions.iter()) {
            glyphs.push(ShapedGlyph {
                glyph_id: info.glyph_id,
                advance: pos.x_advance as f32 * scale,
                // rustybuzz offsets are y-up; layout is y-down.
                dx: pos.x_offset as f32 * scale,
                dy: -(pos.y_offset as f32) * scale,
                cluster: info.cluster,
            });
        }
        let width = ShapedRun::width_of(&glyphs);
        ShapedRun { glyphs, width, rtl }
    }
}

/// UAX #14 line breaking (Q13). `Mandatory` breaks must happen (newlines,
/// end of text); `Allowed` breaks are permitted wrap points. Re-exported so
/// callers never depend on the crate directly.
pub use unicode_linebreak::BreakOpportunity;

/// UAX #14 break opportunities of `text` as `(byte_index, kind)` pairs where
/// the index points **after** the break and the iterator ends with
/// `(len, Mandatory)`. Layout segments each bidi run at these points so
/// Latin words stay whole while CJK wraps per character and kinsoku (never
/// before closing punctuation) falls out of the Unicode classes.
pub fn line_breaks(text: &str) -> impl Iterator<Item = (usize, BreakOpportunity)> + use<'_> {
    unicode_linebreak::linebreaks(text)
}

/// Width cache keyed by (text, size bucket, style bits, spacing bits):
/// shells memoize shaped widths across relayouts; the engine owns
/// invalidation (slice G). Spacing is part of the key because letter-spaced
/// runs measure differently from unspaced ones (Q3).
#[derive(Debug, Default)]
pub struct WidthCache {
    map: HashMap<(String, u32, u32, u32), f32>,
}

impl WidthCache {
    pub fn new() -> WidthCache {
        WidthCache::default()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn get_or_shape<S: Shaper>(
        &mut self,
        shaper: &S,
        text: &str,
        size_px: f32,
        bold: bool,
        italic: bool,
        mono: bool,
        rtl: bool,
        letter_spacing: f32,
    ) -> f32 {
        let key = (
            text.to_string(),
            size_px.to_bits(),
            (bold as u32) | ((italic as u32) << 1) | ((mono as u32) << 2) | ((rtl as u32) << 3),
            letter_spacing.to_bits(),
        );
        if let Some(w) = self.map.get(&key) {
            return *w;
        }
        let w = shaper.shape(text, size_px, bold, italic, mono, rtl).width;
        self.map.insert(key, w);
        w
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_advances_clusters_and_direction() {
        let shaper = FallbackShaper::fixed(0.5, 0.6);
        let run = shaper.shape("ab", 16.0, false, false, false, false);
        assert_eq!(run.glyphs.len(), 2);
        assert_eq!(run.glyphs[0].cluster, 0);
        assert_eq!(run.glyphs[1].cluster, 1);
        assert!((run.width - 16.0).abs() < 0.01);
        assert!(!run.rtl);
        let rtl = shaper.shape("ab", 16.0, false, false, false, true);
        assert!(rtl.rtl);
        let mono = shaper.shape("ab", 16.0, false, false, true, false);
        assert!((mono.width - 19.2).abs() < 0.01);
        // Combining mark attaches with zero advance.
        let run = shaper.shape("é", 16.0, false, false, false, false);
        assert_eq!(run.glyphs.len(), 2);
        assert_eq!(run.glyphs[1].advance, 0.0);
    }

    #[test]
    fn width_cache_memoizes_and_clears() {
        let shaper = FallbackShaper::fixed(0.5, 0.6);
        let mut cache = WidthCache::new();
        let a = cache.get_or_shape(&shaper, "hello", 16.0, false, false, false, false, 0.0);
        let b = cache.get_or_shape(&shaper, "hello", 16.0, false, false, false, false, 0.0);
        assert_eq!(a, b);
        assert_eq!(cache.len(), 1);
        cache.get_or_shape(&shaper, "hello", 16.0, true, false, false, false, 0.0);
        assert_eq!(cache.len(), 2);
        // Spacing is part of the key (Q3): a letter-spaced query is a
        // distinct entry, not a stale hit on the unspaced width.
        cache.get_or_shape(&shaper, "hello", 16.0, false, false, false, false, 2.0);
        assert_eq!(cache.len(), 3);
        cache.clear();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn line_breaks_latin_after_spaces_and_end() {
        // The only Allowed break sits after "one " (the space stays with the
        // line that ends, LB18); end-of-text is Mandatory.
        let pts: Vec<_> = line_breaks("one two").collect();
        assert_eq!(
            pts,
            vec![
                (4, BreakOpportunity::Allowed),
                (7, BreakOpportunity::Mandatory),
            ]
        );
    }

    #[test]
    fn line_breaks_cjk_per_char_and_kinsoku() {
        // Ideographs break between characters — but never BEFORE closing
        // punctuation (UAX #14 LB13-class rules), so after 字 (byte 6, right
        // before 。) is forbidden while after 。 (byte 9) is allowed.
        let pts: Vec<(usize, _)> = line_breaks("漢字。次").collect();
        let allowed: Vec<usize> = pts
            .iter()
            .filter(|(_, k)| *k == BreakOpportunity::Allowed)
            .map(|(i, _)| *i)
            .collect();
        assert_eq!(allowed, vec![3, 9]);
        assert_eq!(*pts.last().unwrap(), (12, BreakOpportunity::Mandatory));
    }

    #[test]
    fn line_breaks_nbsp_glues_words() {
        // GL class: no break before or after a no-break space — "a\u{a0}b"
        // stays one unbreakable segment (the old split_whitespace path
        // wrongly broke it and ate the NBSP).
        let pts: Vec<_> = line_breaks("a\u{a0}b").collect();
        assert_eq!(pts, vec![(4, BreakOpportunity::Mandatory)]);
    }

    /// D4: the fallback shaper has nothing to toggle, so features are a
    /// no-op and the run is identical to a plain `shape`.
    #[test]
    fn fallback_shaper_ignores_font_features() {
        let shaper = FallbackShaper::fixed(0.5, 0.6);
        let a = shaper.shape("ab", 16.0, false, false, false, false);
        let b = shaper.shape_with_features(
            "ab",
            16.0,
            false,
            false,
            false,
            false,
            &[("liga".to_string(), 0)],
        );
        assert_eq!(a, b);
    }

    /// D4: real feature application (macOS system font; gated like the
    /// latin test above). `liga` off splits the "fi" ligature back into
    /// two glyphs; `liga` on (the HarfBuzz default, made explicit) fuses it.
    #[cfg(target_os = "macos")]
    #[test]
    fn rustybuzz_font_features_toggle_ligatures() {
        let path = std::path::Path::new("/System/Library/Fonts/Helvetica.ttc");
        let shaper = RustybuzzShaper::from_file(path, 0).expect("helvetica loads");
        let off = shaper.shape_with_features(
            "fi",
            16.0,
            false,
            false,
            false,
            false,
            &[("liga".to_string(), 0)],
        );
        assert_eq!(off.glyphs.len(), 2, "liga 0 keeps f and i separate");
        let on = shaper.shape_with_features(
            "fi",
            16.0,
            false,
            false,
            false,
            false,
            &[("liga".to_string(), 1)],
        );
        assert_eq!(on.glyphs.len(), 1, "liga on fuses the fi ligature");
        let plain = shaper.shape("fi", 16.0, false, false, false, false);
        assert_eq!(plain.glyphs.len(), on.glyphs.len(), "default = liga on");
    }

    /// Real-font shaping (macOS system font; gated — shaping needs a font
    /// file and this project is macOS-only by standing decision).
    #[cfg(target_os = "macos")]
    #[test]
    fn rustybuzz_shapes_latin_with_sane_advances() {
        let path = std::path::Path::new("/System/Library/Fonts/Helvetica.ttc");
        let shaper = RustybuzzShaper::from_file(path, 0).expect("helvetica loads");
        let run = shaper.shape("Hello", 16.0, false, false, false, false);
        assert_eq!(run.glyphs.len(), 5, "no ligatures in plain latin");
        assert!(run.width > 20.0 && run.width < 80.0, "width={}", run.width);
        for g in &run.glyphs {
            assert!(g.advance > 0.0, "positive advances: {g:?}");
        }
        // Clusters track source bytes.
        assert_eq!(run.glyphs[0].cluster, 0);
        assert_eq!(run.glyphs[4].cluster, 4);
    }

    /// D1 conformance: complex-script shaping through GeezaPro (macOS
    /// system font). Rustybuzz applies the Arabic joining table — each
    /// letter maps to a distinct contextual form (isolated/initial/medial/
    /// final), the lam-alef ligature fuses to one glyph, and clusters come
    /// back in visual (right-to-left) order.
    #[cfg(target_os = "macos")]
    #[test]
    fn d1_arabic_joining_forms_and_rtl_clusters() {
        let shaper = RustybuzzShaper::from_file(
            std::path::Path::new("/System/Library/Fonts/GeezaPro.ttc"),
            0,
        )
        .expect("GeezaPro loads");
        // بب — same letter twice; isolation vs medial context must give
        // two DIFFERENT glyphs (the whole point of joining forms).
        let run = shaper.shape("بب", 16.0, false, false, false, true);
        assert_eq!(run.glyphs.len(), 2);
        assert_ne!(
            run.glyphs[0].glyph_id, run.glyphs[1].glyph_id,
            "identical letters must take different joining forms"
        );
        assert!(run.rtl);
        // Lam-alef (لا): mandatory ligature — one glyph covering both
        // codepoints, cluster anchored at the first.
        let run = shaper.shape("لا", 16.0, false, false, false, true);
        assert_eq!(run.glyphs.len(), 1, "lam-alef fuses to one glyph");
        assert_eq!(run.glyphs[0].cluster, 0);
        // مرحبا (marhaba): 5 letters in, 5 glyphs out, visual order is
        // reversed (cluster of the first letter LAST), every letter keeps
        // a positive advance, and no glyph is the isolated form's id.
        let run = shaper.shape("مرحبا", 16.0, false, false, false, true);
        assert_eq!(run.glyphs.len(), 5);
        let clusters: Vec<u32> = run.glyphs.iter().map(|g| g.cluster).collect();
        assert_eq!(
            clusters,
            vec![8, 6, 4, 2, 0],
            "visual = reversed source order"
        );
        assert!(run.glyphs.iter().all(|g| g.advance > 0.0));
        // No glyph in the joined word is the isolated form: a word-initial
        // meem takes its INITIAL form, never the isolated one.
        let isolated_meem = shaper.shape("م", 16.0, false, false, false, true);
        assert!(
            run.glyphs
                .iter()
                .all(|g| g.glyph_id != isolated_meem.glyphs[0].glyph_id),
            "word-initial meem must use the initial form, not isolated"
        );
        // Total advance is a plausible text width (positive, not absurd).
        assert!(run.width > 10.0 && run.width < 100.0, "width={}", run.width);
    }

    /// D1 conformance: Indic reordering + conjuncts through Devanagari
    /// system fonts. The i-matra reorders BEFORE its base consonant, the
    /// virama fuses consonant pairs into conjunct glyphs (font-dependent:
    /// MT composes क्ष from one glyph id, Sangam MN may use half-forms),
    /// marks carry zero advance, and clusters stay anchored at the base.
    #[cfg(target_os = "macos")]
    #[test]
    fn d1_devanagari_matra_reorder_and_conjuncts() {
        // Two different Devanagari fonts (different upem/strategies).
        for path in [
            "/System/Library/Fonts/Supplemental/DevanagariMT.ttc",
            "/System/Library/Fonts/Supplemental/Devanagari Sangam MN.ttc",
        ] {
            let shaper = RustybuzzShaper::from_file(std::path::Path::new(path), 0)
                .unwrap_or_else(|| panic!("{path} loads"));
            // क्ष (k + virama + ṣ): virama fuses the pair — strictly FEWER
            // glyphs than codepoints, and every cluster maps to the base.
            let run = shaper.shape("\u{915}\u{94d}\u{937}", 16.0, false, false, false, false);
            assert!(run.glyphs.len() < 3, "क्ष conjunct must fuse: {run:?}");
            assert!(run.glyphs.iter().all(|g| g.cluster == 0), "{run:?}");
            // कि (k + i-matra): the matra reorders BEFORE the base — both
            // glyphs anchor to the क cluster (0), and the base glyph id
            // matches the standalone क shaped alone.
            let run = shaper.shape("\u{915}\u{93F}", 16.0, false, false, false, false);
            let ka_alone = shaper.shape("\u{915}", 16.0, false, false, false, false);
            assert_eq!(run.glyphs.len(), 2);
            assert!(run.glyphs.iter().all(|g| g.cluster == 0), "{run:?}");
            assert_eq!(
                run.glyphs[1].glyph_id, ka_alone.glyphs[0].glyph_id,
                "base consonant comes second — the i-matra reordered before it"
            );
            // क्क, क्त्र: conjunct chains fuse below codepoint count.
            let run = shaper.shape("\u{915}\u{94d}\u{915}", 16.0, false, false, false, false);
            assert!(run.glyphs.len() < 3, "क्क must fuse: {run:?}");
            let run = shaper.shape(
                "\u{915}\u{94d}\u{924}\u{94d}\u{930}",
                16.0,
                false,
                false,
                false,
                false,
            );
            assert!(run.glyphs.len() < 5, "क्त्र must fuse: {run:?}");
            // हिंदी (Hindi): matra+anusvara share the ह cluster, positive
            // advances everywhere, sane total width.
            let run = shaper.shape(
                "\u{939}\u{93F}\u{902}\u{926}\u{940}",
                16.0,
                false,
                false,
                false,
                false,
            );
            assert!(run.glyphs.iter().all(|g| g.advance >= 0.0), "{run:?}");
            assert!(run.width > 10.0 && run.width < 100.0, "width={}", run.width);
        }
    }

    /// D1 audit fix: the FALLBACK shaper (no font) must give zero advance
    /// to nonspacing marks from ALL scripts — Arabic diacritics, Devanagari
    /// virama/anusvara, Thai — not just Latin combining diacritics.
    #[test]
    fn d1_fallback_zero_advances_nonspacing_marks_all_scripts() {
        let shaper = FallbackShaper::fixed(0.5, 0.6);
        // (text, index of the mark whose advance must be 0)
        for (text, mark_byte) in [
            ("a\u{301}", 1),       // Latin acute
            ("\u{628}\u{64E}", 2), // Arabic fatha on beh
            ("\u{915}\u{94D}", 3), // Devanagari virama
            ("\u{915}\u{902}", 3), // Devanagari anusvara
            ("\u{E01}\u{E34}", 3), // Thai sarah i
            ("\u{5D0}\u{5B0}", 2), // Hebrew sheva
        ] {
            let run = shaper.shape(text, 16.0, false, false, false, false);
            let mark = run
                .glyphs
                .iter()
                .find(|g| g.cluster as usize == mark_byte)
                .unwrap_or_else(|| panic!("no glyph at byte {mark_byte} for {text:?}"));
            assert_eq!(mark.advance, 0.0, "mark of {text:?} must not advance");
        }
        // Spacing vowel signs (Mc) keep their advance: Devanagari ा.
        let run = shaper.shape("\u{915}\u{93E}", 16.0, false, false, false, false);
        assert!(run.glyphs[1].advance > 0.0, "spacing aa-matra advances");
    }

    /// D2: ZWJ emoji sequences are ONE grapheme cluster — the family emoji
    /// 👨‍👩‍👧 (3 emoji + 2 ZWJ, 18 bytes) occupies a single emoji's
    /// advance, not five characters'.
    #[test]
    fn d2_zwj_family_is_one_cluster() {
        let shaper = FallbackShaper::fixed(0.5, 0.6);
        let run = shaper.shape("👨‍👩‍👧", 16.0, false, false, false, false);
        assert_eq!(run.glyphs.len(), 1, "ZWJ family merges: {run:?}");
        assert_eq!(run.glyphs[0].cluster, 0);
        assert!(
            (run.width - 8.0).abs() < 0.01,
            "one emoji wide: {}",
            run.width
        );
        // Skin-tone modifier: 👋🏽 also one cluster.
        let run = shaper.shape("👋🏽", 16.0, false, false, false, false);
        assert_eq!(run.glyphs.len(), 1, "{run:?}");
        assert!((run.width - 8.0).abs() < 0.01);
        // Regional-indicator pair (flag): one cluster.
        let run = shaper.shape("🇺🇸", 16.0, false, false, false, false);
        assert_eq!(run.glyphs.len(), 1, "{run:?}");
        // Keycap 1️⃣ = digit + VS16 + U+20E3.
        let run = shaper.shape("1\u{FE0F}\u{20E3}", 16.0, false, false, false, false);
        assert_eq!(run.glyphs.len(), 1, "{run:?}");
        assert!((run.width - 8.0).abs() < 0.01, "{}", run.width);
        // Two emoji WITHOUT a ZWJ stay separate clusters.
        let run = shaper.shape("😀😀", 16.0, false, false, false, false);
        assert_eq!(run.glyphs.len(), 2);
        // Mixed text: the cluster occupies the base's slot between letters.
        let run = shaper.shape("a👨‍👩‍👧b", 16.0, false, false, false, false);
        assert_eq!(run.glyphs.len(), 3);
        assert_eq!(run.glyphs[1].cluster, 1);
        assert_eq!(run.glyphs[2].cluster, 19);
    }
}
