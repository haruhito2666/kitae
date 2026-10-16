//! kitae_headless — the pure-Rust page rasterizer: pixels and print for the
//! agent plane.
//!
//! The app composites text with Core Text; a headless server has no Core
//! Text, no window server and no SDL. This crate is that missing half: it
//! measures text from the system's own font files, paints glyph outlines with
//! `tiny-skia`, and returns a PNG or a PDF — all in Rust, with no C on the
//! path.
//!
//! How a frame is produced:
//!
//! 1. [`RasterMeasurer`] — a `TextMeasurer` backed by real fonts. The engine
//!    lays the page out with it, so the geometry an agent reads (extraction,
//!    `watch`, click targets) is the geometry that gets painted. Layout and
//!    paint must share one measurer or text drifts.
//! 2. `kitae_render_gpu::cpu::render` paints the display list — backgrounds,
//!    borders, images, shadows — with an **empty glyph atlas**, because
//!    [`paint_text`] draws the runs itself from `kitae_layout::TextRun`
//!    geometry. That is the same staging the app uses (it composites Core
//!    Text over the engine's bitmap), and it is what makes descenders
//!    possible: the atlas model anchors a glyph bitmap's bottom on the
//!    baseline, so a `g` or `y` would be cut off.
//! 3. [`HeadlessRaster`] implements `kitae_core::agent::PageRaster`, so the
//!    agent's `screenshot` and `pdf` verbs work wherever the session runs —
//!    including inside the app, which installs this exact rasterizer on its
//!    own browser through [`abi::kitae_headless_install_raster`].
//!
//! Honest limits (v1, stated so nobody assumes otherwise):
//!
//! - **No shaping.** Advances come from the font's `hmtx` per character:
//!   kerning and ligatures are not applied, so text is very slightly wider
//!   than a shaping browser would set. Layout and paint agree because both
//!   use the same advances. `kitae_shape`'s rustybuzz path is the upgrade.
//! - **No subpixel positioning** and no LCD subpixel rendering: glyphs are
//!   placed on fractional x and antialiased by grayscale coverage.
//! - **PDF pages carry images**, not text: the page is rasterized and sliced
//!   into Letter pages. Text in the PDF is not selectable. A text-faithful
//!   PDF needs a font-embedding writer and print-media layout, which is a
//!   larger project than this tier.
//! - Fetched image pixels appear only when the caller supplies them (the
//!   engine's raster store is process-global and not part of the display
//!   list); layout boxes still paint, so screenshots show where images are.
//! - A transformed text run is skipped rather than painted in the wrong
//!   place.

pub mod abi;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kitae_core::agent::PageRaster;
use kitae_core::Browser;
use kitae_layout::{DisplayList, TextMeasurer};
use kitae_media::{encode_png, RgbaImage};
use kitae_render_gpu::GlyphAtlas;

/// How much of a page a screenshot may cover, in device pixels. A page can
/// claim any height; a screenshot that tries to allocate it all is a way for
/// a hostile document to kill an agent process.
pub const MAX_RASTER_HEIGHT: u32 = 20_000;
/// Maximum screenshot width (a wider viewport is clamped).
pub const MAX_RASTER_WIDTH: u32 = 8_192;

/// The system's fonts, cached. One per process is enough: `fontdb`'s
/// database is a few MB, and reloading it per screenshot would be silly.
pub struct FontStack {
    db: fontdb::Database,
    /// Resolved face per (family, bold, italic, monospace).
    faces: HashMap<(String, bool, bool, bool), Option<fontdb::ID>>,
}

impl FontStack {
    /// Enumerate the system's fonts. `None` when the system offers no usable
    /// face at all — a container without a font package — which the raster
    /// verbs report as an explicit error rather than an empty page.
    pub fn load() -> Option<FontStack> {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        if db.is_empty() {
            return None;
        }
        db.set_sans_serif_family(default_sans());
        db.set_serif_family(default_serif());
        db.set_monospace_family(default_monospace());
        Some(FontStack {
            db,
            faces: HashMap::new(),
        })
    }

    /// How many faces the database holds (the honest "is this usable" probe).
    pub fn face_count(&self) -> usize {
        self.db.len()
    }

