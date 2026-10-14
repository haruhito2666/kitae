//! kitae_render — the safe Rust half of the FFI bridge to the C++/SDL2 shell.
//!
//! The shell (`native/`) owns the OS window, chrome, rasterization and the
//! event queue behind a flat C ABI (see `native/include/kitae_renderer.h`).
//! This crate mirrors that ABI in `#[repr(C)]` structs and exposes:
//!
//! * [`Renderer`] — lifecycle, display-list submission, event pump
//! * [`FfiMeasurer`] — a [`TextMeasurer`] backed by the shell's real fonts,
//!   so layout widths match what is painted
//! * [`store_links`] + the exported `kitae_app_hit_link` — the link hit-test
//!   the shell calls back into on mouse moves
//!
//! All page coordinates are logical points in page space; the shell applies
//! scroll and clips below the 52px chrome.

use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::Mutex;

use kitae_layout::{DisplayList, LinkArea, TextMeasurer};

// ---------------------------------------------------------------------------
// Raw ABI (mirrors native/include/kitae_renderer.h exactly)
// ---------------------------------------------------------------------------

#[repr(C)]
struct RawRenderer {
    _private: [u8; 0],
}

#[repr(C)]
struct RawRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: u32,
    radius: [f32; 4],
    paint_tag: u32,
    angle: f32,
    nstops: u32,
    offsets: [f32; 8],
    colors: [u32; 8],
    rcx: f32,
    rcy: f32,
    rradius: f32,
    /// Item-local 2×3 matrix (row-major; Q2). Applied only when `transformed`.
    m: [f32; 6],
    transformed: c_int,
}

#[repr(C)]
struct RawShadow {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: [f32; 4],
    color: u32,
    blur: f32,
    spread: f32,
    offset_x: f32,
    offset_y: f32,
    inset: c_int,
    /// Item-local 2×3 matrix (row-major; Q2). Applied only when `transformed`.
    m: [f32; 6],
    transformed: c_int,
}

#[repr(C)]
struct RawText {
    x: f32,
    y: f32, // baseline origin
    text: *const c_char,
    size: f32,
    weight: u32,
    italic: c_int,
    monospace: c_int,
    color: u32,
    /// Extra px after each glyph except the last (Q3). 0.0 = normal run.
    letter_spacing: f32,
    /// Item-local 2×3 matrix (row-major; Q2). Applied only when `transformed`.
    m: [f32; 6],
    transformed: c_int,
}

/// Mirrors `KitaeDLImage` (native/include/kitae_renderer.h): a decoded raster
/// plus its page-space box. `pixels` must outlive the call — the shell copies
/// on submit.
#[repr(C)]
struct RawImage {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fit: u32,
    payload: u64,
    width: u32,
    height: u32,
    pixels: *const u8,
    /// Item-local 2×3 matrix (row-major; Q2). Applied only when `transformed`.
    m: [f32; 6],
    transformed: c_int,
}

#[repr(C)]
struct RawEvent {
    kind: i32,
    a: f32,
    b: f32,
    url: [c_char; 4096],
}

extern "C" {
    fn kitae_renderer_init(width: c_int, height: c_int, title: *const c_char) -> *mut RawRenderer;
    fn kitae_renderer_set_title(r: *mut RawRenderer, title: *const c_char);
    fn kitae_renderer_set_address(r: *mut RawRenderer, url: *const c_char);
    fn kitae_renderer_viewport(r: *const RawRenderer, w: *mut c_int, h: *mut c_int);
    fn kitae_renderer_clear_list(r: *mut RawRenderer);
    fn kitae_renderer_push_rect(r: *mut RawRenderer, rect: *const RawRect);
    fn kitae_renderer_push_text(r: *mut RawRenderer, text: *const RawText);
    fn kitae_renderer_push_image(r: *mut RawRenderer, img: *const RawImage);
    fn kitae_renderer_push_shadow(r: *mut RawRenderer, shadow: *const RawShadow);
    fn kitae_renderer_set_scroll(r: *mut RawRenderer, scroll_y: f32, content_height: f32);
    fn kitae_renderer_measure_text(
        r: *const RawRenderer,
        text: *const c_char,
        size: f32,
        weight: u32,
        italic: c_int,
        monospace: c_int,
    ) -> f32;
    fn kitae_renderer_font_metrics(
        r: *const RawRenderer,
        size: f32,
        monospace: c_int,
        ascent: *mut f32,
        descent: *mut f32,
        line_gap: *mut f32,
    );
    fn kitae_renderer_poll_event(r: *mut RawRenderer, out: *mut RawEvent) -> c_int;
    fn kitae_renderer_destroy(r: *mut RawRenderer);
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// An input event, translated from the shell's event queue.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Quit,
    /// Address bar submit; carries the raw typed text / URL.
    Navigate(String),
    Back,
    Forward,
    Reload,
    /// Scroll by `dy` points (positive = down).
    Scroll(f32),
    /// Window resized: (viewport width, viewport height) of the page area.
    Resize(f32, f32),
    /// Click in page coordinates (scroll already included).
    Click(f32, f32),
}

