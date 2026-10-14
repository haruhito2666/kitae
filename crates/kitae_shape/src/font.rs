//! Font collection (Phase M2): family-chain resolution without a font
//! engine dependency. A host `FontCollection` enumerates system fonts (Core
//! Text on macOS); this module contributes the pure parts — script
//! inference, per-script fallback tables, generic-family mapping — plus the
//! [`is_emoji_presentation`] helper routing color emoji to image strikes
//! (drawn through the existing raster path, L2 mechanism).

/// Scripts the fallback tables distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Script {
    Latin,
    Arabic,
    Hebrew,
    Devanagari,
    Bengali,
    Tamil,
    Thai,
    Cjk,
    Hangul,
    Emoji,
    Other,
}

impl Script {
    /// Best-effort script of one character (ranges per Unicode blocks).
    pub fn of(ch: char) -> Script {
        let u = ch as u32;
        match u {
            0x0600..=0x06FF
            | 0x0750..=0x077F
            | 0x08A0..=0x08FF
            | 0xFB50..=0xFDFF
            | 0xFE70..=0xFEFF => Script::Arabic,
            0x0590..=0x05FF | 0xFB1D..=0xFB4F => Script::Hebrew,
            0x0900..=0x097F => Script::Devanagari,
            0x0980..=0x09FF => Script::Bengali,
            0x0B80..=0x0BFF => Script::Tamil,
            0x0E00..=0x0E7F => Script::Thai,
            0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3000..=0x303F | 0xFF00..=0xFFEF => Script::Cjk,
            0xAC00..=0xD7AF | 0x1100..=0x11FF => Script::Hangul,
            _ if is_emoji_presentation(ch) => Script::Emoji,
            _ if u < 0x2500 => Script::Latin,
            _ => Script::Other,
        }
    }

    /// Dominant script of a run (first non-`Other` script wins; `Latin` for
    /// empty/pure-`Other` runs).
    pub fn of_run(text: &str) -> Script {
        for ch in text.chars() {
            match Script::of(ch) {
                Script::Other => continue,
                Script::Latin => continue,
                s => return s,
            }
        }
        Script::Latin
    }
}

/// True for codepoints that default to emoji presentation (plus VS16).
/// Color emoji render as image strikes into the atlas (L2 mechanism); the
/// layout box model is unchanged.
pub fn is_emoji_presentation(ch: char) -> bool {
    matches!(ch as u32,
        0x1F300..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0xFE0F
        | 0x1F000..=0x1F2FF | 0x2190..=0x21FF | 0x2300..=0x23FF | 0x25A0..=0x25FF)
}

/// System font enumeration (macOS Core Text, hand-rolled FFI — no new
/// dependencies; the host side already links Core Text for measurement).
/// Non-macOS builds return an empty list (hosts substitute their own
/// enumerator behind the same function shape).
pub fn available_families() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        coretext::available_families()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
mod coretext {
    use std::ffi::c_void;

    type CFRef = *const c_void;