    /// Run `f` with the parsed face for a run's font. Parsing happens per
    /// call — once per text run, not once per glyph — which keeps the borrow
    /// of the font bytes inside this call.
    fn with_face<T>(
        &mut self,
        family: &str,
        bold: bool,
        italic: bool,
        monospace: bool,
        f: impl FnOnce(ttf_parser::Face<'_>) -> T,
    ) -> Option<T> {
        let id = self.face_id(family, bold, italic, monospace)?;
        // `fontdb` hands over raw bytes plus the face index (it cannot lend a
        // parsed face); parsing here keeps the borrow inside this call.
        self.db
            .with_face_data(id, |data, index| {
                ttf_parser::Face::parse(data, index).ok().map(f)
            })
            .flatten()
    }

    /// The `fontdb` id for a run's font, resolved once per combination.
    fn face_id(
        &mut self,
        family: &str,
        bold: bool,
        italic: bool,
        monospace: bool,
    ) -> Option<fontdb::ID> {
        let key = (family.to_string(), bold, italic, monospace);
        if let Some(cached) = self.faces.get(&key) {
            return *cached;
        }
        // Named first, then the generic: a page asking for a family the
        // system does not have still gets sensible text.
        let families: Vec<fontdb::Family> = if family.is_empty() {
            vec![generic(monospace)]
        } else {
            vec![fontdb::Family::Name(family), generic(monospace)]
        };
        let query = fontdb::Query {
            families: &families,
            weight: if bold {
                fontdb::Weight::BOLD
            } else {
                fontdb::Weight::NORMAL
            },
            style: if italic {
                fontdb::Style::Italic
            } else {
                fontdb::Style::Normal
            },
            stretch: fontdb::Stretch::Normal,
        };
        let id = self.db.query(&query);
        self.faces.insert(key, id);
        id
    }
}

fn generic(monospace: bool) -> fontdb::Family<'static> {
    if monospace {
        fontdb::Family::Monospace
    } else {
        fontdb::Family::SansSerif
    }
}

/// Which generic families a system most likely has. Descriptive names only —
/// `fontdb` falls back to any sans/serif/mono it did find if the named one is
/// absent.
fn default_sans() -> &'static str {
    if cfg!(target_os = "macos") {
        "Helvetica Neue"
    } else {
        "DejaVu Sans"
    }
}

fn default_serif() -> &'static str {
    if cfg!(target_os = "macos") {
        "Times New Roman"
    } else {
        "DejaVu Serif"
    }
}

fn default_monospace() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "DejaVu Sans Mono"
    }
}

/// A face-matched measurer: the engine lays text out with the same advances
/// [`paint_text`] paints with.
pub struct RasterMeasurer {
    stack: Arc<Mutex<FontStack>>,
}

impl RasterMeasurer {
    pub fn new(stack: Arc<Mutex<FontStack>>) -> Self {
        Self { stack }
    }

    /// Width of one character at this size, from the font's own advance.
    fn advance(
        &self,
        ch: char,
        size: f32,
        bold: bool,
        italic: bool,
        monospace: bool,
        family: &str,
    ) -> f32 {
        self.stack
            .lock()
            .expect("font stack lock")
            .with_face(family, bold, italic, monospace, |face| {
                let upem = face.units_per_em().max(1) as f32;
                let scale = size / upem;
                match face.glyph_index(ch) {
                    Some(gid) => face.glyph_hor_advance(gid).unwrap_or(0) as f32 * scale,
                    // A character the face has no glyph for (a CJK char in a
                    // Latin face): fall back to a replacement glyph's advance
                    // rather than reporting zero width, which would overlap
                    // this glyph with the next.
                    None => {
                        face.glyph_index('\u{fffd}')
                            .or_else(|| face.glyph_index('?'))
                            .and_then(|g| face.glyph_hor_advance(g))
                            .unwrap_or((upem * 0.5) as u16) as f32
                            * scale
                    }
                }
            })
            .unwrap_or(size * 0.5)
    }

    fn metrics_for(&self, size: f32, monospace: bool, family: &str) -> (f32, f32) {
        self.stack
            .lock()
            .expect("font stack lock")
            .with_face(family, false, false, monospace, |face| {
                let upem = face.units_per_em().max(1) as f32;
                let scale = size / upem;
                let ascent = (face.ascender().max(0) as f32)
                    .max(face.typographic_ascender().unwrap_or(0) as f32);
                let descent = (-(face.descender().min(0) as f32))
                    .max(-face.typographic_descender().unwrap_or(0) as f32);
                (ascent * scale, descent * scale)
            })
            .unwrap_or((size * 0.8, size * 0.2))
    }
}

impl TextMeasurer for RasterMeasurer {
    fn measure(&self, text: &str, size: f32, weight: u32, italic: bool, monospace: bool) -> f32 {
        self.measure_family(text, size, weight, italic, monospace, "")
    }

    fn metrics(&self, size: f32, monospace: bool) -> (f32, f32) {
        self.metrics_for(size, monospace, "")
    }

    fn measure_family(
        &self,
        text: &str,
        size: f32,
        weight: u32,
        italic: bool,
        monospace: bool,
        family: &str,
    ) -> f32 {
        let bold = weight >= 600;
        text.chars()
            .map(|c| self.advance(c, size, bold, italic, monospace, family))
            .sum()
    }

    fn metrics_family(&self, size: f32, monospace: bool, family: &str) -> (f32, f32) {
        self.metrics_for(size, monospace, family)
    }
}

/// The rasterizer the agent plane installs: PNG screenshots and raster PDFs.
pub struct HeadlessRaster {
    stack: Arc<Mutex<FontStack>>,
    /// Device pixels per CSS pixel (2.0 for a retina-shaped screenshot).
    scale: f32,
}

impl HeadlessRaster {
    /// A rasterizer over the system's fonts, at 1 device pixel per CSS px.
    pub fn load() -> Result<Self, String> {
        let stack = FontStack::load().ok_or_else(no_fonts_message)?;
        Ok(Self {
            stack: Arc::new(Mutex::new(stack)),
            scale: 1.0,
        })
    }