// ---------------------------------------------------------------------------
// Renderer — safe wrapper with Drop
// ---------------------------------------------------------------------------

/// Handle to the native window. Drop closes it.
pub struct Renderer {
    raw: *mut RawRenderer,
}

// The shell is only touched from the main thread (SDL requirement); the app's
// event loop enforces that by construction.
unsafe impl Send for Renderer {}

/// UTF-8 → CString, dropping interior NULs (they cannot cross the ABI).
fn cstr(s: &str) -> CString {
    let cleaned: String = s.chars().filter(|&c| c != '\0').collect();
    // Filtering out '\0' guarantees CString::new succeeds.
    CString::new(cleaned).unwrap()
}

/// Convert a layout rect to its C ABI mirror (radius resolved against the
/// box width, paint mapped to the fixed tag/stops layout).
/// Identity 2×3 matrix (row-major) plus the C flag that says "apply it".
const NO_TRANSFORM: ([f32; 6], c_int) = ([1.0, 0.0, 0.0, 0.0, 1.0, 0.0], 0);

/// Layout matrix → ABI mirror: row-major 2×3 plus a flag (the identity is by
/// far the common case, so the shell can skip the quad path entirely).
fn map_transform(m: Option<kitae_layout::Matrix2d>) -> ([f32; 6], c_int) {
    match m {
        Some(t) => (t.0, 1),
        None => NO_TRANSFORM,
    }
}

fn raw_rect(r: &kitae_layout::Rect) -> RawRect {
    let radii = r.radius.resolve(16.0, 16.0, r.w.max(0.0));
    let (m, transformed) = map_transform(r.transform);
    let mut out = RawRect {
        x: r.x,
        y: r.y,
        w: r.w,
        h: r.h,
        color: r.color,
        radius: radii,
        paint_tag: 0,
        angle: 0.0,
        nstops: 0,
        offsets: [0.0; 8],
        colors: [0; 8],
        rcx: 0.5,
        rcy: 0.5,
        rradius: -1.0,
        m,
        transformed,
    };
    match &r.paint {
        kitae_layout::RectPaint::Solid => {}
        kitae_layout::RectPaint::LinearGradient { angle_deg, stops } => {
            out.paint_tag = 1;
            out.angle = *angle_deg;
            fill_stops(&mut out, stops);
        }
        kitae_layout::RectPaint::RadialGradient {
            cx,
            cy,
            radius,
            stops,
        } => {
            out.paint_tag = 2;
            out.rcx = *cx;
            out.rcy = *cy;
            out.rradius = radius.unwrap_or(-1.0);
            fill_stops(&mut out, stops);
        }
        // B5 conic: angle in `angle`, center fractions in rcx/rcy.
        kitae_layout::RectPaint::ConicGradient {
            angle_deg,
            cx,
            cy,
            stops,
        } => {
            out.paint_tag = 4;
            out.angle = *angle_deg;
            out.rcx = *cx;
            out.rcy = *cy;
            fill_stops(&mut out, stops);
        }
        kitae_layout::RectPaint::Image(_) => {
            out.paint_tag = 3;
        }
    }
    out
}

fn fill_stops(out: &mut RawRect, stops: &[(f32, u32)]) {
    let n = stops.len().min(8);
    out.nstops = n as u32;
    for (i, (offset, color)) in stops.iter().take(8).enumerate() {
        out.offsets[i] = *offset;
        out.colors[i] = *color;
    }
}

