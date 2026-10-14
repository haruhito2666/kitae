//! kitae_render_gpu — the Servo-style GPU render path (Phase L).
//!
//! The display list stays the contract: [`GpuScene`] is built from it with no
//! scene graph and no retention (instance buffers are rebuilt every frame).
//! Two backends consume the same scene:
//!
//! * [`cpu`] — pure-Rust reference rasterizer (the permanent software
//!   fallback, the regression gate, and the golden-test oracle).
//! * [`backend`] — real `wgpu` over Metal: solid-quad + textured pipelines,
//!   offscreen render + readback. Gracefully unavailable without an adapter.
//!
//! [`GlyphAtlas`] (L2) stores rasterized glyphs (r8 pages, LRU page
//! eviction); hosts upload bitmaps (Swift Core Text via the C ABI). Shaped
//! runs (Phase M) reuse the same atlas struct with shaped positions.

pub mod backend;
pub mod cpu;
pub mod ffi;
mod frame;
pub mod layers;
pub mod tiles;

pub use frame::parse_frame_scene;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use kitae_layout::{DisplayList, Matrix2d, RectPaint, ResolvedClip};
use kitae_style::ObjectFit;

/// One filled quad: box + fast-path color + accurate paint + radii.
///
/// `transform` (Q2) is the enclosing CSS box's 2×3 page-space matrix when
/// non-identity: stored geometry stays pre-transform; painting evaluates in
/// that local space and warps geometry through the matrix (device anchoring
/// in [`device_matrix`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SceneRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: u32,
    pub radius: [f32; 4],
    pub paint: ScenePaint,
    pub transform: Option<Matrix2d>,
    /// E8: `clip-path`/`mask` region in the item's stored (local) space —
    /// samples outside it are discarded. `None` = unclipped.
    pub clip: Option<SceneClip>,
}

/// Accurate fill, mirrored from [`RectPaint`] without the layout dependency.
#[derive(Debug, Clone, PartialEq)]
pub enum ScenePaint {
    Solid,
    Linear {
        angle_deg: f32,
        stops: Vec<(f32, u32)>,
    },
    Radial {
        cx: f32,
        cy: f32,
        radius: Option<f32>,
        stops: Vec<(f32, u32)>,
    },
}

impl From<&RectPaint> for ScenePaint {
    fn from(p: &RectPaint) -> ScenePaint {
        match p {
            RectPaint::Solid => ScenePaint::Solid,
            RectPaint::LinearGradient { angle_deg, stops } => ScenePaint::Linear {
                angle_deg: *angle_deg,
                stops: stops.clone(),
            },
            RectPaint::RadialGradient {
                cx,
                cy,
                radius,
                stops,
            } => ScenePaint::Radial {
                cx: *cx,
                cy: *cy,
                radius: *radius,
                stops: stops.clone(),
            },
            RectPaint::Image(_) => ScenePaint::Solid,
            // B5: conic paints as the base color on the GPU path for now
            // (CPU + Swift renderers have the full per-pixel sweep).
            RectPaint::ConicGradient { .. } => ScenePaint::Solid,
        }
    }
}

/// E8: complex clip region, mirroring kitae_layout's `ResolvedClip`. The
/// geometry is absolute page-space-of-the-stored-box — the same space the
/// item's own coordinates live in — so an untransformed item tests page
/// points directly and a transformed item tests its per-pixel local points
/// (the painter inverse-maps first).
#[derive(Debug, Clone, PartialEq)]
pub enum SceneClip {
    Circle {
        cx: f32,
        cy: f32,
        r: f32,
    },
    Ellipse {
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
    },
    Inset {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radii: [(f32, f32); 4],
    },
    Polygon {
        points: Vec<(f32, f32)>,
        evenodd: bool,
    },
}