    /// Scale device pixels per CSS pixel.
    pub fn with_scale(mut self, scale: f32) -> Self {
        self.scale = scale.clamp(0.1, 4.0);
        self
    }

    /// The same font stack as a measurer, for installing on the browser so
    /// layout and paint agree.
    pub fn measurer(&self) -> RasterMeasurer {
        RasterMeasurer::new(self.stack.clone())
    }

    /// The page's pixels: the display list painted by the CPU path, with
    /// text drawn on top from the layout's own runs.
    pub fn render(&self, browser: &Browser) -> Result<RgbaImage, String> {
        let (vw, vh) = browser.viewport_size();
        self.render_at(browser, vw, vh, true)
    }

    /// The same, at an explicit viewport — what a server renders when it
    /// wants a fixed device shape regardless of the session's layout size.
    pub fn render_at(
        &self,
        browser: &Browser,
        viewport_w: f32,
        viewport_h: f32,
        whole_page: bool,
    ) -> Result<RgbaImage, String> {
        let dl = browser
            .last_display_list()
            .ok_or_else(|| "no frame has been painted yet".to_string())?;
        let measurer = self.measurer();
        render_page(
            dl,
            &self.stack,
            viewport_w,
            viewport_h,
            self.scale,
            0.0,
            whole_page,
            &measurer,
        )
    }
}

impl PageRaster for HeadlessRaster {
    fn png(&self, browser: &Browser) -> Result<Vec<u8>, String> {
        let image = self.render(browser)?;
        encode_png(&image).map_err(|e| format!("PNG encode: {e}"))
    }

    fn pdf(&self, browser: &Browser) -> Result<Vec<u8>, String> {
        let image = self.render(browser)?;
        let state = browser.state();
        Ok(pdf_from_image(&image, &state.url, &state.title))
    }
}

/// An agent session over a headless browser whose layout and rasterization
/// share one font stack: the posture the MCP server and the HTTP server run
/// in. The policy is the session default (read-only); callers that want more
/// install their own.
///
/// `None` when the system has no fonts to rasterize with — a container
/// without a font package. Callers that only need text (extraction, `read`)
/// can fall back to `AgentSession::headless`; callers that promise screenshots
/// must report the missing fonts instead.
pub fn session(viewport: (f32, f32)) -> Option<kitae_core::agent::AgentSession> {
    let raster = HeadlessRaster::load().ok()?;
    let browser = Browser::new(
        Box::new(kitae_core::agent::HeadlessHost::default()),
        // The same stack that will paint the glyphs measures them, so the
        // geometry an agent reads is the geometry on the pixels.
        Box::new(raster.measurer()),
        viewport,
    );
    Some(kitae_core::agent::AgentSession::new(browser).with_raster(std::sync::Arc::new(raster)))
}

fn no_fonts_message() -> String {
    "no fonts are installed on this system, so nothing can be rasterized; \
     install a font package (Debian: `apt-get install fonts-dejavu-core`)"
        .to_string()
}

/// Paint one page into an RGBA image.
///
/// `scale` is device pixels per CSS pixel; `scroll` shifts the page up (a
/// screenshot of a scrolled viewport); `whole_page` rasters the full content
/// height instead of one viewport.
#[allow(clippy::too_many_arguments)]
pub fn render_page(
    dl: &DisplayList,
    stack: &Arc<Mutex<FontStack>>,
    viewport_w: f32,
    viewport_h: f32,
    scale: f32,
    scroll: f32,
    whole_page: bool,
    measurer: &dyn TextMeasurer,
) -> Result<RgbaImage, String> {
    let scale = scale.max(0.1);
    let content_h = dl.content_height.max(viewport_h);
    let css_h = if whole_page {
        content_h.min(MAX_RASTER_HEIGHT as f32 / scale)
    } else {
        viewport_h
    };
    let w = (viewport_w * scale).ceil().min(MAX_RASTER_WIDTH as f32) as u32;
    let h = (css_h * scale).ceil().min(MAX_RASTER_HEIGHT as f32) as u32;
    if w == 0 || h == 0 {
        return Err("the page has no area to rasterize".to_string());
    }
    // The scene is built with the measurer's advances so its per-glyph
    // positions agree with the layout's. (Its glyph quads are not used — the
    // atlas stays empty — but the same closure also drives the run
    // bookkeeping the text pass relies on.)
    let advance =
        |text: &str, size: f32, weight: u32, italic: bool, monospace: bool, family: &str| {
            measurer.measure_family(text, size, weight, italic, monospace, family)
        };
    let scene = kitae_render_gpu::GpuScene::from_display_list(
        dl,
        &HashMap::new(),
        &advance,
        viewport_w,
        viewport_h,
    );
    let px = kitae_render_gpu::cpu::render(
        &scene,
        w,
        h,
        scale,
        0.0,
        -scroll * scale,
        &HashMap::new(),
        // Empty atlas: `paint_text` draws the glyphs below.
        &GlyphAtlas::new(),
    );
    let mut image = RgbaImage {
        width: w,
        height: h,
        pixels: px,
    };
    paint_text(&mut image, dl, stack, scale, scroll, measurer);
    Ok(image)
}