    #[link(name = "CoreText", kind = "framework")]
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CTFontCollectionCreateFromAvailableFonts(options: CFRef) -> CFRef;
        fn CTFontCollectionCreateMatchingFontDescriptors(collection: CFRef) -> CFRef;
        fn CTFontDescriptorCopyAttribute(descriptor: CFRef, attribute: CFRef) -> CFRef;
        static kCTFontFamilyNameAttribute: CFRef;
        fn CFArrayGetCount(array: CFRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFRef, idx: isize) -> CFRef;
        fn CFStringGetLength(s: CFRef) -> isize;
        fn CFStringGetCString(s: CFRef, buf: *mut i8, len: isize, enc: u32) -> i32;
        fn CFRelease(obj: CFRef);
    }

    const UTF8: u32 = 0x08000100;

    /// Owned Core Foundation reference (released exactly once).
    struct Owned(CFRef);

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }

    pub fn available_families() -> Vec<String> {
        unsafe {
            let collection = Owned(CTFontCollectionCreateFromAvailableFonts(std::ptr::null()));
            if collection.0.is_null() {
                return Vec::new();
            }
            let descriptors = Owned(CTFontCollectionCreateMatchingFontDescriptors(collection.0));
            if descriptors.0.is_null() {
                return Vec::new();
            }
            // The family-name constant is borrowed, never released.
            let attr = kCTFontFamilyNameAttribute;
            let n = CFArrayGetCount(descriptors.0);
            let mut out = Vec::new();
            for i in 0..n {
                let desc = CFArrayGetValueAtIndex(descriptors.0, i);
                if desc.is_null() {
                    continue;
                }
                let name = Owned(CTFontDescriptorCopyAttribute(desc, attr));
                if name.0.is_null() {
                    continue;
                }
                let len = CFStringGetLength(name.0);
                if len <= 0 || len > 512 {
                    continue;
                }
                // UTF-8 worst case: 4 bytes per char + NUL.
                let mut buf = vec![0i8; (len as usize) * 4 + 1];
                if CFStringGetCString(name.0, buf.as_mut_ptr(), buf.len() as isize, UTF8) == 0 {
                    continue;
                }
                let bytes: Vec<u8> = buf
                    .iter()
                    .take_while(|&&b| b != 0)
                    .map(|&b| b as u8)
                    .collect();
                if let Ok(s) = String::from_utf8(bytes) {
                    if !s.is_empty() && !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
            out.sort();
            out
        }
    }
}

/// macOS families backing each script (Core Text cascade order). Non-macOS
/// hosts substitute their own table behind the same function shape.
pub fn fallback_families(script: Script) -> &'static [&'static str] {
    match script {
        Script::Latin => &[],
        Script::Arabic => &["Geeza Pro", "Al Bayan", "Noto Naskh Arabic"],
        Script::Hebrew => &["New Peninim MT", "Lucida Grande"],
        Script::Devanagari => &[
            "Devanagari MT",
            "Devanagari Sangam MN",
            "Noto Sans Devanagari",
        ],
        Script::Bengali => &["Bangla MN", "Bangla Sangam MN"],
        Script::Tamil => &["Tamil MN", "Tamil Sangam MN"],
        Script::Thai => &["Thonburi", "Ayuthaya"],
        Script::Cjk => &[
            "Hiragino Sans GB",
            "Hiragino Kaku Gothic ProN",
            "PingFang SC",
            "PingFang TC",
            "Apple SD Gothic Neo",
        ],
        Script::Hangul => &["Apple SD Gothic Neo", "Nanum Gothic"],
        Script::Emoji => &["Apple Color Emoji", "Noto Color Emoji"],
        Script::Other => &[],
    }
}

/// macOS families for CSS generic families.
pub fn generic_family(name: &str) -> &'static [&'static str] {
    match name.to_ascii_lowercase().as_str() {
        "serif" => &["New York", "Georgia"],
        "sans-serif" => &["Helvetica Neue", "Helvetica"],
        "monospace" => &["Menlo", "SF Mono"],
        "cursive" => &["Apple Chancery", "Snell Roundhand"],
        "fantasy" => &["Papyrus", "Herculanum"],
        _ => &[],
    }
}

/// Resolve a CSS `font-family` chain for a run: requested families first
/// (deduplicated, in order), then the run script's fallback table, then the
/// generic mapping when the chain names one. Empty chain → system default.
pub fn fallback_chain(requested: &[String], sample: &str) -> Vec<String> {
    fallback_chain_with_lang(requested, sample, None)
}

/// D3: language-preferred families for the classic Han-unification picks
/// (zh/ja/ko) — `Script::of_run` sees only codepoints, but zh/ja/ko text
/// shares codepoints across regional variants, so the `lang` tag (lowercased
/// `ComputedStyle.lang`) picks ahead of the script table.
pub fn lang_families(lang: Option<&str>) -> &'static [&'static str] {
    let Some(lang) = lang else { return &[] };
    match lang {
        l if l.starts_with("zh-tw") || l.starts_with("zh-hant") => {
            &["PingFang TC", "Hiragino Sans TC"]
        }
        l if l.starts_with("zh-hk") || l.starts_with("zh-mo") => &["PingFang HK", "PingFang TC"],
        l if l.starts_with("zh") => &["PingFang SC", "Hiragino Sans GB"],
        l if l.starts_with("ja") => &["Hiragino Kaku Gothic ProN", "Hiragino Mincho ProN"],
        l if l.starts_with("ko") => &["Apple SD Gothic Neo", "Nanum Gothic"],
        _ => &[],
    }
}

/// [`fallback_chain`] with D3 language preference: when the run is CJK or
/// Hangul and `lang` picks families, they sit between the requested chain
/// and the script table (codepoint inference stays the last resort — a
/// `ja` request must not have `zh` families ahead of it).
pub fn fallback_chain_with_lang(
    requested: &[String],
    sample: &str,
    lang: Option<&str>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for fam in requested {
        for part in fam.split(',') {
            push_unique(&mut out, part);
        }
    }
    let script = Script::of_run(sample);
    if matches!(script, Script::Cjk | Script::Hangul) {
        for fam in lang_families(lang) {
            push_unique(&mut out, fam);
        }
    }
    for fam in fallback_families(script) {
        push_unique(&mut out, fam);
    }
    // Generic families named in the chain resolve to their platform lists.
    let generics: Vec<String> = out.clone();
    for g in &generics {
        for fam in generic_family(g) {
            push_unique(&mut out, fam);
        }
    }
    if out.is_empty() {
        out.push("Helvetica Neue".to_string());
    }
    out
}