impl SceneClip {
    /// Hard point membership (no anti-aliased edge — a boolean discard).
    /// Inset honors per-corner elliptical radii (tl, tr, br, bl): a sample
    /// inside a corner's radius box must also sit inside that corner's
    /// ellipse. Polygons use the even-odd rule when `evenodd`, else the
    /// nonzero winding rule.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        match self {
            SceneClip::Circle { cx, cy, r } => {
                let (dx, dy) = (x - cx, y - cy);
                dx * dx + dy * dy <= r * r
            }
            SceneClip::Ellipse { cx, cy, rx, ry } => {
                let (dx, dy) = (x - cx, y - cy);
                (dx * dx) / (rx * rx).max(f32::MIN_POSITIVE)
                    + (dy * dy) / (ry * ry).max(f32::MIN_POSITIVE)
                    <= 1.0
            }
            SceneClip::Inset {
                x: rx,
                y: ry,
                w,
                h,
                radii,
            } => {
                if x < *rx || x >= rx + w || y < *ry || y >= ry + h {
                    return false;
                }
                // Corner centers and their radius boxes. A sample inside the
                // radius box is inside the shape only if it is inside the
                // corner ellipse (normalized distance from the center ≤ 1).
                let corners: [(f32, f32, bool, bool, (f32, f32)); 4] = [
                    // (cx, cy, box extends +x?, box extends +y?, radii)
                    (*rx, *ry, true, true, radii[0]),
                    (rx + w, *ry, false, true, radii[1]),
                    (rx + w, ry + h, false, false, radii[2]),
                    (*rx, ry + h, true, false, radii[3]),
                ];
                for (cx, cy, plus_x, plus_y, (hr, vr)) in corners {
                    if hr <= 0.0 || vr <= 0.0 {
                        continue;
                    }
                    let in_x = if plus_x { x < cx + hr } else { x > cx - hr };
                    let in_y = if plus_y { y < cy + vr } else { y > cy - vr };
                    if in_x && in_y {
                        let (ox, oy) = ((x - cx) / hr, (y - cy) / vr);
                        if ox * ox + oy * oy > 1.0 {
                            return false;
                        }
                    }
                }
                true
            }
            SceneClip::Polygon { points, evenodd } => {
                Self::polygon_contains(points, *evenodd, x, y)
            }
        }
    }

    fn polygon_contains(points: &[(f32, f32)], evenodd: bool, x: f32, y: f32) -> bool {
        let n = points.len();
        if n < 3 {
            return false;
        }
        if evenodd {
            // Ray cast to +x: a crossing flips membership.
            let mut inside = false;
            let mut j = n - 1;
            for i in 0..n {
                let (xi, yi) = points[i];
                let (xj, yj) = points[j];
                if (yi > y) != (yj > y) {
                    let xing = xi + (y - yi) / (yj - yi) * (xj - xi);
                    if x < xing {
                        inside = !inside;
                    }
                }
                j = i;
            }
            inside
        } else {
            // Nonzero winding: signed crossings around the sample.
            let mut winding = 0i32;
            let mut j = n - 1;
            for i in 0..n {
                let (xi, yi) = points[i];
                let (xj, yj) = points[j];
                if yi <= y {
                    if yj > y && (xj - xi) * (y - yi) - (xi - x) * (yj - yi) > 0.0 {
                        winding += 1;
                    }
                } else if yj <= y && (xj - xi) * (y - yi) - (xi - x) * (yj - yi) < 0.0 {
                    winding -= 1;
                }
                j = i;
            }
            winding != 0
        }
    }
}

impl From<&ResolvedClip> for SceneClip {
    fn from(c: &ResolvedClip) -> SceneClip {
        match c {
            ResolvedClip::Circle { cx, cy, r } => SceneClip::Circle {
                cx: *cx,
                cy: *cy,
                r: *r,
            },
            ResolvedClip::Ellipse { cx, cy, rx, ry } => SceneClip::Ellipse {
                cx: *cx,
                cy: *cy,
                rx: *rx,
                ry: *ry,
            },
            ResolvedClip::Inset { x, y, w, h, radii } => SceneClip::Inset {
                x: *x,
                y: *y,
                w: *w,
                h: *h,
                radii: *radii,
            },
            ResolvedClip::Polygon { points, evenodd } => SceneClip::Polygon {
                points: points.clone(),
                evenodd: *evenodd,
            },
        }
    }
}