fn raw_solid(x: f32, y: f32, w: f32, h: f32, color: u32) -> RawRect {
    RawRect {
        x,
        y,
        w,
        h,
        color,
        radius: [0.0; 4],
        paint_tag: 0,
        angle: 0.0,
        nstops: 0,
        offsets: [0.0; 8],
        colors: [0; 8],
        rcx: 0.5,
        rcy: 0.5,
        rradius: -1.0,
        m: NO_TRANSFORM.0,
        transformed: NO_TRANSFORM.1,
    }
}

impl Renderer {
    /// Open the window. Returns `None` if SDL/video init fails.
    pub fn new(width: i32, height: i32, title: &str) -> Option<Renderer> {
        let title = cstr(title);
        let raw = unsafe { kitae_renderer_init(width, height, title.as_ptr()) };
        if raw.is_null() {
            None
        } else {
            Some(Renderer { raw })
        }
    }

    /// Window/tab title (chrome shows it next to the address bar).
    pub fn set_title(&self, title: &str) {
        let t = cstr(title);
        unsafe { kitae_renderer_set_title(self.raw, t.as_ptr()) }
    }

    /// Reflect the current URL in the chrome address bar.
    pub fn set_address(&self, url: &str) {
        let u = cstr(url);
        unsafe { kitae_renderer_set_address(self.raw, u.as_ptr()) }
    }

    /// Size of the page viewport (window minus chrome), logical points.
    pub fn viewport(&self) -> (f32, f32) {
        let (mut w, mut h) = (0 as c_int, 0 as c_int);
        unsafe { kitae_renderer_viewport(self.raw, &mut w, &mut h) };
        (w as f32, h as f32)
    }

    /// Hand a full display list to the shell. The shell keeps its own copy;
    /// call again after any layout change or scroll.
    pub fn submit(&self, dl: &DisplayList, scroll_y: f32) {
        unsafe {
            kitae_renderer_clear_list(self.raw);
            for s in &dl.shadows {
                let radii = s.radius.resolve(16.0, 16.0, s.w.max(0.0));
                let (m, transformed) = map_transform(s.transform);
                let raw = RawShadow {
                    x: s.x,
                    y: s.y,
                    w: s.w,
                    h: s.h,
                    radius: radii,
                    color: s.color,
                    blur: s.blur,
                    spread: s.spread,
                    offset_x: s.offset_x,
                    offset_y: s.offset_y,
                    inset: s.inset as c_int,
                    m,
                    transformed,
                };
                kitae_renderer_push_shadow(self.raw, &raw);
            }
            for r in &dl.rects {
                kitae_renderer_push_rect(self.raw, &raw_rect(r));
            }
            // Images: resolve the payload id against the engine's raster
            // side-table (J3/J5 — plain QTSQ images and video frames both
            // land here) and push the decoded RGBA. Payload 0 or an
            // unresolvable id paints the placeholder box.
            for im in &dl.images {
                let mut pushed = false;
                if let Some(payload) = im.payload {
                    if let Some(raster) = kitae_core::qtsq_raster_for_id(payload) {
                        let (m, transformed) = map_transform(im.transform);
                        let raw = RawImage {
                            x: im.x,
                            y: im.y,
                            w: im.w,
                            h: im.h,
                            fit: match im.fit {
                                kitae_style::ObjectFit::Fill => 0u32,
                                kitae_style::ObjectFit::Contain => 1u32,
                                kitae_style::ObjectFit::Cover => 2u32,
                                kitae_style::ObjectFit::None => 3u32,
                                kitae_style::ObjectFit::ScaleDown => 4u32,
                            },
                            payload,
                            width: raster.width,
                            height: raster.height,
                            pixels: raster.pixels.as_ptr(),
                            m,
                            transformed,
                        };
                        kitae_renderer_push_image(self.raw, &raw);
                        pushed = true;
                    }
                }
                if !pushed {
                    let bg = raw_solid(im.x, im.y, im.w, im.h, 0xE5E7EBFF);
                    kitae_renderer_push_rect(self.raw, &bg);
                    // 1px outline so the box reads against white pages.
                    let border = 0x9AA0A6FF;
                    if im.w > 2.0 && im.h > 2.0 {
                        for b in [
                            raw_solid(im.x, im.y, im.w, 1.0, border),
                            raw_solid(im.x, im.y + im.h - 1.0, im.w, 1.0, border),
                            raw_solid(im.x, im.y, 1.0, im.h, border),
                            raw_solid(im.x + im.w - 1.0, im.y, 1.0, im.h, border),
                        ] {
                            kitae_renderer_push_rect(self.raw, &b);
                        }
                    }
                }
            }
            for t in &dl.texts {
                let text = cstr(&t.text);
                let (m, transformed) = map_transform(t.transform);
                let rt = RawText {
                    x: t.x,
                    y: t.y,
                    text: text.as_ptr(),
                    size: t.size,
                    weight: t.weight,
                    italic: t.italic as c_int,
                    monospace: t.monospace as c_int,
                    color: t.color,
                    letter_spacing: t.letter_spacing,
                    m,
                    transformed,
                };
                kitae_renderer_push_text(self.raw, &rt);
            }
            kitae_renderer_set_scroll(self.raw, scroll_y, dl.content_height);
        }
    }