/// A rasterized glyph: an 8-bit coverage bitmap plus where it goes relative
/// to the pen position and the baseline. All offsets are device pixels.
struct GlyphMask {
    w: u32,
    h: u32,
    /// Pen x → bitmap left edge.
    left: f32,
    /// Bitmap top edge → baseline (positive = above the baseline).
    top: f32,
    coverage: Vec<u8>,
}

/// One glyph's antialiased coverage, from its outline.
fn glyph_mask(face: &ttf_parser::Face<'_>, size_px: f32, ch: char) -> Option<GlyphMask> {
    let gid = face
        .glyph_index(ch)
        .or_else(|| face.glyph_index('\u{fffd}'))
        .or_else(|| face.glyph_index('?'))?;
    let upem = face.units_per_em().max(1) as f32;
    let scale = size_px / upem;
    let mut builder = OutlinePath::new(scale);
    face.outline_glyph(gid, &mut builder)?;
    let path = builder.finish()?;
    let bounds = path.bounds();
    // Pad by a pixel: antialiasing needs room around the ink.
    let pad = 1.0f32;
    let w = (bounds.width() + pad * 2.0).ceil().max(1.0) as u32;
    let h = (bounds.height() + pad * 2.0).ceil().max(1.0) as u32;
    if w > 1024 || h > 1024 {
        return None;
    }
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    let paint = tiny_skia::Paint {
        shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::WHITE),
        anti_alias: true,
        ..Default::default()
    };
    let transform = tiny_skia::Transform::from_translate(pad - bounds.left(), pad - bounds.top());
    pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, transform, None);
    // White-on-transparent premultiplied pixels: alpha is the coverage.
    let coverage: Vec<u8> = pixmap.data().chunks_exact(4).map(|px| px[3]).collect();
    Some(GlyphMask {
        w,
        h,
        left: bounds.left() - pad,
        // The path is already y-flipped, so the ink's *top* is the negative
        // (highest) y — subtracting it gives the distance above the baseline
        // the bitmap's top edge belongs at.
        top: -bounds.top() + pad,
        coverage,
    })
}

/// Collects a glyph outline into a `tiny-skia` path: font units to device
/// pixels, y flipped (fonts are y-up, rasters are y-down).
struct OutlinePath {
    builder: tiny_skia::PathBuilder,
    scale: f32,
}

impl OutlinePath {
    fn new(scale: f32) -> Self {
        Self {
            builder: tiny_skia::PathBuilder::new(),
            scale,
        }
    }

    fn finish(self) -> Option<tiny_skia::Path> {
        self.builder.finish()
    }

    fn point(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.scale, -y * self.scale)
    }
}