/// E1: intern a font-family name into a dense id for glyph keys, so a run
/// measured (and rasterized) with a web font never aliases the
/// default-family raster of the same character. "" — the host default — is
/// always 0 and never allocates. Ids are process-wide and opaque.
pub fn family_id(family: &str) -> u32 {
    if family.is_empty() {
        return 0;
    }
    static NEXT: AtomicU32 = AtomicU32::new(1);
    static IDS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    let mut ids = IDS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(&id) = ids.get(family) {
        return id;
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    ids.insert(family.to_string(), id);
    id
}

/// One image quad: layout box + decoded size + fit (UV math at paint).
#[derive(Debug, Clone, PartialEq)]
pub struct SceneImage {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub payload: u64,
    pub fit: ObjectFit,
    pub img_w: f32,
    pub img_h: f32,
    pub transform: Option<Matrix2d>,
    /// E8 clip (see [`SceneRect::clip`]).
    pub clip: Option<SceneClip>,
}

/// One shadow shell: border box + expansion (blur/offset applied at paint).
/// The offset is applied in the shadow's own (pre-transform) space.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneShadow {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: [f32; 4],
    pub color: u32,
    pub blur: f32,
    pub spread: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub inset: bool,
    pub transform: Option<Matrix2d>,
    /// E8 clip (see [`SceneRect::clip`]).
    pub clip: Option<SceneClip>,
}

/// One laid-out glyph instance (L2): shaping-free id + page-space position.
/// Phase M replaces per-char positions with shaped runs; the atlas struct
/// and quad format are unchanged. Every quad of a transformed run carries
/// that run's matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphQuad {
    pub x: f32,
    /// Baseline y.
    pub y: f32,
    pub size: f32,
    pub color: u32,
    pub key: GlyphKey,
    pub transform: Option<Matrix2d>,
    /// E3: blur radius of the text-shadow this quad paints (0 = crisp text).
    /// The quad's color already carries the shadow color; the backend blurs
    /// the glyph's alpha coverage instead of stamping it hard.
    pub blur: f32,
}

/// Atlas key: one rasterized glyph. Weight is bucketed (bold ≥ 600) so the
/// atlas stays small — matching Core Text measurement on the Swift side.
/// E1: `family` is the interned font-family id ([`family_id`]); a web-font
/// run must not alias the default family's raster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub ch: char,
    pub size_px: u32,
    pub bold: bool,
    pub italic: bool,
    pub monospace: bool,
    pub family: u32,
}

/// Where a glyph lives in the atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphLoc {
    pub page: usize,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Glyph atlas (L2): r8 pages with shelf packing, LRU page eviction. Hosts
/// upload bitmaps; both backends sample them. Empty pages are 256×256.
#[derive(Debug, Default)]
pub struct GlyphAtlas {
    pages: Vec<AtlasPage>,
    map: HashMap<GlyphKey, GlyphLoc>,
    /// Insertion order for LRU page eviction.
    page_lru: Vec<usize>,
}

#[derive(Debug)]
struct AtlasPage {
    w: u32,
    h: u32,
    pixels: Vec<u8>,
    cursor_x: u32,
    cursor_y: u32,
    row_h: u32,
}

impl GlyphAtlas {
    pub const PAGE: u32 = 256;
    /// Maximum pages before the stalest page is dropped wholesale.
    pub const MAX_PAGES: usize = 8;

    pub fn new() -> GlyphAtlas {
        GlyphAtlas::default()
    }

    /// Upload an alpha bitmap for `key` (top row first). Returns its location.
    /// Zero-size bitmaps are rejected (`None`).
    pub fn upload(&mut self, key: GlyphKey, w: u32, h: u32, bitmap: &[u8]) -> Option<GlyphLoc> {
        if w == 0 || h == 0 || bitmap.len() < (w * h) as usize {
            return None;
        }
        if let Some(loc) = self.map.get(&key) {
            return Some(*loc);
        }
        // Shelf pack across pages, newest (back) first.
        for pi in (0..self.pages.len()).rev() {
            if let Some(loc) = self.place(pi, key, w, h, bitmap) {
                self.touch_page(pi);
                return Some(loc);
            }
        }
        if self.pages.len() >= Self::MAX_PAGES {
            let stale = self.page_lru.remove(0);
            self.map.retain(|_, loc| loc.page != stale);
            // Renumber pages above the hole.
            for loc in self.map.values_mut() {
                if loc.page > stale {
                    loc.page -= 1;
                }
            }
            self.pages.remove(stale);
            for p in self.page_lru.iter_mut() {
                if *p > stale {
                    *p -= 1;
                }
            }
        }
        let pi = self.pages.len();
        self.pages.push(AtlasPage {
            w: Self::PAGE,
            h: Self::PAGE,
            pixels: vec![0u8; (Self::PAGE * Self::PAGE) as usize],
            cursor_x: 0,
            cursor_y: 0,
            row_h: 0,
        });
        self.page_lru.push(pi);
        self.place(pi, key, w, h, bitmap)
    }