fn push_unique(out: &mut Vec<String>, name: &str) {
    let name = name.trim().trim_matches('"').trim_matches('\'').trim();
    if !name.is_empty() && !out.iter().any(|e| e.eq_ignore_ascii_case(name)) {
        out.push(name.to_string());
    }
}

/// One registered `@font-face` (B11): a family plus the descriptors that
/// tell its faces apart. A family shipping a 400 and a 700 face registers
/// two entries — the registry keys on the face, not the family name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFontFace {
    pub family: String,
    /// `font-weight` (1–1000); 400 = normal.
    pub weight: u32,
    /// `font-style` keyword (`normal`/`italic`/`oblique`).
    pub style: String,
}

/// Process-global registry of `@font-face` faces the engine has downloaded
/// and handed to the platform (Phase N5, descriptors per B11). Chains
/// resolve requested names through the platform first, so a registered
/// family wins by position, not by flag — this set is bookkeeping (what's
/// usable) plus the test surface. Entries are keyed on
/// (family, weight, style): the install pass asks [`web_font_registered`]
/// with the face's own descriptors, so a second weight of an already-loaded
/// family is still fetched instead of being mistaken for a duplicate.
pub fn register_web_font_face(family: &str, weight: u32, style: &str) {
    let family = family.trim();
    if family.is_empty() {
        return;
    }
    let style = normalize_style(style);
    let mut set = web_font_registry().lock().unwrap();
    if !set
        .iter()
        .any(|f| f.family.eq_ignore_ascii_case(family) && f.weight == weight && f.style == style)
    {
        set.push(WebFontFace {
            family: family.to_string(),
            weight,
            style,
        });
    }
}

/// Whether a face with exactly these descriptors is registered. A
/// different weight or slant is a *different* face and answers `false`, so
/// the caller installs it (per CSS, a family keeps one face per
/// weight/slant combination).
pub fn web_font_registered(family: &str, weight: u32, style: &str) -> bool {
    let family = family.trim();
    let style = normalize_style(style);
    web_font_registry()
        .lock()
        .unwrap()
        .iter()
        .any(|f| f.family.eq_ignore_ascii_case(family) && f.weight == weight && f.style == style)
}

/// Every registered face, in insertion order (one entry per
/// family/weight/slant combination).
pub fn registered_web_font_faces() -> Vec<WebFontFace> {
    web_font_registry().lock().unwrap().clone()
}

/// Families registered so far (insertion order, one entry per family even
/// when several of its faces are installed).
pub fn registered_web_fonts() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for face in web_font_registry().lock().unwrap().iter() {
        if !out.iter().any(|f| f.eq_ignore_ascii_case(&face.family)) {
            out.push(face.family.clone());
        }
    }
    out
}

/// Register the default upright `normal` (400) face of a family — the
/// descriptor-less entry point kept for callers that only know a name.
pub fn register_web_font(family: &str) {
    register_web_font_face(family, 400, "normal");
}

fn normalize_style(style: &str) -> String {
    let s = style.trim().to_ascii_lowercase();
    if s.is_empty() {
        "normal".to_string()
    } else {
        s
    }
}