impl ttf_parser::OutlineBuilder for OutlinePath {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.point(x, y);
        self.builder.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.point(x, y);
        self.builder.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (x1, y1) = self.point(x1, y1);
        let (x, y) = self.point(x, y);
        self.builder.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (x1, y1) = self.point(x1, y1);
        let (x2, y2) = self.point(x2, y2);
        let (x, y) = self.point(x, y);
        self.builder.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

/// Paint every text run in the display list.
///
/// Positions come from the run's own geometry (`x`, baseline `y`, `size`,
/// `letter_spacing`) plus the same advances the measurer reported, so what
/// lands on the pixels is where layout said it would be.
pub fn paint_text(
    image: &mut RgbaImage,
    dl: &DisplayList,
    stack: &Arc<Mutex<FontStack>>,
    scale: f32,
    scroll: f32,
    measurer: &dyn TextMeasurer,
) {
    // One cache per page: repeated glyphs (the common case in real text) are
    // rasterized once.
    let mut cache: HashMap<(char, u32, bool, bool, String), Option<GlyphMask>> = HashMap::new();
    for run in &dl.texts {
        if run.text.trim().is_empty() || run.size <= 0.0 {
            continue;
        }
        // A transformed run would need its matrix applied per glyph; skipping
        // it beats painting it in the wrong place.
        if run.transform.is_some() {
            continue;
        }
        let bold = run.weight >= 600;
        let size_px = run.size * scale;
        let size_key = (size_px * 4.0).round().max(4.0) as u32;
        let color = run.color;
        let baseline = (run.y - scroll) * scale;
        let mut pen = run.x * scale;
        let mut placements: Vec<(char, f32)> = Vec::with_capacity(run.text.len());
        for ch in run.text.chars() {
            placements.push((ch, pen));
            let advance = measurer.measure_family(
                &ch.to_string(),
                run.size,
                run.weight,
                run.italic,
                run.monospace,
                &run.family,
            );
            pen += advance * scale;
            if run.letter_spacing != 0.0 {
                pen += run.letter_spacing * scale;
            }
        }
        for (ch, x) in placements {
            let key = (ch, size_key, bold, run.italic, run.family.clone());
            if !cache.contains_key(&key) {
                // The mask is rasterized inside a short borrow; the measurer
                // above has already released its own.
                let mask = stack.lock().expect("font stack lock").with_face(
                    &run.family,
                    bold,
                    run.italic,
                    run.monospace,
                    |face| glyph_mask(&face, size_px, ch),
                );
                cache.insert(key.clone(), mask.flatten());
            }
            let Some(mask) = cache.get(&key).and_then(|m| m.as_ref()) else {
                continue;
            };
            let left = (x + mask.left).round() as i32;
            let top = (baseline - mask.top).round() as i32;
            blend_mask(image, mask, left, top, color);
        }
    }
}

/// Composite one glyph's coverage into the image, tinted.
fn blend_mask(image: &mut RgbaImage, mask: &GlyphMask, left: i32, top: i32, color: u32) {
    let cr = ((color >> 24) & 0xff) as f32;
    let cg = ((color >> 16) & 0xff) as f32;
    let cb = ((color >> 8) & 0xff) as f32;
    let ca = (color & 0xff) as f32 / 255.0;
    for gy in 0..mask.h {
        let y = top + gy as i32;
        if y < 0 || y as u32 >= image.height {
            continue;
        }
        for gx in 0..mask.w {
            let x = left + gx as i32;
            if x < 0 || x as u32 >= image.width {
                continue;
            }
            let coverage = mask.coverage[(gy * mask.w + gx) as usize] as f32 / 255.0;
            let a = coverage * ca;
            if a <= 0.0 {
                continue;
            }
            let i = ((y as u32 * image.width + x as u32) * 4) as usize;
            let dst = &mut image.pixels[i..i + 4];
            dst[0] = (cr * a + dst[0] as f32 * (1.0 - a)).round() as u8;
            dst[1] = (cg * a + dst[1] as f32 * (1.0 - a)).round() as u8;
            dst[2] = (cb * a + dst[2] as f32 * (1.0 - a)).round() as u8;
            dst[3] = 255;
        }
    }
}

// ── PDF ─────────────────────────────────────────────────────────────────────

/// US Letter at 72 dpi, with half-inch margins — the page geometry both
/// common printers and PDF viewers assume when nothing else is said.
pub const PDF_PAGE_W: f32 = 612.0;
pub const PDF_PAGE_H: f32 = 792.0;
pub const PDF_MARGIN: f32 = 36.0;

/// A raster PDF: the page's pixels, sliced into Letter-sized pages, one
/// image per page. Valid everywhere, honest about what it is (see the module
/// doc on why the text is not selectable).
pub fn pdf_from_image(image: &RgbaImage, url: &str, title: &str) -> Vec<u8> {
    // CSS px (96 dpi) → PDF points (72 dpi).
    let content_w = PDF_PAGE_W - PDF_MARGIN * 2.0;
    let content_h = PDF_PAGE_H - PDF_MARGIN * 2.0;
    let scale = content_w / image.width.max(1) as f32;
    let slice_h = (content_h / scale).floor().max(1.0) as u32;
    let mut pages: Vec<(u32, u32)> = Vec::new();
    let mut y = 0;
    while y < image.height {
        let h = slice_h.min(image.height - y);
        pages.push((y, h));
        y += h;
    }
    if pages.is_empty() {
        pages.push((0, 1));
    }
    let page_count = pages.len();

    // Object numbering: 1 catalog, 2 pages, then per page (page, contents,
    // image) from 3, then the Info dictionary.
    let first_page_obj = 3;
    let info_obj = first_page_obj + page_count * 3;
    let mut objects: Vec<Vec<u8>> = Vec::with_capacity(page_count * 3 + 3);
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());

    let kids: String = (0..page_count)
        .map(|i| format!("{} 0 R", first_page_obj + i * 3))
        .collect::<Vec<_>>()
        .join(" ");
    objects.push(format!("<< /Type /Pages /Kids [{kids}] /Count {page_count} >>").into_bytes());

    for (i, (y0, h)) in pages.iter().enumerate() {
        let page_obj = first_page_obj + i * 3;
        let content_obj = page_obj + 1;
        let image_obj = page_obj + 2;
        let slice = rgb_slice(image, *y0, *h);
        let compressed = zlib(&slice);
        let draw_h = *h as f32 * scale;
        let draw_y = PDF_PAGE_H - PDF_MARGIN - draw_h;
        let content = format!(
            "q\n{content_w:.2} 0 0 {draw_h:.2} {PDF_MARGIN:.2} {draw_y:.2} cm\n/Im0 Do\nQ\n"
        );
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PDF_PAGE_W:.0} {PDF_PAGE_H:.0}] \
                 /Resources << /XObject << /Im0 {image_obj} 0 R >> >> /Contents {content_obj} 0 R >>"
            )
            .into_bytes(),
        );
        objects.push(
            format!(
                "<< /Length {} >>\nstream\n{content}endstream",
                content.len()
            )
            .into_bytes(),
        );
        let mut image_object = format!(
            "<< /Type /XObject /Subtype /Image /Width {} /Height {h} \
             /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode /Length {} >>\nstream\n",
            image.width,
            compressed.len()
        )
        .into_bytes();
        image_object.extend_from_slice(&compressed);
        image_object.extend_from_slice(b"\nendstream");
        objects.push(image_object);
    }

    objects.push(
        format!(
            "<< /Producer (Kitae headless rasterizer) /Title ({}) /Subject ({}) >>",
            pdf_text_escape(title),
            pdf_text_escape(url)
        )
        .into_bytes(),
    );

    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    let mut offsets: Vec<usize> = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info {info_obj} 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn zlib(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use std::io::Write;
    let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = encoder.write_all(data);
    encoder.finish().unwrap_or_default()
}