    fn place(
        &mut self,
        pi: usize,
        key: GlyphKey,
        w: u32,
        h: u32,
        bitmap: &[u8],
    ) -> Option<GlyphLoc> {
        let page = &mut self.pages[pi];
        if w + 1 > page.w || h + 1 > page.h {
            return None;
        }
        if page.cursor_x + w + 1 > page.w {
            page.cursor_x = 0;
            page.cursor_y += page.row_h + 1;
            page.row_h = 0;
        }
        if page.cursor_y + h + 1 > page.h {
            return None;
        }
        let (x, y) = (page.cursor_x, page.cursor_y);
        for row in 0..h {
            let dst = ((y + row) * page.w + x) as usize;
            let src = (row * w) as usize;
            page.pixels[dst..dst + w as usize].copy_from_slice(&bitmap[src..src + w as usize]);
        }
        page.cursor_x += w + 1;
        page.row_h = page.row_h.max(h);
        let loc = GlyphLoc {
            page: pi,
            x,
            y,
            w,
            h,
        };
        self.map.insert(key, loc);
        Some(loc)
    }

    fn touch_page(&mut self, pi: usize) {
        if let Some(i) = self.page_lru.iter().position(|&p| p == pi) {
            self.page_lru.remove(i);
            self.page_lru.push(pi);
        }
    }

    pub fn lookup(&self, key: &GlyphKey) -> Option<GlyphLoc> {
        self.map.get(key).copied()
    }

    pub fn page_pixels(&self, page: usize) -> Option<(u32, u32, &[u8])> {
        self.pages
            .get(page)
            .map(|p| (p.w, p.h, p.pixels.as_slice()))
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn glyph_count(&self) -> usize {
        self.map.len()
    }
}

/// The whole paint plan for one frame at device px size.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuScene {
    pub w: u32,
    pub h: u32,
    pub bg: u32,
    pub rects: Vec<SceneRect>,
    pub images: Vec<SceneImage>,
    pub glyphs: Vec<GlyphQuad>,
    pub shadows: Vec<SceneShadow>,
    /// E2 sticky boxes (Q5): the painter shifts each box's items by
    /// [`SceneSticky::shift`] at its scroll offset. Mirrors kitae_layout's
    /// `StickyItem`; ranges index the scene's own item lists.
    pub stickies: Vec<SceneSticky>,
    /// E2 scrollable containers (B2): scrollable regions the painter can
    /// clip and (future) scroll-offset independently.
    pub scroll_containers: Vec<SceneScroll>,
    /// E6: LCD (subpixel) text antialiasing request. The CPU backend
    /// renders per-channel coverage; the GPU backend stays grayscale
    /// (documented deviation). Off by default.
    pub lcd_text: bool,
    /// E2: per display-list text run, the (start, count) range of this
    /// scene's `glyphs` the run produced. Sticky `text` ranges index runs,
    /// not glyphs, so painters expand them through this map.
    pub text_spans: Vec<(u32, u32)>,
}

/// E2: painter-side sticky box — same shape as kitae_layout `StickyItem`
/// (ranges into the scene's own lists, all page-space px).
#[derive(Debug, Clone, PartialEq)]
pub struct SceneSticky {
    pub rect: (u32, u32),
    pub text: (u32, u32),
    pub link: (u32, u32),
    pub image: (u32, u32),
    pub media: (u32, u32),
    pub shadow: (u32, u32),
    pub base_y: f32,
    pub height: f32,
    pub container_top: f32,
    pub container_bottom: f32,
    pub top_offset: f32,
    pub bottom_offset: f32,
    pub has_top: bool,
    pub has_bottom: bool,
}