fn web_font_registry() -> &'static std::sync::Mutex<Vec<WebFontFace>> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<Vec<WebFontFace>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_inference_covers_major_scripts() {
        assert_eq!(Script::of('a'), Script::Latin);
        assert_eq!(Script::of('ع'), Script::Arabic);
        assert_eq!(Script::of('ש'), Script::Hebrew);
        assert_eq!(Script::of('अ'), Script::Devanagari);
        assert_eq!(Script::of('ক'), Script::Bengali);
        assert_eq!(Script::of('ก'), Script::Thai);
        assert_eq!(Script::of('漢'), Script::Cjk);
        assert_eq!(Script::of('한'), Script::Hangul);
        assert_eq!(Script::of('😀'), Script::Emoji);
        assert_eq!(Script::of_run("hello"), Script::Latin);
        assert_eq!(Script::of_run("hello مرحبا"), Script::Arabic);
        assert_eq!(Script::of_run(""), Script::Latin);
    }

    #[test]
    fn chains_request_first_then_script_fallback() {
        let chain = fallback_chain(&["Unknown Font".into(), "serif".into()], "hello");
        assert_eq!(chain[0], "Unknown Font");
        assert!(chain.contains(&"Georgia".to_string()), "{chain:?}");
        let chain = fallback_chain(&["Helvetica".into()], "مرحبا");
        assert_eq!(chain[0], "Helvetica");
        assert!(chain.contains(&"Geeza Pro".to_string()), "{chain:?}");
        // Deduplication is case-insensitive.
        let chain = fallback_chain(&["helvetica".into(), "Helvetica".into()], "x");
        assert_eq!(
            chain
                .iter()
                .filter(|f| f.eq_ignore_ascii_case("helvetica"))
                .count(),
            1
        );
    }

    #[test]
    fn emoji_routes_to_image_strikes() {
        assert!(is_emoji_presentation('😀'));
        assert!(is_emoji_presentation('★'));
        assert!(!is_emoji_presentation('a'));
        assert_eq!(Script::of('😀'), Script::Emoji);
        assert!(fallback_families(Script::Emoji).contains(&"Apple Color Emoji"));
    }

    #[test]
    fn d3_lang_picks_han_unification_families() {
        // Latin runs are untouched by lang (the script table has no pick).
        let chain = fallback_chain_with_lang(&["Helvetica".into()], "hello", Some("zh-cn"));
        assert_eq!(chain[0], "Helvetica");
        assert!(
            !chain.iter().any(|f| f.starts_with("PingFang")),
            "{chain:?}"
        );
        // CJK + lang: language families land between requested and script
        // table; zh-hant prefers Traditional.
        let chain = fallback_chain_with_lang(&["Custom".into()], "漢字", Some("zh-hant-tw"));
        assert!(chain.contains(&"PingFang TC".to_string()), "{chain:?}");
        assert!(chain.contains(&"Hiragino Sans GB".to_string()), "{chain:?}");
        let chain = fallback_chain_with_lang(&[], "漢字", Some("ja"));
        assert!(
            chain.contains(&"Hiragino Kaku Gothic ProN".to_string()),
            "{chain:?}"
        );
        // No lang → legacy chain (script table only).
        assert_eq!(
            fallback_chain_with_lang(&["X".into()], "漢字", None),
            fallback_chain(&["X".into()], "漢字")
        );
        // Korean picks Hangul families ahead of the script table's.
        let chain = fallback_chain_with_lang(&[], "한글", Some("ko"));
        assert_eq!(
            chain.first(),
            Some(&"Apple SD Gothic Neo".to_string()),
            "{chain:?}"
        );
    }

    /// Real Core Text enumeration (macOS-only by standing decision; other
    /// platforms return empty and hosts substitute their enumerator).
    #[cfg(target_os = "macos")]
    #[test]
    fn core_text_enumerates_system_families() {
        let fams = available_families();
        assert!(fams.len() > 20, "a real Mac ships dozens of families");
        for expected in ["Helvetica", "Menlo", "Geeza Pro", "Apple Color Emoji"] {
            assert!(fams.iter().any(|f| f == expected), "missing {expected}");
        }
        // Sorted, deduplicated.
        assert!(fams.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn web_font_registry_records_families_once() {
        let tag = format!("KitaeTestFace{}", line!());
        register_web_font(&tag);
        register_web_font(&tag.to_ascii_lowercase());
        assert!(registered_web_fonts().iter().any(|f| f == &tag));
        assert_eq!(
            registered_web_fonts()
                .iter()
                .filter(|f| f.eq_ignore_ascii_case(&tag))
                .count(),
            1
        );
    }

    /// B11: two weights (and two slants) of one family are separate faces;
    /// the family list still shows the family once.
    #[test]
    fn web_font_registry_separates_weights_and_styles() {
        let fam = format!("KitaeDuplexFace{}", line!());
        assert!(!web_font_registered(&fam, 400, "normal"));
        register_web_font_face(&fam, 400, "normal");
        register_web_font_face(&fam, 700, "normal");
        register_web_font_face(&fam, 400, "italic");
        // Same weight, different slant → its own face.
        assert!(web_font_registered(&fam, 400, "normal"));
        assert!(web_font_registered(&fam, 700, "normal"));
        assert!(web_font_registered(&fam, 400, "italic"));
        // A weight/slant never registered answers false, so the install
        // pass still fetches it instead of mistaking it for a duplicate.
        assert!(!web_font_registered(&fam, 500, "normal"));
        assert!(!web_font_registered(&fam, 700, "italic"));
        // Case and keyword spelling do not create extra faces; blank style
        // means upright.
        register_web_font_face(&fam.to_ascii_lowercase(), 400, "NORMAL");
        register_web_font_face(&fam, 400, "");
        assert_eq!(
            registered_web_font_faces()
                .iter()
                .filter(|f| f.family.eq_ignore_ascii_case(&fam))
                .count(),
            3
        );
        // The family list is deduplicated across its faces.
        assert_eq!(
            registered_web_fonts()
                .iter()
                .filter(|f| f.eq_ignore_ascii_case(&fam))
                .count(),
            1
        );
        // `register_web_font` is the descriptor-less 400/upright face.
        let plain = format!("KitaePlainFace{}", line!());
        register_web_font(&plain);
        assert!(web_font_registered(&plain, 400, "normal"));
        assert!(!web_font_registered(&plain, 700, "normal"));
    }
}