    /// Update just the scroll state (keeps the existing display list).
    /// Used on scroll events so pages repaint without a full resubmit.
    pub fn set_scroll(&self, scroll_y: f32, content_height: f32) {
        unsafe { kitae_renderer_set_scroll(self.raw, scroll_y, content_height) }
    }

    /// Pump one event. `None` means the queue was empty and a frame was
    /// painted — keep looping.
    pub fn poll_event(&self) -> Option<Event> {
        let mut ev = RawEvent {
            kind: 0,
            a: 0.0,
            b: 0.0,
            url: [0; 4096],
        };
        let got = unsafe { kitae_renderer_poll_event(self.raw, &mut ev) };
        if got == 0 {
            return None;
        }
        let event = match ev.kind {
            1 => Event::Quit,
            2 => Event::Navigate(url_from_raw(&ev)),
            3 => Event::Back,
            4 => Event::Forward,
            5 => Event::Reload,
            6 => Event::Scroll(ev.a),
            7 => Event::Resize(ev.a, ev.b),
            8 => Event::Click(ev.a, ev.b),
            _ => return None,
        };
        Some(event)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe { kitae_renderer_destroy(self.raw) }
    }
}

fn url_from_raw(ev: &RawEvent) -> String {
    unsafe {
        CStr::from_ptr(ev.url.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

// ---------------------------------------------------------------------------
// Text measurement backed by the shell's real fonts
// ---------------------------------------------------------------------------

/// [`TextMeasurer`] that asks the native renderer, so layout widths match the
/// glyphs actually drawn (Helvetica for text, Menlo for pre/code).
pub struct FfiMeasurer<'a> {
    renderer: &'a Renderer,
}

impl<'a> FfiMeasurer<'a> {
    pub fn new(renderer: &'a Renderer) -> FfiMeasurer<'a> {
        FfiMeasurer { renderer }
    }
}

impl TextMeasurer for FfiMeasurer<'_> {
    fn measure(&self, text: &str, size: f32, weight: u32, italic: bool, monospace: bool) -> f32 {
        let t = cstr(text);
        unsafe {
            kitae_renderer_measure_text(
                self.renderer.raw,
                t.as_ptr(),
                size,
                weight,
                italic as c_int,
                monospace as c_int,
            )
        }
    }

    fn metrics(&self, size: f32, monospace: bool) -> (f32, f32) {
        let (mut ascent, mut descent, mut gap) = (0.0f32, 0.0f32, 0.0f32);
        unsafe {
            kitae_renderer_font_metrics(
                self.renderer.raw,
                size,
                monospace as c_int,
                &mut ascent,
                &mut descent,
                &mut gap,
            );
        }
        (ascent, descent)
    }
}

// ---------------------------------------------------------------------------
// Link hit-test callback (called from C++ on mouse moves)
// ---------------------------------------------------------------------------

/// Clickable areas in page coordinates, refreshed by the app after every
/// layout. The shell asks about them via `kitae_app_hit_link`.
static LINK_AREAS: Mutex<Vec<LinkArea>> = Mutex::new(Vec::new());

/// Replace the hit-test areas with the current display list's links.
pub fn store_links(dl: &DisplayList) {
    let mut areas = LINK_AREAS.lock().unwrap();
    areas.clear();
    areas.extend(dl.links.iter().cloned());
}

/// Exported to the shell: is there a link at this page-space point?
/// The shell passes viewport coords already translated to page space
/// (`y - chrome_height + scroll_y`).
#[no_mangle]
pub extern "C" fn kitae_app_hit_link(x: f32, y: f32) -> bool {
    let areas = LINK_AREAS.lock().unwrap();
    areas.iter().any(|link| link.contains(x, y))
}