impl SceneSticky {
    /// Y shift at `scroll_y` with a `viewport_h`-tall viewport — mirrors
    /// kitae_layout's `StickyItem::shift` exactly.
    pub fn shift(&self, scroll_y: f32, viewport_h: f32) -> f32 {
        if self.has_top {
            let target = scroll_y + self.top_offset - self.base_y;
            let down_max =
                (self.container_bottom - self.bottom_offset - self.height - self.base_y).max(0.0);
            target.clamp(0.0, down_max)
        } else if self.has_bottom {
            let target = scroll_y + viewport_h - self.bottom_offset - self.height - self.base_y;
            let up_min = (self.container_top + self.top_offset - self.base_y).min(0.0);
            target.clamp(up_min, 0.0)
        } else {
            0.0
        }
    }
}

/// E2: one scrollable region (`overflow: auto|scroll`) — viewport rect and
/// content height, page-space. Scene-level signal for painters that own
/// scrolling.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneScroll {
    pub node: u32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub content_h: f32,
}

impl GpuScene {
    /// Build from a display list. `dims` maps image payload ids to decoded
    /// (w, h) for UV math; `advance` measures a single character's advance
    /// width at size in the named family (E1 — the family string must match
    /// what the run was measured with; tests pass a mock). `viewport` is the
    /// page area in CSS px (the scene is page-space; backends clip to it).
    pub fn from_display_list(
        dl: &DisplayList,
        dims: &HashMap<u64, (u32, u32)>,
        advance: &dyn Fn(&str, f32, u32, bool, bool, &str) -> f32,
        viewport_w: f32,
        viewport_h: f32,
    ) -> GpuScene {
        let _ = viewport_w;
        let _ = viewport_h;
        let mut scene = GpuScene {
            w: 0,
            h: 0,
            bg: dl.page_background,
            ..GpuScene::default()
        };
        for s in &dl.shadows {
            scene.shadows.push(SceneShadow {
                x: s.x,
                y: s.y,
                w: s.w,
                h: s.h,
                radius: s.radius.resolve(16.0, 16.0, s.w.max(0.0)),
                color: s.color,
                blur: s.blur,
                spread: s.spread,
                offset_x: s.offset_x,
                offset_y: s.offset_y,
                inset: s.inset,
                transform: s.transform,
                clip: s.clip.as_ref().map(SceneClip::from),
            });
        }
        for r in &dl.rects {
            scene.rects.push(SceneRect {
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                color: r.color,
                radius: r.radius.resolve(16.0, 16.0, r.w.max(0.0)),
                paint: ScenePaint::from(&r.paint),
                transform: r.transform,
                clip: r.clip.as_ref().map(SceneClip::from),
            });
        }
        for im in &dl.images {
            let (iw, ih) = im
                .payload
                .and_then(|id| dims.get(&id).copied())
                .unwrap_or((im.w.max(1.0) as u32, im.h.max(1.0) as u32));
            scene.images.push(SceneImage {
                x: im.x,
                y: im.y,
                w: im.w,
                h: im.h,
                payload: im.payload.unwrap_or(0),
                fit: im.fit,
                img_w: iw as f32,
                img_h: ih as f32,
                transform: im.transform,
                clip: im.clip.as_ref().map(SceneClip::from),
            });
        }
        for t in &dl.texts {
            // Shape-free per-char positions (kerning/shaping is Phase M);
            // advances come from the host measurer so scene and CPU paint
            // agree exactly. Letter-spaced runs (Q3) add the spacing to every
            // advance except the last glyph's — the same sum layout measured.
            // E3: a blurred run is an R8 text-shadow duplicate; every glyph
            // of it carries the shadow's blur radius.
            let run_start = scene.glyphs.len() as u32;
            let mut x = t.x;
            let n = t.text.chars().count();
            for (idx, ch) in t.text.chars().enumerate() {
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                let mut adv =
                    advance(s, t.size, t.weight, t.italic, t.monospace, &t.family).max(0.0);
                if t.letter_spacing != 0.0 && idx + 1 < n {
                    adv += t.letter_spacing;
                }
                scene.glyphs.push(GlyphQuad {
                    x,
                    y: t.y,
                    size: t.size,
                    color: t.color,
                    key: GlyphKey {
                        ch,
                        size_px: t.size.max(1.0).round() as u32,
                        bold: t.weight >= 600,
                        italic: t.italic,
                        monospace: t.monospace,
                        family: family_id(&t.family),
                    },
                    transform: t.transform,
                    blur: t.shadow_blur,
                });
                x += adv;
            }
            scene
                .text_spans
                .push((run_start, scene.glyphs.len() as u32 - run_start));
        }
        // E2: sticky boxes and scroll containers ride along so painters can
        // shift sticky items at their scroll offset (ranges are 1:1 — the
        // scene lists hold the same items in the same order as the display
        // list).
        for st in &dl.stickies {
            scene.stickies.push(SceneSticky {
                rect: st.rect,
                text: st.text,
                link: st.link,
                image: st.image,
                media: st.media,
                shadow: st.shadow,
                base_y: st.base_y,
                height: st.height,
                container_top: st.container_top,
                container_bottom: st.container_bottom,
                top_offset: st.top_offset,
                bottom_offset: st.bottom_offset,
                has_top: st.has_top,
                has_bottom: st.has_bottom,
            });
        }
        for sc in &dl.scroll_containers {
            scene.scroll_containers.push(SceneScroll {
                node: sc.node,
                x: sc.x,
                y: sc.y,
                w: sc.w,
                h: sc.h,
                content_h: sc.content_h,
            });
        }
        scene
    }
}