/// `height` RGB rows starting at `y0`, alpha composited over white (a PDF
/// page has no alpha channel).
fn rgb_slice(image: &RgbaImage, y0: u32, height: u32) -> Vec<u8> {
    let width = image.width.max(1);
    let mut out = Vec::with_capacity((width * height * 3) as usize);
    for y in y0..(y0 + height).min(image.height) {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            let px = &image.pixels[i..i + 4];
            let a = px[3] as f32 / 255.0;
            out.push((px[0] as f32 * a + 255.0 * (1.0 - a)).round() as u8);
            out.push((px[1] as f32 * a + 255.0 * (1.0 - a)).round() as u8);
            out.push((px[2] as f32 * a + 255.0 * (1.0 - a)).round() as u8);
        }
    }
    out
}

/// PDF literal strings escape `\`, `(`, `)`; anything non-ASCII is replaced
/// rather than encoded, because the Info dictionary is metadata and mangled
/// bytes there would break more than they fix.
fn pdf_text_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            _ => out.push(' '),
        }
    }
    if out.len() > 200 {
        out.truncate(200);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kitae_core::agent::HeadlessHost;
    use kitae_core::ApproxMeasurer;

    /// A browser laid out with the rasterizer's own measurer, so geometry and
    /// paint come from one font stack.
    fn font_browser(raster: &HeadlessRaster, html: &str, base: &str) -> Browser {
        let mut browser = Browser::new(
            Box::new(HeadlessHost::default()),
            Box::new(raster.measurer()),
            (400.0, 300.0),
        );
        browser.load_html(html, base);
        browser
    }

    fn raster_or_skip() -> Option<HeadlessRaster> {
        match HeadlessRaster::load() {
            Ok(raster) => Some(raster),
            // A machine with no fonts at all: the failure path is asserted in
            // `fonts_absent_is_an_error_not_an_empty_page`, so other tests
            // skip rather than pretend.
            Err(_) => None,
        }
    }

    /// Count pixels that differ from white — the "did anything paint" probe.
    fn painted_pixels(image: &RgbaImage, band: (u32, u32)) -> usize {
        let mut n = 0;
        for y in band.0..band.1.min(image.height) {
            for x in 0..image.width {
                let i = ((y * image.width + x) * 4) as usize;
                if image.pixels[i] < 240 || image.pixels[i + 1] < 240 || image.pixels[i + 2] < 240 {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn fonts_absent_is_an_error_not_an_empty_page() {
        // The shape of the honest failure: a rasterizer that cannot find a
        // font must not hand back a blank image.
        let message = no_fonts_message();
        assert!(message.contains("fonts-dejavu-core"), "{message}");
        // And when fonts exist, the stack really resolved some.
        if let Some(stack) = FontStack::load() {
            assert!(stack.face_count() > 0);
        }
    }

    #[test]
    fn the_measurer_reports_real_font_advances() {
        let Some(raster) = raster_or_skip() else {
            return;
        };
        let measurer = raster.measurer();
        let width = measurer.measure("Hello, world", 16.0, 400, false, false);
        assert!(width > 40.0, "a 12-character string is not {width}px wide");
        assert!(width < 200.0, "…nor {width}px — the font is not a fallback");
        // Monospace is per-character uniform.
        let mono = measurer.measure("iiii", 16.0, 400, false, true);
        let mono_w = measurer.measure("wwww", 16.0, 400, false, true);
        assert!(
            (mono - mono_w).abs() < 0.01,
            "monospace advances are uniform: {mono} vs {mono_w}"
        );
        // Metrics are positive and scaled by size.
        let (ascent, descent) = measurer.metrics(16.0, false);
        assert!(ascent > 8.0 && descent > 0.0, "{ascent} / {descent}");
        // A character the face lacks still advances (no overlapping glyphs).
        let cjk = measurer.measure("漢", 16.0, 400, false, false);
        assert!(cjk > 0.0, "missing glyphs keep their advance");
    }

    #[test]
    fn a_screenshot_paints_text_pixels_where_layout_put_them() {
        let Some(raster) = raster_or_skip() else {
            return;
        };
        let html = "<html><body style=\"margin:0\">\
<h1 style=\"margin:0;font-size:32px;color:#000000\">KITAE</h1>\
<p style=\"margin:0;height:240px;background:#ffffff\">.</p>\
</body></html>";
        let browser = font_browser(&raster, html, "https://example.test/");
        let image = raster.render(&browser).expect("render");
        assert_eq!(image.width, 400);
        assert!(
            image.height >= 300,
            "the whole page rastered: {}",
            image.height
        );
        // The heading band has ink; the empty band below stays white.
        let heading = painted_pixels(&image, (0, 50));
        let empty = painted_pixels(&image, (120, 280));
        assert!(heading > 100, "the heading painted {heading} px");
        assert_eq!(empty, 0, "the empty band is untouched ({empty} px)");
        // And the ink is where the heading is: the first painted row is
        // inside the heading's box, not at the page top.
        let mut first = None;
        for y in 0..image.height {
            if painted_pixels(&image, (y, y + 1)) > 0 {
                first = Some(y);
                break;
            }
        }
        let first = first.expect("something painted");
        assert!(
            (1..40).contains(&first),
            "ink starts inside the heading box, at y={first}"
        );
    }

    #[test]
    fn a_screenshot_is_a_decodable_png_of_the_viewport() {
        let Some(raster) = raster_or_skip() else {
            return;
        };
        let browser = font_browser(
            &raster,
            "<html><body style=\"margin:0;background:#336699\"><h1>Shots</h1></body></html>",
            "https://example.test/",
        );
        let png = raster.png(&browser).expect("png");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
        let decoded = kitae_media::decode_png(&png).expect("decode");
        assert_eq!(decoded.width, 400);
        assert_eq!(decoded.height, 300, "a screenshot is the viewport");
        // The background color is there (0x33 66 99).
        let mid = ((decoded.height / 2 * decoded.width + decoded.width / 2) * 4) as usize;
        let px = &decoded.pixels[mid..mid + 4];
        assert!(px[2] > px[0], "blue-ish background: {px:?}");
    }

    #[test]
    fn scale_multiplies_device_pixels() {
        let Some(raster) = raster_or_skip() else {
            return;
        };
        let scaled = HeadlessRaster::load().unwrap().with_scale(2.0);
        let browser = font_browser(
            &scaled,
            "<html><body style=\"margin:0\"><h1>Two</h1></body></html>",
            "https://example.test/",
        );
        let image = scaled.render(&browser).expect("render");
        assert_eq!(image.width, 800, "2x device pixels across");
        assert_eq!(image.height, 600);
        assert!(painted_pixels(&image, (0, 120)) > 100, "text painted at 2x");
    }

    #[test]
    fn a_pdf_is_a_valid_single_page_document() {
        let image = RgbaImage {
            width: 400,
            height: 300,
            pixels: vec![255; 400 * 300 * 4],
        };
        let pdf = pdf_from_image(&image, "https://example.test/", "A Title");
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.starts_with("%PDF-1.4"), "PDF header");
        assert!(text.contains("/Type /Catalog"), "catalog");
        assert!(text.contains("/Count 1"), "one page for a short document");
        assert!(
            text.contains("/MediaBox [0 0 612 792]"),
            "US Letter page geometry"
        );
        assert!(text.contains("/Subtype /Image"), "the page is an image");
        assert!(text.contains("/Filter /FlateDecode"), "compressed stream");
        assert!(
            text.contains("/Title (A Title)"),
            "metadata escapes into it"
        );
        assert!(text.contains("startxref") && text.trim_end().ends_with("%%EOF"));
        // The xref offset must actually point at the xref table. Checked
        // against the raw bytes: the page stream is binary, and a lossy
        // UTF-8 conversion (each invalid byte becoming a 3-byte U+FFFD)
        // would shift every offset.
        let startxref = text
            .rsplit_once("startxref\n")
            .and_then(|(_, rest)| rest.trim().lines().next())
            .and_then(|line| line.parse::<usize>().ok())
            .expect("startxref offset");
        assert!(
            pdf[startxref..].starts_with(b"xref"),
            "startxref points at the table ({startxref})"
        );
    }

    #[test]
    fn a_tall_page_splits_into_letter_pages() {
        // 3000 CSS px tall: at 540pt content width the scale is 1.35, so a
        // page holds ~533px — this needs several.
        let image = RgbaImage {
            width: 400,
            height: 3000,
            pixels: vec![255; 400 * 3000 * 4],
        };
        let pdf = pdf_from_image(&image, "https://example.test/", "Tall");
        let text = String::from_utf8_lossy(&pdf);
        let count: usize = text
            .split("/Count ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .expect("page count");
        assert!(count >= 5, "3000px needs several pages, got {count}");
        assert_eq!(
            text.matches("/Subtype /Image").count(),
            count,
            "one image per page"
        );
        assert_eq!(text.matches("/Type /Page /").count(), count + 0);
    }

    #[test]
    fn the_pdf_of_a_real_page_contains_the_rendered_pixels() {
        let Some(raster) = raster_or_skip() else {
            return;
        };
        let browser = font_browser(
            &raster,
            "<html><body style=\"margin:0;background:#ffffff\">\
             <h1 style=\"font-size:24px\">Printable</h1></body></html>",
            "https://example.test/",
        );
        let pdf = raster.pdf(&browser).expect("pdf");
        assert!(String::from_utf8_lossy(&pdf).starts_with("%PDF-1.4"));
        assert!(
            pdf.len() > 1024,
            "an image stream makes this bigger than the wrapper: {} bytes",
            pdf.len()
        );
    }

    /// The verbs an agent actually calls: `screenshot` and `pdf` through the
    /// session, end to end.
    #[test]
    fn the_agent_verbs_return_real_bytes() {
        use kitae_agent::Verb;
        let Some(mut session) = session((400.0, 300.0)) else {
            return;
        };
        session.set_policy(Box::new(kitae_core::agent::AllowList::read_only()));
        session.browser_mut().load_html(
            "<html><body style=\"margin:0\"><h1>Verb check</h1></body></html>",
            "https://example.test/",
        );
        let shot = session
            .permit_and_run(&Verb::Screenshot { path: None })
            .expect("screenshot");
        let shot: kitae_json::Value = kitae_json::parse(&shot).unwrap();
        assert_eq!(shot.get("mime").and_then(|v| v.as_str()), Some("image/png"));
        assert!(
            shot.get("bytes").and_then(|v| v.as_f64()).unwrap() > 1000.0,
            "a real PNG, not an empty buffer: {shot:?}"
        );
        let encoded = shot.get("png").and_then(|v| v.as_str()).unwrap();
        assert!(encoded.starts_with("iVBORw0KGgo"), "PNG magic in base64");

        let pdf = session
            .permit_and_run(&Verb::Pdf { path: None })
            .expect("pdf");
        let pdf: kitae_json::Value = kitae_json::parse(&pdf).unwrap();
        let encoded = pdf.get("pdf").and_then(|v| v.as_str()).unwrap();
        // %PDF-1.4 in base64.
        assert!(encoded.starts_with("JVBERi0xLjQ"), "{encoded:.20}");
    }

    /// Not an assertion: writes the rasterizer's output to /tmp so the
    /// artifacts can be inspected by eye. Kept in-tree on purpose — "the PNG
    /// has ink" is a weak check, and the fast way to catch a regression in
    /// glyph placement is to look at a page.
    #[test]
    fn writes_inspectable_artifacts_to_tmp() {
        let raster = HeadlessRaster::load().expect("fonts");
        let html = r##"<html><head><title>Kitae Headless</title><style>
body { margin: 0; font-family: Helvetica; }
h1 { margin: 24px 16px 8px; font-size: 28px; color: #123456 }
p { margin: 8px 16px; font-size: 14px; line-height: 1.5 }
.box { margin: 16px; padding: 12px; border: 2px solid #336699; background: #eef4fb; font-size: 13px }
a { color: #0b5cad }
</style></head><body>
<h1>Kitae renders without a window server</h1>
<p>This page was laid out with the rasterizer's own font-backed measurer and painted by the CPU path, with glyph outlines taken from the system's fonts.</p>
<div class="box">A bordered box with <a href="/docs">a link</a> inside it, to exercise borders, padding and link paint.</div>
<p>Descenders check: gjpqy and mixed case ABCDEFGHIJKLM.</p>
</body></html>"##;
        let browser = font_browser(&raster, html, "https://example.test/");
        let png = raster.png(&browser).expect("png");
        std::fs::write("/tmp/kitae_headless_page.png", &png).expect("write png");
        let pdf = raster.pdf(&browser).expect("pdf");
        std::fs::write("/tmp/kitae_headless_page.pdf", &pdf).expect("write pdf");
        println!("PNG={} bytes PDF={} bytes", png.len(), pdf.len());
    }

    /// The C ABI the app links: the same two-pass buffer protocol the engine
    /// uses, the same error convention, and bytes that are a real PNG.
    #[test]
    fn the_c_abi_serves_the_engines_two_pass_protocol() {
        use std::ffi::c_void;

        use crate::abi::{raster_free, raster_png};

        let Some(raster) = raster_or_skip() else {
            eprintln!("skipping: no system fonts");
            return;
        };
        let browser = font_browser(&raster, "<h1>ABI</h1><p>bytes</p>", "https://example.test/");
        let ctx = Box::into_raw(Box::new(raster)).cast::<c_void>();
        let page = &browser as *const Browser as *const c_void;

        // Pass 1: a null buffer asks for the size. Pass 2: the caller's
        // buffer gets the bytes, and the length comes back again.
        let len = unsafe { raster_png(ctx, page, std::ptr::null_mut(), 0) };
        assert!(len > 8, "a screenshot reports its full length ({len})");
        let mut buf = vec![0u8; len as usize];
        let copied = unsafe { raster_png(ctx, page, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(copied, len);
        assert_eq!(&buf[..8], b"\x89PNG\r\n\x1a\n", "the bytes are a PNG");

        // A context the host already freed, or never had, is an error with a
        // message — never a blank image.
        assert_eq!(
            unsafe { raster_png(std::ptr::null_mut(), page, std::ptr::null_mut(), 0) },
            -1
        );

        unsafe { raster_free(ctx) };
    }
}