/// Device-space 2×3 (row-major, same layout as [`Matrix2d`]) equivalent of a
/// page-space matrix on one tile: the linear part survives the uniform scale
/// unchanged and the translation re-anchors the tile origin, so applying it
/// to `(p − origin) · scale` is exactly `(M·p − origin) · scale`. Every
/// painter (SDL C++, CPU reference, GPU, Swift CGContext) warps geometry
/// with this same device matrix, keeping backends in parity.
pub(crate) fn device_matrix(m: &Matrix2d, scale: f32, ox: f32, oy: f32) -> [f32; 6] {
    let [a, b, c, d, e, f] = m.0;
    [
        a,
        b,
        scale * (a * ox + b * oy + c - ox),
        d,
        e,
        scale * (d * ox + e * oy + f - oy),
    ]
}

/// Inverse of a row-major 2×3 (identity when singular — a degenerate
/// transform collapses to nothing paintable anyway). Used by the CPU
/// reference's per-pixel device→local mapping.
pub(crate) fn invert_matrix(m: &[f32; 6]) -> [f32; 6] {
    let det = m[0] * m[4] - m[1] * m[3];
    if det.abs() < 1e-9 {
        return [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
    }
    let (i0, i1, i3, i4) = (m[4] / det, -m[1] / det, -m[3] / det, m[0] / det);
    [
        i0,
        i1,
        -(i0 * m[2] + i1 * m[5]),
        i3,
        i4,
        -(i3 * m[2] + i4 * m[5]),
    ]
}

/// Identity 2×3 for the fast-path checks.
pub(crate) const MATRIX_IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_advance(s: &str, size: f32, _w: u32, _i: bool, _m: bool, _f: &str) -> f32 {
        s.chars().count() as f32 * size * 0.5
    }

    fn key(ch: char) -> GlyphKey {
        GlyphKey {
            ch,
            size_px: 16,
            bold: false,
            italic: false,
            monospace: false,
            family: 0,
        }
    }

    #[test]
    fn atlas_upload_lookup_and_lru_page_eviction() {
        let mut atlas = GlyphAtlas::new();
        let bmp = vec![200u8; 10 * 12];
        let loc = atlas.upload(key('a'), 10, 12, &bmp).expect("fits");
        assert_eq!(atlas.lookup(&key('a')), Some(loc));
        assert_eq!(atlas.page_count(), 1);
        // Zero-size rejected.
        assert_eq!(atlas.upload(key('b'), 0, 5, &[]), None);
        // Oversized glyph rejected (bigger than a page).
        assert_eq!(
            atlas.upload(key('c'), 300, 300, &vec![0u8; 300 * 300]),
            None
        );
        // Fill past the page budget: stalest page drops wholesale.
        for i in 0..(GlyphAtlas::MAX_PAGES as u32 + 2) {
            // 200px-wide glyphs: one row each, forcing fresh pages.
            let ch = char::from_u32('d' as u32 + i).unwrap();
            let big = vec![128u8; 200 * 200];
            atlas.upload(
                GlyphKey {
                    ch,
                    size_px: 16,
                    bold: false,
                    italic: false,
                    monospace: false,
                    family: 0,
                },
                200,
                200,
                &big,
            );
        }
        assert!(atlas.page_count() <= GlyphAtlas::MAX_PAGES);
        // Evicted entries are gone from the map.
        assert_eq!(atlas.lookup(&key('a')), None);
    }

    #[test]
    fn scene_built_from_display_list() {
        let (dom, _) = kitae_html::parse("<p>hi</p>");
        let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
        let dl = kitae_layout::layout(&dom, &styles, 100.0, 100.0, &MockMeasurer);
        let scene = GpuScene::from_display_list(&dl, &HashMap::new(), &mock_advance, 100.0, 100.0);
        assert_eq!(scene.bg, 0xFFFFFFFF);
        // "hi" → two glyph quads with Mock-consistent advances.
        assert_eq!(scene.glyphs.len(), 2);
        assert!((scene.glyphs[1].x - scene.glyphs[0].x - 8.0).abs() < 0.01);
    }

    /// Q3: letter-spacing advances every glyph but the last by the run's
    /// spacing — the same sum `TextMeasurer::measure_run_sp` uses, so the GPU
    /// scene matches layout widths exactly.
    #[test]
    fn scene_glyphs_advance_by_letter_spacing() {
        let (dom, _) = kitae_html::parse("<p style=\"letter-spacing:3px\">hi</p><p>plain</p>");
        let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
        let dl = kitae_layout::layout(&dom, &styles, 200.0, 100.0, &MockMeasurer);
        let scene = GpuScene::from_display_list(&dl, &HashMap::new(), &mock_advance, 200.0, 100.0);
        // hi: 'h' at x, 'i' at x + 8 + 3.
        let hi = &scene.glyphs[0..2];
        assert!((hi[1].x - hi[0].x - (8.0 + 3.0)).abs() < 0.01);
        // Plain run keeps unspaced advances.
        let plain = &scene.glyphs[2..7];
        assert!((plain[1].x - plain[0].x - 8.0).abs() < 0.01);
    }

    /// E1: distinct families intern to distinct glyph-key ids (a web-font
    /// run must not alias the default family's raster); "" is always 0 and
    /// repeated calls are stable.
    #[test]
    fn family_ids_are_stable_and_distinct() {
        assert_eq!(family_id(""), 0);
        assert_eq!(family_id(""), 0);
        let a = family_id("Helvetica");
        let b = family_id("Helvetica");
        assert_eq!(a, b, "same name interns once");
        assert_ne!(a, 0);
        assert_ne!(a, family_id("Times New Roman"));
    }

    /// E2: the scene carries stickies + scroll containers with ranges that
    /// index its own lists, and `shift` mirrors layout's formula.
    #[test]
    fn scene_carries_stickies_and_scroll_containers() {
        let (dom, _) = kitae_html::parse(
            "<div style=\"height:60px\">lead</div>\
             <div style=\"position:sticky;top:12px;height:30px;background:#369\">head</div>\
             <div style=\"height:900px\">tail</div>",
        );
        let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
        let dl = kitae_layout::layout(&dom, &styles, 200.0, 600.0, &MockMeasurer);
        assert_eq!(dl.stickies.len(), 1, "layout stamped the sticky");
        let scene = GpuScene::from_display_list(&dl, &HashMap::new(), &mock_advance, 200.0, 600.0);
        assert_eq!(scene.stickies.len(), 1);
        let st = &scene.stickies[0];
        assert!(st.rect.1 >= 1, "sticky paints its background rect");
        assert!(st.text.1 >= 1, "sticky paints its text run");
        assert!((st.top_offset - 12.0).abs() < 0.5);
        // Same formula as kitae_layout::StickyItem::shift.
        let expect = dl.stickies[0].shift(100.0, 600.0);
        assert!((st.shift(100.0, 600.0) - expect).abs() < 0.001);
    }

    struct MockMeasurer;

    impl kitae_layout::TextMeasurer for MockMeasurer {
        fn measure(&self, text: &str, size: f32, _w: u32, _i: bool, mono: bool) -> f32 {
            text.chars().count() as f32 * size * if mono { 0.6 } else { 0.5 }
        }
        fn metrics(&self, size: f32, _mono: bool) -> (f32, f32) {
            (size * 0.8, size * 0.2)
        }
    }
}
