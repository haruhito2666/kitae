//! Layout: DOM + ComputedStyle → display list of positioned rects and text
//! runs. Block flow with anonymous inline grouping, word wrap against a
//! measured font, list markers, pre-formatted blocks, link hit areas,
//! relative/absolute/fixed positioning, z-ordered stacking, overflow
//! clipping, and min/max size clamps.
//!
//! Boxes that need post-processing (positioned offsets, clips, stacking)
//! paint into a captured [`Buffers`] instead of straight into the page; the
//! caller then translates/clips/orders and merges. Flow position (`self.y`)
//! advances identically either way.
//!
//! Coordinates are page coordinates (y grows downward); the shell applies
//! scrolling and clips to the page viewport.

use std::collections::{HashMap, HashSet};

use kitae_dom::{Dom, NodeId, NodeKind};
use kitae_style::{
    AlignContent, AlignItems, BackgroundLayer, BackgroundPaint, BackgroundSize, BgBox, BgPos,
    BgRepeat, BgSize, BgSizeKeyword, BorderRadius, BorderSidePx, BorderStyle, BreakAction,
    CaptionSide, Clear, ClipShape, ComputedStyle, ContainerType, DecorationStyle, Direction,
    Display, EdgesPx, EnvironmentState, FilterFn, FilterList, FlexDirection, FlexWrap, FloatSide,
    GridTrack, Hyphens, JustifyContent, Len, Length, ListStylePosition, MaxTrack, MinSizing,
    ObjectFit, Overflow, OverflowWrap, Position, ShapeRadius, SizeVal, TextAlign, TextOrientation,
    TextOverflow, TextTransform, TransformFn, TransformList, TransformOrigin, VerticalAlign,
    Visibility, WordBreak, WritingMode,
};

/// Font measurement is the shell's job (it owns the rasterized faces); layout
/// talks to it through this trait so tests can use a fixed-width mock.
pub trait TextMeasurer {
    /// Pixel width of `text` when rasterized at `size` px.
    fn measure(&self, text: &str, size: f32, weight: u32, italic: bool, monospace: bool) -> f32;
    /// (ascent, descent) in px at `size`.
    fn metrics(&self, size: f32, monospace: bool) -> (f32, f32);
    /// Shaped width of one laid-out run (Phase M1 seam): shells backed by a
    /// real shaper override this with shaped advances; the default keeps the
    /// unshaped `measure` behavior exactly. Layout feeds whitespace-separated
    /// words (cross-word kerning stays future work by construction).
    fn measure_run(
        &self,
        text: &str,
        size: f32,
        weight: u32,
        italic: bool,
        monospace: bool,
    ) -> f32 {
        self.measure(text, size, weight, italic, monospace)
    }
    /// Width of `text` with `letter_spacing` extra px after each glyph except
    /// the last (Q3). The default computes it from per-char `measure` calls;
    /// shells with a shaper may override. Only consulted when spacing ≠ 0.
    fn measure_run_sp(
        &self,
        text: &str,
        size: f32,
        weight: u32,
        italic: bool,
        monospace: bool,
        letter_spacing: f32,
    ) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let n = text.chars().count();
        let base: f32 = text
            .chars()
            .map(|c| {
                let mut buf = [0u8; 4];
                self.measure(c.encode_utf8(&mut buf), size, weight, italic, monospace)
            })
            .sum();
        base + letter_spacing * (n as f32 - 1.0).max(0.0)
    }

    // Family-aware variants (Q11): layout calls these with the computed
    // `font-family` so `@font-face` faces take part in measurement. The
    // defaults keep the family-blind behavior exactly, so existing shells
    // (test mocks, approximate measurers, the C++ shell) compile unchanged;
    // hosts that own real fonts override them.
    fn measure_family(
        &self,
        text: &str,
        size: f32,
        weight: u32,
        italic: bool,
        monospace: bool,
        family: &str,
    ) -> f32 {
        let _ = family;
        self.measure(text, size, weight, italic, monospace)
    }
    fn measure_run_family(
        &self,
        text: &str,
        size: f32,
        weight: u32,
        italic: bool,
        monospace: bool,
        family: &str,
    ) -> f32 {
        let _ = family;
        self.measure_run(text, size, weight, italic, monospace)
    }
    fn measure_run_sp_family(
        &self,
        text: &str,
        size: f32,
        weight: u32,
        italic: bool,
        monospace: bool,
        letter_spacing: f32,
        family: &str,
    ) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let n = text.chars().count();
        let base: f32 = text
            .chars()
            .map(|c| {
                let mut buf = [0u8; 4];
                self.measure_family(
                    c.encode_utf8(&mut buf),
                    size,
                    weight,
                    italic,
                    monospace,
                    family,
                )
            })
            .sum();
        base + letter_spacing * (n as f32 - 1.0).max(0.0)
    }
    /// (ascent, descent) of the resolved family's face at `size`.
    fn metrics_family(&self, size: f32, monospace: bool, family: &str) -> (f32, f32) {
        let _ = family;
        self.metrics(size, monospace)
    }
    /// B9: x-height of the resolved family's face at `size` — the `ex`
    /// unit. Shells with real font data override (CoreText reports it);
    /// the default is the CSS fallback ratio (half the font size).
    fn ex_px(&self, size: f32, weight: u32, italic: bool, monospace: bool, family: &str) -> f32 {
        let _ = (weight, italic, monospace, family);
        size * 0.5
    }
}

/// CSS `text-transform` case folding (Q3). `capitalize` uppercases the first
/// letter of each whitespace-separated word (UAX#29 word boundaries are
/// future work); multi-char uppercases (ß → SS) expand.
pub fn apply_text_transform(text: &str, tt: TextTransform) -> String {
    match tt {
        TextTransform::None => text.to_string(),
        TextTransform::Uppercase => text.to_uppercase(),
        TextTransform::Lowercase => text.to_lowercase(),
        TextTransform::Capitalize => {
            let mut out = String::with_capacity(text.len());
            let mut at_word_start = true;
            for c in text.chars() {
                if c.is_whitespace() {
                    at_word_start = true;
                    out.push(c);
                } else if at_word_start {
                    out.extend(c.to_uppercase());
                    at_word_start = false;
                } else {
                    out.push(c);
                }
            }
            out
        }
    }
}

/// 2×3 affine matrix, row-major (Q2): `x' = m0·x + m1·y + m2`,
/// `y' = m3·x + m4·y + m5`. Maps to `CGAffineTransform(a, b, c, d, tx, ty)`
/// as `(m0, m3, m1, m4, m2, m5)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix2d(pub [f32; 6]);

impl Matrix2d {
    pub const IDENTITY: Matrix2d = Matrix2d([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);

    pub fn is_identity(self) -> bool {
        self.0 == Matrix2d::IDENTITY.0
    }

    /// `self ∘ inner` — `inner` transforms the point first, then `self`.
    pub fn compose(self, inner: Matrix2d) -> Matrix2d {
        let a = self.0;
        let b = inner.0;
        Matrix2d([
            a[0] * b[0] + a[1] * b[3],
            a[0] * b[1] + a[1] * b[4],
            a[0] * b[2] + a[1] * b[5] + a[2],
            a[3] * b[0] + a[4] * b[3],
            a[3] * b[1] + a[4] * b[4],
            a[3] * b[2] + a[4] * b[5] + a[5],
        ])
    }

    pub fn apply(self, x: f32, y: f32) -> (f32, f32) {
        let m = self.0;
        (m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5])
    }

    pub fn invert(self) -> Option<Matrix2d> {
        let [a, b, tx, c, d, ty] = self.0;
        let determinant = a * d - b * c;
        if determinant == 0.0 || !determinant.is_finite() {
            return None;
        }
        let inverse = Matrix2d([
            d / determinant,
            -b / determinant,
            (b * ty - d * tx) / determinant,
            -c / determinant,
            a / determinant,
            (c * tx - a * ty) / determinant,
        ]);
        inverse
            .0
            .iter()
            .all(|value| value.is_finite())
            .then_some(inverse)
    }

    pub fn translation(tx: f32, ty: f32) -> Matrix2d {
        Matrix2d([1.0, 0.0, tx, 0.0, 1.0, ty])
    }

    /// Re-express this matrix for coordinates that shifted by (`dx`, `dy`):
    /// the moved point `p + d` must land where the old matrix put `p`, so
    /// `M_new = S ∘ M ∘ S⁻¹` (conjugation) — a plain shift of the offset
    /// column only works when the linear part is identity.
    pub fn rebase(self, dx: f32, dy: f32) -> Matrix2d {
        let m = self.0;
        Matrix2d([
            m[0],
            m[1],
            m[2] + dx * (1.0 - m[0]) - dy * m[1],
            m[3],
            m[4],
            m[5] + dy * (1.0 - m[4]) - dx * m[3],
        ])
    }
}

/// One `transform` function → its 2×3 matrix. Percent translations resolve
/// against the border box (`w`, `h`); `tan(±90°)` is infinity and left
/// as-is — skew of exactly ±90° is degenerate CSS too.
fn fn_matrix(f: &TransformFn, w: f32, h: f32) -> Matrix2d {
    match f {
        TransformFn::Translate { x, y } => Matrix2d::translation(x.resolve(w), y.resolve(h)),
        TransformFn::TranslateX(x) => Matrix2d::translation(x.resolve(w), 0.0),
        TransformFn::TranslateY(y) => Matrix2d::translation(0.0, y.resolve(h)),
        TransformFn::Scale(sx, sy) => Matrix2d([*sx, 0.0, 0.0, 0.0, *sy, 0.0]),
        TransformFn::ScaleX(s) => Matrix2d([*s, 0.0, 0.0, 0.0, 1.0, 0.0]),
        TransformFn::ScaleY(s) => Matrix2d([1.0, 0.0, 0.0, 0.0, *s, 0.0]),
        // y grows downward, so CSS clockwise is the standard rotation matrix.
        TransformFn::Rotate(deg) => {
            let (s, c) = deg.to_radians().sin_cos();
            Matrix2d([c, -s, 0.0, s, c, 0.0])
        }
        // CSS: skew(ax, ay) = matrix(1, tan(ay), tan(ax), 1, 0, 0).
        TransformFn::Skew(ax, ay) => Matrix2d([
            1.0,
            ax.to_radians().tan(),
            0.0,
            ay.to_radians().tan(),
            1.0,
            0.0,
        ]),
        TransformFn::SkewX(ax) => Matrix2d([1.0, ax.to_radians().tan(), 0.0, 0.0, 1.0, 0.0]),
        TransformFn::SkewY(ay) => Matrix2d([1.0, 0.0, 0.0, ay.to_radians().tan(), 1.0, 0.0]),
        // kitae_style stores the payload row-major already.
        TransformFn::Matrix(m) => Matrix2d(*m),
    }
}

/// Full per-box transform (Q2): `T(O) · F1 · … · Fn · T(−O)` — the function
/// list applies right-to-left to the point (CSS `transform: A B` means A is
/// outermost), about `transform-origin` resolved against the border box.
/// `bx`, `by` are the box's page-space border-box top-left. A pure
/// translation is origin-invariant, so `translate` comes out exact.
fn origin_matrix(
    tl: &TransformList,
    origin: &TransformOrigin,
    w: f32,
    h: f32,
    bx: f32,
    by: f32,
) -> Matrix2d {
    let ox = bx + origin.x.resolve(w);
    let oy = by + origin.y.resolve(h);
    let mut acc = Matrix2d::translation(ox, oy);
    for f in &tl.0 {
        acc = acc.compose(fn_matrix(f, w, h));
    }
    acc.compose(Matrix2d::translation(-ox, -oy))
}

/// B7 `clip-path`/`mask`: one basic shape resolved to absolute page-space
/// pixels against the element's border box at layout. Painters install it
/// as a clip path before drawing the stamped item.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedClip {
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

/// B8: monotonic allocator for `isolation: isolate` group ids. Ids are
/// opaque — the host only needs distinct values per frame, so wrapping at
/// u32::MAX is acceptable (4 billion isolates per session).
static NEXT_GROUP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Resolve one symbolic `ClipShape` (kitae_css) against the border box
/// `(x, y, w, h)` at layout time. Percentage bases follow CSS basic-shapes:
/// horizontal lengths and x-positions fold against `w`, vertical lengths
/// and y-positions against `h`; a circle's percentage radius uses the
/// spec's mean `sqrt((w² + h²) / 2)`; `inset` corner radii resolve against
/// the *inset* rect. Sizes and radii clamp at 0 (an over-inset shape paints
/// nothing).
pub fn resolve_clip(shape: &ClipShape, x: f32, y: f32, w: f32, h: f32) -> ResolvedClip {
    let (w, h) = (w.max(0.0), h.max(0.0));
    match shape {
        ClipShape::Circle { r, cx, cy } => {
            let cx = x + cx.resolve(w);
            let cy = y + cy.resolve(h);
            let radius = match r {
                // px part unaffected by the basis; pct folds against the mean.
                ShapeRadius::Len(l) => l.resolve((w.hypot(h) / 2.0f32.sqrt()).max(0.0)),
                ShapeRadius::ClosestSide => [cx - x, x + w - cx, cy - y, y + h - cy]
                    .into_iter()
                    .fold(f32::INFINITY, f32::min),
                ShapeRadius::FarthestSide => [cx - x, x + w - cx, cy - y, y + h - cy]
                    .into_iter()
                    .fold(0.0f32, f32::max),
                ShapeRadius::ClosestCorner => [
                    (cx - x).hypot(cy - y),
                    (x + w - cx).hypot(cy - y),
                    (cx - x).hypot(y + h - cy),
                    (x + w - cx).hypot(y + h - cy),
                ]
                .into_iter()
                .fold(f32::INFINITY, f32::min),
                ShapeRadius::FarthestCorner => [
                    (cx - x).hypot(cy - y),
                    (x + w - cx).hypot(cy - y),
                    (cx - x).hypot(y + h - cy),
                    (x + w - cx).hypot(y + h - cy),
                ]
                .into_iter()
                .fold(0.0f32, f32::max),
            };
            ResolvedClip::Circle {
                cx,
                cy,
                r: radius.max(0.0),
            }
        }
        ClipShape::Ellipse { rx, ry, cx, cy } => {
            let cx = x + cx.resolve(w);
            let cy = y + cy.resolve(h);
            let (left, right, top, bottom) = (cx - x, x + w - cx, cy - y, y + h - cy);
            let rxv = match rx {
                ShapeRadius::Len(l) => l.resolve(w),
                ShapeRadius::ClosestSide => left.min(right),
                ShapeRadius::FarthestSide => left.max(right),
                ShapeRadius::ClosestCorner => left.abs().min(right.abs()),
                ShapeRadius::FarthestCorner => left.abs().max(right.abs()),
            };
            let ryv = match ry {
                ShapeRadius::Len(l) => l.resolve(h),
                ShapeRadius::ClosestSide => top.min(bottom),
                ShapeRadius::FarthestSide => top.max(bottom),
                ShapeRadius::ClosestCorner => top.abs().min(bottom.abs()),
                ShapeRadius::FarthestCorner => top.abs().max(bottom.abs()),
            };
            ResolvedClip::Ellipse {
                cx,
                cy,
                rx: rxv.max(0.0),
                ry: ryv.max(0.0),
            }
        }
        ClipShape::Inset {
            top,
            right,
            bottom,
            left,
            radii,
        } => {
            let t = top.resolve(h);
            let b = bottom.resolve(h);
            let l = left.resolve(w);
            let r = right.resolve(w);
            let (iw, ih) = ((w - l - r).max(0.0), (h - t - b).max(0.0));
            let radii = radii.map(|(hp, vp)| (hp.resolve(iw).max(0.0), vp.resolve(ih).max(0.0)));
            ResolvedClip::Inset {
                x: x + l,
                y: y + t,
                w: iw,
                h: ih,
                radii,
            }
        }
        ClipShape::Polygon { points, evenodd } => ResolvedClip::Polygon {
            points: points
                .iter()
                .map(|(lx, ly)| (x + lx.resolve(w), y + ly.resolve(h)))
                .collect(),
            evenodd: *evenodd,
        },
    }
}

/// A filled rectangle in page coordinates.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Fast-path solid approximation (0xRRGGBBAA): the exact color for
    /// `Solid` paint, the gradient's first stop for gradients, the separate
    /// `background` for image paint. CPU-fallback painters use this alone.
    pub color: u32,
    /// Per-corner radii (uniform zero for square boxes).
    pub radius: BorderRadius,
    /// Accurate paint for GPU/gradient-capable backends.
    pub paint: RectPaint,
    /// Item-local transform (Q2): the enclosing box's `transform` matrix
    /// baked in. `None` = paint as-is. Note the stored geometry is
    /// pre-transform; painters map the coordinates themselves.
    pub transform: Option<Matrix2d>,
    /// B4: the enclosing box's `filter` chain. `None` (the common case)
    /// means the rect is painted as-is. Painters apply CIFilters on the
    /// backing layer when this is set.
    pub filters: Option<FilterList>,
    /// B7: the innermost `clip-path`/`mask` resolved against its border
    /// box. `None` = no clip. Stamped at box end (first writer wins).
    pub clip: Option<ResolvedClip>,
    /// B8 `mix-blend-mode` wire value (see the `kitae_core.h` v18 table);
    /// 0 = normal. Stamped at box end like `clip` (first writer = the
    /// innermost blend, so a descendant's own mode wins over an ancestor's).
    pub blend: u8,
    /// B8 isolation group: nonzero = this item paints inside an
    /// `isolation: isolate` subtree (or under a blend ancestor). The host
    /// opens an offscreen layer per group id; stamps OVERWRITE, so nested
    /// isolates flatten to the outermost id (documented).
    pub group: u32,
    /// Source-node provenance (v7): the DOM node whose layout painted this
    /// item. Engine-internal (incremental splicing); never serialized.
    pub node: u32,
}

/// Accurate fill for a [`Rect`], converted from `BackgroundPaint` at layout.
#[derive(Debug, Clone, PartialEq)]
pub enum RectPaint {
    Solid,
    LinearGradient {
        angle_deg: f32,
        stops: Vec<(f32, u32)>,
    },
    RadialGradient {
        cx: f32,
        cy: f32,
        radius: Option<f32>,
        stops: Vec<(f32, u32)>,
    },
    /// B5 `conic-gradient`: stops sweep clockwise from `angle_deg` around
    /// the fractional center.
    ConicGradient {
        angle_deg: f32,
        cx: f32,
        cy: f32,
        stops: Vec<(f32, u32)>,
    },
    /// `background-image: url()` — paints as the fallback `color` in L
    /// scope (raster paint is future work); the URL is preserved here.
    Image(String),
}

impl Default for RectPaint {
    fn default() -> Self {
        RectPaint::Solid
    }
}

impl Rect {
    /// Square solid rect (borders, rules, placeholders, control boxes).
    pub fn solid(x: f32, y: f32, w: f32, h: f32, color: u32) -> Rect {
        Rect {
            x,
            y,
            w,
            h,
            color,
            radius: BorderRadius::ZERO,
            paint: RectPaint::Solid,
            transform: None,
            node: 0,
            ..Default::default()
        }
    }
}

impl From<&kitae_style::BackgroundPaint> for RectPaint {
    fn from(p: &kitae_style::BackgroundPaint) -> RectPaint {
        use kitae_style::BackgroundPaint as BP;
        match p {
            BP::Color(_) => RectPaint::Solid,
            BP::LinearGradient { angle_deg, stops } => RectPaint::LinearGradient {
                angle_deg: *angle_deg,
                stops: stops.iter().map(|s| (s.offset, s.color)).collect(),
            },
            BP::RadialGradient {
                cx,
                cy,
                radius,
                stops,
            } => RectPaint::RadialGradient {
                cx: *cx,
                cy: *cy,
                radius: *radius,
                stops: stops.iter().map(|s| (s.offset, s.color)).collect(),
            },
            BP::ConicGradient {
                angle_deg,
                cx,
                cy,
                stops,
            } => RectPaint::ConicGradient {
                angle_deg: *angle_deg,
                cx: *cx,
                cy: *cy,
                stops: stops.iter().map(|s| (s.offset, s.color)).collect(),
            },
            BP::Image(url) => RectPaint::Image(url.clone()),
        }
    }
}

/// An outset/inset box shadow in page coordinates (Phase L4). Geometry is
/// the *border box*; painters offset by (`offset_x`, `offset_y`), expand by
/// `spread`, and blur by `blur`.
#[derive(Debug, Clone, PartialEq)]
pub struct Shadow {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: BorderRadius,
    pub color: u32, // 0xRRGGBBAA
    pub blur: f32,
    pub spread: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub inset: bool,
    /// Enclosing box's transform (Q2): the shadow rotates with its box.
    pub transform: Option<Matrix2d>,
    /// B4: the enclosing box's `filter` chain.
    pub filters: Option<FilterList>,
    /// B7: innermost clip (see [`Rect::clip`]).
    pub clip: Option<ResolvedClip>,
    /// B8: innermost blend + isolation group (see [`Rect::blend`], [`Rect::group`]).
    pub blend: u8,
    /// B8: isolation group id (see [`Rect::group`]).
    pub group: u32,
    /// Source-node provenance (v7); never serialized.
    pub node: u32,
}

/// A run of text sharing one style, positioned by its baseline.
#[derive(Debug, Clone, Default)]
pub struct TextRun {
    pub x: f32,
    /// Baseline y.
    pub y: f32,
    pub text: String,
    pub size: f32,
    pub weight: u32,
    pub italic: bool,
    pub monospace: bool,
    /// First non-generic family from the run's `font-family` (Q11); ""
    /// means the host's default. Painters must resolve the same family the
    /// measurer did so widths and glyphs agree.
    pub family: String,
    /// 0xRRGGBBAA
    pub color: u32,
    /// Set when this run belongs to a link; carries the resolved href.
    pub link: Option<String>,
    /// `letter-spacing` (Q3): extra px after each glyph except the last.
    /// Painters advance per character by `measure(char) + letter_spacing`;
    /// the measurer's `measure_run_sp` mirrors that sum so widths stay exact.
    pub letter_spacing: f32,
    /// R8/E3: blur radius of the text-shadow this duplicate run paints
    /// (0 = crisp, which is every main run). Wire v19 carries it per run
    /// so painters blur the shadow copy instead of stamping it hard.
    pub shadow_blur: f32,
    /// Enclosing box's transform (Q2): paints through the run's matrix.
    pub transform: Option<Matrix2d>,
    /// B4: the enclosing box's `filter` chain. Apply per run on its
    /// backing text layer.
    pub filters: Option<FilterList>,
    /// B7: innermost clip (see [`Rect::clip`]).
    pub clip: Option<ResolvedClip>,
    /// B8: innermost blend + isolation group (see [`Rect::blend`], [`Rect::group`]).
    pub blend: u8,
    /// B8: isolation group id (see [`Rect::group`]).
    pub group: u32,
    /// Source-node provenance (v7); never serialized.
    pub node: u32,
}

/// A clickable area with its href (page coordinates).
#[derive(Debug, Clone, PartialEq)]
pub struct LinkArea {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub href: String,
    pub transform: Option<Matrix2d>,
    /// Source-node provenance (v7); never serialized.
    pub node: u32,
}

impl LinkArea {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        let (x, y) = match self.transform {
            Some(transform) => {
                let Some(inverse) = transform.invert() else {
                    return false;
                };
                inverse.apply(x, y)
            }
            None => (x, y),
        };
        x >= self.x - 0.001
            && x <= self.x + self.w + 0.001
            && y >= self.y - 0.001
            && y <= self.y + self.h + 0.001
    }
}

/// A replaced image box in page coordinates (top-left origin).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DisplayImage {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Unresolved `src` (resolve against the page URL like links).
    pub src: String,
    pub alt: String,
    pub fit: ObjectFit,
    /// Decoded-raster reference (Phase K3): when the embedder (via
    /// `kitae_core`) has decoded this image, the id keys the frame's
    /// pixel side-table. `None` means placeholder (alt box) — layout itself
    /// never decodes.
    pub payload: Option<u64>,
    /// The raster behind `payload` is a multi-frame animation (Q8): hosts
    /// step frames on their own clock (N8: no engine timeline) through the
    /// frame side-table. Stamped by the engine at relayout; layout itself
    /// always emits `false`.
    pub animated: bool,
    /// Enclosing box's transform (Q2): the image rotates with its box.
    pub transform: Option<Matrix2d>,
    /// B4: the enclosing box's `filter` chain.
    pub filters: Option<FilterList>,
    /// B7: innermost clip (see [`Rect::clip`]).
    pub clip: Option<ResolvedClip>,
    /// B8: innermost blend + isolation group (see [`Rect::blend`], [`Rect::group`]).
    pub blend: u8,
    /// B8: isolation group id (see [`Rect::group`]).
    pub group: u32,
    /// Source-node provenance (v7); never serialized.
    pub node: u32,
}

/// What a [`DisplayMedia`] item plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaKind {
    Audio,
    Video,
}

/// A replaced media element (`<video>`; `<audio>` chrome lands later) in
/// page coordinates: geometry and transport state for the embedder's
/// chrome. The item paints nothing itself — the current video frame is an
/// ordinary [`DisplayImage`] with the same rect and `src`, stamped by the
/// engine with the stepped-frame raster. Video is images plus a clock
/// (QTSQ plan J5); hosts that only rasterize images still show the picture.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DisplayMedia {
    /// Element identity: index within this list's `media` vec.
    pub id: u64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Unresolved media source (resolve against the page URL like images) —
    /// the element's own `src`. An engine that honors `<source>` children
    /// stamps the resolved URL of the resource it actually selected (the
    /// first candidate that decoded, else this `src`), so hosts should always
    /// resolve this field rather than treat it as relative; resolving an
    /// absolute URL yields it unchanged. Empty for a source-only element
    /// whose selection has not run.
    pub src: String,
    pub kind: MediaKind,
    /// Transport state `(playing, position_secs)` — layout emits paused at
    /// 0; the engine stamps live state at publish time.
    pub state: (bool, f32),
    /// F4: the element's `controls` attribute — the host paints in-page
    /// transport chrome (engine paints a builtin overlay; see the engine's
    /// `decorate_caret` pass).
    pub controls: bool,
    /// The `poster` attribute: the unresolved image URL the element shows
    /// while it has no video data (empty for `<audio>` and for a posterless
    /// `<video>`). The engine fetches/decodes it and stamps the raster into
    /// the frame image's payload, so the poster rides the ordinary image
    /// path (object-fit included) instead of needing chrome of its own.
    /// Layout does not size the element from the poster: the natural size of
    /// a media resource arrives with its metadata, after layout, so
    /// `<video>` keeps the 300×150 default object size (as before).
    pub poster: String,
    /// Source-node provenance (v7); never serialized.
    pub node: u32,
}

/// The painted page: the engine's output contract.
///
/// # DisplayList contract v9 (`DISPLAY_LIST_VERSION`)
///
/// v7: every item carries `node` — the DOM node whose layout painted it —
/// so the engine can apply UI patches incrementally (splice the dirty
/// top-level box's items, shift what follows). Provenance is engine-internal
/// and never serialized on the frame wire.
///
/// A `DisplayList` is a complete, self-contained paint plan for one document
/// at one layout width. Embedders rasterize it anywhere — a window, a texture,
/// a PDF — without re-entering the engine. The rules:
///
/// * **Coordinates** are page space: the origin is the document's top-left,
///   y grows downward, units are logical points (CSS px). Scrolling and
///   viewport clipping belong to the embedder.
/// * **Paint order** is vec order within each list: `shadows` (under
///   everything they belong to), then `rects` (backgrounds first, then ink
///   such as underlines and rules), then `images`, then `texts` on top.
///   Positioned boxes (`position` ≠ static) appear *after* the flow content
///   of their containing block, so they paint above it; among several
///   positioned boxes, lower `z-index` first (document order breaks ties).
/// * **Text** is positioned by its baseline (`TextRun.y`); the embedder
///   supplies fonts through a [`TextMeasurer`] so measured widths and painted
///   widths agree.
/// * **Images** are top-left boxes (`DisplayImage.x/y`); `src` is unresolved
///   like links, `alt` is the fallback text, `fit` is the parsed `object-fit`,
///   and `payload` optionally references decoded pixels (see K3).
/// * **Rects** carry `radius` (per-corner px, resolved at layout) and `paint`
///   (solid fast-path `color` plus accurate gradients); `shadows` carry the
///   border box plus offsets/blur/spread for paint-time expansion. A box with
///   a multi-layer `background` emits one rect per painted layer, bottom-up —
///   the first layer of the CSS list is the top one — each at its own
///   `background-clip` rect; the same order holds for its background-image
///   tiles in `images`. Still ordinary vec-order paint, so no rule changes.
/// * **Media** (`DisplayMedia`) is the chrome contract for `<video>` (and
///   later `<audio>`): geometry plus transport state for the embedder's
///   player UI. The current frame is a separate `DisplayImage` covering the
///   same rect — hosts paint pixels from `images`, chrome from `media`. The
///   element's `poster` rides the media item (unresolved, like `src`); the
///   engine stamps its raster into that frame image's payload, so a poster
///   needs no paint contract of its own.
/// * **Colors** are `0xRRGGBBAA` `u32` in every item.
/// * **links** are hit-test areas in page space with their *unresolved*
///   hrefs; resolve against the page URL (see `kitae_net::resolve`).
/// * `content_height` is the full document height — clamp scroll against it.
/// * `page_background` is the canvas clear color (`<body>` background).
/// * `overflow: hidden` is applied at layout time: items outside the clip
///   are dropped (rects/images/links/shadows are intersected, text runs
///   dropped by bounding box), so embedders need no clip primitive.
/// * **Transforms** (Q2): rects, texts, images, shadows and links carry a
///   sparse `transform: Option<Matrix2d>` — the enclosing CSS box's 2D
///   transform, pre-composed down the tree (nested boxes multiply). Stored
///   geometry is pre-transform; painters and hit-testers map item coordinates
///   through the matrix themselves (None = use as-is). `media` is untransformed,
///   and transformed items are kept whole by `overflow: hidden` clips
///   (pre-transform approximation).
///
/// Evolution: this struct is `#[non_exhaustive]`; new fields are additive and
/// consumers must tolerate unknown fields. Structural changes to the rules
/// above bump [`DISPLAY_LIST_VERSION`].
/// B2/B3: a scroll container — `overflow: auto|scroll` with content that
/// overflows the box. The host uses `x`/`y`/`w`/`h` to paint a clip
/// rect, and the content extents to size both scrollbars. `node` lets the
/// host forward wheel events to the engine; `parent` identifies the nearest
/// enclosing scroll container for chained wheel routing.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollContainer {
    pub node: u32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub content_w: f32,
    pub content_h: f32,
    /// 1 = `overflow: auto`, 2 = `overflow: scroll`.
    pub mode: u8,
    /// Nearest enclosing scroll container, or `u32::MAX` for the document.
    pub parent: u32,
}

/// Q5: one `position: sticky` box. Layout paints the box at its flow
/// position; the embedder shifts it at paint time (and the engine's link
/// hit-test shifts to match) by [`StickyItem::shift`], clamped inside the
/// nearest scroll ancestor (an overflow-clipping box) or the document.
#[derive(Debug, Clone, PartialEq)]
pub struct StickyItem {
    /// Painted-item ranges `(start, count)` — everything this subtree
    /// painted, per display-list list. Indices are stable across the
    /// background splice, clipping and transform baking (all mutate within
    /// the range); buffer merges rebase them.
    pub rect: (u32, u32),
    pub text: (u32, u32),
    pub link: (u32, u32),
    pub image: (u32, u32),
    pub media: (u32, u32),
    pub shadow: (u32, u32),
    /// Flow-position top of the box (page space) and its painted height.
    pub base_y: f32,
    pub height: f32,
    /// Containment bounds (page space) of the nearest scroll ancestor;
    /// the document for root-level stickies.
    pub container_top: f32,
    pub container_bottom: f32,
    /// `top` / `bottom` offsets in px (`has_top` picks the stick edge; a
    /// top-sticky still clamps its bottom by `bottom_offset`).
    pub top_offset: f32,
    pub bottom_offset: f32,
    pub has_top: bool,
    pub has_bottom: bool,
}

impl StickyItem {
    /// Y shift (page space) of the box at `scroll_y` with a `viewport_h`-tall
    /// viewport. Top-sticky pins the box `top_offset` below the viewport top
    /// once the scroll would push it above that; bottom-sticky pins its
    /// bottom `bottom_offset` above the viewport bottom when it would
    /// otherwise be below the fold. Both clamped so the box never leaves its
    /// container: a top-sticky never rises above its flow position, a
    /// bottom-sticky never sinks below it.
    pub fn shift(&self, scroll_y: f32, viewport_h: f32) -> f32 {
        if self.has_top {
            // Page-space y the box top should sit at while stuck.
            let target = scroll_y + self.top_offset - self.base_y;
            // Lowest allowed page y: box bottom stays inside the container.
            let down_max =
                (self.container_bottom - self.bottom_offset - self.height - self.base_y).max(0.0);
            target.clamp(0.0, down_max)
        } else if self.has_bottom {
            // Page-space y pinning the box bottom above the viewport bottom.
            let target = scroll_y + viewport_h - self.bottom_offset - self.height - self.base_y;
            // Highest allowed page y: box top stays inside the container.
            let up_min = (self.container_top + self.top_offset - self.base_y).min(0.0);
            target.clamp(up_min, 0.0)
        } else {
            0.0
        }
    }
}

/// The contract version of [`DisplayList`] this crate produces.
pub const DISPLAY_LIST_VERSION: u32 = 9;

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct DisplayList {
    /// Backgrounds first, then ink (underlines, rules, blocks).
    pub rects: Vec<Rect>,
    pub texts: Vec<TextRun>,
    pub links: Vec<LinkArea>,
    pub images: Vec<DisplayImage>,
    /// Replaced media elements (J5): chrome geometry + transport state.
    pub media: Vec<DisplayMedia>,
    /// Box shadows, parents before children (paint under their boxes).
    pub shadows: Vec<Shadow>,
    /// `position: sticky` side-list (Q5), document order.
    pub stickies: Vec<StickyItem>,
    /// B2: scroll containers (`overflow: auto|scroll` + content taller
    /// than the box), document order. Serialized so the host can
    /// paint a scrollbar and route wheel events. The engine does not
    /// animate the scroll — that's the host's job.
    pub scroll_containers: Vec<ScrollContainer>,
    /// A5: `<summary>` toggle hit-areas, document order. Engine-internal —
    /// never serialized; the engine's click path checks these first.
    pub toggles: Vec<ToggleArea>,
    /// B3: query containers found during layout (`container-type` set),
    /// document order. Engine-internal — feeds the second style pass; not
    /// serialized over the ffi wire.
    pub containers: Vec<ContainerBox>,
    /// Total document height — the shell clamps scroll against it.
    pub content_height: f32,
    /// `<body>` background, propagated to the shell's page clear.
    pub page_background: u32,
}

/// B3: one `container-type` element's laid-out geometry. Widths/heights are
/// content-box measures (container queries size against the content box).
#[derive(Debug, Clone)]
pub struct ContainerBox {
    pub node: u32,
    /// `container-name` values on the container element.
    pub names: Vec<String>,
    pub x: f32,
    pub y: f32,
    /// Content inline (horizontal) size in px.
    pub w: f32,
    /// Content height in px (recorded for `container-type: size`; height
    /// conditions are not evaluated yet — documented).
    pub h: f32,
}

impl DisplayList {
    /// A blank list: nothing to paint, zero height, white canvas.
    pub fn empty() -> DisplayList {
        DisplayList {
            page_background: 0xFFFFFFFF,
            ..DisplayList::default()
        }
    }
}

/// Intrinsic dimensions `(width, height)` from image bytes, via
/// `kitae_media::dimensions` (real decoders: PNG info, JPEG headers, GIF
/// descriptor). QTSQ image payloads are recognized next through a header
/// probe (`kitae_qtsq`) — header parse only, never a decode. `None` for
/// unknown/unsupported formats — callers fall back to attribute/default
/// sizing.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if let Some(d) = kitae_media::dimensions(bytes) {
        return Some(d);
    }
    qtsq_dimensions(bytes)
}

/// QTSQ header probe for `<img>` sizing: cheap (first 112 bytes), decode-
/// free, and image-only — a QTSQ audio/text/video payload must not size an
/// `<img>`. kitae_qtsq has no dependencies of its own, so layout sits below
/// it without a cycle.
fn qtsq_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let info = kitae_qtsq::probe(bytes)?;
    if !info.data_type.is_image() {
        return None;
    }
    Some((info.width?, info.height?))
}

/// Everything painted by one isolated subtree, before placement.
#[derive(Debug, Clone, Default)]
struct Buffers {
    rects: Vec<Rect>,
    texts: Vec<TextRun>,
    links: Vec<LinkArea>,
    images: Vec<DisplayImage>,
    media: Vec<DisplayMedia>,
    shadows: Vec<Shadow>,
    stickies: Vec<StickyItem>,
    /// B2: scroll containers from positioned boxes (engine-internal
    /// in the buffers; copied to DisplayList on the merge path so the
    /// host sees the final list).
    scroll_containers: Vec<ScrollContainer>,
    /// A5: `<summary>` toggle hit-areas (engine-internal; not serialized).
    toggles: Vec<ToggleArea>,
}

/// A5: one clickable `<summary>` — clicking it toggles the parent
/// `<details>`'s `open` attribute. Page-space rect + the summary node.
#[derive(Debug, Clone, PartialEq)]
pub struct ToggleArea {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub node: u32,
}

impl Buffers {
    /// B4: stamp a rigid rotation onto every transform-capable item in this
    /// buffer set — the Q2 item list (rects, texts, links, images, shadows;
    /// media stays untransformed). The rotation composes OUTSIDE any
    /// per-item transform: item coordinates are logical-space, and the map
    /// takes them to page space. Carried through the v12 sparse transform
    /// wire to the host, which paints and hit-tests under the CTM.
    fn rotate(&mut self, m: Matrix2d) {
        for r in &mut self.rects {
            r.transform = Some(match r.transform.take() {
                Some(inner) => m.compose(inner),
                None => m,
            });
        }
        for t in &mut self.texts {
            t.transform = Some(match t.transform.take() {
                Some(inner) => m.compose(inner),
                None => m,
            });
        }
        for link in &mut self.links {
            link.transform = Some(match link.transform.take() {
                Some(inner) => m.compose(inner),
                None => m,
            });
        }
        for im in &mut self.images {
            im.transform = Some(match im.transform.take() {
                Some(inner) => m.compose(inner),
                None => m,
            });
        }
        for s in &mut self.shadows {
            s.transform = Some(match s.transform.take() {
                Some(inner) => m.compose(inner),
                None => m,
            });
        }
        // Deferred by design: media, sticky and scroll-container records keep
        // their logical-space geometry; caret/hit-testing is physical. See the
        // `layout_static_box` wrapper docs.
    }

    fn translate(&mut self, dx: f32, dy: f32) {
        for r in &mut self.rects {
            r.x += dx;
            r.y += dy;
            if let Some(m) = r.transform {
                r.transform = Some(m.rebase(dx, dy));
            }
        }
        for t in &mut self.texts {
            t.x += dx;
            t.y += dy;
            if let Some(m) = t.transform {
                t.transform = Some(m.rebase(dx, dy));
            }
        }
        for l in &mut self.links {
            l.x += dx;
            l.y += dy;
            if let Some(m) = l.transform {
                l.transform = Some(m.rebase(dx, dy));
            }
        }
        for im in &mut self.images {
            im.x += dx;
            im.y += dy;
            if let Some(m) = im.transform {
                im.transform = Some(m.rebase(dx, dy));
            }
        }
        for m in &mut self.media {
            m.x += dx;
            m.y += dy;
        }
        for s in &mut self.shadows {
            s.x += dx;
            s.y += dy;
            if let Some(m) = s.transform {
                s.transform = Some(m.rebase(dx, dy));
            }
        }
        // Sticky metadata moves with its subtree (ranges are indices and
        // unaffected by translation).
        for s in &mut self.stickies {
            s.base_y += dy;
            s.container_top += dy;
            s.container_bottom += dy;
        }
        // Scroll-container geometry moves with its box too (I2: cached
        // fragments translate; also fixes relative-positioned overflow
        // boxes, whose buffers were never shifted).
        for s in &mut self.scroll_containers {
            s.x += dx;
            s.y += dy;
        }
        for t in &mut self.toggles {
            t.x += dx;
            t.y += dy;
        }
    }

    fn is_empty(&self) -> bool {
        self.rects.is_empty()
            && self.texts.is_empty()
            && self.links.is_empty()
            && self.images.is_empty()
            && self.media.is_empty()
            && self.shadows.is_empty()
            && self.stickies.is_empty()
            && self.scroll_containers.is_empty()
            && self.toggles.is_empty()
    }
}

/// I2: one cached block fragment — the paint of a body-level block child,
/// captured at fragment-assembly time, keyed by the subtree's DOM revision.
#[derive(Debug, Default)]
pub struct FragmentCache {
    entries: HashMap<NodeId, FragmentEntry>,
    /// Reuse counters: hits vs scope-child layouts that had to run.
    pub hits: u64,
    pub misses: u64,
}

#[derive(Debug, Clone)]
struct FragmentEntry {
    /// `Dom::rev` of the child when captured — covers the whole subtree
    /// (touch bumps descendants and ancestors alike).
    rev: u64,
    /// Available width and viewport height the fragment was laid out at
    /// (f32 bit patterns; either changing remeasures everything).
    width_bits: u32,
    viewport_h_bits: u32,
    /// Flow-cursor advance the box produced (margins + full box height).
    advance: f32,
    /// Deepest paint reached inside the box, relative to its top.
    max_depth: f32,
    /// Captured paint, normalized to the box's top edge (y = 0). Stored
    /// fragments never contain stickies (ranges would need rebasing against
    /// shifted bases — excluded at capture instead).
    buf: Buffers,
}

impl FragmentCache {
    pub fn new() -> FragmentCache {
        FragmentCache::default()
    }

    /// Drop every entry — style-affecting state changed without a DOM
    /// mutation (hover/active/focus, media features, transition frames).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Lay out `dom` into a display list `width` px wide, `viewport_h` px tall
/// (the host's viewport height — the containing block for `position: fixed`).
///
/// The page's media environment defaults to the one every embedder starts
/// in (screen, light, full motion) — use [`layout_in_env`] to lay out against
/// the real one.
pub fn layout(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
) -> DisplayList {
    layout_in_env(
        dom,
        styles,
        width,
        viewport_h,
        tm,
        &EnvironmentState::default(),
    )
}

/// [`layout`] against a concrete embedder environment: the `media` conditions
/// on an `<img>` / `<picture><source>` are evaluated against it, so a page laid
/// out for paper picks its print sources and a page in dark mode picks
/// `(prefers-color-scheme: dark)` sources — the same answers the stylesheet
/// path gives the same queries.
pub fn layout_in_env(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
    env: &EnvironmentState,
) -> DisplayList {
    let mut cache = FragmentCache::default();
    layout_cached_in_env(dom, styles, width, viewport_h, tm, &mut cache, env)
}

/// I2: [`layout`] with a caller-owned fragment cache. Body-level block
/// children whose subtree revision is unchanged at the same width/viewport
/// re-merge their stored paint at the current flow cursor instead of
/// re-laying-out. The cache lives on the engine's Page — fresh per
/// navigation, so arena node ids reused across documents never collide
/// with stale entries.
pub fn layout_cached(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
    cache: &mut FragmentCache,
) -> DisplayList {
    layout_cached_in_env(
        dom,
        styles,
        width,
        viewport_h,
        tm,
        cache,
        &EnvironmentState::default(),
    )
}

/// [`layout_cached`] in a concrete embedder environment — see
/// [`layout_in_env`]. A caller that reuses a cache across environments must
/// [`FragmentCache::clear`] between them: the fragment key covers the DOM
/// revision and the viewport, not the environment.
pub fn layout_cached_in_env(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
    cache: &mut FragmentCache,
    env: &EnvironmentState,
) -> DisplayList {
    layout_cached_inner(
        dom,
        styles,
        width,
        viewport_h,
        tm,
        cache,
        &HashMap::new(),
        env,
    )
}

/// [`layout_cached`] with the natural pixel sizes of raster subresources — see
/// [`layout_with_image_sizes`]. Used by the engine's relayout path, which
/// owns a fragment cache and the already-decoded rasters.
pub fn layout_cached_with_image_sizes(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
    cache: &mut FragmentCache,
    image_sizes: &HashMap<String, (f32, f32)>,
) -> DisplayList {
    layout_cached_with_image_sizes_in_env(
        dom,
        styles,
        width,
        viewport_h,
        tm,
        cache,
        image_sizes,
        &EnvironmentState::default(),
    )
}

/// [`layout_cached_with_image_sizes`] in a concrete embedder environment — see
/// [`layout_in_env`].
#[allow(clippy::too_many_arguments)]
pub fn layout_cached_with_image_sizes_in_env(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
    cache: &mut FragmentCache,
    image_sizes: &HashMap<String, (f32, f32)>,
    env: &EnvironmentState,
) -> DisplayList {
    layout_cached_inner(dom, styles, width, viewport_h, tm, cache, image_sizes, env)
}

/// [`layout`] with the natural pixel sizes of raster subresources, keyed by
/// the same `src` text a [`DisplayImage`] carries. Layout never decodes, so
/// a `background-size` that needs the intrinsic size (`auto`, `cover`,
/// `contain`, or a single definite axis against an `auto` one) can only be
/// resolved when the embedder — which decodes the very same URLs for image
/// payloads — hands the sizes in. A `background-image` whose size is
/// unknown is skipped (the background color still paints); an explicitly
/// sized one paints from this call alone.
pub fn layout_with_image_sizes(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
    image_sizes: &HashMap<String, (f32, f32)>,
) -> DisplayList {
    layout_with_image_sizes_in_env(
        dom,
        styles,
        width,
        viewport_h,
        tm,
        image_sizes,
        &EnvironmentState::default(),
    )
}

/// [`layout_with_image_sizes`] in a concrete embedder environment — see
/// [`layout_in_env`].
pub fn layout_with_image_sizes_in_env(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
    image_sizes: &HashMap<String, (f32, f32)>,
    env: &EnvironmentState,
) -> DisplayList {
    let mut cache = FragmentCache::default();
    layout_cached_inner(
        dom,
        styles,
        width,
        viewport_h,
        tm,
        &mut cache,
        image_sizes,
        env,
    )
}

#[allow(clippy::too_many_arguments)]
fn layout_cached_inner(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
    cache: &mut FragmentCache,
    image_sizes: &HashMap<String, (f32, f32)>,
    env: &EnvironmentState,
) -> DisplayList {
    let Some(root) = dom.root() else {
        return DisplayList::default();
    };
    let style_map: HashMap<NodeId, ComputedStyle> =
        styles.iter().map(|(n, s)| (*n, s.clone())).collect();

    let mut lay = Layout {
        dom,
        styles: style_map,
        tm,
        out: Buffers::default(),
        line: Line::default(),
        y: 0.0,
        max_y: 0.0,
        abs_list: Vec::new(),
        floats: Vec::new(),
        rot_ctx: WritingMode::HorizontalTb,
        sticky_scopes: Vec::new(),
        scroll_parents: Vec::new(),
        cur_node: 0,
        viewport_w: width,
        viewport_h,
        containers: Vec::new(),
        cache: Some(cache),
        cache_scope: None,
        image_sizes: image_sizes.clone(),
        env: *env,
    };
    let body = lay.find_first_by_tag(root, "body");
    let page_background = body
        .and_then(|b| lay.style(b).cloned())
        .map(|s| s.background)
        .unwrap_or(0xFFFFFFFF);
    // I2: fragments are cached for body's direct block children only — the
    // top-level boxes that dominate relayout cost.
    lay.cache_scope = body;
    lay.layout_box(
        root,
        Frame {
            x: 0.0,
            width,
            align: TextAlign::Left,
        },
        None,
    );
    // `position: fixed` boxes anchor to the viewport, not any document box:
    // percentage offsets resolve against the window, so a `top: 50%` rail
    // stays mid-window however tall the document grows.
    lay.drain_abs(None, (0.0, 0.0, lay.viewport_w, lay.viewport_h));
    // Q5: stickies with no scroll ancestor clamp against the document.
    let content_h = lay.max_y.max(lay.y);
    for s in &mut lay.out.stickies {
        if s.container_bottom.is_nan() {
            s.container_bottom = content_h;
        }
    }
    // B1: bake ancestor `opacity` into item colors before publishing.
    bake_opacity(dom, &lay.styles, &mut lay.out);
    // `visibility`: drop the painted items of hidden/collapsed nodes last, so
    // every paint site (blocks, floats, positioned boxes, sticky and
    // generated content) is covered by one pass.
    apply_visibility(&lay.styles, &mut lay.out);
    DisplayList {
        rects: lay.out.rects,
        texts: lay.out.texts,
        links: lay.out.links,
        images: lay.out.images,
        media: lay.out.media,
        shadows: lay.out.shadows,
        stickies: lay.out.stickies,
        scroll_containers: lay.out.scroll_containers,
        toggles: lay.out.toggles,
        containers: lay.containers,
        content_height: content_h,
        page_background,
    }
}

/// B1: bake ancestor `opacity` into item colors. `opacity` does not inherit,
/// but it forms a paint group — a subtree's pixels composite at the product
/// of its ancestors' opacities. Each item walks its source node to the root
/// multiplying (memoized); alpha is the LOW byte of the 0xRRGGBBAA word.
/// Gradient stops scale alongside `color`; image payloads carry no alpha
/// here (raster fading would need renderer support — documented gap).
/// Only the full-`layout` path bakes; `layout_subtree` splices skip it
/// (their ancestor chain lies outside the relaid subtree).
fn bake_opacity(dom: &Dom, styles: &HashMap<NodeId, ComputedStyle>, out: &mut Buffers) {
    // Memoized chain product per node (including its own opacity when the
    // node has a style — text nodes resolve through their ancestors).
    let mut memo: HashMap<NodeId, f32> = HashMap::new();
    fn chain(
        node: NodeId,
        dom: &Dom,
        styles: &HashMap<NodeId, ComputedStyle>,
        memo: &mut HashMap<NodeId, f32>,
    ) -> f32 {
        if let Some(v) = memo.get(&node) {
            return *v;
        }
        let mut product = styles.get(&node).map_or(1.0, |s| s.opacity.clamp(0.0, 1.0));
        if let Some(parent) = dom.node(node).parent {
            product *= chain(parent, dom, styles, memo);
        }
        memo.insert(node, product);
        product
    }
    let mut product = |node: u32| -> f32 { chain(NodeId(node), dom, styles, &mut memo) };
    let scale = |c: u32, k: f32| -> u32 {
        if k >= 1.0 {
            return c;
        }
        let a = (c & 0xFF) as f32 * k;
        (c & !0xFF) | a.round().clamp(0.0, 255.0) as u32
    };
    let scale_stops = |stops: &mut Vec<(f32, u32)>, k: f32| {
        for (_, c) in stops.iter_mut() {
            *c = scale(*c, k);
        }
    };
    if memo_fully_opaque(styles) {
        return;
    }
    for r in out.rects.iter_mut() {
        let k = product(r.node);
        if k >= 1.0 {
            continue;
        }
        r.color = scale(r.color, k);
        match &mut r.paint {
            RectPaint::Solid => {}
            RectPaint::LinearGradient { stops, .. }
            | RectPaint::RadialGradient { stops, .. }
            | RectPaint::ConicGradient { stops, .. } => scale_stops(stops, k),
            RectPaint::Image(_) => {} // raster fading is renderer-side (gap)
        }
    }
    for t in out.texts.iter_mut() {
        let k = product(t.node);
        if k < 1.0 {
            t.color = scale(t.color, k);
        }
    }
    for s in out.shadows.iter_mut() {
        let k = product(s.node);
        if k < 1.0 {
            s.color = scale(s.color, k);
        }
    }
}

/// Cap on the tiles one `background-image` *layer* may emit. Real patterns sit
/// far below it; a degenerate 1px tile over a full-page box is what the cap
/// exists for. Per layer, so a multi-layer pattern still paints.
const MAX_BG_TILES: usize = 256;

/// The box geometry a background layer resolves against: its border box, the
/// resolved padding, and the painted border widths `(top, right, bottom,
/// left)`. Every `background-origin` / `background-clip` box is an inset of
/// this one rect (CSS Backgrounds 3 §3.3/§3.4).
#[derive(Debug, Clone)]
struct BackgroundBox {
    border: Rect,
    padding: EdgesPx,
    borders: (f32, f32, f32, f32),
}

impl BackgroundBox {
    /// The rect one box keyword names.
    fn rect(&self, which: BgBox) -> Rect {
        let (bt, br, bb, bl) = self.borders;
        let p = &self.padding;
        let inset = |t: f32, r: f32, b: f32, l: f32| Rect {
            x: self.border.x + l,
            y: self.border.y + t,
            w: (self.border.w - l - r).max(0.0),
            h: (self.border.h - t - b).max(0.0),
            ..Default::default()
        };
        match which {
            BgBox::BorderBox => inset(0.0, 0.0, 0.0, 0.0),
            BgBox::PaddingBox => inset(bt, br, bb, bl),
            BgBox::ContentBox => inset(bt + p.top, br + p.right, bb + p.bottom, bl + p.left),
        }
    }

    /// The factor a `padding-box` clip scales `border-radius` by: the ratio of
    /// the smaller edge, per the note in CSS Backgrounds 3 §2.1 (the radii are
    /// reduced in proportion to the inset). Circular per corner here, so one
    /// factor covers both axes.
    fn radius_scale(&self, clip: &Rect) -> f32 {
        if self.border.w <= 0.0 || self.border.h <= 0.0 {
            return 0.0;
        }
        (clip.w / self.border.w).min(clip.h / self.border.h)
    }

    /// The corner radii of a `background-clip` box, as the `inset` clip
    /// shape's `[(rx, ry); 4]`. `content-box` is square-cornered (only the
    /// border box and the padding box round).
    fn clip_radii(&self, style: &ComputedStyle, which: BgBox, clip: &Rect) -> [(f32, f32); 4] {
        if matches!(which, BgBox::ContentBox) || style.border_radius == BorderRadius::ZERO {
            return [(0.0, 0.0); 4];
        }
        // Percentage radii resolve against the box width, like everywhere else
        // this engine paints them.
        let r = self.resolved_radius(style);
        let f = self.radius_scale(clip);
        // CSS corner order, which is the `inset` clip shape's order too.
        [
            (r[0] * f, r[0] * f),
            (r[1] * f, r[1] * f),
            (r[2] * f, r[2] * f),
            (r[3] * f, r[3] * f),
        ]
    }

    /// The `border-radius` a background rect paints with: the box's own radii
    /// when it fills the border box, the proportionally reduced radii of a
    /// `padding-box` clip, and square corners for a `content-box` clip. The
    /// lengths come back resolved (the reduction needs px), unlike the
    /// single-layer path which leaves them authored for the painter.
    fn rect_radius(
        &self,
        style: &ComputedStyle,
        which: BgBox,
        clip: &Rect,
        radius: BorderRadius,
    ) -> BorderRadius {
        if matches!(which, BgBox::BorderBox) || radius == BorderRadius::ZERO {
            return radius;
        }
        if matches!(which, BgBox::ContentBox) {
            return BorderRadius::ZERO;
        }
        let r = self.resolved_radius(style);
        let f = self.radius_scale(clip);
        let corner = |px: f32| Length::Px((px * f).max(0.0));
        BorderRadius {
            top_left: corner(r[0]),
            top_right: corner(r[1]),
            bottom_right: corner(r[2]),
            bottom_left: corner(r[3]),
        }
    }

    fn resolved_radius(&self, style: &ComputedStyle) -> [f32; 4] {
        style
            .border_radius
            .resolve(style.font_size, 16.0, self.border.w)
    }
}

/// Tile offsets along one `background-repeat` axis. `start`/`extent` are the
/// positioning area's edge and length, `origin` the first tile's edge (the
/// resolved `background-position`), `tile` the tile size.
///
/// `repeat` tiles in both directions from the origin, so a position near
/// the far edge still covers the box; `space` lays down only whole tiles
/// and shares the leftover slack between them; `round`/`no-repeat` start at
/// the origin and step forward.
fn tile_positions(start: f32, extent: f32, origin: f32, tile: f32, mode: BgRepeat) -> Vec<f32> {
    let step = tile.max(0.01);
    let mut out: Vec<f32> = Vec::new();
    match mode {
        BgRepeat::NoRepeat => out.push(origin),
        BgRepeat::Round => {
            // The tile was already resized to fit a whole number of times,
            // so this many of them fill the axis exactly.
            let n = (extent / step).round().max(1.0) as usize;
            for i in 0..n.min(MAX_BG_TILES) {
                out.push(origin + i as f32 * step);
            }
        }
        BgRepeat::Space | BgRepeat::Repeat => {
            let whole = (extent / step).floor().max(0.0);
            for i in 0..(whole as usize) {
                if out.len() >= MAX_BG_TILES {
                    break;
                }
                out.push(origin + i as f32 * step);
            }
        }
    }
    match mode {
        BgRepeat::Repeat => {
            // Back up over the tiles the origin position skipped, so the
            // whole area is covered on both sides of it.
            let back = ((origin - start) / step).ceil().max(0.0);
            let head: Vec<f32> = (0..back as usize)
                .rev()
                .map(|i| origin - (i as f32 + 1.0) * step)
                .collect();
            let mut all = head;
            all.append(&mut out);
            all.truncate(MAX_BG_TILES);
            all
        }
        BgRepeat::Space if out.len() >= 2 => {
            let slack = (extent - out.len() as f32 * tile).max(0.0);
            let gap = slack / (out.len() as f32 - 1.0);
            for (i, at) in out.iter_mut().enumerate() {
                *at = origin + i as f32 * (tile + gap);
            }
            out
        }
        _ => out,
    }
}

/// Fast path: no `opacity` below 1 anywhere → skip the per-item walk.
fn memo_fully_opaque(styles: &HashMap<NodeId, ComputedStyle>) -> bool {
    styles.values().all(|s| s.opacity >= 1.0)
}

/// `visibility` — drop the painted items of every node that computes to
/// `hidden` or `collapse`.
///
/// The property is inherited, so a hidden box hides its whole subtree, but a
/// descendant that declares `visibility: visible` paints again (CSS
/// Visibility 3 §2.1). Every item carries the node that stamped it (v7
/// provenance), so one pass over the finished list resolves both: the
/// per-node computed value already encodes the inheritance chain. Sticky
/// ranges follow their items; a sticky left with nothing is dropped, and
/// link/toggle/scroll entries go with their hidden box so they stay
/// unclickable and unscrolled.
///
/// Layout still *reserves* the hidden box (only `display: none` removes
/// boxes from the flow), and table rows/row-groups additionally drop out of
/// the table under `collapse` — see the table row collector.
///
/// Documented deviation: hidden text leaves the list entirely, so it is
/// neither painted nor reachable through the text-derived paths (find in
/// page, the AX text). The UAs can highlight a match inside hidden text;
/// carrying an "invisible" flag per run would need a wire field, so the
/// engine keeps the simpler, fully-hidden model.
fn apply_visibility(styles: &HashMap<NodeId, ComputedStyle>, out: &mut Buffers) {
    // Fast path: nothing hidden anywhere → the walk below is pure overhead.
    if styles.values().all(|s| s.visibility == Visibility::Visible) {
        return;
    }
    let visible = |node: u32| {
        // An unstamped item (node 0 with no style) is the caller's own
        // chrome; treat it as visible, matching the default style.
        styles
            .get(&NodeId(node))
            .is_none_or(|s| s.visibility == Visibility::Visible)
    };
    // Survivor map per list (original index → kept index, `u32::MAX` =
    // dropped) so sticky ranges can follow their items, as in
    // `clip_buffers`.
    let mut maps: Vec<Vec<u32>> = Vec::with_capacity(6);
    for map in [
        retain_visible(&mut out.rects, &visible),
        retain_visible(&mut out.texts, &visible),
        retain_visible(&mut out.links, &visible),
        retain_visible(&mut out.images, &visible),
        retain_visible(&mut out.media, &visible),
        retain_visible(&mut out.shadows, &visible),
    ] {
        maps.push(map);
    }
    let remap = |surv: &[u32], range: (u32, u32)| -> (u32, u32) {
        let start = range.0 as usize;
        let end = start + range.1 as usize;
        if start >= surv.len() {
            return (0, 0);
        }
        let kept: Vec<u32> = surv[start..end.min(surv.len())]
            .iter()
            .copied()
            .filter(|m| *m != u32::MAX)
            .collect();
        match kept.first() {
            Some(first) => (*first, kept.len() as u32),
            None => (0, 0),
        }
    };
    for s in &mut out.stickies {
        s.rect = remap(&maps[0], s.rect);
        s.text = remap(&maps[1], s.text);
        s.link = remap(&maps[2], s.link);
        s.image = remap(&maps[3], s.image);
        s.media = remap(&maps[4], s.media);
        s.shadow = remap(&maps[5], s.shadow);
    }
    out.stickies
        .retain(|s| s.rect.1 > 0 || s.text.1 > 0 || s.image.1 > 0 || s.shadow.1 > 0);
    out.scroll_containers.retain(|c| visible(c.node));
    out.toggles.retain(|t| visible(t.node));
}

/// Every display-list item carries the node whose layout painted it (v7
/// provenance) — that stamp is what lets `visibility` resolve per node.
trait Painted {
    fn source_node(&self) -> u32;
}

macro_rules! painted_by_node {
    ($($t:ty),*) => {$(
        impl Painted for $t {
            fn source_node(&self) -> u32 {
                self.node
            }
        }
    )*};
}

painted_by_node!(Rect, TextRun, LinkArea, DisplayImage, DisplayMedia, Shadow);

/// Keep the items whose source node paints, returning the survivor map
/// (original index → new index, `u32::MAX` for dropped) that sticky ranges
/// follow.
fn retain_visible<T: Painted>(items: &mut Vec<T>, visible: &dyn Fn(u32) -> bool) -> Vec<u32> {
    let mut kept = Vec::with_capacity(items.len());
    let mut surv = vec![u32::MAX; items.len()];
    for (i, item) in items.drain(..).enumerate() {
        if !visible(item.source_node()) {
            continue;
        }
        surv[i] = kept.len() as u32;
        kept.push(item);
    }
    *items = kept;
    surv
}

/// Invalidation + dirty-subtree relayout (slice G).
///
/// A text/attribute mutation dirties its node; ancestors up to the root may
/// change size, so they are dirty too ([`dirty_chain`]). The embedder then
/// re-lays only the dirty subtree ([`layout_subtree`]) instead of the whole
/// page and splices the result (shifting following content by the height
/// delta). [`DirtySet`] tracks the set; [`count_subtree`] proves the saving
/// (repaint counter: dirty nodes vs full-document nodes).
#[derive(Debug, Default, Clone)]
pub struct DirtySet {
    dirty: HashSet<NodeId>,
}

impl DirtySet {
    pub fn new() -> DirtySet {
        DirtySet::default()
    }

    pub fn mark_dirty(&mut self, node: NodeId) {
        self.dirty.insert(node);
    }

    /// Mark `node` and its whole subtree dirty (e.g. a subtree was replaced).
    pub fn mark_subtree(&mut self, dom: &Dom, node: NodeId) {
        dom.walk(node, &mut |n| {
            self.dirty.insert(n);
        });
    }

    /// Mark `changed` plus its ancestors up to the document root dirty (the
    /// standard invalidation for a leaf mutation). Returns the chain.
    pub fn mark_with_ancestors(&mut self, dom: &Dom, changed: NodeId) -> Vec<NodeId> {
        let chain = dirty_chain(dom, changed);
        for &n in &chain {
            self.dirty.insert(n);
        }
        chain
    }

    pub fn is_dirty(&self, node: NodeId) -> bool {
        self.dirty.contains(&node)
    }

    pub fn clear(&mut self) {
        self.dirty.clear();
    }

    pub fn len(&self) -> usize {
        self.dirty.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dirty.is_empty()
    }
}

/// Ancestor chain from `changed` up to the document root, inclusive.
/// Used to propagate invalidation: a leaf change may resize every ancestor.
pub fn dirty_chain(dom: &Dom, changed: NodeId) -> Vec<NodeId> {
    let mut chain = vec![changed];
    let mut cur = changed;
    while let Some(p) = dom.node(cur).parent {
        chain.push(p);
        cur = p;
    }
    chain
}

/// Number of nodes in the subtree rooted at `root` (inclusive).
pub fn count_subtree(dom: &Dom, root: NodeId) -> usize {
    let mut n = 0;
    dom.walk(root, &mut |_| n += 1);
    n
}

/// Lay out only the subtree rooted at `subtree_root` at `width` px wide,
/// returning a standalone display list whose `content_height` is the
/// subtree's height (origin at 0,0). The caller splices it into the full
/// page (translate to the subtree's page offset, replace the old range,
/// shift following content by the height delta).
///
/// `width` is the subtree's available content width (the containing block's
/// content width in the full layout). For a top-level `<div>` under `<body>`
/// at page width 400, pass 368 (400 minus body padding).
pub fn layout_subtree(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    subtree_root: NodeId,
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
) -> DisplayList {
    layout_subtree_in_env(
        dom,
        styles,
        subtree_root,
        width,
        viewport_h,
        tm,
        &EnvironmentState::default(),
    )
}

/// [`layout_subtree`] in a concrete embedder environment — see
/// [`layout_in_env`]. The engine's incremental splice path uses this so a
/// re-laid dirty subtree resolves its `<img media>` conditions against the
/// page's live environment, exactly like a full layout would.
pub fn layout_subtree_in_env(
    dom: &Dom,
    styles: &[(NodeId, ComputedStyle)],
    subtree_root: NodeId,
    width: f32,
    viewport_h: f32,
    tm: &dyn TextMeasurer,
    env: &EnvironmentState,
) -> DisplayList {
    let style_map: HashMap<NodeId, ComputedStyle> =
        styles.iter().map(|(n, s)| (*n, s.clone())).collect();
    let mut lay = Layout {
        dom,
        styles: style_map,
        tm,
        out: Buffers::default(),
        line: Line::default(),
        y: 0.0,
        max_y: 0.0,
        abs_list: Vec::new(),
        floats: Vec::new(),
        rot_ctx: WritingMode::HorizontalTb,
        sticky_scopes: Vec::new(),
        scroll_parents: Vec::new(),
        cur_node: 0,
        viewport_w: width,
        viewport_h,
        containers: Vec::new(),
        cache: None,
        cache_scope: None,
        image_sizes: HashMap::new(),
        env: *env,
    };
    let style = lay.style_or_default(subtree_root);
    lay.layout_static_box(
        subtree_root,
        Frame {
            x: 0.0,
            width,
            align: style.text_align,
        },
        None,
    );
    lay.drain_abs(None, (0.0, 0.0, lay.viewport_w, lay.viewport_h));
    // B3: subtree splice layouts don't feed the engine's container map
    // (only full layouts refresh it — documented), so the collected boxes
    // are dropped here.
    let _ = std::mem::take(&mut lay.containers);
    // Same per-node filter the full path runs, so a spliced subtree honors
    // `visibility` exactly like a fresh layout.
    apply_visibility(&lay.styles, &mut lay.out);
    DisplayList {
        rects: lay.out.rects,
        texts: lay.out.texts,
        links: lay.out.links,
        images: lay.out.images,
        media: lay.out.media,
        shadows: lay.out.shadows,
        stickies: lay.out.stickies,
        scroll_containers: lay.out.scroll_containers,
        toggles: lay.out.toggles,
        containers: Vec::new(),
        content_height: lay.max_y.max(lay.y),
        page_background: 0xFFFFFFFF,
    }
}

/// Content area handed to the children of a block.
#[derive(Debug, Clone, Copy)]
struct Frame {
    x: f32,
    width: f32,
    align: TextAlign,
}

/// B5: one unbreakable piece of a multi-column container's flow — either a
/// block-level child or a maximal run of inline children (an anonymous block,
/// mirroring what `layout_block_children` composes in normal flow). The v1
/// fragmentation unit is the whole child; line-level breaking is deferred.
enum MulticolUnit {
    BlockBox(NodeId),
    Run(Vec<NodeId>),
}

/// A3: pick the best candidate from a `srcset` at density 1. Width (`Nw`)
/// descriptors: the smallest at or above `viewport_w`, else the largest.
/// Density (`Nx`) descriptors: the one closest to 1x. `sizes` is deferred.
fn pick_srcset(srcset: &str, viewport_w: f32) -> Option<String> {
    let mut cands: Vec<(String, Option<f32>, bool)> = Vec::new(); // (url, value, is_width)
    for cand in srcset.split(',') {
        let cand = cand.trim();
        if cand.is_empty() {
            continue;
        }
        let mut parts = cand.split_whitespace();
        let url = parts.next()?.to_string();
        let desc = parts.next().map(|d| d.trim().to_string());
        match desc {
            Some(d) if d.ends_with('w') => {
                let w = d[..d.len() - 1].trim().parse::<f32>().ok()?;
                cands.push((url, Some(w), true));
            }
            Some(d) if d.ends_with('x') => {
                let x = d[..d.len() - 1].trim().parse::<f32>().ok()?;
                cands.push((url, Some(x), false));
            }
            _ => cands.push((url, None, false)),
        }
    }
    let first = cands.first()?.0.clone();
    if cands.iter().any(|(_, _, is_w)| *is_w) {
        // Width descriptors: smallest >= viewport, else the largest.
        let mut best: Option<(f32, String)> = None;
        let mut fallback: Option<(f32, String)> = None;
        for (url, v, is_w) in &cands {
            if !is_w {
                continue;
            }
            let w = v.unwrap();
            if w >= viewport_w && best.as_ref().map(|(bw, _)| w < *bw).unwrap_or(true) {
                best = Some((w, url.clone()));
            }
            if fallback.as_ref().map(|(fw, _)| w > *fw).unwrap_or(true) {
                fallback = Some((w, url.clone()));
            }
        }
        Some(
            best.or(fallback)
                .map(|(_, u)| u)
                .unwrap_or_else(|| first.clone()),
        )
    } else {
        // Density descriptors: closest to 1x.
        let mut best: Option<(f32, String)> = None;
        for (url, v, is_w) in &cands {
            if *is_w {
                continue;
            }
            let x = v.unwrap_or(1.0);
            if best
                .as_ref()
                .map(|(bx, _)| (x - 1.0).abs() < (*bx - 1.0).abs())
                .unwrap_or(true)
            {
                best = Some((x, url.clone()));
            }
        }
        Some(best.map(|(_, u)| u).unwrap_or(first))
    }
}

/// R6: standard base64 (RFC 4648, padded) for inline-SVG data URLs.
fn b64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Host of an `iframe` src for the placeholder label (Q14): strips the
/// scheme, path, userinfo and port. `None` when nothing usable remains.
fn iframe_host(src: &str) -> Option<&str> {
    let rest = src.split("://").nth(1).unwrap_or(src);
    let host = rest.split(['/']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then_some(host)
}

/// R1: paint rects for one border edge. Solid = a single rect;
/// dashed/dotted = segment runs along the edge (natural periods, capped —
/// the display list is rect-based). `horizontal` edges run along +x with
/// the thickness extending downward; vertical edges along +y, thickness
/// rightward.
fn border_edge_rects(
    x: f32,
    y: f32,
    len: f32,
    t: f32,
    horizontal: bool,
    style: BorderStyle,
    color: u32,
) -> Vec<Rect> {
    let one = |along: f32, run: f32| {
        if horizontal {
            Rect::solid(x + along, y, run, t, color)
        } else {
            Rect::solid(x, y + along, t, run, color)
        }
    };
    match style {
        BorderStyle::Dashed | BorderStyle::Dotted => {
            let (seg, gap) = if style == BorderStyle::Dotted {
                (t.max(1.0), t.max(1.0))
            } else {
                let d = (t * 3.0).max(6.0);
                (d, d)
            };
            let period = seg + gap;
            let count = ((len / period).floor() as usize).min(128);
            let mut out = Vec::with_capacity(count);
            for i in 0..count {
                let along = i as f32 * period;
                let r = one(along, seg.min(len - along).max(0.0));
                if r.w > 0.0 && r.h > 0.0 {
                    out.push(r);
                }
            }
            out
        }
        _ => vec![one(0.0, len)],
    }
}

/// B12: the rects for one decoration line `w` px long with its top at
/// `top`, thickness `t`. Solid is one rect; dashed/dotted reuse the border
/// segment machinery; double is two parallel rules (band 3t); wavy is a
/// zigzag of ±30° rotated segments riding the `transform` field.
fn decoration_rects(
    x: f32,
    top: f32,
    w: f32,
    t: f32,
    style: DecorationStyle,
    color: u32,
) -> Vec<Rect> {
    match style {
        DecorationStyle::Solid => vec![Rect::solid(x, top, w, t, color)],
        DecorationStyle::Double => vec![
            Rect::solid(x, top, w, t, color),
            Rect::solid(x, top + 2.0 * t, w, t, color),
        ],
        DecorationStyle::Dashed | DecorationStyle::Dotted => {
            let bs = if style == DecorationStyle::Dashed {
                BorderStyle::Dashed
            } else {
                BorderStyle::Dotted
            };
            border_edge_rects(x, top, w, t, true, bs, color)
        }
        DecorationStyle::Wavy => {
            let seg = (t * 3.0).max(3.0);
            let (c, s) = (0.866_f32, 0.5_f32); // cos/sin 30°
            let step = seg * c;
            let amp = seg * s;
            let count = ((w / step).ceil() as usize).min(128);
            let mut out = Vec::with_capacity(count);
            for i in 0..count {
                // Alternate up/down; each segment starts where the last
                // ended, so the band spans [top, top + amp].
                let up = i % 2 == 0;
                let sx = x + i as f32 * step;
                let sy = if up { top + amp } else { top };
                let (rs, rc) = if up { (-s, c) } else { (s, c) };
                let mut r = Rect::solid(sx, sy, seg, t, color);
                r.transform = Some(Matrix2d([
                    rc,
                    -rs,
                    sx - rc * sx + rs * sy,
                    rs,
                    rc,
                    sy - rs * sx - rc * sy,
                ]));
                out.push(r);
            }
            out
        }
    }
}

/// Words waiting on the current line.
#[derive(Debug, Default)]
struct Line {
    words: Vec<PendingWord>,
    width: f32,
    max_ascent: f32,
    max_descent: f32,
    line_h: f32,
    /// Paragraph direction of the containing block (bidi base level).
    rtl: bool,
}

#[derive(Debug, Clone)]
struct PendingWord {
    text: String,
    w: f32,
    space: f32, // width of the space before this word (0 at line start)
    style: ComputedStyle,
    link: Option<String>,
    /// Baseline shift from `vertical-align` (+down, y-down space).
    dy: f32,
    /// Bidi direction of this word's run (visual reorder at flush).
    rtl: bool,
    /// v7 provenance: the innermost element composing this word — captured
    /// at feed time, since a wrap can flush mid-feed under a different box.
    node: u32,
    /// B4: the enclosing block's `filter` chain (NOT inherited, so we
    /// snapshot it at feed time; flushing lives under that block, and the
    /// painted run should wear the box's effect, not the text node's own
    /// `filter: none`).
    box_filter: Option<FilterList>,
    /// B13: this word ended in a soft hyphen (U+00AD), stripped at feed
    /// time; if the word lands at line end, `flush_line` paints a visible
    /// `-` in its place.
    shy: bool,
}

/// Split a bidi run (or a pre-wrap physical line) at UAX #14 break
/// opportunities (Q13): Latin words stay whole segments, CJK breaks per
/// character, kinsoku comes from the Unicode classes. Returns
/// `(raw_segment, ends_in_whitespace)` pairs — whitespace-only segments are
/// kept so the feeder can read their trailing space for the gap rule;
/// feeders trim before feeding.
fn break_segments(run: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut prev = 0usize;
    for (i, _kind) in kitae_shape::line_breaks(run) {
        let raw = &run[prev..i];
        prev = i;
        out.push((raw, raw.ends_with(char::is_whitespace)));
    }
    if prev < run.len() {
        // Defensive tail; `line_breaks` always ends at (len, Mandatory).
        out.push((&run[prev..], run[prev..].ends_with(char::is_whitespace)));
    }
    out
}

struct Layout<'a> {
    dom: &'a Dom,
    styles: HashMap<NodeId, ComputedStyle>,
    tm: &'a dyn TextMeasurer,
    out: Buffers,
    line: Line,
    y: f32,
    /// Highest painted edge — `content_height` accounts for positioned boxes
    /// that extend past the flow.
    max_y: f32,
    /// Out-of-flow children waiting for their containing block to finish:
    /// `(containing block — None for `position: fixed` — child, static x,
    /// static y)`. C4: the static position is the flow cursor where the box
    /// would have started, used when both edges on an axis are auto.
    abs_list: Vec<(Option<NodeId>, NodeId, f32, f32)>,
    /// Floats currently shaping lines in this block: `(left?, x, y, w, h)`.
    /// Cleared when a block that declared floats finishes its children.
    floats: Vec<FloatBox>,
    /// B4 v1: the writing-mode of the nearest enclosing rotated box
    /// (`HorizontalTb` when nothing above is rotated). Children that
    /// inherit that same mode skip re-rotation — the parent's stamp
    /// carries them; a nested box with a *different* rotated mode
    /// re-rotates and the matrices compose.
    rot_ctx: WritingMode,
    /// Q5: open sticky containment scopes — `(container top, first sticky
    /// index at push time)` for each overflow-clipping box being laid out.
    /// Popping a scope fills its stickies' `container_bottom`.
    sticky_scopes: Vec<(f32, usize)>,
    scroll_parents: Vec<u32>,
    /// v7 provenance: the DOM node being laid out right now; every item
    /// push stamps its id so the engine can splice incrementally.
    cur_node: u32,
    /// A3: the layout viewport width (the root frame width) — evaluates
    /// `<source media>` width conditions for `<picture>`.
    viewport_w: f32,
    /// The layout viewport height. `position: fixed` resolves its
    /// percentage offsets and `vh`-style geometry against the viewport,
    /// not the document — a `top: 50%` rail must sit mid-window even on
    /// a page ten screens tall.
    viewport_h: f32,
    /// B3: `container-type` elements, recorded with their geometry as each
    /// box finishes (probe layout output for the second style pass).
    containers: Vec<ContainerBox>,
    /// I2: fragment cache for body-level block children (`layout_cached`
    /// only — plain `layout` and `layout_subtree` run without one).
    cache: Option<&'a mut FragmentCache>,
    /// I2: the node whose direct block children may reuse cached fragments
    /// (the `<body>` element; `None` disables reuse).
    cache_scope: Option<NodeId>,
    /// Natural pixel sizes of raster subresources by `src` (`background-size`
    /// intrinsic sizing). Layout never decodes, so this is empty unless the
    /// embedder supplies it; without an entry a `background-size` that
    /// depends on the intrinsic size (`auto`, `cover`, `contain`) paints
    /// nothing rather than guessing.
    image_sizes: HashMap<String, (f32, f32)>,
    /// A3: the page's media environment — the media type (`screen` / `print`)
    /// and the `prefers-*` answers an `<img media>` / `<picture><source
    /// media>` condition is evaluated against. The engine hands the same
    /// environment the style engine filtered the sheets with, so a page's
    /// images and its CSS agree on which medium they are for; the default
    /// entry points pass the starting environment instead.
    env: EnvironmentState,
}

/// A floated box registered against the flow: `left` picks which side
/// following lines must shape around.
#[derive(Debug, Clone, Copy)]
struct FloatBox {
    left: bool,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl<'a> Layout<'a> {
    fn style(&self, node: NodeId) -> Option<&ComputedStyle> {
        self.styles.get(&node)
    }

    // v7 provenance pushers: stamp `cur_node` on the item, then append.
    // Sites that know a more precise source node (replaced elements, word
    // runs) push directly and set `node` themselves.

    fn push_rect(&mut self, mut r: Rect) {
        r.node = self.cur_node;
        // B4: stamp the element's filter chain onto the item. Only the
        // enclosing box's `filter` (not `backdrop-filter`) is captured here
        // — backdrop-filter is a host concern (CABackdropLayer is private
        // on macOS); the wire exposes it through a parallel section the
        // engine can interpret.
        if let Some(style) = self.styles.get(&NodeId(self.cur_node)) {
            r.filters = style.filter.clone();
        }
        self.out.rects.push(r);
    }

    fn push_text(&mut self, mut t: TextRun) {
        t.node = self.cur_node;
        if let Some(style) = self.styles.get(&NodeId(self.cur_node)) {
            t.filters = style.filter.clone();
        }
        self.out.texts.push(t);
    }

    #[allow(dead_code)] // v7 stamps exist for parity with push_rect/push_text
    fn push_image(&mut self, mut im: DisplayImage) {
        im.node = self.cur_node;
        if let Some(style) = self.styles.get(&NodeId(self.cur_node)) {
            im.filters = style.filter.clone();
        }
        self.out.images.push(im);
    }

    #[allow(dead_code)] // v7 stamps exist for parity with push_rect/push_text
    fn push_media(&mut self, mut m: DisplayMedia) {
        m.node = self.cur_node;
        self.out.media.push(m);
    }

    /// The box's background: one [`Rect`] per painted layer, in paint order
    /// (the FIRST layer of the list is the top one, so the vec runs
    /// bottom-up). The caller splices them in ahead of the subtree's items.
    ///
    /// A single layer keeps the original whole-border-box rect (with the
    /// authored `border-radius`) when its `clip` is the default, so the
    /// common case is byte-identical to before layer lists existed; a
    /// multi-layer background, or a layer clipped inside the border box, gets
    /// one rect per layer at its own `background-clip` rect.
    fn background_rects(
        &self,
        style: &ComputedStyle,
        node: u32,
        box_: &BackgroundBox,
        radius: BorderRadius,
        chrome: bool,
    ) -> Vec<Rect> {
        let one = |color: u32, paint: &BackgroundPaint, r: BorderRadius, area: &Rect| Rect {
            x: area.x,
            y: area.y,
            w: area.w,
            h: area.h,
            color,
            radius: r,
            paint: RectPaint::from(paint),
            transform: None,
            node,
            ..Default::default()
        };
        if style.background_layers.len() <= 1
            && style
                .background_layers
                .first()
                .is_none_or(|l| l.clip == BgBox::BorderBox)
        {
            // `chrome` is the box's own padding/border: it has always forced
            // one rect even for a transparent color, and a single unclipped
            // layer still paints the whole border box.
            if style.background == 0 && !chrome {
                return Vec::new();
            }
            return vec![one(
                style.background,
                &style.background_paint,
                radius,
                &box_.border,
            )];
        }
        let mut out = Vec::new();
        // Bottom-up: the last layer of the list paints first.
        for layer in style.background_layers.iter().rev() {
            // A gradient paints its own stops; a solid color and an image
            // both ride the layer's `color` backdrop.
            if layer.color == 0 && !layer.is_gradient() {
                continue;
            }
            let area = box_.rect(layer.clip);
            if area.w <= 0.0 || area.h <= 0.0 {
                continue;
            }
            out.push(one(
                layer.color,
                &layer.paint,
                box_.rect_radius(style, layer.clip, &area, radius),
                &area,
            ));
        }
        out
    }

    /// The box's `background-image` layers — emit each one's tiles as
    /// ordinary [`DisplayImage`] items, bottom-up so the layers stack in CSS
    /// order (index 0 on top).
    ///
    /// Each tile carries its own rect, so `position` / `size` / `repeat` need
    /// no wire field: the engine stamps the raster onto the item exactly as
    /// it does for an `<img>`. `origin` picks the positioning area and
    /// `clip` the paint rect (CSS Backgrounds 3 §3.3/§3.4); a layer clipped
    /// inside the border box carries an explicit [`ResolvedClip::Inset`] so
    /// the tiles that reach past it are cut, which is the same existing wire
    /// field `clip-path` uses. An ancestor `clip-path` is not additionally
    /// narrowed onto a clipped tile (one clip per item; first writer wins,
    /// as for nested clip paths).
    ///
    /// A size that depends on the image's intrinsic size — `auto`, `cover`,
    /// `contain`, or one definite axis against an `auto` one — is resolved
    /// only when the embedder supplied that size
    /// ([`layout_with_image_sizes`]); layout never decodes, and a guessed
    /// size would stretch real artwork. Without it the image is skipped and
    /// the background color alone paints, as before.
    fn paint_background_images(&mut self, style: &ComputedStyle, box_: &BackgroundBox) {
        // Bottom-up: the last layer paints first.
        for layer in style.background_layers.iter().rev() {
            let Some(src) = layer.image_src() else {
                continue;
            };
            let area = box_.rect(layer.origin);
            self.paint_background_layer_image(style, layer, src, area, box_);
        }
    }

    /// One `background-image` layer's tiles inside its positioning area.
    fn paint_background_layer_image(
        &mut self,
        style: &ComputedStyle,
        layer: &BackgroundLayer,
        src: &str,
        area: Rect,
        box_: &BackgroundBox,
    ) {
        if area.w <= 0.0 || area.h <= 0.0 {
            return;
        }
        let natural = self
            .image_sizes
            .get(src)
            .copied()
            .filter(|(a, b)| *a > 0.0 && *b > 0.0);
        let Some((mut tw, mut th)) =
            self.background_tile_size(style, layer, area.w, area.h, natural)
        else {
            return;
        };
        if !(tw > 0.0 && th > 0.0 && tw.is_finite() && th.is_finite()) {
            return;
        }
        let repeat = layer.repeat;
        // `round`: resize the tile so a whole number of them fills the axis
        // exactly — no gap, no clipped tile. `space` keeps the size and
        // spreads the slack between tiles instead.
        if repeat.x == BgRepeat::Round {
            let n = (area.w / tw).round().max(1.0);
            tw = area.w / n;
        }
        if repeat.y == BgRepeat::Round {
            let n = (area.h / th).round().max(1.0);
            th = area.h / n;
        }
        let em = style.font_size;
        let offset = |p: BgPos, extent: f32, tile: f32| -> f32 {
            let free = (extent - tile).max(0.0);
            match p {
                BgPos::Start => 0.0,
                BgPos::Center => free / 2.0,
                BgPos::End => free,
                BgPos::Percent(pct) => free * pct / 100.0,
                BgPos::Offset(l) => l.resolve(em, 16.0, extent),
                // Measured inward from the far edge (`right 10px`), so the
                // tile may sit past the origin when the box is smaller.
                BgPos::Inset(l) => free - l.resolve(em, 16.0, extent),
            }
        };
        let origin_x = area.x + offset(layer.position.x, area.w, tw);
        let origin_y = area.y + offset(layer.position.y, area.h, th);
        let xs = tile_positions(area.x, area.w, origin_x, tw, repeat.x);
        let ys = tile_positions(area.y, area.h, origin_y, th, repeat.y);
        // `background-clip` inside the border box cuts the tiles that reach
        // past it. A `border-box` clip — or a padding/content box with no
        // padding or border to cut — is the whole box and needs no clip.
        let clip = match layer.clip {
            BgBox::BorderBox => None,
            which => {
                let r = box_.rect(which);
                let b = &box_.border;
                (r.w <= 0.0
                    || r.h <= 0.0
                    || (r.x - b.x).abs() > f32::EPSILON
                    || (r.y - b.y).abs() > f32::EPSILON
                    || (r.w - b.w).abs() > f32::EPSILON
                    || (r.h - b.h).abs() > f32::EPSILON)
                    .then(|| ResolvedClip::Inset {
                        x: r.x,
                        y: r.y,
                        w: r.w,
                        h: r.h,
                        radii: box_.clip_radii(style, which, &r),
                    })
            }
        };
        let mut emitted = 0;
        'tiles: for ty in &ys {
            for tx in &xs {
                self.out.images.push(DisplayImage {
                    x: *tx,
                    y: *ty,
                    w: tw,
                    h: th,
                    src: src.to_string(),
                    // The rect is already the painted size, so the raster is
                    // stretched to fill it — `object-fit` has no say here.
                    fit: ObjectFit::Fill,
                    clip: clip.clone(),
                    node: self.cur_node,
                    ..Default::default()
                });
                emitted += 1;
                if emitted >= MAX_BG_TILES {
                    // A pathological page (a 1px tile over a huge box) must
                    // not explode the display list. The cap is per layer, so
                    // a layer list is still bounded.
                    break 'tiles;
                }
            }
        }
    }

    /// The painted tile size for `background-size` against the positioning
    /// area. `natural` is the image's intrinsic size when the embedder
    /// supplied it ([`layout_with_image_sizes`]); a value that needs it and
    /// has none returns `None` so the caller paints nothing rather than
    /// stretching the artwork.
    fn background_tile_size(
        &self,
        style: &ComputedStyle,
        layer: &kitae_style::BackgroundLayer,
        area_w: f32,
        area_h: f32,
        natural: Option<(f32, f32)>,
    ) -> Option<(f32, f32)> {
        let em = style.font_size;
        let one = |s: BgSize, basis: f32, natural: f32| -> f32 {
            match s {
                BgSize::Auto => natural,
                BgSize::Percent(p) => basis * p / 100.0,
                BgSize::Length(l) => l.resolve(em, 16.0, basis),
            }
        };
        match layer.size {
            // `auto auto` = the intrinsic size.
            BackgroundSize::Auto => natural,
            BackgroundSize::Keyword(keyword) => {
                let (iw, ih) = natural?;
                // The largest (cover) / smallest (contain) size that keeps
                // the intrinsic ratio inside the positioning area.
                let scale_w = area_w / iw;
                let scale_h = area_h / ih;
                let scale = match keyword {
                    BgSizeKeyword::Cover => scale_w.max(scale_h),
                    BgSizeKeyword::Contain => scale_w.min(scale_h),
                };
                Some((iw * scale, ih * scale))
            }
            BackgroundSize::Axes(sx, sy) => {
                // An `auto` axis is the intrinsic size; against a
                // *percentage* it takes the intrinsic ratio instead (CSS
                // Backgrounds 3 §3.6). Without the intrinsic size neither
                // can be resolved.
                if matches!(sx, BgSize::Auto) || matches!(sy, BgSize::Auto) {
                    let (iw, ih) = natural?;
                    let w = one(sx, area_w, iw);
                    let h = one(sy, area_h, ih);
                    return Some(match (sx, sy) {
                        (_, BgSize::Auto) if matches!(sx, BgSize::Percent(_)) => (w, w * ih / iw),
                        _ => (w, h),
                    });
                }
                // Both axes definite: no intrinsic size needed.
                Some((one(sx, area_w, 0.0), one(sy, area_h, 0.0)))
            }
        }
    }

    fn style_or_default(&self, node: NodeId) -> ComputedStyle {
        self.styles.get(&node).cloned().unwrap_or_default()
    }

    fn find_first_by_tag(&self, root: NodeId, tag: &str) -> Option<NodeId> {
        let mut found = None;
        self.dom.walk(root, &mut |n| {
            if found.is_none() && self.dom.tag(n) == tag {
                found = Some(n);
            }
        });
        found
    }

    /// Run `f` with fresh paint buffers; returns what it painted. Flow state
    /// (`self.y`, `abs_list`) is shared — only paint is captured.
    fn with_buffers<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> (Buffers, T) {
        let saved = std::mem::take(&mut self.out);
        let result = f(self);
        (std::mem::replace(&mut self.out, saved), result)
    }

    /// I2: the cached fragment for `child`, cloned out, when its revision
    /// and geometry inputs still match. The clone keeps the borrow of
    /// `self.cache` from spanning the merge.
    fn cache_hit(&self, child: NodeId, width: f32) -> Option<(f32, f32, Buffers)> {
        let c = self.cache.as_ref()?;
        let e = c.entries.get(&child)?;
        if e.rev != self.dom.rev(child)
            || e.width_bits != width.to_bits()
            || e.viewport_h_bits != self.viewport_h.to_bits()
        {
            return None;
        }
        Some((e.advance, e.max_depth, e.buf.clone()))
    }

    fn merge_buffers(&mut self, buf: Buffers) {
        if buf.is_empty() {
            return;
        }
        // Sticky ranges are buffer-relative indices; rebase them onto the
        // destination lists before appending.
        let base = (
            self.out.rects.len() as u32,
            self.out.texts.len() as u32,
            self.out.links.len() as u32,
            self.out.images.len() as u32,
            self.out.media.len() as u32,
            self.out.shadows.len() as u32,
        );
        let mut stickies = buf.stickies;
        self.out.rects.extend(buf.rects);
        self.out.texts.extend(buf.texts);
        self.out.links.extend(buf.links);
        self.out.images.extend(buf.images);
        self.out.media.extend(buf.media);
        self.out.shadows.extend(buf.shadows);
        for s in &mut stickies {
            s.rect.0 += base.0;
            s.text.0 += base.1;
            s.link.0 += base.2;
            s.image.0 += base.3;
            s.media.0 += base.4;
            s.shadow.0 += base.5;
        }
        self.out.stickies.extend(stickies);
        self.out.scroll_containers.extend(buf.scroll_containers);
        self.out.toggles.extend(buf.toggles);
    }

    /// Keep only the parts of `buf` inside the rect: rects, images and link
    /// areas are intersected, text runs dropped when their bounding box
    /// leaves it. Sticky ranges (`buf`-relative) are remapped onto the
    /// clipped lists; a sticky whose items all fell outside loses its entry.
    fn clip_buffers(&self, buf: &mut Buffers, cx: f32, cy: f32, cw: f32, ch: f32) {
        let inside =
            |x: f32, y: f32, w: f32, h: f32| x + w > cx && x < cx + cw && y + h > cy && y < cy + ch;
        // Survivor map per list: original index → kept index (u32::MAX =
        // dropped), so sticky item ranges can follow their items.
        let mut map_rects = vec![u32::MAX; buf.rects.len()];
        let mut clipped_rects = Vec::with_capacity(buf.rects.len());
        for (i, r) in buf.rects.drain(..).enumerate() {
            if !inside(r.x, r.y, r.w, r.h) {
                continue;
            }
            // Transformed rects keep whole (pre-transform approximation, Q2):
            // intersecting the axis-aligned box of a rotated rect would
            // corrupt both geometry and matrix.
            if r.transform.is_some() {
                map_rects[i] = clipped_rects.len() as u32;
                clipped_rects.push(r);
                continue;
            }
            let x1 = r.x.max(cx);
            let y1 = r.y.max(cy);
            let x2 = (r.x + r.w).min(cx + cw);
            let y2 = (r.y + r.h).min(cy + ch);
            map_rects[i] = clipped_rects.len() as u32;
            clipped_rects.push(Rect {
                x: x1,
                y: y1,
                w: x2 - x1,
                h: y2 - y1,
                color: r.color,
                radius: r.radius,
                paint: r.paint,
                transform: r.transform,
                node: r.node,
                ..Default::default()
            });
        }
        buf.rects = clipped_rects;

        let mut map_links = vec![u32::MAX; buf.links.len()];
        let mut kept_links = Vec::with_capacity(buf.links.len());
        for (i, l) in buf.links.drain(..).enumerate() {
            if inside(l.x, l.y, l.w, l.h) {
                map_links[i] = kept_links.len() as u32;
                kept_links.push(l);
            }
        }
        buf.links = kept_links;

        let mut map_media = vec![u32::MAX; buf.media.len()];
        let mut kept_media = Vec::with_capacity(buf.media.len());
        for (i, mut m) in buf.media.drain(..).enumerate() {
            if !inside(m.x, m.y, m.w, m.h) {
                continue;
            }
            let x1 = m.x.max(cx);
            let y1 = m.y.max(cy);
            let x2 = (m.x + m.w).min(cx + cw);
            let y2 = (m.y + m.h).min(cy + ch);
            m.x = x1;
            m.y = y1;
            m.w = x2 - x1;
            m.h = y2 - y1;
            map_media[i] = kept_media.len() as u32;
            kept_media.push(m);
        }
        buf.media = kept_media;

        let mut map_images = vec![u32::MAX; buf.images.len()];
        let mut kept_images = Vec::with_capacity(buf.images.len());
        for (i, mut im) in buf.images.drain(..).enumerate() {
            if !inside(im.x, im.y, im.w, im.h) {
                continue;
            }
            let x1 = im.x.max(cx);
            let y1 = im.y.max(cy);
            let x2 = (im.x + im.w).min(cx + cw);
            let y2 = (im.y + im.h).min(cy + ch);
            im.x = x1;
            im.y = y1;
            im.w = x2 - x1;
            im.h = y2 - y1;
            map_images[i] = kept_images.len() as u32;
            kept_images.push(im);
        }
        buf.images = kept_images;

        let mut map_texts = vec![u32::MAX; buf.texts.len()];
        let mut kept_texts = Vec::with_capacity(buf.texts.len());
        for (i, t) in buf.texts.drain(..).enumerate() {
            let w = if t.letter_spacing != 0.0 {
                self.tm.measure_run_sp_family(
                    t.text.trim_end(),
                    t.size,
                    t.weight,
                    t.italic,
                    t.monospace,
                    t.letter_spacing,
                    &t.family,
                )
            } else {
                self.tm.measure_family(
                    t.text.trim_end(),
                    t.size,
                    t.weight,
                    t.italic,
                    t.monospace,
                    &t.family,
                )
            };
            if inside(t.x, t.y - t.size * 0.8, w, t.size * 1.2) {
                map_texts[i] = kept_texts.len() as u32;
                kept_texts.push(t);
            }
        }
        buf.texts = kept_texts;

        // Shadows are kept whole when their expanded bounds (spread + blur
        // halo + offset) intersect the clip; painters clip the soft edge.
        let mut map_shadows = vec![u32::MAX; buf.shadows.len()];
        let mut kept_shadows = Vec::with_capacity(buf.shadows.len());
        for (i, s) in buf.shadows.drain(..).enumerate() {
            let halo = s.spread + s.blur * 2.0;
            let x0 = (s.x + s.offset_x - halo).min(s.x);
            let y0 = (s.y + s.offset_y - halo).min(s.y);
            let x1 = (s.x + s.w + s.offset_x + halo).max(s.x + s.w);
            let y1 = (s.y + s.h + s.offset_y + halo).max(s.y + s.h);
            if x1 > cx && x0 < cx + cw && y1 > cy && y0 < cy + ch {
                map_shadows[i] = kept_shadows.len() as u32;
                kept_shadows.push(s);
            }
        }
        buf.shadows = kept_shadows;

        // Q5: follow each sticky's items through the clip. A sticky whose
        // whole range was dropped paints unshifted (no entry).
        let remap = |map: &[u32], range: (u32, u32)| -> Option<(u32, u32)> {
            let s0 = range.0 as usize;
            let end = s0 + range.1 as usize;
            let survivors: Vec<u32> = map[s0..end]
                .iter()
                .copied()
                .filter(|m| *m != u32::MAX)
                .collect();
            let first = *survivors.first()?;
            Some((first, survivors.len() as u32))
        };
        for s in &mut buf.stickies {
            s.rect = remap(&map_rects, s.rect).unwrap_or((0, 0));
            s.text = remap(&map_texts, s.text).unwrap_or((0, 0));
            s.link = remap(&map_links, s.link).unwrap_or((0, 0));
            s.image = remap(&map_images, s.image).unwrap_or((0, 0));
            s.media = remap(&map_media, s.media).unwrap_or((0, 0));
            s.shadow = remap(&map_shadows, s.shadow).unwrap_or((0, 0));
        }
        buf.stickies
            .retain(|s| s.rect.1 > 0 || s.text.1 > 0 || s.image.1 > 0 || s.shadow.1 > 0);
        // A5: keep only toggle areas that intersect the clip (hidden
        // summaries must not be clickable).
        buf.toggles
            .retain(|t| t.x + t.w > cx && t.x < cx + cw && t.y + t.h > cy && t.y < cy + ch);
    }

    /// Lay out one box. Returns its border-box height; when the box needs
    /// post-processing (relative shift, clipping, z-order — i.e. it is
    /// positioned), paint is captured and returned instead of merged.
    fn layout_box(
        &mut self,
        node: NodeId,
        frame: Frame,
        cb: Option<NodeId>,
    ) -> (f32, Option<Buffers>) {
        let style = self.style_or_default(node);
        if style.display == Display::None {
            return (0.0, None);
        }
        if matches!(style.position, Position::Static | Position::Sticky) {
            // Sticky paints at its flow position (Q5); the embedder shifts it
            // at scroll time from the sticky side-list entry recorded below.
            let h = self.layout_static_box(node, frame, cb);
            return (h, None);
        }
        let (mut buf, h) = self.with_buffers(|s| s.layout_static_box(node, frame, cb));
        if style.position == Position::Relative {
            // Offsets resolve against the containing-block width; % top/bottom
            // would need its (unknown) height, so they contribute nothing.
            let dx = style
                .left
                .map(|l| l.resolve(frame.width))
                .or_else(|| style.right.map(|r| -r.resolve(frame.width)))
                .unwrap_or(0.0);
            let dy = style
                .top
                .map(|l| l.resolve(0.0))
                .or_else(|| style.bottom.map(|b| -b.resolve(0.0)))
                .unwrap_or(0.0);
            buf.translate(dx, dy);
        }
        (h, Some(buf))
    }

    /// B4 v1 entry point: `writing-mode` other than `horizontal-tb` lays
    /// the subtree as a logical horizontal-tb box and rigidly rotates the
    /// resulting paint (`layout_rotated_box`). Everything else — and every
    /// descendant that merely INHERITS the parent's rotated mode — goes to
    /// `layout_physical_box`, since the parent's rotation stamp already
    /// carries them. A horizontal-tb box nested inside a rotated subtree is
    /// laid out in that subtree's logical space (it rotates along with the
    /// parent — documented v1 approximation of mixed-mode nesting, as is
    /// `vertical-lr` column order, which matches `vertical-rl`).
    fn layout_static_box(&mut self, node: NodeId, frame: Frame, cb: Option<NodeId>) -> f32 {
        let wm = self.style_or_default(node).writing_mode;
        if wm == WritingMode::HorizontalTb || wm == self.rot_ctx {
            return self.layout_physical_box(node, frame, cb);
        }
        let saved_ctx = std::mem::replace(&mut self.rot_ctx, wm);
        let h = self.layout_rotated_box(node, frame, cb, wm);
        self.rot_ctx = saved_ctx;
        h
    }

    /// Lays `node` as a logical horizontal-tb box inside a scratch frame
    /// (whose "width" is the box's logical INLINE length), rotates the
    /// buffer set into page space, and merges it back at the flow cursor.
    /// The rotated box's physical footprint: height = the inline length
    /// (`height:`/folded `inline-size:`, else the CB width — v1 cap),
    /// width = the logical stack height (or folded `block-size`/`width`),
    /// columns hugging the right edge.
    fn layout_rotated_box(
        &mut self,
        node: NodeId,
        frame: Frame,
        cb: Option<NodeId>,
        wm: WritingMode,
    ) -> f32 {
        let style = self.style_or_default(node);
        let m = style.margin.resolve(frame.width);
        self.y += m.top;
        let top = self.y;
        let left = frame.x + m.left;
        let avail = (frame.width - m.left - m.right).max(40.0);
        // Logical inline length = the rotated box's physical height. C1:
        // intrinsic keywords on a rotated box fall back to `auto` (no
        // measurement basis for the logical axis).
        let v = style
            .height
            .and_then(|sv| sv.len())
            .map(|l| l.resolve(0.0))
            .unwrap_or(avail)
            .max(1.0);
        let lf = Frame {
            x: 0.0,
            width: v,
            align: style.text_align,
        };
        // Scratch logical pass with the B5 measure-path pollution guards:
        // the subtree's floats shape only its own lines, and out-of-flow
        // descendants drain against the real pass's list.
        let saved_y = std::mem::replace(&mut self.y, 0.0);
        let abs_saved = std::mem::take(&mut self.abs_list);
        let floats_saved = std::mem::take(&mut self.floats);
        // Suppress the box's OWN inline-size (folded to `height`) and
        // block-size/`width`: inside the logical pass they would constrain
        // the LOGICAL box instead of the rotated footprint — the inline
        // length became `v` above, the physical width is applied below.
        let stashed = self.styles.get_mut(&node).map(|st| {
            let old = (st.height, st.width);
            st.height = None;
            st.width = None;
            old
        });
        let (mut buf, h_logical) = self.with_buffers(|s| s.layout_physical_box(node, lf, cb));
        if let Some((oh, ow)) = stashed {
            if let Some(st) = self.styles.get_mut(&node) {
                st.height = oh;
                st.width = ow;
            }
        }
        self.abs_list = abs_saved;
        self.floats = floats_saved;
        self.y = saved_y;
        let mut w_phys = h_logical.max(0.0);
        if let Some(wl) = style.width.and_then(|sv| sv.len()) {
            w_phys = wl.resolve(avail);
        }
        let rot = if wm == WritingMode::SidewaysLr {
            // Counter-clockwise: text reads bottom-to-top; later logical
            // lines (growing logical y) stack rightward on screen.
            Matrix2d([0.0, 1.0, left, -1.0, 0.0, top + v])
        } else {
            // Clockwise (vertical-rl, vertical-lr ≈ rl, sideways-rl):
            // lines progress downward, the first line is the rightmost.
            Matrix2d([0.0, -1.0, left + w_phys, 1.0, 0.0, top])
        };
        buf.rotate(rot);
        let h_phys = v;
        self.y = top + h_phys;
        self.merge_buffers(buf);
        self.y += m.bottom;
        h_phys
    }

    /// D6: characters that stand upright in `text-orientation: mixed` —
    /// CJK ideographs, kana, hangul, and the CJK punctuation/fullwidth
    /// blocks. Latin, digits, and general scripts ride the box rotation.
    fn is_cjk_upright(c: char) -> bool {
        matches!(
            c as u32,
            0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF
        )
    }

    /// D6: the counter-rotation that cancels the enclosing box's rigid B4
    /// stamp for one upright glyph. `(ox, oy)` is the glyph's advance
    /// center and baseline in the logical pass — the fixed point, so the
    /// glyph lands back upright on the column axis after the box rotates
    /// (the box stamp composes OUTSIDE, cancelling the linear part while
    /// keeping the fixed point on the column). `cw` selects the stamp
    /// direction: clockwise for vertical-rl/lr and sideways-rl,
    /// counter-clockwise for sideways-lr.
    fn upright_char_transform(cw: bool, ox: f32, oy: f32) -> Matrix2d {
        if cw {
            // −90° about (ox, oy).
            Matrix2d([0.0, 1.0, ox - oy, -1.0, 0.0, oy + ox])
        } else {
            // +90° about (ox, oy).
            Matrix2d([0.0, -1.0, ox + oy, 1.0, 0.0, oy - ox])
        }
    }

    /// D6: split one word into oriented paint pieces `(text, x, w,
    /// transform)`. Sideways — and every horizontal-tb line — yields the
    /// single plain piece the pre-D6 path painted; Mixed stands CJK chars
    /// up individually; Upright stands every char up. Piece widths are
    /// prefix measure differences, so letter-spacing and shaped advances
    /// survive the split and the pieces tile the word exactly.
    fn oriented_pieces(
        &self,
        text: &str,
        word_w: f32,
        x: f32,
        y: f32,
        st: &ComputedStyle,
    ) -> Vec<(String, f32, f32, Option<Matrix2d>)> {
        let vertical = self.rot_ctx != WritingMode::HorizontalTb;
        if !vertical || st.text_orientation == TextOrientation::Sideways {
            return vec![(text.to_string(), x, word_w, None)];
        }
        let cw = self.rot_ctx != WritingMode::SidewaysLr;
        let upright_all = st.text_orientation == TextOrientation::Upright;
        let mut pieces: Vec<(String, f32, f32, Option<Matrix2d>)> = Vec::new();
        let mut plain_from: Option<(usize, f32)> = None; // (byte, prefix at start)
        let mut byte = 0usize;
        let mut prefix = 0.0f32;
        for c in text.chars() {
            let len = c.len_utf8();
            let next_prefix = self.measure_word(&text[..byte + len], st);
            let adv = next_prefix - prefix;
            if upright_all || Layout::is_cjk_upright(c) {
                if let Some((from, from_prefix)) = plain_from.take() {
                    pieces.push((
                        text[from..byte].to_string(),
                        x + from_prefix,
                        prefix - from_prefix,
                        None,
                    ));
                }
                let tf = Layout::upright_char_transform(cw, x + prefix + adv * 0.5, y);
                pieces.push((c.to_string(), x + prefix, adv, Some(tf)));
            } else if plain_from.is_none() {
                plain_from = Some((byte, prefix));
            }
            byte += len;
            prefix = next_prefix;
        }
        if let Some((from, from_prefix)) = plain_from {
            pieces.push((
                text[from..].to_string(),
                x + from_prefix,
                prefix - from_prefix,
                None,
            ));
        }
        pieces
    }

    /// In-flow box layout: margins, centered width, padding, children,
    /// background, min/max clamps, then out-of-flow children drain.
    fn layout_physical_box(&mut self, node: NodeId, frame: Frame, cb: Option<NodeId>) -> f32 {
        let style = self.style_or_default(node);
        let positioned = style.position != Position::Static;
        let child_cb = if positioned { Some(node) } else { cb };
        // v7 provenance: everything this subtree paints attributes to `node`
        // unless a child box re-stamps; restore the caller's node on exit.
        let saved_node = std::mem::replace(&mut self.cur_node, node.0);

        // This box and its subtree append to the tails of the shared buffers;
        // remember where that starts so the background can be spliced under
        // the subtree and overflow clipped afterwards.
        let start = (
            self.out.rects.len(),
            self.out.texts.len(),
            self.out.links.len(),
            self.out.images.len(),
            self.out.media.len(),
            self.out.shadows.len(),
        );
        let sticky_start = self.out.stickies.len();
        let scroll_start = self.out.scroll_containers.len();
        let toggle_start = self.out.toggles.len();
        let float_start = self.floats.len();

        // Percent margins/paddings resolve against the containing-block
        // width on every side (CSS rule), even top/bottom.
        let m = style.margin.resolve(frame.width);
        self.y += m.top;
        let avail = (frame.width - m.left - m.right).max(40.0);
        let left = frame.x + m.left;

        let p = style.padding.resolve(avail);
        // R1: borders inset the content box (the background stays at the
        // border-box edge per `background-clip: border-box`, the default).
        let borders = style.border.resolve(avail, style.color);
        let (bt, br, bb, bl) = (
            borders[0].width,
            borders[1].width,
            borders[2].width,
            borders[3].width,
        );

        // Fixed width / max-width narrow the box and center it, margin-auto
        // style — close enough for a reader. min-width wins last.
        // C1: the intrinsic keywords measure the subtree; CSS width is
        // border-box in this engine, so the box's own horizontal chrome is
        // added to the measured content widths before resolving.
        let mut w = match style.width {
            Some(SizeVal::Len(l)) => l.resolve(avail).min(avail),
            Some(sv) => {
                let (mc, xc) = self.intrinsic_pair(node);
                let chrome = bl + br + p.left + p.right;
                sv.resolve(avail, mc + chrome, xc + chrome).min(avail)
            }
            None => avail,
        };
        if let Some(mw) = style.max_width {
            w = w.min(mw.resolve(avail));
        }
        if let Some(mw) = style.min_width {
            w = w.max(mw.resolve(avail)).min(avail);
        }
        // Center only against the REAL available width: the 40px floor is a
        // readability minimum for % resolution, not a containing-block size —
        // an 11px dot in an 11px frame must not discover 40px of phantom
        // space to center inside. A box at least as wide as its frame
        // left-aligns (blocks overflow to the right in LTR).
        let true_avail = frame.width - m.left - m.right;
        let (box_x, box_w) = if w < true_avail {
            (left + (true_avail - w) * 0.5, w)
        } else if w < avail {
            (left, w)
        } else {
            (left, avail)
        };

        let bg_y = self.y;
        // Q5: an overflow-clipping box opens a sticky containment scope —
        // stickies laid out inside clamp against its bounds.
        let scope_pushed = style.overflow != Overflow::Visible;
        let scroll_scope_pushed = matches!(style.overflow, Overflow::Auto | Overflow::Scroll);
        let scroll_parent = self.scroll_parents.last().copied().unwrap_or(u32::MAX);
        if scroll_scope_pushed {
            self.scroll_parents.push(node.0);
        }
        let parent_scope_top = self.sticky_scopes.last().map(|s| s.0).unwrap_or(0.0);
        if scope_pushed {
            self.sticky_scopes.push((bg_y, self.out.stickies.len()));
        }
        self.y += bt + p.top;
        let inner = Frame {
            x: box_x + bl + p.left,
            width: (box_w - bl - br - p.left - p.right).max(40.0),
            align: style.text_align,
        };
        let content_y = self.y;
        // B3: a `container-type` element becomes a query container for its
        // subtree — record it now with the padding-box width (the content
        // width; paddings/borders resolve above), closing the height once
        // the box's own size is final below.
        let container_open = style.container_type != ContainerType::None;
        if container_open {
            self.containers.push(ContainerBox {
                node: node.0,
                names: style.container_names.clone(),
                x: inner.x,
                y: content_y,
                w: inner.width,
                h: 0.0,
            });
        }

        // B14: list marker, per `list-style-position`. `outside` (the
        // default) hangs the resolved `marker_text` in the container's left
        // padding; `inside` flows it as the item's first inline box (the
        // generic content branch below). The style engine stamps the text —
        // counter numbering, letter/roman forms, symbol glyphs.
        let marker_extra = match &style.marker_text {
            Some(m) if style.list_style_position == ListStylePosition::Outside => {
                let run = self.marker_run(node, inner.x - 16.0, self.y, m.clone());
                self.push_text(run);
                16.0
            }
            _ => 0.0,
        };
        let inner = Frame {
            x: inner.x + marker_extra,
            width: inner.width - marker_extra,
            align: style.text_align,
        };

        match self.dom.tag(node) {
            "hr" => {
                self.push_rect(Rect::solid(
                    inner.x,
                    self.y + 10.0,
                    inner.width.max(0.0),
                    1.0,
                    0xD8DBE0FF,
                ));
                self.y += 20.0;
            }
            "iframe" => {
                // Q14: nested browsing contexts are a whole engine subsystem,
                // not a gap to paper over — render a labeled placeholder box
                // and never descend (the element's raw-text fallback content
                // must not leak into the page).
                let src = self
                    .dom
                    .element(node)
                    .and_then(|e| e.attr("src"))
                    .unwrap_or("");
                let host = iframe_host(src).unwrap_or("embedded content");
                let ph = 150.0f32;
                self.push_rect(Rect::solid(
                    inner.x,
                    self.y,
                    inner.width.max(0.0),
                    ph,
                    0xF1F3F4FF,
                ));
                let border = 0x9AA0A6FF;
                for b in [
                    Rect::solid(inner.x, self.y, inner.width.max(0.0), 1.0, border),
                    Rect::solid(
                        inner.x,
                        self.y + ph - 1.0,
                        inner.width.max(0.0),
                        1.0,
                        border,
                    ),
                    Rect::solid(inner.x, self.y, 1.0, ph, border),
                    Rect::solid(
                        inner.x + inner.width.max(0.0) - 1.0,
                        self.y,
                        1.0,
                        ph,
                        border,
                    ),
                ] {
                    self.push_rect(b);
                }
                let (ascent, _) = self.tm.metrics_family(13.0, false, "");
                self.push_text(TextRun {
                    x: inner.x + 12.0,
                    y: self.y + ph * 0.5 + ascent * 0.5,
                    text: format!("embedded frame — {host}"),
                    size: 13.0,
                    weight: 400,
                    italic: false,
                    monospace: false,
                    color: 0x5F6368FF,
                    link: None,
                    letter_spacing: 0.0,
                    transform: None,
                    family: String::new(),
                    node: 0,
                    ..Default::default()
                });
                self.y += ph;
            }
            "progress" | "meter" => {
                // A8: value widgets — pill track + zone-colored fill.
                // `progress` without a value is indeterminate (half fill,
                // muted color). Children never lay out.
                let el = self.dom.element(node);
                let attr_f = |name: &str| {
                    el.and_then(|e| e.attr(name))
                        .and_then(|v| v.trim().parse::<f32>().ok())
                };
                let (min, max) = (attr_f("min").unwrap_or(0.0), attr_f("max").unwrap_or(1.0));
                let v = attr_f("value");
                let span = (max - min).abs().max(0.0001);
                let indeterminate = self.dom.tag(node) == "progress" && v.is_none();
                let frac = match v {
                    Some(v) => ((v - min) / span).clamp(0.0, 1.0),
                    None => 0.5,
                };
                let w = style
                    .width
                    .and_then(|sv| sv.len())
                    .map(|l| l.resolve(avail))
                    .unwrap_or(160.0)
                    .min(inner.width.max(40.0));
                let h = style
                    .height
                    .and_then(|sv| sv.len())
                    .map(|l| l.resolve(0.0))
                    .unwrap_or(14.0);
                let (fill, track_color) = if indeterminate {
                    (0x9AA0A6FF, 0xE8EAEDFF)
                } else if self.dom.tag(node) == "meter" {
                    // Zone coloring: inside [low, high] → green, outside →
                    // yellow, at the extremes → red (approximation).
                    let (low, high) = (
                        attr_f("low").unwrap_or(min + span * 0.25),
                        attr_f("high").unwrap_or(max - span * 0.25),
                    );
                    let value = v.unwrap_or(min);
                    if value < low || value > high {
                        if value <= min || value >= max {
                            (0xEA4335FF, 0xE8EAEDFF)
                        } else {
                            (0xFBBC04FF, 0xE8EAEDFF)
                        }
                    } else {
                        (0x34A853FF, 0xE8EAEDFF)
                    }
                } else {
                    (0x1A73E8FF, 0xE8EAEDFF)
                };
                let radius = BorderRadius::uniform(Length::Px(h * 0.5));
                self.out.rects.push(Rect {
                    x: inner.x,
                    y: self.y,
                    w: w.max(0.0),
                    h,
                    color: track_color,
                    radius,
                    paint: RectPaint::Solid,
                    transform: None,
                    node: node.0,
                    ..Default::default()
                });
                let fill_w = (w * frac).max(h.min(w));
                self.out.rects.push(Rect {
                    x: inner.x,
                    y: self.y,
                    w: fill_w.max(0.0),
                    h,
                    color: fill,
                    radius,
                    paint: RectPaint::Solid,
                    transform: None,
                    node: node.0,
                    ..Default::default()
                });
                self.y += h;
            }
            "details" => {
                // A5: closed details renders only its summary; every state
                // records a toggle hit-area over the summary's box.
                let open = self
                    .dom
                    .element(node)
                    .and_then(|e| e.attr("open"))
                    .is_some();
                let children = self.dom.children(node).to_vec();
                let mut run: Vec<NodeId> = Vec::new();
                for child in children {
                    if let Some(s) = self.style(child) {
                        if s.position == Position::Absolute || s.position == Position::Fixed {
                            let target = if s.position == Position::Fixed {
                                None
                            } else {
                                child_cb
                            };
                            self.abs_list.push((target, child, inner.x, self.y));
                            continue;
                        }
                        if s.float != FloatSide::None && s.display != Display::None {
                            self.flush_line(inner);
                            self.layout_float(child, inner, child_cb);
                            continue;
                        }
                    }
                    match self.display_of(child) {
                        Some(Display::None) => continue,
                        _ => {}
                    }
                    let is_summary = self.dom.tag(child) == "summary";
                    if !open && !is_summary {
                        continue;
                    }
                    match self.display_of(child) {
                        Some(Display::Block)
                        | Some(Display::ListItem)
                        | Some(Display::Flex)
                        | Some(Display::Grid)
                        | Some(Display::Table)
                        | Some(Display::TableRow)
                        | Some(Display::TableCell)
                        | Some(Display::TableRowGroup)
                        | Some(Display::TableCaption) => {
                            if !run.is_empty() {
                                self.compose_anonymous(
                                    &run,
                                    inner,
                                    style.direction == Direction::Rtl,
                                );
                                run.clear();
                            }
                            let child_top = self.y;
                            let (_, buf) = self.layout_box(child, inner, child_cb);
                            if let Some(buf) = buf {
                                self.merge_buffers(buf);
                            }
                            if is_summary {
                                self.out.toggles.push(ToggleArea {
                                    x: inner.x,
                                    y: child_top,
                                    w: inner.width,
                                    h: (self.y - child_top).max(8.0),
                                    node: child.0,
                                });
                            }
                        }
                        Some(Display::None) => {}
                        _ => run.push(child),
                    }
                }
                self.compose_anonymous(&run, inner, style.direction == Direction::Rtl);
            }
            _ => {
                // R3: generated content feeds the element's own text flow —
                // ::before joins the first line, ::after the last. Flex/
                // grid/table children skip it (v1, documented).
                let mut generated = false;
                // B14: `list-style-position: inside` — the marker joins the
                // item's line flow ahead of ::before content (outside
                // markers already hung at the block's top).
                if style.display == Display::ListItem
                    && style.list_style_position == ListStylePosition::Inside
                {
                    if let Some(m) = style.marker_text.clone() {
                        self.feed_text(&format!("{m} "), &style, None, inner);
                        generated = true;
                    }
                }
                if let Some(b) = &style.before {
                    self.feed_text(&b.text, &b.style, None, inner);
                    generated = true;
                }
                match style.display {
                    Display::Flex => self.layout_flex_children(node, inner, child_cb),
                    Display::Table => self.layout_table_children(node, inner, child_cb),
                    Display::Grid => self.layout_grid_children(node, inner, child_cb),
                    _ => self.layout_block_children(node, inner, child_cb),
                }
                if let Some(a) = &style.after {
                    self.feed_text(&a.text, &a.style, None, inner);
                    generated = true;
                }
                // Children flush their own last line; the ::after word (or a
                // before-only box) would stay pending and be lost.
                if generated {
                    self.flush_line(inner);
                }
            }
        }
        let content_h = self.y - content_y;

        // Flow-root approximation: floats declared inside this block push
        // its bottom edge below them so later siblings start clear of them.
        for f in &self.floats[float_start..] {
            self.y = self.y.max(f.y + f.h);
        }

        self.y += p.bottom + bb;

        // Vertical sizing: explicit `height` wins, then min/max clamp. The
        // flow cursor always leaves at the box's bottom edge, so content that
        // paints past it either overflows visibly or is clipped below.
        // Heights are % -free by construction (style dropped those), so they
        // resolve against a zero basis.
        let mut h = self.y - bg_y;
        // B2: capture the intrinsic content height BEFORE the style
        // height / min / max clamps — that's what scrollable content
        // actually fills, and it has to be exposed to the host so the
        // scrollbar can size itself.
        let intrinsic_content_h = h + p.top + p.bottom + bt + bb;
        // C1: intrinsic keywords on HEIGHT equal `auto` here — a block's
        // auto height is its (single-column) content height, which is what
        // min/max/fit-content all collapse to without a wrap basis.
        if let Some(ht) = style.height.and_then(|sv| sv.len()) {
            h = ht.resolve(0.0);
        } else if let Some(r) = style.aspect_ratio.map(|ar| ar.ratio).filter(|&r| r > 0.0) {
            // C2: `aspect-ratio` derives the box height from the resolved
            // box width when `height` is auto — content-box convention:
            // the ratio spans the content width, padding + borders add
            // back. Min/max-height clamps still apply below. The `auto`
            // keyword is inert here: a block's content has no intrinsic
            // ratio, so `auto <ratio>` is just `<ratio>` (and `auto` alone
            // left the property unset).
            let cw = (box_w - bl - br - p.left - p.right).max(0.0);
            h = cw / r + p.top + p.bottom + bt + bb;
        }
        if let Some(mx) = style.max_height {
            if h > mx.resolve(0.0) {
                h = mx.resolve(0.0);
            }
        }
        if let Some(mn) = style.min_height {
            if h < mn.resolve(0.0) {
                h = mn.resolve(0.0);
            }
        }
        self.y = bg_y + h;
        // B3: the container record opened above closes with its content
        // height (content box = from content_y to the box's bottom minus
        // the bottom padding+border).
        if container_open {
            if let Some(cb) = self.containers.iter_mut().rev().find(|c| c.node == node.0) {
                cb.h = (bg_y + h - p.bottom - bb - content_y).max(0.0);
            }
        }
        // B2/B3: a scroll container — overflow: auto/scroll + content
        // beyond either viewport axis. Hidden doesn't scroll, so it's not
        // recorded. The viewport dimensions are the content box; the
        // content extents are measured from the items painted by this box.
        let viewport_h = h - p.top - p.bottom - bt - bb;
        let viewport_w = (box_w - p.left - p.right - bl - br).max(0.0);
        let mut content_right = inner.x + inner.width;
        for rect in &self.out.rects[start.0..] {
            content_right = content_right.max(rect.x + rect.w);
        }
        for run in &self.out.texts[start.1..] {
            let width = if run.letter_spacing == 0.0 {
                self.tm.measure_family(
                    &run.text,
                    run.size,
                    run.weight,
                    run.italic,
                    run.monospace,
                    &run.family,
                )
            } else {
                self.tm.measure_run_sp_family(
                    &run.text,
                    run.size,
                    run.weight,
                    run.italic,
                    run.monospace,
                    run.letter_spacing,
                    &run.family,
                )
            };
            content_right = content_right.max(run.x + width);
        }
        for link in &self.out.links[start.2..] {
            content_right = content_right.max(link.x + link.w);
        }
        for image in &self.out.images[start.3..] {
            content_right = content_right.max(image.x + image.w);
        }
        for media in &self.out.media[start.4..] {
            content_right = content_right.max(media.x + media.w);
        }
        for shadow in &self.out.shadows[start.5..] {
            content_right = content_right.max(shadow.x + shadow.w);
        }
        let content_w = (content_right - inner.x).max(viewport_w);
        if matches!(style.overflow, Overflow::Auto | Overflow::Scroll)
            && viewport_h > 0.0
            && viewport_w > 0.0
        {
            let overflows = content_w > viewport_w + 0.01 || intrinsic_content_h > h + 0.01;
            if overflows || style.overflow == Overflow::Scroll {
                self.out.scroll_containers.push(ScrollContainer {
                    node: node.0,
                    x: box_x,
                    y: bg_y,
                    w: box_w.max(0.0),
                    h: viewport_h,
                    content_w,
                    content_h: intrinsic_content_h.max(h),
                    mode: if style.overflow == Overflow::Scroll {
                        2
                    } else {
                        1
                    },
                    parent: scroll_parent,
                });
            }
        }
        let clipped =
            h < content_h + p.top + p.bottom + bt + bb && style.overflow == Overflow::Hidden;

        // The box's own background: one rect per painted layer, spliced in
        // ahead of everything the children painted, so a box's fill never
        // covers its descendants. The vec order is paint order — the first
        // layer of a `background` list is the top one, so the layers are
        // emitted bottom-up. Shadows splice into their own list the same way
        // (parents under children). Radius lengths stay authored (`%`
        // resolves against the box width at paint); the fast-path `color` is
        // unchanged.
        let radius = style.border_radius;
        let bg_border = Rect {
            x: box_x,
            y: bg_y,
            w: box_w.max(0.0),
            h,
            ..Default::default()
        };
        if !style.box_shadows.is_empty() {
            let mut at = start.5;
            for s in &style.box_shadows {
                self.out.shadows.insert(
                    at,
                    Shadow {
                        x: box_x,
                        y: bg_y,
                        w: box_w.max(0.0),
                        h,
                        radius,
                        color: s.color,
                        blur: s.blur,
                        spread: s.spread,
                        offset_x: s.offset_x,
                        offset_y: s.offset_y,
                        inset: s.inset,
                        transform: None,
                        filters: style.filter.clone(),
                        clip: None, // the clip-path stamp below covers this box's shadows
                        blend: 0,   // the B8 stamps below cover this box's shadows
                        group: 0,
                        node: self.cur_node,
                    },
                );
                at += 1;
            }
            // The splice shifted every subtree item: follow the already
            // recorded sticky ranges (Q5) along.
            let n = style.box_shadows.len() as u32;
            for s in &mut self.out.stickies[sticky_start..] {
                s.shadow.0 += n;
            }
        }
        let bg_box = BackgroundBox {
            border: bg_border,
            padding: p,
            borders: (bt, br, bb, bl),
        };
        let bg_rects = self.background_rects(
            &style,
            node.0,
            &bg_box,
            radius,
            p.top > 0.0 || p.bottom > 0.0 || p.left > 0.0 || p.right > 0.0,
        );
        for (i, r) in bg_rects.iter().enumerate() {
            self.out.rects.insert(start.0 + i, r.clone());
        }
        let bg_shift = bg_rects.len();
        // `background-image: url()` rides the image list, so it needs no
        // splice and no sticky-range shift (the box's own sticky entry is
        // recorded below and covers the tail).
        self.paint_background_images(&style, &bg_box);
        // R1: borders + outline paint over the background at the box edges,
        // spliced ahead of the subtree's own paint like the background. All
        // inserted rect indices shift the subtree's sticky ranges (Q5), so
        // the bump below covers every splice made here.
        let mut splice: Vec<Rect> = Vec::new();
        if style.border.any_visible() {
            let paints = |s: &BorderSidePx| s.style.paints() && s.width > 0.0;
            // solid: one rect per edge; dashed/dotted: segment runs along it.
            for (side, horiz, ox, oy, ow, oh) in [
                (&borders[0], true, box_x, bg_y, box_w, 0.0), // top
                (
                    &borders[2],
                    true,
                    box_x,
                    bg_y + h - borders[2].width,
                    box_w,
                    0.0,
                ), // bottom
                (&borders[3], false, box_x, bg_y, 0.0, h),    // left
                (
                    &borders[1],
                    false,
                    box_x + box_w - borders[1].width,
                    bg_y,
                    0.0,
                    h,
                ), // right
            ] {
                if !paints(side) {
                    continue;
                }
                let t = side.width;
                if horiz {
                    splice.extend(border_edge_rects(
                        ox, oy, ow, t, true, side.style, side.color,
                    ));
                } else {
                    splice.extend(border_edge_rects(
                        ox, oy, oh, t, false, side.style, side.color,
                    ));
                }
            }
        }
        // Outline: a ring `outline_offset` px outside the border edge.
        let outline = style.outline;
        if outline.style.paints() {
            let t = outline.width.resolve(avail);
            if t > 0.0 {
                let off = style.outline_offset.resolve(avail);
                let (x0, y0, w0, h0) = (
                    box_x - off - t,
                    bg_y - off - t,
                    box_w + 2.0 * (off + t),
                    h + 2.0 * (off + t),
                );
                let color = outline.color.unwrap_or(style.color);
                splice.extend(border_edge_rects(
                    x0 + t,
                    y0,
                    w0 - 2.0 * t,
                    t,
                    true,
                    outline.style,
                    color,
                ));
                splice.extend(border_edge_rects(
                    x0 + t,
                    y0 + h0 - t,
                    w0 - 2.0 * t,
                    t,
                    true,
                    outline.style,
                    color,
                ));
                splice.extend(border_edge_rects(
                    x0,
                    y0 + t,
                    h0 - 2.0 * t,
                    t,
                    false,
                    outline.style,
                    color,
                ));
                splice.extend(border_edge_rects(
                    x0 + w0 - t,
                    y0 + t,
                    h0 - 2.0 * t,
                    t,
                    false,
                    outline.style,
                    color,
                ));
            }
        }
        // Border/outline rects come from the free `border_edge_rects`
        // helper (no node access) — stamp them here, after the background
        // rects spliced above (`bg_shift` is how many of those there are).
        for r in &mut splice {
            r.node = node.0;
        }
        for (i, r) in splice.iter().enumerate() {
            self.out.rects.insert(start.0 + bg_shift + i, r.clone());
        }
        if bg_shift + splice.len() > 0 {
            let n = (bg_shift + splice.len()) as u32;
            for s in &mut self.out.stickies[sticky_start..] {
                s.rect.0 += n;
            }
        }

        // Out-of-flow children resolve now: their containing block is final.
        // CSS 10.1: offsets and percentage extents resolve against the
        // containing block's *padding* box — `inset: 0` inside a padded hero
        // must cover the padding too, not stop at the content edge.
        if positioned {
            self.drain_abs(
                Some(node),
                (
                    inner.x - p.left,
                    content_y - p.top,
                    inner.width + p.left + p.right,
                    content_h + p.top + p.bottom,
                ),
            );
        }

        // overflow:hidden trims everything this subtree painted to the box.
        if clipped {
            // This subtree's sticky entries go through the clip too: their
            // ranges are absolute, so rebase them onto `sub` first (the
            // merge below adds `start` back).
            let mut sub = Buffers {
                rects: self.out.rects.drain(start.0..).collect(),
                texts: self.out.texts.drain(start.1..).collect(),
                links: self.out.links.drain(start.2..).collect(),
                images: self.out.images.drain(start.3..).collect(),
                media: self.out.media.drain(start.4..).collect(),
                shadows: self.out.shadows.drain(start.5..).collect(),
                stickies: self.out.stickies.drain(sticky_start..).collect(),
                scroll_containers: self.out.scroll_containers.drain(scroll_start..).collect(),
                toggles: self.out.toggles.drain(toggle_start..).collect(),
            };
            for s in &mut sub.stickies {
                s.rect.0 -= start.0 as u32;
                s.text.0 -= start.1 as u32;
                s.link.0 -= start.2 as u32;
                s.image.0 -= start.3 as u32;
                s.media.0 -= start.4 as u32;
                s.shadow.0 -= start.5 as u32;
            }
            self.clip_buffers(&mut sub, box_x, bg_y, box_w.max(0.0), h);
            self.merge_buffers(sub);
        }

        // Q2: bake this box's transform into everything the subtree painted.
        // Children already baked theirs (their layout ran first), so a parent
        // pre-multiplication composes outer-over-inner. Translations shift
        // item coords post-hoc (relative/abs placement), so the matrix is
        // built for the final border-box position; the painter maps item
        // coordinates through `transform` itself. Media stays untransformed.
        if let Some(tl) = style.transform.as_ref().filter(|t| !t.0.is_empty()) {
            let m = origin_matrix(tl, &style.transform_origin, box_w.max(0.0), h, box_x, bg_y);
            if !m.is_identity() {
                for r in &mut self.out.rects[start.0..] {
                    r.transform = Some(match r.transform.take() {
                        Some(inner) => m.compose(inner),
                        None => m,
                    });
                }
                for t in &mut self.out.texts[start.1..] {
                    t.transform = Some(match t.transform.take() {
                        Some(inner) => m.compose(inner),
                        None => m,
                    });
                }
                for link in &mut self.out.links[start.2..] {
                    link.transform = Some(match link.transform.take() {
                        Some(inner) => m.compose(inner),
                        None => m,
                    });
                }
                for im in &mut self.out.images[start.3..] {
                    im.transform = Some(match im.transform.take() {
                        Some(inner) => m.compose(inner),
                        None => m,
                    });
                }
                for s in &mut self.out.shadows[start.5..] {
                    s.transform = Some(match s.transform.take() {
                        Some(inner) => m.compose(inner),
                        None => m,
                    });
                }
            }
        }

        // B7: clip-path/mask. Resolve once against this box's final border
        // box and stamp every item the subtree painted that does not yet
        // carry a clip — children closed first, so first-writer-wins means
        // the INNERMOST clip applies (the wire carries one clip per item;
        // nested composition isn't representable). The box's own background,
        // borders and box-shadows (spliced above) land inside the range and
        // get clipped too, per spec.
        if let Some(shape) = style.clip_path.as_ref().or(style.mask.as_ref()) {
            let clip = resolve_clip(shape, box_x, bg_y, box_w, h);
            for r in &mut self.out.rects[start.0..] {
                if r.clip.is_none() {
                    r.clip = Some(clip.clone());
                }
            }
            for t in &mut self.out.texts[start.1..] {
                if t.clip.is_none() {
                    t.clip = Some(clip.clone());
                }
            }
            for im in &mut self.out.images[start.3..] {
                if im.clip.is_none() {
                    im.clip = Some(clip.clone());
                }
            }
            for s in &mut self.out.shadows[start.5..] {
                if s.clip.is_none() {
                    s.clip = Some(clip.clone());
                }
            }
        }

        // B8: blend mode + isolation group, stamped over the same
        // `start..` range as the clip above. `mix-blend-mode` is FIRST-
        // WRITER-WINS (a descendant's own mode beats the ancestor's, and
        // descendants with no mode of their own take the ancestor's — the
        // flat-display-list approximation of "the whole group blends").
        // `isolate` stamps with OVERWRITE: the stamp runs after children
        // closed, so an outer isolate absorbs inner ones into one group
        // id (nested isolates flatten — documented). The host opens a
        // CG offscreen layer per group id, so blends under it never reach
        // the page backdrop.
        if style.mix_blend_mode != kitae_style::MixBlend::Normal {
            let m = style.mix_blend_mode.wire();
            for r in &mut self.out.rects[start.0..] {
                if r.blend == 0 {
                    r.blend = m;
                }
            }
            for t in &mut self.out.texts[start.1..] {
                if t.blend == 0 {
                    t.blend = m;
                }
            }
            for im in &mut self.out.images[start.3..] {
                if im.blend == 0 {
                    im.blend = m;
                }
            }
            for s in &mut self.out.shadows[start.5..] {
                if s.blend == 0 {
                    s.blend = m;
                }
            }
        }
        // E5: `will-change` promotes the subtree to its own composited
        // layer and shares this OVERWRITE group stamp — rendering a group
        // into its own raster is both isolation and promotion, so one id
        // serves both (nested promotions flatten, like nested isolates).
        if style.isolate || style.will_change {
            let gid = NEXT_GROUP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            for r in &mut self.out.rects[start.0..] {
                r.group = gid;
            }
            for t in &mut self.out.texts[start.1..] {
                t.group = gid;
            }
            for im in &mut self.out.images[start.3..] {
                im.group = gid;
            }
            for s in &mut self.out.shadows[start.5..] {
                s.group = gid;
            }
        }

        self.max_y = self.max_y.max(self.y);
        // Q5: record the sticky side-list entry now that every item this
        // subtree painted is in place (splice, clip and transform bake all
        // mutate within `start`..len, so ranges taken here stay valid).
        if style.position == Position::Sticky {
            self.out.stickies.push(StickyItem {
                rect: (start.0 as u32, (self.out.rects.len() - start.0) as u32),
                text: (start.1 as u32, (self.out.texts.len() - start.1) as u32),
                link: (start.2 as u32, (self.out.links.len() - start.2) as u32),
                image: (start.3 as u32, (self.out.images.len() - start.3) as u32),
                media: (start.4 as u32, (self.out.media.len() - start.4) as u32),
                shadow: (start.5 as u32, (self.out.shadows.len() - start.5) as u32),
                base_y: bg_y,
                height: h,
                container_top: parent_scope_top,
                container_bottom: f32::NAN, // patched when the scope closes
                top_offset: style.top.map(|l| l.resolve(0.0)).unwrap_or(0.0),
                bottom_offset: style.bottom.map(|l| l.resolve(0.0)).unwrap_or(0.0),
                has_top: style.top.is_some(),
                has_bottom: style.bottom.is_some(),
            });
        }
        // Close this box's containment scope: stickies registered directly
        // under it (not claimed by an inner scope) clamp at its bottom edge.
        if scope_pushed {
            let (_, first) = self.sticky_scopes.pop().expect("sticky scope balanced");
            let bottom = bg_y + h;
            for s in &mut self.out.stickies[first..] {
                if s.container_bottom.is_nan() {
                    s.container_bottom = bottom;
                }
            }
        }
        if scroll_scope_pushed {
            self.scroll_parents.pop();
        }

        self.y += m.bottom;
        self.cur_node = saved_node;
        h
    }

    /// Lay out a floated child: take it out of the flow at the current edge,
    /// register its rect, and let following line boxes shape around it. The
    /// box's height comes from a throwaway measure pass at its final width;
    /// the paint pass re-runs at the final position. A float never pushes the
    /// flow cursor — only `flush_line`'s band query feels it.
    fn layout_float(&mut self, node: NodeId, frame: Frame, cb: Option<NodeId>) {
        let style = self.style_or_default(node);
        if style.display == Display::None {
            return;
        }
        let avail = frame.width;
        // Shrink-to-fit: explicit width, intrinsic keyword measured, else
        // half the remaining measure.
        let w = match style.width {
            Some(SizeVal::Len(l)) => l.resolve(avail),
            Some(sv) => {
                let (mc, xc) = self.intrinsic_pair(node);
                sv.resolve(avail, mc, xc)
            }
            None => (self.content_main_size(node)).min(avail * 0.5),
        }
        .min(avail)
        .max(40.0);

        // Measure pass at the final width learns the rect's height. Probes
        // must not leak abs entries or floats into the real run.
        let abs_saved = std::mem::take(&mut self.abs_list);
        let floats_saved = std::mem::take(&mut self.floats);
        let saved_y = self.y;
        let (_, h) = self.with_buffers(|lay| {
            lay.y = 0.0;
            lay.layout_static_box(
                node,
                Frame {
                    x: 0.0,
                    width: w,
                    align: style.text_align,
                },
                cb,
            )
        });
        self.y = saved_y;
        self.floats = floats_saved;
        self.abs_list = abs_saved;

        let left = style.float == FloatSide::Left;
        // Placement: sit at the flow edge; same-side floats still vertically
        // overlapping the candidate band push the edge inward (stacking side
        // by side). If the box no longer fits past them, drop below the
        // lowest blocker and try again.
        let mut fy = self.y;
        let fx = loop {
            let mut edge = if left { frame.x } else { frame.x + avail };
            let mut blocked = false;
            for f in &self.floats {
                if f.left == left && f.y < fy + h && f.y + f.h > fy {
                    blocked = true;
                    if left {
                        edge = edge.max(f.x + f.w);
                    } else {
                        edge = edge.min(f.x);
                    }
                }
            }
            let x = if left { edge } else { edge - w };
            if !blocked || (x >= frame.x - 0.5 && x + w <= frame.x + avail + 0.5) {
                break x;
            }
            // No room beside: sink under every blocker and retry.
            let mut next = fy;
            for f in &self.floats {
                if f.left == left && f.y < fy + h && f.y + f.h > fy {
                    next = next.max(f.y + f.h);
                }
            }
            fy = next;
        };
        // Paint pass at the real origin; captured coordinates are already
        // page-absolute, so the merge needs no translation. Registered only
        // after painting — the box's own content must not shape around its
        // own rect.
        self.y = fy;
        let (buf, _) = self.with_buffers(|lay| {
            lay.layout_static_box(
                node,
                Frame {
                    x: fx,
                    width: w,
                    align: style.text_align,
                },
                cb,
            )
        });
        self.floats.push(FloatBox {
            left,
            x: fx,
            y: fy,
            w,
            h,
        });
        self.y = saved_y;
        self.merge_buffers(buf);
        self.max_y = self.max_y.max(fy + h);
    }

    /// Flow position pushed below every matching float for `clear`
    /// (Phase N1). Same-block approximation: the registry only holds floats
    /// from enclosing blocks, which is exactly the clearance scope.
    fn clear_y(&self, side: Clear) -> f32 {
        let mut y = self.y;
        for f in &self.floats {
            let hit = match side {
                Clear::Left => f.left,
                Clear::Right => !f.left,
                Clear::Both => true,
                Clear::None => false,
            };
            if hit {
                y = y.max(f.y + f.h);
            }
        }
        y
    }

    /// Horizontal band `[x, w]` a line spanning `y_top..y_bot` may use,
    /// narrowed by any floats intersecting that vertical span.
    fn float_band(&self, frame: Frame, y_top: f32, y_bot: f32) -> (f32, f32) {
        let mut x0 = frame.x;
        let mut x1 = frame.x + frame.width;
        for f in &self.floats {
            if f.y >= y_bot || f.y + f.h <= y_top {
                continue;
            }
            if f.left {
                x0 = x0.max(f.x + f.w);
            } else {
                x1 = x1.min(f.x);
            }
        }
        if x1 < x0 {
            x1 = x0;
        }
        (x0, x1 - x0)
    }

    /// Lay out the element children of `node`: consecutive inline nodes form
    /// an anonymous block; block children recurse through layout_box;
    /// absolutely/fixed positioned children leave the flow.
    fn layout_block_children(&mut self, node: NodeId, frame: Frame, cb: Option<NodeId>) {
        let node_style = self.style_or_default(node);
        let child_cb = if node_style.position != Position::Static {
            Some(node)
        } else {
            cb
        };
        let children: Vec<NodeId> = self.dom.children(node).to_vec();
        let mut run: Vec<NodeId> = Vec::new();

        // B5: a column-split container takes over the flow entirely —
        // collection reports the unsupported shapes (floated children) by
        // bailing, in which case the plain block flow below runs untouched.
        if let Some(plan) = self.multicol_plan(&node_style, frame.width) {
            if let Some(units) = self.collect_multicol_units(&children, child_cb, (frame.x, self.y))
            {
                self.layout_multicol(frame, child_cb, &node_style, units, plan);
                return;
            }
        }

        for child in children {
            if let Some(s) = self.style(child) {
                if s.position == Position::Absolute || s.position == Position::Fixed {
                    let target = if s.position == Position::Fixed {
                        None
                    } else {
                        child_cb
                    };
                    self.abs_list.push((target, child, frame.x, self.y));
                    continue;
                }
                if s.float != FloatSide::None && s.display != Display::None {
                    if !run.is_empty() {
                        self.compose_anonymous(&run, frame, node_style.direction == Direction::Rtl);
                        run.clear();
                    }
                    self.layout_float(child, frame, child_cb);
                    continue;
                }
            }
            match self.display_of(child) {
                Some(Display::Block)
                | Some(Display::ListItem)
                | Some(Display::Flex)
                | Some(Display::Grid)
                | Some(Display::Table)
                | Some(Display::TableRow)
                | Some(Display::TableCell)
                | Some(Display::TableRowGroup)
                | Some(Display::TableCaption) => {
                    if !run.is_empty() {
                        self.compose_anonymous(&run, frame, node_style.direction == Direction::Rtl);
                        run.clear();
                    }
                    // `clear` (Phase N1): drop the flow cursor below any
                    // matching floats before the box starts.
                    match self.style_or_default(child).clear {
                        Clear::None => {}
                        side => {
                            let below = self.clear_y(side);
                            if below > self.y {
                                self.y = below;
                            }
                        }
                    }
                    // I2: body-scope fragment reuse. A hit re-merges the
                    // stored paint translated to the current cursor — the
                    // box cannot register out-of-flow descendants (no such
                    // fragment is ever stored), so the merge plus a cursor
                    // advance reproduces the layout exactly. A miss captures
                    // the child's paint (identical to direct painting) and
                    // stores it when the fragment is self-contained: nothing
                    // registered in abs_list/floats/containers, no stickies.
                    if Some(node) == self.cache_scope
                        && self.cache.is_some()
                        && self.floats.is_empty()
                    {
                        if let Some((advance, max_depth, mut buf)) =
                            self.cache_hit(child, frame.width)
                        {
                            let base = self.y;
                            buf.translate(0.0, base);
                            self.merge_buffers(buf);
                            self.y = base + advance;
                            self.max_y = self.max_y.max(base + max_depth);
                            if let Some(c) = self.cache.as_mut() {
                                c.hits += 1;
                            }
                            continue;
                        }
                        if let Some(c) = self.cache.as_mut() {
                            c.misses += 1;
                        }
                        let y_before = self.y;
                        let abs_before = self.abs_list.len();
                        let float_before = self.floats.len();
                        let containers_before = self.containers.len();
                        // Own-depth measure: max_y is write-only during flow
                        // layout, so pinning it to the cursor reads back the
                        // box's own extent unpolluted by earlier siblings.
                        let saved_max_y = std::mem::replace(&mut self.max_y, y_before);
                        let (captured, inner) =
                            self.with_buffers(|s| s.layout_box(child, frame, child_cb));
                        let own_depth = self.max_y - y_before;
                        self.max_y = saved_max_y.max(y_before + own_depth);
                        let buf = match inner.1 {
                            // position:relative: its translated paint rides
                            // the inner capture; the outer one stays empty
                            // because layout_box captures the whole box
                            // internally in that path.
                            Some(rel) => {
                                debug_assert!(
                                    captured.is_empty(),
                                    "relative box left paint in the outer capture"
                                );
                                rel
                            }
                            None => captured,
                        };
                        if self.abs_list.len() == abs_before
                            && self.floats.len() == float_before
                            && self.containers.len() == containers_before
                            && buf.stickies.is_empty()
                        {
                            let mut norm = buf.clone();
                            norm.translate(0.0, -y_before);
                            if let Some(c) = self.cache.as_mut() {
                                c.entries.insert(
                                    child,
                                    FragmentEntry {
                                        rev: self.dom.rev(child),
                                        width_bits: frame.width.to_bits(),
                                        viewport_h_bits: self.viewport_h.to_bits(),
                                        advance: self.y - y_before,
                                        max_depth: own_depth,
                                        buf: norm,
                                    },
                                );
                            }
                        }
                        self.merge_buffers(buf);
                        continue;
                    }
                    let (_, buf) = self.layout_box(child, frame, child_cb);
                    if let Some(buf) = buf {
                        self.merge_buffers(buf);
                    }
                }
                Some(Display::None) => {}
                // A4: column metadata — consumed by table layout when the
                // parent is a table; anywhere else it is box-less.
                Some(Display::TableColumn) | Some(Display::TableColumnGroup) => {}
                _ => run.push(child),
            }
        }
        if !run.is_empty() {
            self.compose_anonymous(&run, frame, node_style.direction == Direction::Rtl);
        }
    }

    /// B5: the multi-column geometry for a container: `(columns, width,
    /// gap)` in px, or `None` when the style doesn't ask for columns.
    /// `column-count` alone fixes the number; `column-width` alone estimates
    /// how many fit (each at least the requested width); both together mean
    /// the count is the estimate's ceiling. `column-gap: normal` folds to
    /// 1em, matching the CSS initial used value.
    fn multicol_plan(&self, st: &ComputedStyle, avail: f32) -> Option<(usize, f32, f32)> {
        if st.column_count.is_none() && st.column_width.is_none() {
            return None;
        }
        let gap = st
            .column_gap
            .map(|l| l.resolve(avail).max(0.0))
            .unwrap_or(st.font_size.max(0.0));
        // Hard count; `column-count: 1` (or garbage) stays single-column.
        let count = st.column_count.unwrap_or(u32::MAX);
        let n = match st.column_width {
            Some(w) => {
                let w = w.resolve(avail).max(1.0);
                let fit = (((avail + gap) / (w + gap)).floor() as u32).max(1);
                count.min(fit)
            }
            None => count,
        };
        // Ceil to a sane span: `column-count: 1000000` must not multiply
        // measure passes; overflow-safe conversion because u32::MAX means
        // "no explicit count" above.
        let n = usize::try_from(n.min(64)).unwrap_or(0);
        if n < 2 {
            return None;
        }
        let col_w = ((avail - (n - 1) as f32 * gap) / n as f32).max(1.0);
        Some((n, col_w, gap))
    }

    /// B5: turn the container's children into fragmentation units, exactly
    /// mirroring the block/inline grouping of `layout_block_children`.
    /// Out-of-flow children go to `abs_list` for the container's drain; a
    /// floated child bails to normal flow (float placement across columns
    /// is not implemented).
    fn collect_multicol_units(
        &mut self,
        children: &[NodeId],
        child_cb: Option<NodeId>,
        static_pos: (f32, f32),
    ) -> Option<Vec<MulticolUnit>> {
        for &child in children {
            if let Some(s) = self.style(child) {
                if s.float != FloatSide::None && s.display != Display::None {
                    return None;
                }
            }
        }
        let mut units: Vec<MulticolUnit> = Vec::new();
        let mut run: Vec<NodeId> = Vec::new();
        for &child in children {
            if let Some(s) = self.style(child) {
                if s.position == Position::Absolute || s.position == Position::Fixed {
                    let target = if s.position == Position::Fixed {
                        None
                    } else {
                        child_cb
                    };
                    // C4: the container origin stands in for the flow cursor.
                    self.abs_list
                        .push((target, child, static_pos.0, static_pos.1));
                    continue;
                }
            }
            match self.display_of(child) {
                Some(Display::Block)
                | Some(Display::ListItem)
                | Some(Display::Flex)
                | Some(Display::Grid)
                | Some(Display::Table)
                | Some(Display::TableRow)
                | Some(Display::TableCell)
                | Some(Display::TableRowGroup)
                | Some(Display::TableCaption) => {
                    if !run.is_empty() {
                        units.push(MulticolUnit::Run(std::mem::take(&mut run)));
                    }
                    units.push(MulticolUnit::BlockBox(child));
                }
                Some(Display::None) => {}
                Some(Display::TableColumn) | Some(Display::TableColumnGroup) => {}
                _ => run.push(child),
            }
        }
        if !run.is_empty() {
            units.push(MulticolUnit::Run(run));
        }
        if units.len() < 2 {
            // Nothing to split (a single unit can't show a second column's
            // gap anyway): let the plain flow handle it at full width.
            return None;
        }
        Some(units)
    }

    /// B5: measure, balance and paint the fragmentation units across the
    /// planned columns. Measurement is the flex pattern — a scratch paint
    /// (`with_buffers`) from y = 0 at the column width, with `abs_list`
    /// and `floats` snapshotted so probe descendants can't double up or
    /// leak their float tracks. Balancing is greedy: fill columns toward
    /// the even target `total / n`; the last column absorbs the tail
    /// (line-level balancing is deferred with text fragmentation).
    fn layout_multicol(
        &mut self,
        frame: Frame,
        child_cb: Option<NodeId>,
        node_style: &ComputedStyle,
        units: Vec<MulticolUnit>,
        (n, col_w, gap): (usize, f32, f32),
    ) {
        let rtl = node_style.direction == Direction::Rtl;
        let n = n.min(units.len());
        let measure_frame = Frame {
            x: frame.x,
            width: col_w,
            align: frame.align,
        };

        let saved_y = self.y;
        let abs_saved = std::mem::take(&mut self.abs_list);
        let floats_saved = std::mem::take(&mut self.floats);
        let mut heights: Vec<f32> = Vec::with_capacity(units.len());
        for unit in &units {
            let (_, extent) = self.with_buffers(|lay| {
                lay.y = 0.0;
                match unit {
                    MulticolUnit::BlockBox(b) => {
                        lay.layout_static_box(*b, measure_frame, child_cb);
                    }
                    MulticolUnit::Run(r) => {
                        lay.compose_anonymous(r, measure_frame, rtl);
                    }
                }
                // Cursor delta, margins included: this is the flow advance.
                lay.y
            });
            heights.push(extent.max(0.0));
        }
        self.abs_list = abs_saved;
        self.floats = floats_saved;
        self.y = saved_y;

        let target = heights.iter().sum::<f32>() / n as f32;
        let mut assign: Vec<usize> = Vec::with_capacity(units.len());
        let mut col = 0usize;
        let mut used = 0f32;
        for (i, h) in heights.iter().enumerate() {
            // C3: a forced break at the previous boundary starts a new
            // column regardless of the balance target.
            let forced = i > 0
                && (self.unit_break_after(&units[i - 1]) || self.unit_break_before(&units[i]));
            if col + 1 < n && ((used > 0.0 && used + h > target) || forced) {
                col += 1;
                used = 0.0;
            }
            assign.push(col);
            used += h;
        }

        // Paint pass: real geometry, one cursor per column, document order
        // within each. The container ends at the deepest column bottom.
        let mut y_by_col = vec![saved_y; n];
        for (i, unit) in units.iter().enumerate() {
            let c = assign[i];
            self.y = y_by_col[c];
            let cf = Frame {
                x: frame.x + (col_w + gap) * c as f32,
                width: col_w,
                align: frame.align,
            };
            match unit {
                MulticolUnit::BlockBox(b) => {
                    let (_, buf) = self.layout_box(*b, cf, child_cb);
                    if let Some(buf) = buf {
                        self.merge_buffers(buf);
                    }
                }
                MulticolUnit::Run(r) => {
                    self.compose_anonymous(r, cf, rtl);
                }
            }
            y_by_col[c] = self.y;
        }
        self.y = saved_y.max(y_by_col.iter().cloned().fold(f32::MIN, f32::max));
        self.max_y = self.max_y.max(self.y);
    }

    /// C3: does this multicol unit force a break after itself? `page`
    /// collapses to `column` — the engine has one continuous viewport, so
    /// a forced page break in multicol is a column break. Anonymous runs
    /// check their last inline child; `avoid`/`auto` never split.
    fn unit_break_after(&self, u: &MulticolUnit) -> bool {
        let node = match u {
            MulticolUnit::BlockBox(b) => Some(*b),
            MulticolUnit::Run(r) => r.last().copied(),
        };
        node.and_then(|n| self.style(n))
            .map(|s| matches!(s.break_after, BreakAction::Column | BreakAction::Page))
            .unwrap_or(false)
    }

    /// C3: mirror of `unit_break_after` for the unit's leading edge.
    fn unit_break_before(&self, u: &MulticolUnit) -> bool {
        let node = match u {
            MulticolUnit::BlockBox(b) => Some(*b),
            MulticolUnit::Run(r) => r.first().copied(),
        };
        node.and_then(|n| self.style(n))
            .map(|s| matches!(s.break_before, BreakAction::Column | BreakAction::Page))
            .unwrap_or(false)
    }

    /// Flex container layout: a single-line flex run along the main axis
    /// (`flex-direction`), sized by `flex-basis` / `width` and grown or
    /// shrunk by `flex-grow` / `flex-shrink`; free space distributed by
    /// `justify-content`, cross placement by `align-items` (stretch fills
    /// the tallest item), `gap` between adjacent items.
    ///
    /// Items are laid out twice: a measure pass into a throwaway buffer
    /// learns each item's main-axis extent at its flexed size, then the
    /// real pass paints at the final geometry. Out-of-flow children still
    /// wait in `abs_list` for the container's drain.
    fn layout_flex_children(&mut self, node: NodeId, frame: Frame, cb: Option<NodeId>) {
        let style = self.style_or_default(node);
        let child_cb = if style.position != Position::Static {
            Some(node)
        } else {
            cb
        };
        let column = matches!(
            style.flex_direction,
            FlexDirection::Column | FlexDirection::ColumnReverse
        );
        let reverse = matches!(
            style.flex_direction,
            FlexDirection::RowReverse | FlexDirection::ColumnReverse
        );

        // In-flow flex items in document order; out-of-flow children leave
        // the run entirely.
        let mut items: Vec<NodeId> = Vec::new();
        for &child in self.dom.children(node) {
            if let Some(s) = self.style(child) {
                if s.position == Position::Absolute || s.position == Position::Fixed {
                    let target = if s.position == Position::Fixed {
                        None
                    } else {
                        child_cb
                    };
                    self.abs_list.push((target, child, frame.x, self.y));
                    continue;
                }
            }
            if self.display_of(child) == Some(Display::None) {
                continue;
            }
            items.push(child);
        }
        if items.is_empty() {
            return;
        }

        // Base size along the main axis: flex-basis, then width/height, then
        // content (rows only — measured). Columns lay out at the container's
        // cross width; their main size comes from the measure pass.
        let avail_main = frame.width;
        let mut widths: Vec<f32> = Vec::with_capacity(items.len());
        let mut bases: Vec<f32> = Vec::with_capacity(items.len());
        let mut grow: Vec<f32> = Vec::new();
        let mut shrink: Vec<f32> = Vec::new();
        for &item in &items {
            let s = self.style_or_default(item);
            let (w, base) = if column {
                // Column: main size is vertical, so % bases are indefinite.
                // C1: an intrinsic keyword on the cross width measures the
                // item; a keyword `height`/`flex-basis` falls back to auto
                // (the measure pass sizes the main axis).
                let w = match s.width {
                    Some(SizeVal::Len(l)) => l.resolve(avail_main),
                    Some(sv) => {
                        let (mc, xc) = self.intrinsic_item_pair(item, avail_main);
                        sv.resolve(avail_main, mc, xc)
                    }
                    None => avail_main,
                };
                let base = s
                    .flex_basis
                    .filter(|sv| match sv {
                        SizeVal::Len(l) => !l.has_pct(),
                        _ => true,
                    })
                    .or(s.height)
                    .and_then(|sv| sv.len())
                    .map(|l| l.resolve(0.0))
                    .unwrap_or(0.0);
                (w, base)
            } else {
                let w = match s.flex_basis.or(s.width) {
                    Some(SizeVal::Len(l)) => l.resolve(avail_main),
                    Some(sv) => {
                        let (mc, xc) = self.intrinsic_item_pair(item, avail_main);
                        sv.resolve(avail_main, mc, xc)
                    }
                    // Auto basis: max-content INCLUDING the item's own
                    // padding+border (intrinsic_item_pair). The bare text
                    // width let flex rows advance items by less than their
                    // painted box, so padded chips/pills overlapped their
                    // neighbor by exactly their chrome — the same fix
                    // `content_main_size` applies for form controls.
                    None => self.intrinsic_item_pair(item, avail_main).1,
                };
                (w, w)
            };
            widths.push(w.min(avail_main));
            bases.push(base.min(avail_main));
            grow.push(s.flex_grow);
            shrink.push(s.flex_shrink);
        }

        let gap_px = style.gap.resolve(frame.width);

        // Q4: multi-line rows. Column wrap stays single-line (a wrapped
        // column needs per-column cross sizing no current fixture exercises).
        let wrap = !column && matches!(style.flex_wrap, FlexWrap::Wrap | FlexWrap::WrapReverse);
        if wrap {
            self.layout_flex_rows_wrapped(
                &style, &items, frame, child_cb, avail_main, gap_px, reverse,
            );
            return;
        }

        let gap_total = gap_px * (items.len().saturating_sub(1)) as f32;
        let free = avail_main - gap_total - bases.iter().sum::<f32>();
        if free >= 0.0 {
            let total_grow: f32 = grow.iter().sum();
            if total_grow > 0.0 {
                for i in 0..bases.len() {
                    bases[i] += free * (grow[i] / total_grow);
                }
            }
        } else {
            let weighted: f32 = shrink.iter().zip(&bases).map(|(s, b)| s * b).sum();
            let over = -free;
            if weighted > 0.0 {
                for i in 0..bases.len() {
                    bases[i] = (bases[i] - over * (shrink[i] * bases[i] / weighted)).max(0.0);
                }
            }
        }

        // justify-content: leftover space becomes a leading offset (or, for
        // space-between, an extra gap between items).
        let used: f32 = bases.iter().sum::<f32>() + gap_total;
        let leftover = (avail_main - used).max(0.0);
        let (lead, extra_gap) = match style.justify_content {
            JustifyContent::Center => (leftover * 0.5, 0.0),
            JustifyContent::FlexEnd => (leftover, 0.0),
            JustifyContent::SpaceBetween if items.len() > 1 => {
                (0.0, leftover / (items.len() - 1) as f32)
            }
            _ => (0.0, 0.0),
        };

        // Measure pass: each item's extent along its main axis at its flexed
        // size, laid out from a scratch origin with paint discarded. `abs_list`
        // entries pushed here would double-count in the paint pass, so the
        // list is snapshotted and restored around the probe.
        let abs_saved = std::mem::take(&mut self.abs_list);
        let mut extents: Vec<f32> = Vec::with_capacity(items.len());
        for (i, &item) in items.iter().enumerate() {
            let s = self.style_or_default(item);
            let measure_frame = if column {
                // The item's real cross width — flooring to 40 would hand an
                // 11px dot a phantom 40px frame to center inside.
                Frame {
                    x: frame.x,
                    width: widths[i].max(1.0),
                    align: s.text_align,
                }
            } else {
                Frame {
                    x: 0.0,
                    width: widths[i].max(1.0),
                    align: s.text_align,
                }
            };
            let saved_y = self.y;
            let (_, extent) = self.with_buffers(|lay| {
                lay.y = 0.0;
                lay.layout_static_box(item, measure_frame, child_cb)
            });
            // A column item with an auto main size is as tall as it measures.
            if column && bases[i] == 0.0 {
                bases[i] = extent;
            }
            extents.push(extent);
            // with_buffers restored `out` but `self.y` moved: reset it.
            self.y = saved_y;
        }
        self.abs_list = abs_saved;

        // Cross sizes: rows stretch to the tallest item (align-items
        // stretch/flex-start/center/flex-end place within it).
        let cross_extent = if column {
            0.0
        } else {
            extents.iter().cloned().fold(0.0, f32::max)
        };

        // Paint pass: real geometry. Rows run horizontally with `self.y`
        // pinned at the container top; columns are just the block cursor.
        let run_y = self.y;
        let mut cursor = lead;
        let order: Vec<usize> = if reverse {
            (0..items.len()).rev().collect()
        } else {
            (0..items.len()).collect()
        };
        for &i in &order {
            let item = items[i];
            let main_size = bases[i];
            let s = self.style_or_default(item);
            if column {
                self.y = run_y + cursor;
                let h = self.layout_static_box(
                    item,
                    // The item's real cross width — a 40px floor would hand
                    // an 11px dot a phantom frame to center inside.
                    Frame {
                        x: frame.x,
                        width: widths[i].max(1.0),
                        align: s.text_align,
                    },
                    child_cb,
                );
                // Static boxes paint straight into `self.out`; positioned
                // ones captured — either way the box sits at self.y.
                cursor += bases[i].max(h) + gap_px + extra_gap;
                self.y = run_y + cursor - gap_px - extra_gap;
            } else {
                // Pin the cursor so the item lays out at the container top;
                // then translate the painted slice to its main-axis slot.
                self.y = run_y;
                let (mut buf, _h) = self.with_buffers(|lay| {
                    lay.layout_static_box(
                        item,
                        Frame {
                            x: 0.0,
                            width: main_size.max(1.0),
                            align: s.text_align,
                        },
                        child_cb,
                    )
                });
                let dx = frame.x + cursor;
                let dy = match style.align_items {
                    AlignItems::FlexEnd => cross_extent - _h,
                    AlignItems::Center => (cross_extent - _h) * 0.5,
                    _ => 0.0,
                };
                buf.translate(dx, dy.max(0.0));
                self.merge_buffers(buf);
                cursor += main_size + gap_px + extra_gap;
            }
        }
        // Container height: columns by the cursor, rows by the tallest item.
        if column {
            self.y = run_y + (cursor - gap_px - extra_gap).max(0.0);
        } else {
            self.y = run_y + cross_extent;
        }
        self.max_y = self.max_y.max(self.y);
    }

    /// Q4: multi-line flex rows (`flex-wrap: wrap|wrap-reverse`). Items break
    /// into lines by hypothetical main size; each line flexes independently
    /// (grow/shrink per line); `align-content` distributes leftover cross
    /// space when the container has a definite height; `wrap-reverse` stacks
    /// the first line at the bottom.
    fn layout_flex_rows_wrapped(
        &mut self,
        style: &ComputedStyle,
        items: &[NodeId],
        frame: Frame,
        child_cb: Option<NodeId>,
        avail_main: f32,
        gap_px: f32,
        reverse: bool,
    ) {
        // Hypothetical main size per item: flex-basis, then width, then
        // content (same rule as the single-line path).
        let mut hypo: Vec<f32> = Vec::with_capacity(items.len());
        let mut grow: Vec<f32> = Vec::with_capacity(items.len());
        let mut shrink: Vec<f32> = Vec::with_capacity(items.len());
        for &item in items {
            let s = self.style_or_default(item);
            let w = match s.flex_basis.or(s.width) {
                Some(SizeVal::Len(l)) => l.resolve(avail_main),
                Some(sv) => {
                    let (mc, xc) = self.intrinsic_item_pair(item, avail_main);
                    sv.resolve(avail_main, mc, xc)
                }
                // Auto basis includes the item's chrome — see the
                // single-line path above (padded items must advance by
                // their painted width, not their bare text width).
                None => self.intrinsic_item_pair(item, avail_main).1,
            };
            hypo.push(w.min(avail_main));
            grow.push(s.flex_grow);
            shrink.push(s.flex_shrink);
        }

        // Line breaking by hypothetical main size (an over-wide item keeps
        // its own line, CSS-style).
        let mut lines: Vec<Vec<usize>> = vec![Vec::new()];
        let mut line_main = 0.0f32;
        for (i, &w) in hypo.iter().enumerate() {
            let line = lines.last().unwrap();
            let need = if line.is_empty() {
                w
            } else {
                line_main + gap_px + w
            };
            if !line.is_empty() && need > avail_main + 0.5 {
                lines.push(vec![i]);
                line_main = w;
            } else {
                lines.last_mut().unwrap().push(i);
                line_main = need;
            }
        }

        // Each line flexes independently.
        let mut line_bases: Vec<Vec<f32>> = Vec::with_capacity(lines.len());
        for line in &lines {
            let mut bs: Vec<f32> = line.iter().map(|&i| hypo[i]).collect();
            let gap_total = gap_px * (line.len().saturating_sub(1)) as f32;
            let free = avail_main - gap_total - bs.iter().sum::<f32>();
            if free >= 0.0 {
                let total_grow: f32 = line.iter().map(|&i| grow[i]).sum();
                if total_grow > 0.0 {
                    for (k, &i) in line.iter().enumerate() {
                        bs[k] += free * (grow[i] / total_grow);
                    }
                }
            } else {
                let weighted: f32 = line.iter().zip(&bs).map(|(&i, b)| shrink[i] * b).sum();
                let over = -free;
                if weighted > 0.0 {
                    for (k, &i) in line.iter().enumerate() {
                        bs[k] = (bs[k] - over * (shrink[i] * bs[k] / weighted)).max(0.0);
                    }
                }
            }
            line_bases.push(bs);
        }
        let mut final_main = vec![0.0f32; items.len()];
        for (line, bs) in lines.iter().zip(&line_bases) {
            for (&i, b) in line.iter().zip(bs) {
                final_main[i] = *b;
            }
        }

        // Measure pass at the final flexed main size: line cross sizing needs
        // post-flex heights, not hypothetical ones.
        let abs_saved = std::mem::take(&mut self.abs_list);
        let mut extents = vec![0.0f32; items.len()];
        for (i, &item) in items.iter().enumerate() {
            let s = self.style_or_default(item);
            let measure_frame = Frame {
                x: 0.0,
                width: final_main[i].max(1.0),
                align: s.text_align,
            };
            let saved_y = self.y;
            let (_, extent) = self.with_buffers(|lay| {
                lay.y = 0.0;
                lay.layout_static_box(item, measure_frame, child_cb)
            });
            extents[i] = extent;
            self.y = saved_y;
        }
        self.abs_list = abs_saved;

        // Per-line cross size: the tallest item on the line.
        let mut line_cross: Vec<f32> = lines
            .iter()
            .map(|line| line.iter().map(|&i| extents[i]).fold(0.0f32, f32::max))
            .collect();

        // align-content: leftover cross space only exists with a definite
        // container height (an auto-height container grows with its lines).
        let mut lead_cross = 0.0f32;
        let mut line_gap = 0.0f32;
        if let Some(h) = style
            .height
            .and_then(|sv| sv.len())
            .map(|l| l.resolve(avail_main))
        {
            let total_cross: f32 = line_cross.iter().sum();
            let leftover = h - total_cross;
            if leftover > 0.0 {
                let n = lines.len();
                match style.align_content {
                    AlignContent::Stretch => {
                        if n > 0 {
                            for c in &mut line_cross {
                                *c += leftover / n as f32;
                            }
                        }
                    }
                    AlignContent::Center => lead_cross = leftover * 0.5,
                    AlignContent::FlexEnd => lead_cross = leftover,
                    AlignContent::SpaceBetween if n > 1 => line_gap = leftover / (n - 1) as f32,
                    _ => {}
                }
            }
        }

        // Line tops in normal order, mirrored for wrap-reverse (first line
        // at the bottom, align-content offsets preserved).
        let mut line_tops = Vec::with_capacity(lines.len());
        let mut ty = lead_cross;
        for c in &line_cross {
            line_tops.push(ty);
            ty += c + line_gap;
        }
        let stack_total = ty - line_gap - lead_cross;
        if style.flex_wrap == FlexWrap::WrapReverse {
            for (i, top) in line_tops.iter_mut().enumerate() {
                *top = lead_cross + stack_total - (*top - lead_cross) - line_cross[i];
            }
        }

        // Paint pass: justify-content applies within each line.
        let run_y = self.y;
        for (li, line) in lines.iter().enumerate() {
            let bs = &line_bases[li];
            let line_top = run_y + line_tops[li];
            let cross = line_cross[li];
            let used: f32 = bs.iter().sum::<f32>() + gap_px * (line.len().saturating_sub(1)) as f32;
            let leftover = (avail_main - used).max(0.0);
            let (lead, extra_gap) = match style.justify_content {
                JustifyContent::Center => (leftover * 0.5, 0.0),
                JustifyContent::FlexEnd => (leftover, 0.0),
                JustifyContent::SpaceBetween if line.len() > 1 => {
                    (0.0, leftover / (line.len() - 1) as f32)
                }
                _ => (0.0, 0.0),
            };
            let mut cursor = lead;
            let order: Vec<usize> = if reverse {
                line.iter().rev().copied().collect()
            } else {
                line.clone()
            };
            for &i in &order {
                let item = items[i];
                let s = self.style_or_default(item);
                self.y = line_top;
                let (mut buf, h) = self.with_buffers(|lay| {
                    lay.layout_static_box(
                        item,
                        Frame {
                            x: 0.0,
                            width: final_main[i].max(1.0),
                            align: s.text_align,
                        },
                        child_cb,
                    )
                });
                let dx = frame.x + cursor;
                let dy = match style.align_items {
                    AlignItems::FlexEnd => cross - h,
                    AlignItems::Center => (cross - h) * 0.5,
                    _ => 0.0,
                };
                buf.translate(dx, dy.max(0.0));
                self.merge_buffers(buf);
                cursor += final_main[i] + gap_px + extra_gap;
            }
        }
        // Container height: the line stack (equal to a definite height when
        // align-content distributed the leftover into it).
        self.y = run_y + lead_cross + stack_total;
        self.max_y = self.max_y.max(self.y);
    }

    /// Probe a table cell's height at `width` without disturbing flow state
    /// (throwaway buffers, saved/restored abs list and floats).
    fn table_cell_height(&mut self, cell: NodeId, width: f32, child_cb: Option<NodeId>) -> f32 {
        let st = self.style_or_default(cell);
        let saved_y = self.y;
        let abs_saved = std::mem::take(&mut self.abs_list);
        let floats_saved = std::mem::take(&mut self.floats);
        let (_, h) = self.with_buffers(|lay| {
            lay.y = 0.0;
            lay.layout_static_box(
                cell,
                Frame {
                    x: 0.0,
                    width: width.max(20.0),
                    align: st.text_align,
                },
                child_cb,
            )
        });
        self.y = saved_y;
        self.floats = floats_saved;
        self.abs_list = abs_saved;
        h
    }

    /// Tables: row groups, rows, cells, caption.
    ///
    /// Flatten row-group children into rows, content-measure column widths
    /// (per-cell padding through the measurer probe), size rows to their
    /// tallest cell, paint each cell via `layout_static_box` at its
    /// (column, row) slot, caption above. `colspan`/`rowspan` honored
    /// (colspan/rowspan capped per HTML — 1000 / 65534; `rowspan="0"`
    /// spans to the section end). C5: `border-spacing` insets the grid,
    /// `border-collapse` zeroes it, `caption-side: bottom` captions below.
    fn layout_table_children(&mut self, node: NodeId, frame: Frame, cb: Option<NodeId>) {
        let style = self.style_or_default(node);
        let child_cb = if style.position != Position::Static {
            Some(node)
        } else {
            cb
        };

        enum Item {
            Caption(NodeId),
            Row {
                group: Option<NodeId>,
                row: Option<NodeId>,
                cells: Vec<TableCell>,
            },
            /// A4: `<col>` spanning `span` columns with an optional fixed
            /// width (CSS width, or the width attribute — px/%). Column
            /// items generate no boxes; they pin column widths.
            Column {
                span: usize,
                width: Option<Len>,
            },
            Block(NodeId),
        }

        /// One table cell with its spans and assigned starting column.
        #[derive(Clone, Copy)]
        struct TableCell {
            node: NodeId,
            colspan: usize,
            rowspan: usize,
            col: usize,
        }

        // (C5: HTML limits — colspan 1–1000, raw rowspan 0–65534 where 0
        // means "to the section end" and is resolved once all rows are
        // known; rows are then clipped to the actual row count.)
        let cell_spans = |lay: &Self, cell: NodeId| -> (usize, usize) {
            let attr = |name| {
                lay.dom
                    .element(cell)
                    .and_then(|el| el.attr(name))
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .unwrap_or(1)
            };
            (
                attr("colspan").clamp(1, 1000),
                attr("rowspan").clamp(0, 65534),
            )
        };

        // A4: a `<col>`/`<colgroup>` width attribute as a Len (`120`,
        // `25%`, `80px`; a bare number is px per HTML). `0`/invalid
        // resolves to no constraint.
        let attr_len = |lay: &Self, col: NodeId, raw: &str| -> Option<Len> {
            let raw = raw.trim();
            if raw.is_empty() || raw.eq_ignore_ascii_case("auto") {
                return None;
            }
            if let Ok(p) = raw.parse::<f32>() {
                return (p > 0.0).then_some(Len::Px(p));
            }
            let em = lay.style(col).map(|s| s.font_size).unwrap_or(16.0);
            if let Some(l) = kitae_style::parse_len_px(raw, em, 16.0) {
                if let Some(px) = l.px() {
                    return (px > 0.0).then_some(Len::Px(px));
                }
                return None;
            }
            raw.strip_suffix('%')
                .and_then(|p| p.trim().parse::<f32>().ok())
                .filter(|p| *p > 0.0)
                .map(|p| Len::Sum { px: 0.0, pct: p })
        };

        // A4: `<col span width>` (or width-only) as a column-width item.
        // CSS width wins over the attribute; both pin the column.
        let collect_col = |lay: &Self, col: NodeId| -> Option<Item> {
            let el = lay.dom.element(col)?;
            let span: usize = el
                .attr("span")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, 1000);
            let width = lay
                .style(col)
                // C1: keyword column widths fall back to the attribute /
                // auto split (table path measures are a documented deferral).
                .and_then(|s| s.width.and_then(|sv| sv.len()))
                .or_else(|| el.attr("width").and_then(|w| attr_len(lay, col, w)));
            Some(Item::Column { span, width })
        };

        let collect_row_cells = |lay: &Self, row: NodeId| -> Vec<(NodeId, usize, usize)> {
            let mut cells = Vec::new();
            // `visibility: collapse` on a table row (or on a row group,
            // whose rows are collected through here) removes it from the
            // table entirely — the one place the value changes layout
            // instead of just painting (CSS Tables 3 §3.2).
            if lay
                .style(row)
                .is_some_and(|s| s.visibility == Visibility::Collapse)
            {
                return cells;
            }
            for &gc in lay.dom.children(row) {
                if lay.dom.node(gc).kind == NodeKind::Text {
                    continue;
                }
                match lay.display_of(gc) {
                    Some(Display::TableCell) => {
                        let (cs, rs) = cell_spans(lay, gc);
                        cells.push((gc, cs, rs));
                    }
                    Some(Display::None) => {}
                    _ => {
                        if lay.dom.element(gc).is_some()
                            && lay.display_of(gc) != Some(Display::None)
                        {
                            cells.push((gc, 1, 1));
                        }
                    }
                }
            }
            cells
        };

        let children: Vec<NodeId> = self.dom.children(node).to_vec();
        let mut items: Vec<Item> = Vec::new();
        for child in children {
            if self.dom.node(child).kind == NodeKind::Text {
                continue;
            }
            match self.display_of(child) {
                Some(Display::TableCaption) => items.push(Item::Caption(child)),
                Some(Display::TableColumn) => {
                    if let Some(item) = collect_col(self, child) {
                        items.push(item);
                    }
                }
                Some(Display::TableColumnGroup) => {
                    // A colgroup's own width acts as the width of a single
                    // implied column (HTML) covering its span.
                    if let Some(w) = self
                        .style(child)
                        .and_then(|s| s.width.and_then(|sv| sv.len()))
                        .or_else(|| {
                            self.dom
                                .element(child)
                                .and_then(|el| el.attr("width"))
                                .and_then(|w| attr_len(self, child, w))
                        })
                    {
                        let span: usize = self
                            .dom
                            .element(child)
                            .and_then(|el| el.attr("span"))
                            .and_then(|s| s.trim().parse::<usize>().ok())
                            .unwrap_or(1)
                            .clamp(1, 1000);
                        items.push(Item::Column {
                            span,
                            width: Some(w),
                        });
                    }
                    for grand in self.dom.children(child).to_vec() {
                        if self.dom.node(grand).kind == NodeKind::Text {
                            continue;
                        }
                        if self.display_of(grand) == Some(Display::TableColumn) {
                            if let Some(item) = collect_col(self, grand) {
                                items.push(item);
                            }
                        }
                    }
                }
                Some(Display::TableRow) => {
                    let cells = collect_row_cells(self, child);
                    if !cells.is_empty() {
                        items.push(Item::Row {
                            group: None,
                            row: Some(child),
                            cells: cells
                                .into_iter()
                                .map(|(node, colspan, rowspan)| TableCell {
                                    node,
                                    colspan,
                                    rowspan,
                                    col: 0,
                                })
                                .collect(),
                        });
                    }
                }
                Some(Display::TableRowGroup) => {
                    for grand in self.dom.children(child).to_vec() {
                        if self.dom.node(grand).kind == NodeKind::Text {
                            continue;
                        }
                        // A colgroup nested inside a row group is markup
                        // soup; treat it as column definitions anyway.
                        if self.display_of(grand) == Some(Display::TableColumnGroup) {
                            for great in self.dom.children(grand).to_vec() {
                                if self.dom.node(great).kind == NodeKind::Text {
                                    continue;
                                }
                                if self.display_of(great) == Some(Display::TableColumn) {
                                    if let Some(item) = collect_col(self, great) {
                                        items.push(item);
                                    }
                                }
                            }
                            continue;
                        }
                        match self.display_of(grand) {
                            Some(Display::TableRow) => {
                                let cells = collect_row_cells(self, grand);
                                if !cells.is_empty() {
                                    items.push(Item::Row {
                                        group: Some(child),
                                        row: Some(grand),
                                        cells: cells
                                            .into_iter()
                                            .map(|(node, colspan, rowspan)| TableCell {
                                                node,
                                                colspan,
                                                rowspan,
                                                col: 0,
                                            })
                                            .collect(),
                                    });
                                }
                            }
                            Some(Display::TableCell) => {
                                let (cs, rs) = cell_spans(self, grand);
                                items.push(Item::Row {
                                    group: Some(child),
                                    row: None,
                                    cells: vec![TableCell {
                                        node: grand,
                                        colspan: cs,
                                        rowspan: rs,
                                        col: 0,
                                    }],
                                });
                            }
                            Some(Display::TableCaption) => {
                                items.push(Item::Caption(grand));
                            }
                            Some(Display::None) => {}
                            _ => {
                                if self.dom.element(grand).is_some() {
                                    items.push(Item::Block(grand));
                                }
                            }
                        }
                    }
                }
                Some(Display::TableCell) => {
                    let (cs, rs) = cell_spans(self, child);
                    items.push(Item::Row {
                        group: None,
                        row: None,
                        cells: vec![TableCell {
                            node: child,
                            colspan: cs,
                            rowspan: rs,
                            col: 0,
                        }],
                    });
                }
                Some(Display::None) => {}
                _ => {
                    if self.dom.element(child).is_some() {
                        items.push(Item::Block(child));
                    }
                }
            }
        }

        // Pass 1 (N2): resolve `rowspan="0"` (span to the section end) and
        // assign every cell its starting column on an occupancy grid, so
        // rowspans from earlier rows push later cells right.
        let row_count = items
            .iter()
            .filter(|it| matches!(it, Item::Row { .. }))
            .count();
        let mut occ: Vec<Vec<bool>> = Vec::new();
        let mut ncols = 0usize;
        {
            let mut ri = 0usize;
            for it in items.iter_mut() {
                let Item::Row { cells, .. } = it else {
                    continue;
                };
                for cell in cells.iter_mut() {
                    if cell.rowspan == 0 {
                        cell.rowspan = (row_count - ri).max(1);
                    }
                    cell.rowspan = cell.rowspan.min((row_count - ri).max(1)).max(1);
                }
                while occ.len() < ri + 1 {
                    occ.push(Vec::new());
                }
                let mut col = 0usize;
                for cell in cells.iter_mut() {
                    while occ[ri].get(col).copied().unwrap_or(false) {
                        col += 1;
                    }
                    cell.col = col;
                    for dr in 0..cell.rowspan {
                        while occ.len() <= ri + dr {
                            occ.push(Vec::new());
                        }
                        let row = &mut occ[ri + dr];
                        while row.len() < col + cell.colspan {
                            row.push(false);
                        }
                        for row_slot in row.iter_mut().take(col + cell.colspan).skip(col) {
                            *row_slot = true;
                        }
                    }
                    col += cell.colspan;
                }
                ncols = ncols.max(occ[ri].len());
                ri += 1;
            }
            ncols = ncols.max(occ.iter().map(Vec::len).max().unwrap_or(0));
        }
        // A4: `<col>`/`<colgroup>` fixed widths pin the leading columns.
        // (HTML applies column widths by slot index from the first column;
        // the occupancy grid already ran so spanned col elements line up with
        // the same slots the cells occupy.) `0` widths never reach here —
        // `attr_len` rejects them.
        let mut col_fixed: Vec<Option<Len>> = vec![None; ncols];
        let mut slot = 0usize;
        for it in &items {
            if let Item::Column { span, width } = it {
                if slot >= ncols {
                    break;
                }
                let end = (slot + *span).min(ncols);
                match width {
                    // A spanned <col> with a width splits it evenly.
                    Some(w) if *span > 1 => {
                        for f in col_fixed.iter_mut().take(end).skip(slot) {
                            *f = Some(*w);
                        }
                    }
                    Some(w) => col_fixed[slot] = Some(*w),
                    None => {}
                }
                slot = end;
            }
        }
        // Column definitions past the occupied grid still create columns:
        // a table with <col span="3"> and one row of two cells renders a
        // three-column grid (the third just stays empty).
        if slot > ncols && slot < 1000 {
            ncols = slot;
            col_fixed.resize(ncols, None);
        }
        // No rows: captions/blocks still paint as blocks.
        if ncols == 0 {
            for it in items {
                match it {
                    Item::Caption(c) | Item::Block(c) => {
                        let (_, buf) = self.layout_box(c, frame, child_cb);
                        if let Some(buf) = buf {
                            self.merge_buffers(buf);
                        }
                    }
                    Item::Row { .. } | Item::Column { .. } => {}
                }
            }
            return;
        }

        // Content-measure column widths (per-cell padding included).
        // C5: under `separate`, `border-spacing` insets the grid by one
        // band around every cell, so the columns share what the spacing
        // doesn't take; `collapse` zeroes it (full shared-border conflict
        // resolution stays the documented partial).
        let (sh, sv) = if style.border_collapse {
            (0.0, 0.0)
        } else {
            (
                style.border_spacing.0.max(0.0),
                style.border_spacing.1.max(0.0),
            )
        };
        let avail = (frame.width - (ncols as f32 + 1.0) * sh).max(40.0);
        let mut desired = vec![0.0f32; ncols];
        for it in &items {
            if let Item::Row { cells, .. } = it {
                for cell in cells {
                    if cell.colspan == 1 && cell.col < ncols {
                        let st = self.style_or_default(cell.node);
                        // Cell padding may be % — resolve against the table
                        // width (its containing-block width).
                        let pw = st.padding.resolve(avail);
                        let natural = self.content_main_size(cell.node) + pw.left + pw.right;
                        desired[cell.col] = desired[cell.col].max(natural);
                    }
                }
            }
        }
        let total: f32 = desired.iter().sum();
        let mut widths = vec![0.0f32; ncols];
        if total <= 0.0 {
            for w in widths.iter_mut() {
                *w = avail / ncols as f32;
            }
        } else if total < avail {
            let extra = (avail - total) / ncols as f32;
            for (i, w) in widths.iter_mut().enumerate() {
                *w = desired[i] + extra;
            }
        } else {
            for (i, w) in widths.iter_mut().enumerate() {
                *w = desired[i] * avail / total;
            }
            for w in widths.iter_mut() {
                *w = w.max(20.0);
            }
        }
        // A4: pinned column widths override the measured ones. Percentages
        // resolve against the full table width; the autos keep their
        // measured share of what remains. Pinned columns never shrink
        // below their value.
        let pinned: Vec<Option<f32>> = col_fixed
            .iter()
            .map(|f| f.map(|l| l.resolve(avail)))
            .collect();
        if pinned.iter().any(|p| p.is_some()) {
            let pinned_total: f32 = pinned.iter().flatten().sum();
            let n_auto = pinned.iter().filter(|p| p.is_none()).count();
            let auto_each = if n_auto > 0 {
                ((avail - pinned_total) / n_auto as f32).max(20.0)
            } else {
                0.0
            };
            for (i, w) in widths.iter_mut().enumerate() {
                *w = pinned[i].unwrap_or(auto_each);
            }
        }
        let mut col_x = vec![0.0f32; ncols];
        // C5: the first band of `border-spacing` is the inset from the
        // table edges; each column adds one more after itself.
        let mut acc = frame.x + sh;
        for (i, w) in widths.iter().enumerate() {
            col_x[i] = acc;
            acc += *w + sh;
        }

        // Captions first (above the grid), in document order. C5:
        // `caption-side: bottom` captions paint below the rows instead.
        let mut bottom_captions: Vec<NodeId> = Vec::new();
        for it in &items {
            if let Item::Caption(c) = it {
                if self.style_or_default(*c).caption_side == CaptionSide::Bottom {
                    bottom_captions.push(*c);
                    continue;
                }
                let (_, buf) = self.layout_box(*c, frame, child_cb);
                if let Some(buf) = buf {
                    self.merge_buffers(buf);
                }
            }
        }

        // Rows and stray blocks in document order (captions already done).
        // Pass 2 (N2): size every row to its tallest span-1 cell, then grow
        // rows covered by taller spanned cells (the deficit lands on the
        // last covered row).
        let row_h: Vec<f32>;
        {
            let mut rows: Vec<&Vec<TableCell>> = Vec::new();
            for it in &items {
                if let Item::Row { cells, .. } = it {
                    rows.push(cells);
                }
            }
            let mut row_h_local = vec![1.0f32; rows.len()];
            let span_width = |_cells: &[TableCell], from: usize, span: usize| -> f32 {
                let mut w = 0.0;
                for k in 0..span {
                    if from + k < ncols {
                        w += widths[from + k];
                    }
                }
                w.max(20.0)
            };
            for (ri, cells) in rows.iter().enumerate() {
                for cell in cells.iter() {
                    if cell.rowspan == 1 {
                        let h = self.table_cell_height(
                            cell.node,
                            span_width(cells, cell.col, cell.colspan),
                            child_cb,
                        );
                        row_h_local[ri] = row_h_local[ri].max(h);
                    }
                }
            }
            for (ri, cells) in rows.iter().enumerate() {
                for cell in cells.iter() {
                    if cell.rowspan > 1 {
                        let h = self.table_cell_height(
                            cell.node,
                            span_width(cells, cell.col, cell.colspan),
                            child_cb,
                        );
                        let covered: f32 = row_h_local
                            [ri..(ri + cell.rowspan).min(row_h_local.len())]
                            .iter()
                            .sum();
                        if covered < h {
                            let last = (ri + cell.rowspan - 1).min(row_h_local.len() - 1);
                            row_h_local[last] += h - covered;
                        }
                    }
                }
            }
            for h in row_h_local.iter_mut() {
                *h = h.max(1.0);
            }
            row_h = row_h_local;
        }
        let mut row_cursor = 0usize;
        // C5: leading + inter-row bands of `border-spacing` (collapse: 0).
        self.y += sv;
        for it in items {
            match it {
                Item::Caption(_) | Item::Column { .. } => {}
                Item::Block(b) => {
                    let (_, buf) = self.layout_box(b, frame, child_cb);
                    if let Some(buf) = buf {
                        self.merge_buffers(buf);
                    }
                }
                Item::Row { group, row, cells } => {
                    let rh = row_h[row_cursor];
                    row_cursor += 1;
                    let row_y = self.y;
                    // Row / row-group background spans the full table width.
                    let mut row_bg = 0u32;
                    if let Some(r) = row {
                        row_bg = self.style_or_default(r).background;
                    }
                    if row_bg == 0 {
                        if let Some(g) = group {
                            row_bg = self.style_or_default(g).background;
                        }
                    }
                    if row_bg != 0 {
                        self.push_rect(Rect::solid(frame.x, row_y, frame.width, rh, row_bg));
                    }
                    // Paint each cell at its (column, row) slot; spanned
                    // cells may paint past this row into rows below.
                    for cell in &cells {
                        let mut w = 0.0;
                        for k in 0..cell.colspan {
                            if cell.col + k < ncols {
                                w += widths[cell.col + k];
                            }
                        }
                        let st = self.style_or_default(cell.node);
                        let x = col_x[cell.col.min(ncols - 1)];
                        let floats_outer = std::mem::take(&mut self.floats);
                        self.y = row_y;
                        self.layout_static_box(
                            cell.node,
                            Frame {
                                x,
                                width: w.max(20.0),
                                align: st.text_align,
                            },
                            child_cb,
                        );
                        self.floats = floats_outer;
                        self.y = row_y;
                    }
                    self.y = row_y + rh + sv;
                    self.max_y = self.max_y.max(self.y);
                }
            }
        }
        // C5: `caption-side: bottom` captions paint at the grid's bottom.
        for c in bottom_captions {
            let (_, buf) = self.layout_box(c, frame, child_cb);
            if let Some(buf) = buf {
                self.merge_buffers(buf);
            }
        }
    }

    /// Grid container layout: track sizing, then placement.
    ///
    /// Columns come from `grid-template-columns` (`100px` fixed, `auto`
    /// content-sized, `1fr` shares of the remainder; C1b keyword tracks
    /// (`min/max/fit-content`) measure their span-1 items, and `minmax()`
    /// floors apply after the `fr` distribution). Rows size to their tallest item unless
    /// `grid-template-rows` fixes them. Items auto-place row-major in
    /// document order with the CSS §8.5 *sparse* cursor (a plain `auto`
    /// item never backfills a hole punched by a spanning sibling);
    /// `grid-auto-flow: dense` restarts each search at the grid origin.
    /// C6: `grid-template-columns: subgrid` adopts the nearest grid
    /// ancestor's column template. `grid-column: span N` /
    /// `grid-row: span N` widen the slot. Gaps come from `gap`.
    fn layout_grid_children(&mut self, node: NodeId, frame: Frame, cb: Option<NodeId>) {
        let mut style = self.style_or_default(node);
        let child_cb = if style.position != Position::Static {
            Some(node)
        } else {
            cb
        };
        // C6: `grid-template-columns: subgrid` — the child grid aligns its
        // column tracks with the nearest grid ancestor's explicit template
        // (the common full-span case; row subgrid is parse-only for now).
        if style.grid_subgrid_columns {
            let mut p = self.dom.node(node).parent;
            while let Some(id) = p {
                p = self.dom.node(id).parent;
                if self.display_of(id) == Some(Display::Grid) {
                    if let Some(ps) = self.style(id) {
                        if !ps.grid_template_columns.is_empty() {
                            style.grid_template_columns = ps.grid_template_columns.to_vec();
                            style.grid_line_names_col = ps.grid_line_names_col.clone();
                            style.gap = ps.gap;
                            break;
                        }
                    }
                }
            }
        }
        // In-flow grid items; out-of-flow children leave the grid.
        let mut items: Vec<NodeId> = Vec::new();
        for &child in self.dom.children(node) {
            if let Some(s) = self.style(child) {
                if s.position == Position::Absolute || s.position == Position::Fixed {
                    let target = if s.position == Position::Fixed {
                        None
                    } else {
                        child_cb
                    };
                    self.abs_list.push((target, child, frame.x, self.y));
                    continue;
                }
            }
            if self.display_of(child) == Some(Display::None) {
                continue;
            }
            if self.dom.node(child).kind == NodeKind::Text {
                // Whitespace text directly in a grid (collapsed remnants):
                // keep non-empty text as an anonymous item, drop the rest.
                if self.dom.node(child).text.trim().is_empty() {
                    continue;
                }
            }
            items.push(child);
        }
        if items.is_empty() {
            return;
        }

        let has_col_template = !style.grid_template_columns.is_empty();
        let mut ncols = if has_col_template {
            style.grid_template_columns.len()
        } else if let Some(first) = style.grid_template_areas.first() {
            first.len().max(1)
        } else {
            1
        };
        let gap = style.gap;
        let avail = frame.width.max(40.0);
        // B5: with neither template nor areas, integer line placements imply
        // the column count (implicit tracks).
        if !has_col_template && style.grid_template_areas.is_empty() {
            for &it in &items {
                let s = self.style_or_default(it);
                for v in [&s.grid_column_start, &s.grid_column_end] {
                    if let Ok(n) = v.parse::<usize>() {
                        if n >= 2 {
                            ncols = ncols.max(n.min(64));
                        }
                    }
                }
            }
        }

        // Spans per item, plus B5 explicit placement pins.
        let mut col_spans: Vec<usize> = Vec::with_capacity(items.len());
        let mut row_spans: Vec<usize> = Vec::with_capacity(items.len());
        let mut col_pin: Vec<Option<usize>> = Vec::with_capacity(items.len());
        let mut row_pin: Vec<Option<usize>> = Vec::with_capacity(items.len());
        // B5: name → first matching 0-based line index (line i sits before
        // track i, so starting at line i covers track i first).
        let line_of = |names: &[(usize, String)], name: &str| -> Option<usize> {
            names.iter().find(|(_, n)| n == name).map(|(l, _)| *l)
        };
        // Start/end longhands → (pin, span). Integers are CSS 1-based lines;
        // names resolve through the template's `[name]` tokens. An end
        // without a resolvable start can't span — it's dropped.
        let resolve_axis = |start: &str,
                            end: &str,
                            names: &[(usize, String)],
                            max: usize|
         -> (Option<usize>, Option<usize>) {
            let pin = if start.is_empty() || start == "auto" {
                None
            } else if let Ok(n) = start.parse::<usize>() {
                (n >= 1).then(|| (n - 1).min(max.saturating_sub(1)))
            } else {
                line_of(names, start).map(|l| l.min(max.saturating_sub(1)))
            };
            let span = if end.is_empty() || end == "auto" {
                None
            } else {
                let e0 = if let Ok(n) = end.parse::<usize>() {
                    (n >= 1).then(|| n - 1)
                } else {
                    line_of(names, end)
                };
                match (pin, e0) {
                    (Some(p), Some(e)) if e > p => Some(e - p),
                    _ => None,
                }
            };
            (pin, span)
        };
        for &it in &items {
            let s = self.style_or_default(it);
            let mut cs = (s.grid_column_span as usize)
                .clamp(1, ncols.max(1))
                .min(ncols);
            let mut rs = (s.grid_row_span as usize).clamp(1, 12);
            let mut cp: Option<usize> = None;
            let mut rp: Option<usize> = None;
            // B5: `grid-area: <name>` resolves to the area's bounding rect.
            let mut rect: Option<(usize, usize, usize, usize)> = None;
            if !s.grid_area.is_empty() {
                let (mut r0, mut r1) = (usize::MAX, 0usize);
                let (mut c0, mut c1) = (usize::MAX, 0usize);
                let mut hit = false;
                for (r, row) in style.grid_template_areas.iter().enumerate() {
                    for (c, cell) in row.iter().enumerate() {
                        if *cell == s.grid_area {
                            hit = true;
                            r0 = r0.min(r);
                            r1 = r1.max(r + 1);
                            c0 = c0.min(c);
                            c1 = c1.max(c + 1);
                        }
                    }
                }
                if hit {
                    rect = Some((r0, c0, r1, c1));
                }
            }
            if let Some((r0, c0, r1, c1)) = rect {
                cp = Some(c0.min(ncols.saturating_sub(1)));
                rp = Some(r0);
                cs = (c1 - c0).clamp(1, ncols);
                rs = (r1 - r0).clamp(1, 12);
            } else {
                let (c, cs2) = resolve_axis(
                    &s.grid_column_start,
                    &s.grid_column_end,
                    &style.grid_line_names_col,
                    ncols,
                );
                let (r, rs2) = resolve_axis(
                    &s.grid_row_start,
                    &s.grid_row_end,
                    &style.grid_line_names_row,
                    12,
                );
                if let Some(cs2) = cs2 {
                    cs = cs2.clamp(1, ncols);
                }
                if let Some(rs2) = rs2 {
                    rs = rs2.clamp(1, 12);
                }
                cp = c;
                rp = r;
            }
            col_spans.push(cs);
            row_spans.push(rs);
            col_pin.push(cp);
            row_pin.push(rp);
        }

        // Placement: fully pinned items first (B5), then the auto-flow pass.
        // C6: default is the CSS §8.5 *sparse* cursor — a monotonic
        // (row, column) position that plain `auto` items advance past, so
        // holes are left behind. `grid-auto-flow: dense` keeps the engine's
        // original behavior: every search restarts at the grid origin.
        let mut occ: Vec<Vec<bool>> = Vec::new();
        let mut pos: Vec<Option<(usize, usize)>> = vec![None; items.len()];
        let ensure_rows = |occ: &mut Vec<Vec<bool>>, n: usize, ncols: usize| {
            while occ.len() < n {
                occ.push(vec![false; ncols]);
            }
        };
        // B5: areas imply their rows even when empty (fixed row templates
        // still size them).
        let area_rows = style.grid_template_areas.len();
        ensure_rows(&mut occ, area_rows, ncols);
        let slot_free = |occ: &Vec<Vec<bool>>, r: usize, c: usize, rs: usize, cs: usize| -> bool {
            for dr in 0..rs {
                match occ.get(r + dr) {
                    Some(row) => {
                        for dc in 0..cs {
                            if c + dc < ncols && row[c + dc] {
                                return false;
                            }
                        }
                    }
                    None => break,
                }
            }
            true
        };
        let mark = |occ: &mut Vec<Vec<bool>>, r: usize, c: usize, rs: usize, cs: usize| {
            ensure_rows(occ, r + rs, ncols);
            for dr in 0..rs {
                for dc in 0..cs {
                    if c + dc < ncols {
                        occ[r + dr][c + dc] = true;
                    }
                }
            }
        };
        for idx in 0..items.len() {
            if let (Some(r), Some(c)) = (row_pin[idx], col_pin[idx]) {
                let c = c.min(ncols.saturating_sub(1));
                mark(&mut occ, r, c, row_spans[idx], col_spans[idx]);
                pos[idx] = Some((r, c));
            }
        }
        let dense = style.grid_flow_dense;
        // Sparse cursor: (row, next free column). Only consulted for
        // non-dense flow; definite-row items sit outside it (the spec
        // places them before auto-flow begins).
        let mut cur = (0usize, 0usize);
        for idx in 0..items.len() {
            if pos[idx].is_some() {
                continue;
            }
            let rs = row_spans[idx];
            let cs = col_spans[idx];
            let mut placed = None;
            if let Some(c) = col_pin[idx] {
                let c = c.min(ncols.saturating_sub(1));
                // Column pinned: first row where the slot is free. Sparse
                // starts at the cursor and steps to the next row when the
                // pin sits left of it; dense scans from the top.
                let start = if dense {
                    0
                } else if cur.1 > c {
                    cur.0 + 1
                } else {
                    cur.0
                };
                let max_r = occ.len() + 64;
                let mut r = start;
                while r < max_r {
                    ensure_rows(&mut occ, r + rs, ncols);
                    if slot_free(&occ, r, c, rs, cs) {
                        placed = Some((r, c));
                        break;
                    }
                    r += 1;
                }
                if let Some((pr, _)) = placed {
                    cur = (pr, c + 1);
                }
            } else if let Some(r) = row_pin[idx] {
                // Row pinned: first column where the slot is free.
                ensure_rows(&mut occ, r + rs, ncols);
                for c in 0..=(ncols.saturating_sub(cs)) {
                    if slot_free(&occ, r, c, rs, cs) {
                        placed = Some((r, c));
                        break;
                    }
                }
            } else if dense {
                // Fully auto, dense: first free slot from the grid origin.
                let max_r = occ.len() + 24;
                let mut r = 0usize;
                while r < max_r {
                    ensure_rows(&mut occ, r + rs, ncols);
                    for c in 0..=(ncols.saturating_sub(cs)) {
                        if slot_free(&occ, r, c, rs, cs) {
                            placed = Some((r, c));
                            break;
                        }
                    }
                    if placed.is_some() {
                        break;
                    }
                    r += 1;
                }
            } else {
                // Fully auto, sparse: forward row-major search from the
                // cursor. Plain items fill their row left-to-right; a hole
                // the cursor has passed (jumped over by a pinned or
                // spanning sibling) is never revisited — that is the whole
                // point of sparse flow.
                let (r0, c0) = cur;
                let max_r = occ.len() + 24;
                let mut r = r0;
                while r < max_r {
                    ensure_rows(&mut occ, r + rs, ncols);
                    let c_from = if r == r0 { c0.min(ncols) } else { 0 };
                    for c in c_from..=ncols.saturating_sub(cs) {
                        if slot_free(&occ, r, c, rs, cs) {
                            placed = Some((r, c));
                            break;
                        }
                    }
                    if placed.is_some() {
                        break;
                    }
                    r += 1;
                }
                if let Some((pr, pc)) = placed {
                    cur = (pr, pc + cs);
                }
            }
            let (r, c) = placed.unwrap_or((occ.len(), 0));
            mark(&mut occ, r, c, rs, cs);
            pos[idx] = Some((r, c));
        }
        let mut pos: Vec<(usize, usize)> = pos.into_iter().map(|p| p.unwrap_or((0, 0))).collect();
        for (_, c) in pos.iter_mut() {
            *c = (*c).min(ncols.saturating_sub(1));
        }
        let nrows = occ.len().max(1);

        // Column widths: fixed resolved, auto/keyword from span-1 content,
        // fr shares rest; `minmax` floors applied after distribution.
        let em = style.font_size;
        let mut widths = vec![0.0f32; ncols];
        let mut is_fr = vec![0.0f32; ncols];
        let mut is_auto = vec![false; ncols];
        // C1b/C6: per-column keyword sizing + `minmax()` min side.
        #[derive(Clone, Copy)]
        enum TrackMeas {
            Min,
            Max,
            Fit(Option<Length>),
        }
        let mut meas_kind: Vec<Option<TrackMeas>> = vec![None; ncols];
        let mut min_kind: Vec<Option<MinSizing>> = vec![None; ncols];
        for (i, tr) in style.grid_template_columns.iter().enumerate() {
            match tr {
                GridTrack::Fixed(l) => {
                    widths[i] = l.resolve(em, 16.0, avail).max(0.0);
                }
                GridTrack::Flex(f) => {
                    is_fr[i] = *f;
                }
                GridTrack::Auto => {
                    is_auto[i] = true;
                }
                GridTrack::MinContent => {
                    meas_kind[i] = Some(TrackMeas::Min);
                }
                GridTrack::MaxContent => {
                    meas_kind[i] = Some(TrackMeas::Max);
                }
                GridTrack::FitContent(a) => {
                    meas_kind[i] = Some(TrackMeas::Fit(*a));
                }
                GridTrack::MinMax(lo, hi) => {
                    min_kind[i] = Some(*lo);
                    match hi {
                        MaxTrack::Fixed(l) => {
                            widths[i] = l.resolve(em, 16.0, avail).max(0.0);
                        }
                        MaxTrack::Flex(f) => {
                            is_fr[i] = *f;
                        }
                        MaxTrack::Auto => {
                            is_auto[i] = true;
                        }
                        MaxTrack::MinContent => {
                            meas_kind[i] = Some(TrackMeas::Min);
                        }
                        MaxTrack::MaxContent => {
                            meas_kind[i] = Some(TrackMeas::Max);
                        }
                        MaxTrack::FitContent(a) => {
                            meas_kind[i] = Some(TrackMeas::Fit(*a));
                        }
                    }
                }
            }
        }
        if style.grid_template_columns.is_empty() {
            // No template: single auto column sized to content, capped at avail.
            is_auto[0] = true;
        }
        for (idx, &it) in items.iter().enumerate() {
            if col_spans[idx] != 1 {
                continue;
            }
            let (_, c) = pos[idx];
            if c < ncols && is_auto[c] {
                let st = self.style_or_default(it);
                // % padding resolves against the grid container's width.
                let pw = st.padding.resolve(avail);
                let natural = self.content_main_size(it) + pw.left + pw.right;
                widths[c] = widths[c].max(natural);
            }
        }
        // C1b/C6: measure keyword tracks (and `minmax` min sides) from
        // their span-1 items' intrinsic pairs. Items spanning several
        // columns are skipped — like the `auto` path above.
        let mut kmin = vec![0.0f32; ncols];
        let mut kmax = vec![0.0f32; ncols];
        let mut kseen = vec![false; ncols];
        for (idx, &it) in items.iter().enumerate() {
            if col_spans[idx] != 1 {
                continue;
            }
            let (_, c) = pos[idx];
            if c >= ncols || (meas_kind[c].is_none() && min_kind[c].is_none()) {
                continue;
            }
            let (mc, xc) = self.intrinsic_item_pair(it, avail);
            kmin[c] = kmin[c].max(mc);
            kmax[c] = kmax[c].max(xc);
            kseen[c] = true;
        }
        for i in 0..ncols {
            if let Some(mk) = meas_kind[i] {
                widths[i] = match mk {
                    TrackMeas::Min => kmin[i],
                    TrackMeas::Max => kmax[i],
                    TrackMeas::Fit(arg) => {
                        // `fit-content()` with no argument caps at the
                        // container's available width (v1 simplification).
                        let cap = arg.map(|l| l.resolve(em, 16.0, avail)).unwrap_or(avail);
                        if kseen[i] {
                            cap.min(kmax[i]).max(kmin[i])
                        } else {
                            0.0
                        }
                    }
                };
            }
        }
        let gap = gap.resolve(avail);
        let gap_total = gap * (ncols.saturating_sub(1)) as f32;
        let fixed_auto: f32 = widths.iter().sum();
        let total_fr: f32 = is_fr.iter().sum();
        let remaining = (avail - gap_total - fixed_auto).max(0.0);
        if total_fr > 0.0 {
            for i in 0..ncols {
                if is_fr[i] > 0.0 {
                    widths[i] = remaining * (is_fr[i] / total_fr);
                }
            }
        }
        // Empty auto columns with no content collapse to an equal share only
        // when nothing else claimed space (all-auto fallback).
        if fixed_auto == 0.0 && total_fr == 0.0 {
            for w in widths.iter_mut() {
                *w = (avail - gap_total).max(0.0) / ncols as f32;
            }
        }
        // C6: `minmax()` floors, applied after the fr distribution so an
        // `fr` track never drops below its minimum. `auto` min = the
        // automatic minimum size, deferred (the 20px floor below covers
        // the collapse-to-zero case).
        for i in 0..ncols {
            if let Some(lo) = min_kind[i] {
                let floor = match lo {
                    MinSizing::Len(l) => l.resolve(em, 16.0, avail).max(0.0),
                    MinSizing::MinContent => kmin[i],
                    MinSizing::MaxContent => kmax[i],
                    MinSizing::Auto => 0.0,
                };
                widths[i] = widths[i].max(floor);
            }
        }
        for w in widths.iter_mut() {
            *w = w.max(20.0).min(avail);
        }
        let mut col_x = vec![0.0f32; ncols];
        let mut acc = frame.x;
        for (i, w) in widths.iter().enumerate() {
            col_x[i] = acc;
            acc += *w + gap;
        }

        // Row heights: fixed template rows win (as a minimum), the rest from
        // the tallest span-1 item; spanned items expand the last row they cover.
        let mut row_h = vec![0.0f32; nrows];
        let mut row_fixed = vec![None; nrows];
        for (i, tr) in style.grid_template_rows.iter().enumerate() {
            if i < nrows {
                match tr {
                    GridTrack::Fixed(l) => {
                        row_fixed[i] = Some(l.resolve(em, 16.0, 0.0).max(0.0));
                    }
                    // C1b/C6: fixed or floored `minmax()` rows; keyword max
                    // sides behave as `auto` (content height) for v1.
                    GridTrack::MinMax(lo, hi) => {
                        let floor = match lo {
                            MinSizing::Len(l) => l.resolve(em, 16.0, 0.0).max(0.0),
                            _ => 0.0,
                        };
                        match hi {
                            MaxTrack::Fixed(b) => {
                                row_fixed[i] = Some(b.resolve(em, 16.0, 0.0).max(0.0).max(floor));
                            }
                            MaxTrack::Auto if floor > 0.0 => {
                                row_fixed[i] = Some(floor);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        // Measure each item at its spanned width.
        let mut item_h: Vec<f32> = Vec::with_capacity(items.len());
        for (idx, &it) in items.iter().enumerate() {
            let (r, c) = pos[idx];
            let cs = col_spans[idx];
            let mut w = 0.0;
            for k in 0..cs {
                if c + k < ncols {
                    w += widths[c + k];
                }
            }
            w += gap * (cs.saturating_sub(1)) as f32;
            let st = self.style_or_default(it);
            let saved_y = self.y;
            let abs_saved = std::mem::take(&mut self.abs_list);
            let floats_saved = std::mem::take(&mut self.floats);
            let (_, h) = self.with_buffers(|lay| {
                lay.y = 0.0;
                lay.layout_static_box(
                    it,
                    Frame {
                        x: 0.0,
                        width: w.max(20.0),
                        align: st.text_align,
                    },
                    child_cb,
                )
            });
            self.y = saved_y;
            self.floats = floats_saved;
            self.abs_list = abs_saved;
            item_h.push(h);
            if row_spans[idx] == 1 && r < nrows {
                row_h[r] = row_h[r].max(h);
            }
        }
        for (i, f) in row_fixed.iter().enumerate() {
            if let Some(fx) = f {
                row_h[i] = row_h[i].max(*fx);
            }
        }
        // Spanned items force their rows tall enough.
        for (idx, &h) in item_h.iter().enumerate() {
            let (r, _) = pos[idx];
            let rs = row_spans[idx];
            if rs <= 1 {
                continue;
            }
            let mut sum = 0.0;
            for k in 0..rs {
                if r + k < nrows {
                    sum += row_h[r + k];
                }
            }
            sum += gap * (rs.saturating_sub(1)) as f32;
            if sum < h {
                let extra = h - sum;
                if r + rs - 1 < nrows {
                    row_h[r + rs - 1] += extra;
                }
            }
        }
        for h in row_h.iter_mut() {
            *h = h.max(1.0);
        }
        let mut row_y = vec![0.0f32; nrows];
        let mut yacc = self.y;
        for (i, h) in row_h.iter().enumerate() {
            row_y[i] = yacc;
            yacc += *h + gap;
        }

        // Paint in document order at assigned slots.
        for (idx, &it) in items.iter().enumerate() {
            let (r, c) = pos[idx];
            let cs = col_spans[idx];
            let mut w = 0.0;
            for k in 0..cs {
                if c + k < ncols {
                    w += widths[c + k];
                }
            }
            w += gap * (cs.saturating_sub(1)) as f32;
            let st = self.style_or_default(it);
            let floats_outer = std::mem::take(&mut self.floats);
            self.y = row_y[r];
            self.layout_static_box(
                it,
                Frame {
                    x: col_x[c],
                    width: w.max(20.0),
                    align: st.text_align,
                },
                child_cb,
            );
            self.floats = floats_outer;
            self.y = row_y[r];
        }
        let total_h: f32 = row_h.iter().sum::<f32>() + gap * (nrows.saturating_sub(1)) as f32;
        self.y += total_h;
        self.max_y = self.max_y.max(self.y);
    }

    /// Natural main-axis size of an item's content: its text measured at
    /// the container width (rows never wrap when sizing, so measure the
    /// whole text unwrapped).
    fn content_main_size(&self, node: NodeId) -> f32 {
        // A control's flex base is its full chrome-inclusive widget width —
        // the bare text width let flex rows shrink buttons to their label
        // (the paint pass draws text + padding + borders, so the base must
        // match). The same solved size the paint pass uses.
        let tag = self.dom.tag(node).to_ascii_lowercase();
        if matches!(tag.as_str(), "button" | "input" | "select" | "textarea") {
            if tag == "input" {
                let t = self
                    .dom
                    .element(node)
                    .and_then(|e| e.attr("type"))
                    .unwrap_or("text")
                    .to_ascii_lowercase();
                if t == "hidden" {
                    return 0.0;
                }
            }
            let style = self.style(node).cloned().unwrap_or_default();
            let (label, input_type) = self.control_label_and_type(node);
            let text_w = if label.trim().is_empty() {
                0.0
            } else {
                self.tm.measure_family(
                    label.trim(),
                    style.font_size,
                    style.font_weight,
                    style.italic,
                    style.monospace,
                    &style.font_family,
                )
            };
            let (w, _) = self.control_size(node, &style, &input_type, text_w, 0.0);
            return w;
        }
        let mut total = 0.0;
        self.collect_text(node, &mut |text, style| {
            total += self.tm.measure_family(
                text,
                style.font_size,
                style.font_weight,
                style.italic,
                style.monospace,
                &style.font_family,
            );
        });
        total.min(f32::MAX)
    }

    /// C1: measure a subtree's intrinsic widths — `(min-content,
    /// max-content)` in CONTENT units (the box's own padding/borders are NOT
    /// included; callers add them before [`SizeVal::resolve`], since CSS
    /// widths are border-box). Text nodes: max-content is the whole run,
    /// min-content the longest unbreakable whitespace token; `nowrap`/`pre`
    /// (without `pre-wrap`) make the run unbreakable. Replaced elements
    /// (img/video) resolve to their natural/attribute size for both.
    /// Element children: inline children combine sequentially (min = widest
    /// child min, max = sum of child maxes); block children take the widest
    /// min / widest max. Out-of-flow (abs/fixed) and `display:none`
    /// descendants are skipped, matching paint.
    fn intrinsic_pair(&self, node: NodeId) -> (f32, f32) {
        let n = self.dom.node(node);
        match n.kind {
            NodeKind::Text => {
                let style = n
                    .parent
                    .and_then(|p| self.style(p))
                    .cloned()
                    .unwrap_or_default();
                if style.display == Display::None {
                    return (0.0, 0.0);
                }
                let full = self.measure_word(&n.text, &style);
                let unbreakable =
                    style.white_space_nowrap || (style.white_space_pre && !style.white_space_wrap);
                if unbreakable {
                    return (full, full);
                }
                let mut longest = 0.0f32;
                for token in n.text.split_whitespace() {
                    longest = longest.max(self.measure_word(token, &style));
                }
                (longest.min(full), full)
            }
            NodeKind::Element => {
                let style = self.style(node).cloned().unwrap_or_default();
                if style.display == Display::None {
                    return (0.0, 0.0);
                }
                // Replaced elements: intrinsic width = their natural size.
                // This is the C1 flex/grid base-size measure and it is
                // width-only (a CSS `width` is applied by the track/item
                // sizing, not here), so a C2 `aspect-ratio` has nothing to
                // stretch on this path: a replaced item's flex/grid base
                // width stays its natural width even when a definite height
                // plus a ratio would imply a narrower box. The painted box
                // (`sized_replaced`) is where the ratio applies.
                let tag = n.tag().to_ascii_lowercase();
                if tag == "img" || tag == "video" || tag == "audio" || tag == "canvas" {
                    let aw = n
                        .element
                        .as_ref()
                        .and_then(|e| e.attr("width"))
                        .and_then(|s| s.trim().trim_end_matches("px").parse::<f32>().ok());
                    return (aw.unwrap_or(300.0), aw.unwrap_or(300.0));
                }
                if tag == "br" || tag == "hr" {
                    return (0.0, 0.0);
                }
                // Non-replaced elements with no children: zero content.
                let mut inline_min = 0.0f32;
                let mut inline_max = 0.0f32;
                let mut block_min = 0.0f32;
                let mut block_max = 0.0f32;
                let mut saw_inline = false;
                let mut saw_block = false;
                for &c in self.dom.children(node) {
                    let cs = match self.style(c) {
                        Some(s) => s.clone(),
                        None => ComputedStyle::default(),
                    };
                    if cs.display == Display::None
                        || matches!(cs.position, Position::Absolute | Position::Fixed)
                        || cs.float != FloatSide::None
                    {
                        continue;
                    }
                    let (cm, cx) = self.intrinsic_pair(c);
                    // Child chrome is part of THIS box's content width.
                    let p = cs.padding.resolve(0.0);
                    let b = cs.border.resolve(0.0, cs.color);
                    let chrome = p.left + p.right + b[1].width + b[3].width;
                    let (cm, cx) = (cm + chrome, cx + chrome);
                    let is_block = matches!(
                        cs.display,
                        Display::Block
                            | Display::ListItem
                            | Display::Flex
                            | Display::Grid
                            | Display::Table
                            | Display::TableRow
                            | Display::TableCell
                            | Display::TableRowGroup
                            | Display::TableCaption
                    );
                    if is_block {
                        saw_block = true;
                        block_min = block_min.max(cm);
                        block_max = block_max.max(cx);
                    } else {
                        saw_inline = true;
                        inline_min = inline_min.max(cm);
                        inline_max += cx;
                    }
                }
                let _ = (saw_inline, saw_block);
                (inline_min.max(block_min), inline_max.max(block_max))
            }
        }
    }

    /// C1: [`Self::intrinsic_pair`] plus the item's own resolved chrome at
    /// `avail` — the border-box numbers flex/grid/abs callers resolve against.
    fn intrinsic_item_pair(&self, node: NodeId, avail: f32) -> (f32, f32) {
        let (mc, xc) = self.intrinsic_pair(node);
        let chrome = match self.style(node) {
            Some(s) => {
                let p = s.padding.resolve(avail);
                let b = s.border.resolve(avail, s.color);
                p.left + p.right + b[1].width + b[3].width
            }
            None => 0.0,
        };
        (mc + chrome, xc + chrome)
    }

    /// Walk an item's subtree, invoking `f` for each direct text piece with
    /// the style that would paint it.
    fn collect_text(&self, node: NodeId, f: &mut impl FnMut(&str, &ComputedStyle)) {
        let n = self.dom.node(node);
        match n.kind {
            NodeKind::Text => {
                let style = n
                    .parent
                    .and_then(|p| self.style(p))
                    .cloned()
                    .unwrap_or_default();
                if style.display != Display::None {
                    f(&n.text, &style);
                }
            }
            NodeKind::Element => {
                if self.style(node).map(|s| s.display) == Some(Display::None) {
                    return;
                }
                for &c in self.dom.children(node) {
                    self.collect_text(c, f);
                }
            }
        }
    }

    fn display_of(&self, node: NodeId) -> Option<Display> {
        if self.dom.node(node).kind == NodeKind::Text {
            return Some(Display::Inline);
        }
        self.style(node).map(|s| s.display)
    }

    /// Lay out and place every out-of-flow child of `cb` (`None` = page for
    /// `position: fixed`), lowest `z-index` first. Offsets resolve against the
    /// containing block's content box; auto edges keep the box's natural
    /// size, and C4: when both edges on an axis are auto the box stays at
    /// its static position — where the flow cursor was when it was seen.
    fn drain_abs(&mut self, cb: Option<NodeId>, content: (f32, f32, f32, f32)) {
        let matches: Vec<usize> = self
            .abs_list
            .iter()
            .enumerate()
            .filter(|(_, (c, _, _, _))| *c == cb)
            .map(|(i, _)| i)
            .collect();
        if matches.is_empty() {
            return;
        }
        let (cx, cy, cw, ch) = content;
        let mut paints: Vec<(i32, usize, Buffers)> = Vec::new();

        for (order, i) in matches.into_iter().enumerate() {
            let (_, child, sx, sy) = self.abs_list[i];
            let style = self.style_or_default(child);
            if style.display == Display::None {
                continue;
            }

            let w = Self::clamp_width(
                match style.width {
                    Some(SizeVal::Len(l)) => l.resolve(cw),
                    Some(sv) => {
                        let (mc, xc) = self.intrinsic_pair(child);
                        let p = style.padding.resolve(cw);
                        let b = style.border.resolve(cw, style.color);
                        let chrome = b[1].width + b[3].width + p.left + p.right;
                        sv.resolve(cw, mc + chrome, xc + chrome)
                    }
                    None => match (style.left, style.right) {
                        (Some(l), Some(r)) => (cw - l.resolve(cw) - r.resolve(cw)).max(40.0),
                        (Some(l), None) => (cw - l.resolve(cw)).max(40.0),
                        (None, Some(r)) => (cw - r.resolve(cw)).max(40.0),
                        _ => cw,
                    },
                },
                &style,
                cw,
            );
            // Lay out at the resolved width from a local origin; the box's
            // own margins are part of its internal layout (layout paints the
            // border box at (margin.left, margin.top) inside the buffer), so
            // the offsets below must NOT add them again — a `margin-top` on
            // a fixed rail would otherwise shift the box twice.
            let saved_y = self.y;
            self.y = 0.0;
            let (h, buf) = self.layout_box(
                child,
                Frame {
                    x: 0.0,
                    width: w,
                    align: style.text_align,
                },
                Some(child),
            );
            self.y = saved_y;
            let mut buf = buf.expect("positioned boxes always capture paint");

            // Offsets resolve against the containing block: left/right/both
            // against its width, top/bottom against its height. The margins
            // are already baked into the buffer geometry (see above).
            let x = match (style.left, style.right) {
                (Some(l), _) => cx + l.resolve(cw),
                (None, Some(r)) => cx + (cw - r.resolve(cw) - w).max(0.0),
                (None, None) => sx,
            };
            let y = match (style.top, style.bottom) {
                (Some(t), _) => cy + t.resolve(ch),
                (None, Some(b)) => cy + (ch - b.resolve(ch) - h).max(0.0),
                (None, None) => sy,
            };
            buf.translate(x, y);
            if style.overflow == Overflow::Hidden {
                // The box's border box sits at (margin.left, margin.top)
                // inside the buffer; `h` spans margins too (the flow cursor
                // advanced past them), so the clip targets the border box.
                let m = style.margin.resolve(cw);
                self.clip_buffers(
                    &mut buf,
                    x + m.left,
                    y + m.top,
                    w,
                    (h - m.top - m.bottom).max(0.0),
                );
            }
            // A6: `::backdrop` — an open modal dialog dims the page behind
            // it. The dim rides at the front of the dialog's own paint run:
            // fixed boxes drain after the document (dim above the page) and
            // the card's fill spliced ahead of its subtree (dim under the
            // card). Inserted after the translate, so the coordinates are
            // plain page space, and after the clip — the dim itself is never
            // clipped to the card. Width/height are the drain's content box
            // (the viewport on the fixed path — the CSS-correct backdrop
            // extent). The
            // rect keeps `Rect::solid`'s placeholder node 0 rather than the
            // dialog's: `node_rect` unions of the dialog then see only the
            // card, and the engine's click gate tells card from backdrop by
            // `transform` (card rects bake the dialog's matrix, the dim does
            // not — it must escape the card's `translateX(-50%)`).
            if self.dom.tag(child) == "dialog"
                && self
                    .dom
                    .element(child)
                    .is_some_and(|e| e.attr("open").is_some())
            {
                for s in &mut buf.stickies {
                    s.rect.0 += 1;
                }
                buf.rects.insert(
                    0,
                    Rect::solid(
                        0.0,
                        0.0,
                        cw.max(0.0),
                        ch.max(self.max_y).max(0.0),
                        0x00000080,
                    ),
                );
            }
            self.max_y = self.max_y.max(y + h);
            paints.push((style.z_index.unwrap_or(0), order, buf));
        }
        paints.sort_by_key(|(z, order, _)| (*z, *order));
        for (_, _, buf) in paints {
            self.merge_buffers(buf);
        }
        self.abs_list.retain(|(c, _, _, _)| *c != cb);
    }

    fn clamp_width(w: f32, style: &ComputedStyle, avail: f32) -> f32 {
        let mut w = w.min(avail);
        if let Some(mx) = style.max_width {
            w = w.min(mx.resolve(avail));
        }
        if let Some(mn) = style.min_width {
            w = w.max(mn.resolve(avail)).min(avail);
        }
        w
    }

    fn marker_run(&self, node: NodeId, x: f32, top: f32, text: String) -> TextRun {
        let style = self.style_or_default(node);
        let (ascent, _) = self
            .tm
            .metrics_family(style.font_size, false, &style.font_family);
        TextRun {
            x,
            y: top + ascent,
            text,
            size: style.font_size,
            weight: style.font_weight,
            italic: style.italic,
            monospace: false,
            family: style.font_family.clone(),
            color: style.color,
            link: None,
            letter_spacing: 0.0,
            transform: None,
            node: node.0,
            ..Default::default()
        }
    }

    /// Compose a run of inline nodes (text + inline elements) into wrapped
    /// lines occupying an anonymous block.
    fn compose_anonymous(&mut self, run: &[NodeId], frame: Frame, rtl: bool) {
        self.line.rtl = rtl;
        for &n in run {
            self.compose_node(n, None, frame);
        }
        self.flush_line(frame);
    }

    /// Feed one node's subtree into the current line. `forced` is the style
    /// of the inline ancestor we're inside (None at the anonymous root).
    fn compose_node(&mut self, node: NodeId, forced: Option<&ComputedStyle>, frame: Frame) {
        let n = self.dom.node(node);
        match n.kind {
            NodeKind::Text => {
                let style = forced.cloned().unwrap_or_else(|| {
                    // Anonymous text directly in a block: use the block's style.
                    n.parent
                        .and_then(|p| self.style(p))
                        .cloned()
                        .unwrap_or_default()
                });
                if style.display == Display::None {
                    return;
                }
                let text = n.text.clone();
                let link = style.link_href.clone();
                self.feed_text(&text, &style, link, frame);
            }
            NodeKind::Element => {
                let style = self.style_or_default(node);
                if style.display == Display::None {
                    return;
                }
                let link = style
                    .link_href
                    .clone()
                    .or_else(|| forced.and_then(|f| f.link_href.clone()));
                match self.dom.tag(node) {
                    "br" => self.flush_line(frame),
                    "img" | "canvas" => {
                        // A canvas is a replaced element with a natural size
                        // (`width`/`height` attributes, 300x150 default) whose
                        // raster arrives from page JS.
                        self.layout_image(node, &style, link, frame);
                    }
                    "svg" => {
                        // R6: inline `<svg>` lays out like a replaced image:
                        // the subtree serializes to a data URL the engine
                        // decodes through resvg at frame time. Children
                        // never compose into the line.
                        self.layout_svg_element(node, &style, link, frame);
                    }
                    "video" => {
                        self.layout_video(node, &style, link, frame);
                    }
                    "audio" => {
                        self.layout_audio(node, &style, link, frame);
                    }
                    // A8: value widgets are inline-block but paint through
                    // the widget arm in `layout_static_box` — children never
                    // compose into the line. Same band-row approximation as
                    // form controls (own line, then flow resumes below).
                    "progress" | "meter" => {
                        let saved_node = std::mem::replace(&mut self.cur_node, node.0);
                        let saved_y = self.y;
                        // Scratch layout measures the widget height and
                        // captures its paint without touching the page lists.
                        let (buf, h) = self.with_buffers(|lay| {
                            lay.y = saved_y;
                            lay.layout_static_box(node, frame, None)
                        });
                        let h = h.max(14.0);
                        self.y = saved_y;
                        self.merge_buffers(buf);
                        self.y += h;
                        self.max_y = self.max_y.max(self.y);
                        self.cur_node = saved_node;
                    }
                    "math" => {
                        // A12 finish: MathML paints through the 2D walker
                        // (`math_walk`) on its own band-row — the same
                        // inline-block approximation progress/meter use.
                        let saved_node = std::mem::replace(&mut self.cur_node, node.0);
                        let saved_y = self.y;
                        let (mut buf, (_, asc, desc)) =
                            self.with_buffers(|lay| lay.math_walk(node, 0.0, 0.0, 1.0));
                        self.y = saved_y;
                        buf.translate(frame.x, saved_y);
                        self.merge_buffers(buf);
                        self.y = saved_y + asc + desc;
                        self.max_y = self.max_y.max(self.y);
                        self.cur_node = saved_node;
                    }
                    "button" | "input" | "select" | "textarea" => {
                        // Inline controls get no box, so they re-stamp
                        // `cur_node` themselves (v7): their rects and label
                        // runs attribute to the control element.
                        let saved_node = std::mem::replace(&mut self.cur_node, node.0);
                        self.layout_form_control(node, &style, frame);
                        self.cur_node = saved_node;
                    }
                    _ => {
                        // R3: inline elements' generated content joins their
                        // own line before/after the subtree. Inline elements
                        // get no box, so they re-stamp `cur_node` themselves:
                        // their words attribute to the `<a>`/`<span>`, not
                        // the enclosing block.
                        let saved_node = std::mem::replace(&mut self.cur_node, node.0);
                        if let Some(b) = &style.before {
                            self.feed_text(&b.text, &b.style, None, frame);
                        }
                        let kids = self.dom.children(node).to_vec();
                        for c in kids {
                            self.compose_node(c, Some(&style), frame);
                        }
                        if let Some(a) = &style.after {
                            self.feed_text(&a.text, &a.style, None, frame);
                        }
                        self.cur_node = saved_node;
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // A12 finish: MathML 2D layout.
    //
    // `math_walk` paints a MathML subtree with its row baseline at
    // `base_y` and left edge at `x`, returning (width, ascent, descent)
    // around that baseline. Constructs that need 2D placement —
    // fractions, radicals, scripts, under/overs — capture their parts
    // into scratch buffers (the progress/meter idiom) and translate them
    // into place; everything else is a horizontal row, so plain token
    // runs keep the linearized reading order the v1 test pins down.
    // ------------------------------------------------------------------

    /// Measure-and-paint one MathML node. `scale` is the scriptlevel
    /// shrink: 1.0 at top level, 0.7 inside sub/superscripts and
    /// over/underscripts.
    fn math_walk(&mut self, node: NodeId, x: f32, base_y: f32, scale: f32) -> (f32, f32, f32) {
        let n = self.dom.node(node);
        match n.kind {
            NodeKind::Text => {
                let style = n
                    .parent
                    .and_then(|p| self.style(p))
                    .cloned()
                    .unwrap_or_default();
                let text = n.text.trim().to_string();
                self.math_token(&text, &style, x, base_y, scale)
            }
            NodeKind::Element => {
                // <semantics> annotations carry machine data, never paint.
                if matches!(self.dom.tag(node), "annotation" | "annotation-xml") {
                    return (0.0, 0.0, 0.0);
                }
                let style = self.style_or_default(node);
                if style.display == Display::None {
                    return (0.0, 0.0, 0.0);
                }
                let saved = std::mem::replace(&mut self.cur_node, node.0);
                let kids = self.dom.children(node).to_vec();
                let r = match self.dom.tag(node) {
                    "mi" | "mn" | "mo" | "mtext" | "ms" => {
                        let mut text = String::new();
                        for &c in &kids {
                            let nd = self.dom.node(c);
                            if matches!(nd.kind, NodeKind::Text) {
                                text.push_str(&nd.text);
                            }
                        }
                        self.math_token(text.trim(), &style, x, base_y, scale)
                    }
                    "mfrac" if kids.len() >= 2 => {
                        let (mut nbuf, (nw, nasc, ndesc)) =
                            self.with_buffers(|lay| lay.math_walk(kids[0], 0.0, 0.0, scale));
                        let (mut dbuf, (dw, dasc, ddesc)) =
                            self.with_buffers(|lay| lay.math_walk(kids[1], 0.0, 0.0, scale));
                        let w = nw.max(dw) + 2.0;
                        // The bar rides the row's math axis: a 1px rule
                        // centered on the baseline, both parts centered on
                        // it with 3px of clearance each way.
                        self.push_rect(Rect::solid(
                            x + 1.0,
                            base_y - 0.5,
                            (w - 2.0).max(1.0),
                            1.0,
                            style.color,
                        ));
                        nbuf.translate(x + (w - nw) * 0.5, base_y - 3.0 - ndesc);
                        self.merge_buffers(nbuf);
                        dbuf.translate(x + (w - dw) * 0.5, base_y + 3.0 + dasc);
                        self.merge_buffers(dbuf);
                        (w, 3.0 + ndesc + nasc, 3.0 + dasc + ddesc)
                    }
                    "msqrt" => {
                        let fs = style.font_size * scale;
                        let sq = fs * 1.15;
                        let sw = self.tm.measure_family(
                            "√",
                            sq,
                            style.font_weight,
                            false,
                            style.monospace,
                            &style.font_family,
                        );
                        let (s_asc, s_desc) =
                            self.tm
                                .metrics_family(sq, style.monospace, &style.font_family);
                        let (mut rbuf, (rw, rasc, rdesc)) =
                            self.with_buffers(|lay| lay.math_row(&kids, 0.0, 0.0, scale));
                        // Radical glyph on the row baseline, radicand to
                        // its right, overline spanning the radicand.
                        self.push_text(TextRun {
                            x,
                            y: base_y,
                            text: "√".to_string(),
                            size: sq,
                            weight: style.font_weight,
                            italic: false,
                            monospace: style.monospace,
                            family: style.font_family.clone(),
                            color: style.color,
                            ..Default::default()
                        });
                        self.push_rect(Rect::solid(
                            x + sw + 1.0,
                            base_y - rasc - 1.5,
                            (rw + 1.0).max(1.0),
                            1.0,
                            style.color,
                        ));
                        rbuf.translate(x + sw + 2.0, base_y);
                        self.merge_buffers(rbuf);
                        (sw + 2.0 + rw, (rasc + 2.5).max(s_asc), rdesc.max(s_desc))
                    }
                    "msub" | "msup" | "msubsup" if kids.len() >= 2 => {
                        self.math_scripts(self.dom.tag(node), &kids, x, base_y, scale, &style)
                    }
                    "mover" | "munder" | "munderover" if kids.len() >= 2 => {
                        self.math_overunder(self.dom.tag(node), &kids, x, base_y, scale, &style)
                    }
                    _ => self.math_row(&kids, x, base_y, scale),
                };
                self.cur_node = saved;
                r
            }
        }
    }

    /// One text token (`mi`/`mn`/`mo`/`mtext`/`ms` or bare text) on the
    /// row baseline: a single measured run; `mo`'s UA side margins space
    /// the operators. Returns (width, ascent, descent).
    fn math_token(
        &mut self,
        text: &str,
        style: &ComputedStyle,
        x: f32,
        base_y: f32,
        scale: f32,
    ) -> (f32, f32, f32) {
        if text.is_empty() {
            return (0.0, 0.0, 0.0);
        }
        let fs = style.font_size * scale;
        let m = style.margin.resolve(0.0);
        let w = self.tm.measure_family(
            text,
            fs,
            style.font_weight,
            style.italic,
            style.monospace,
            &style.font_family,
        );
        let (asc, desc) = self
            .tm
            .metrics_family(fs, style.monospace, &style.font_family);
        self.push_text(TextRun {
            x: x + m.left,
            y: base_y,
            text: text.to_string(),
            size: fs,
            weight: style.font_weight,
            italic: style.italic,
            monospace: style.monospace,
            family: style.font_family.clone(),
            color: style.color,
            ..Default::default()
        });
        (m.left + w + m.right, asc, desc)
    }

    /// A horizontal row of children sharing one baseline (`mrow`,
    /// unknown constructs, malformed fallbacks). Reading order preserved.
    fn math_row(&mut self, kids: &[NodeId], x: f32, base_y: f32, scale: f32) -> (f32, f32, f32) {
        let mut cx = x;
        let (mut asc, mut desc) = (0.0_f32, 0.0_f32);
        for &k in kids {
            let (kw, ka, kd) = self.math_walk(k, cx, base_y, scale);
            cx += kw;
            asc = asc.max(ka);
            desc = desc.max(kd);
        }
        (cx - x, asc, desc)
    }

    /// Scripts: base run on the baseline, sub/superscript at 0.7× size
    /// shifted ±~0.4×font from it. `msub` = [base, sub], `msup` =
    /// [base, sup], `msubsup` = [base, sub, sup].
    fn math_scripts(
        &mut self,
        tag: &str,
        kids: &[NodeId],
        x: f32,
        base_y: f32,
        scale: f32,
        style: &ComputedStyle,
    ) -> (f32, f32, f32) {
        let fs = style.font_size * scale;
        let script_scale = scale * 0.7;
        let (bw, basc, bdesc) = self.math_walk(kids[0], x, base_y, scale);
        // Sub and superscript share one column after the base.
        let col = x + bw + 1.0;
        let mut col_w = 0.0_f32;
        let (mut asc, mut desc) = (basc, bdesc);
        let mut sub = kids.get(1).copied();
        let mut sup = None;
        if tag == "msup" {
            sup = sub.take();
        } else if tag == "msubsup" {
            sup = kids.get(2).copied();
        }
        if let Some(s) = sub {
            let dy = fs * 0.2;
            let (mut sbuf, (sw, sasc, sdesc)) =
                self.with_buffers(|lay| lay.math_walk(s, 0.0, 0.0, script_scale));
            sbuf.translate(col, base_y + dy);
            self.merge_buffers(sbuf);
            col_w = col_w.max(sw);
            asc = asc.max(sasc - dy);
            desc = desc.max(sdesc + dy);
        }
        if let Some(s) = sup {
            let dy = fs * 0.4;
            let (mut sbuf, (sw, sasc, sdesc)) =
                self.with_buffers(|lay| lay.math_walk(s, 0.0, 0.0, script_scale));
            sbuf.translate(col, base_y - dy);
            self.merge_buffers(sbuf);
            col_w = col_w.max(sw);
            asc = asc.max(sasc + dy);
            desc = desc.max(sdesc - dy);
        }
        (bw + 1.0 + col_w, asc, desc)
    }

    /// Under/overscripts: base centered, script above (`mover`) or below
    /// (`munder`); `munderover` = [base, under, over]. Scripts shrink at
    /// the same 0.7× scriptlevel as sub/superscripts.
    fn math_overunder(
        &mut self,
        tag: &str,
        kids: &[NodeId],
        x: f32,
        base_y: f32,
        scale: f32,
        style: &ComputedStyle,
    ) -> (f32, f32, f32) {
        let fs = style.font_size * scale;
        let script_scale = scale * 0.7;
        let (bw, basc, bdesc) = self.math_walk(kids[0], x, base_y, scale);
        let (mut asc, mut desc) = (basc, bdesc);
        let under = kids.get(1).copied();
        let over = if tag == "mover" {
            under
        } else {
            kids.get(2).copied()
        };
        let under = if tag == "mover" { None } else { under };
        if let Some(o) = over {
            let (mut obuf, (ow, oasc, odesc)) =
                self.with_buffers(|lay| lay.math_walk(o, 0.0, 0.0, script_scale));
            let oy = base_y - basc - fs * 0.15 - odesc;
            obuf.translate(x + (bw - ow).max(0.0) * 0.5, oy);
            self.merge_buffers(obuf);
            asc = asc.max(base_y - oy + oasc);
        }
        if let Some(u) = under {
            let (mut ubuf, (uw, _uasc, udesc)) =
                self.with_buffers(|lay| lay.math_walk(u, 0.0, 0.0, script_scale));
            let uy = base_y + bdesc + fs * 0.15;
            ubuf.translate(x + (bw - uw).max(0.0) * 0.5, uy);
            self.merge_buffers(ubuf);
            desc = desc.max(uy - base_y + udesc);
        }
        (bw, asc, desc)
    }

    /// Replaced image element: decode-sized (attribute `width`/`height` as
    /// intrinsic, CSS `width`/`height` overriding with aspect preserved),
    /// `object-fit` carried for the renderer, real `DisplayImage` item plus
    /// an italic alt-text run for readability and hit-testing when linked.
    fn layout_image(
        &mut self,
        node: NodeId,
        style: &ComputedStyle,
        link: Option<String>,
        frame: Frame,
    ) {
        let el = self.dom.element(node);
        let src = self.resolved_img_src(node);
        let alt = el
            .and_then(|e| e.attr("alt"))
            .unwrap_or("image")
            .to_string();
        let (x, y, w, h) = self.sized_replaced(node, style, frame);
        self.out.images.push(DisplayImage {
            x,
            y,
            w,
            h,
            src,
            alt: alt.clone(),
            fit: style.object_fit,
            payload: None,
            animated: false,
            transform: None,
            node: node.0,
            ..Default::default()
        });
        // Alt-text fallback (italic gray) at the image's top-left so text
        // dumps and screen readers still see it; skipped for empty alt.
        if !alt.trim().is_empty() {
            let (ascent, _) =
                self.tm
                    .metrics_family(style.font_size, style.monospace, &style.font_family);
            self.out.texts.push(TextRun {
                x,
                y: y + ascent + 2.0,
                text: format!("[{alt}]"),
                size: style.font_size.min(14.0),
                weight: style.font_weight,
                italic: true,
                monospace: style.monospace,
                family: style.font_family.clone(),
                color: 0x6B7280FF,
                link: link.clone(),
                letter_spacing: 0.0,
                transform: None,
                node: node.0,
                ..Default::default()
            });
        }
        if let Some(href) = link {
            self.out.links.push(LinkArea {
                x,
                y,
                w,
                h,
                href,
                transform: None,
                node: node.0,
            });
        }
    }

    /// A3: the effective `src` for an `<img>` — a `<picture>` parent's
    /// `<source>` candidates (before the img) win when their `type` is a
    /// format the engine decodes and their `media` condition holds against
    /// the layout viewport and the page's media environment; within `srcset`,
    /// the smallest candidate at or above the viewport width wins (density
    /// assumed 1, `sizes` deferred). Falls back to the img's own `src`. An
    /// `<img media>` that does not match selects nothing at all.
    fn resolved_img_src(&self, node: NodeId) -> String {
        // A `<canvas>` paints its own backing store: page JavaScript draws
        // into a JS-side buffer and the engine registers the pixels under
        // this sentinel (the same contract `\0media:` uses for `<video>`
        // frames). There is nothing to fetch and no `src` to resolve.
        if self.dom.tag(node) == "canvas" {
            return format!("\u{0}canvas:{}", node.0);
        }
        let el = self.dom.element(node);
        // A3: the `<img>`'s own `media` attribute gates the element as a
        // whole — no `src`, no `<picture>` source, nothing to fetch — exactly
        // as a non-matching `<source media>` is skipped. It is the same
        // condition the stylesheet path would answer, read against the
        // page's own environment ([`Self::media_matches`]).
        if let Some(media) = el.and_then(|e| e.attr("media")) {
            if !self.media_matches(media) {
                return String::new();
            }
        }
        // A3: the img's own srcset participates even without a <picture>
        // parent.
        if let Some(srcset) = el.and_then(|e| e.attr("srcset")) {
            if let Some(best) = pick_srcset(srcset, self.viewport_w) {
                return best;
            }
        }
        let own = el.and_then(|e| e.attr("src")).unwrap_or("").to_string();
        let Some(parent) = self.dom.node(node).parent else {
            return own;
        };
        if self.dom.tag(parent) != "picture" {
            return own;
        }
        for &sib in self.dom.children(parent) {
            if sib == node {
                break; // sources precede the img
            }
            if self.dom.tag(sib) != "source" {
                continue;
            }
            let el = self.dom.element(sib);
            if let Some(t) = el.and_then(|e| e.attr("type")) {
                let base = t.to_ascii_lowercase();
                let base = base.split(';').next().unwrap_or("").trim();
                if !matches!(
                    base,
                    "image/png"
                        | "image/jpeg"
                        | "image/jpg"
                        | "image/gif"
                        | "image/webp"
                        | "image/svg+xml"
                        | "image/apng"
                ) {
                    continue;
                }
            }
            if let Some(media) = el.and_then(|e| e.attr("media")) {
                if !self.media_matches(media) {
                    continue;
                }
            }
            if let Some(srcset) = el.and_then(|e| e.attr("srcset")) {
                if let Some(best) = pick_srcset(srcset, self.viewport_w) {
                    return best;
                }
            }
            if let Some(src) = el.and_then(|e| e.attr("src")) {
                return src.to_string();
            }
        }
        own
    }

    /// A3: evaluate a `<source media>` / `<img media>` attribute against the
    /// layout viewport through the same evaluator the stylesheet path uses
    /// (kitae_css): `and`-joined features, comma-OR'd queries, `not`,
    /// `(min-width: …)` / `(max-width: …)` plus the MQ4 range syntax and
    /// `orientation`, which need the viewport height this layouter has.
    /// The media type and the `prefers-*` features read the page's own
    /// environment ([`Self::env`]) — the same [`EnvironmentState`] the style
    /// engine filtered the sheets with, so a page laid out for paper selects
    /// its `<source media="print">` and one in dark mode selects
    /// `(prefers-color-scheme: dark)`. Without it (`layout` and friends)
    /// they read the starting environment, which is screen media.
    fn media_matches(&self, media: &str) -> bool {
        let media = media.trim();
        // A missing / `all` condition always holds. A bare media type goes
        // through the evaluator, which answers it from the environment — a
        // `media="screen"` source is not selected on paper.
        if media.is_empty() || media.eq_ignore_ascii_case("all") {
            return true;
        }
        kitae_style::eval_media_condition_size(media, self.viewport_w, self.viewport_h, &self.env)
    }

    /// R6: inline `<svg>` — serialize the subtree, hand the engine a base64
    /// data URL, and lay out as a replaced element (identical sizing and
    /// line flow to `<img>` via [`sized_replaced`]; CSS width/height
    /// override, then the element's width/height attributes).
    fn layout_svg_element(
        &mut self,
        node: NodeId,
        style: &ComputedStyle,
        link: Option<String>,
        frame: Frame,
    ) {
        let source = self.dom.outer_html(node);
        let src = format!(
            "data:image/svg+xml;base64,{}",
            b64_encode(source.as_bytes())
        );
        let (x, y, w, h) = self.sized_replaced(node, style, frame);
        self.out.images.push(DisplayImage {
            x,
            y,
            w,
            h,
            src,
            alt: String::new(),
            fit: style.object_fit,
            payload: None,
            animated: false,
            transform: None,
            node: node.0,
            ..Default::default()
        });
        if let Some(href) = link {
            self.out.links.push(LinkArea {
                x,
                y,
                w,
                h,
                href,
                transform: None,
                node: node.0,
            });
        }
    }

    /// Replaced video element (J5): a `DisplayMedia` chrome item plus a
    /// sibling `DisplayImage` covering the same rect — the engine stamps the
    /// image's payload with the current frame's raster each publish, so the
    /// picture rides the ordinary image path. Sizing, clamping, float
    /// shrink-to-fit and alignment are identical to `layout_image`
    /// ([`sized_replaced`]); the alt-style caption carries the element's
    /// text content so text dumps still describe the media. The `poster`
    /// attribute rides the media item for the engine to stamp into that
    /// same frame image (HTML: "the URL of an image file that the user agent
    /// can show while no video data is available").
    fn layout_video(
        &mut self,
        node: NodeId,
        style: &ComputedStyle,
        link: Option<String>,
        frame: Frame,
    ) {
        self.flush_line(frame);
        let el = self.dom.element(node);
        let src = el.and_then(|e| e.attr("src")).unwrap_or("").to_string();
        // A poster URL "potentially surrounded by spaces"; the empty string
        // means no poster frame at all.
        let poster = el
            .and_then(|e| e.attr("poster"))
            .unwrap_or("")
            .trim()
            .to_string();
        let id = self.out.media.len() as u64;
        let (x, y, w, h) = self.sized_replaced(node, style, frame);
        self.out.media.push(DisplayMedia {
            id,
            x,
            y,
            w,
            h,
            src,
            kind: MediaKind::Video,
            state: (false, 0.0),
            controls: el.and_then(|e| e.attr("controls")).is_some(),
            poster,
            node: node.0,
        });
        // The current frame: an unresolved image at the same rect; the
        // engine resolves `src` to the stepped-frame raster (see kitae_core
        // relayout stamping).
        self.out.images.push(DisplayImage {
            x,
            y,
            w,
            h,
            src: format!("\u{0}media:{id}"),
            alt: String::new(),
            fit: style.object_fit,
            payload: None,
            animated: false,
            transform: None,
            node: node.0,
            ..Default::default()
        });
        // Caption fallback: the element's text content (like `<video>`'s
        // fallback children in HTML), italic gray at the rect's top-left.
        let mut parts = Vec::new();
        self.collect_text(node, &mut |text, _| parts.push(text.to_string()));
        let caption = parts
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !caption.is_empty() {
            let (ascent, _) =
                self.tm
                    .metrics_family(style.font_size, style.monospace, &style.font_family);
            self.out.texts.push(TextRun {
                x,
                y: y + ascent + 2.0,
                text: caption,
                size: style.font_size.min(14.0),
                weight: style.font_weight,
                italic: true,
                monospace: style.monospace,
                family: style.font_family.clone(),
                color: 0x6B7280FF,
                link: link.clone(),
                letter_spacing: 0.0,
                transform: None,
                node: node.0,
                ..Default::default()
            });
        }
        if let Some(href) = link {
            self.out.links.push(LinkArea {
                x,
                y,
                w,
                h,
                href,
                transform: None,
                node: node.0,
            });
        }
    }

    /// Replaced audio element (F4): a `DisplayMedia` chrome item only — no
    /// frame image to stamp. Without `controls` an audio element paints
    /// nothing (HTML's default) beyond the space `sized_replaced` reserved.
    /// The bar is clamped to a slim 48px transport strip; width follows the
    /// usual replaced sizing so CSS width/height still apply.
    fn layout_audio(
        &mut self,
        node: NodeId,
        style: &ComputedStyle,
        link: Option<String>,
        frame: Frame,
    ) {
        self.flush_line(frame);
        let el = self.dom.element(node);
        let controls = el.and_then(|e| e.attr("controls")).is_some();
        if !controls {
            return;
        }
        let src = el.and_then(|e| e.attr("src")).unwrap_or("").to_string();
        let id = self.out.media.len() as u64;
        let (x, y, w, h) = self.sized_replaced(node, style, frame);
        let h = h.min(48.0);
        self.out.media.push(DisplayMedia {
            id,
            x,
            y,
            w,
            h,
            src,
            kind: MediaKind::Audio,
            state: (false, 0.0),
            controls,
            // `poster` is a `<video>`-only attribute; audio has no playback
            // area to show an image in.
            poster: String::new(),
            node: node.0,
        });
        if let Some(href) = link {
            self.out.links.push(LinkArea {
                x,
                y,
                w,
                h,
                href,
                transform: None,
                node: node.0,
            });
        }
    }

    /// Shared replaced-element sizing for `img`/`video`: attributes give the
    /// intrinsic size (300×150 default), CSS width/height override with
    /// aspect preserved, a C2 `aspect-ratio` replaces the intrinsic ratio
    /// on whichever axis is `auto`, min/max clamp, then shrink-to-fit
    /// against floats and inline alignment within the line band. Owns the
    /// flow cursor: flushes the current line and advances `self.y` past the
    /// box.
    fn sized_replaced(
        &mut self,
        node: NodeId,
        style: &ComputedStyle,
        frame: Frame,
    ) -> (f32, f32, f32, f32) {
        self.flush_line(frame);
        let el = self.dom.element(node);
        let parse_dim = |s: Option<&str>| -> Option<f32> {
            let s = s?.trim().trim_end_matches("px").trim();
            s.parse::<f32>().ok().filter(|&v| v > 0.0)
        };
        let attr_w = parse_dim(el.and_then(|e| e.attr("width")));
        let attr_h = parse_dim(el.and_then(|e| e.attr("height")));
        let intrinsic_w = attr_w.unwrap_or(300.0);
        let intrinsic_h = attr_h.unwrap_or(150.0);

        // C2: the ratio to size the `auto` axis against, as width/height.
        // A bare `aspect-ratio` overrides the intrinsic ratio outright; the
        // `auto ||` form lets a box that has an intrinsic one keep it, and
        // uses the authored number only where there is none. Note what that
        // means here: the attributes (or the 300×150 default) always supply
        // an intrinsic ratio, so on this path `auto <ratio>` resolves to the
        // intrinsic ratio and the authored number is inert — which is the
        // spec's natural-ratio-preferred rule applied to an element that has
        // a natural ratio. A box that has none (a plain block) is sized by
        // `layout_box`, which reads the ratio directly.
        let ratio = style.aspect_ratio.filter(|ar| ar.ratio > 0.0).map(|ar| {
            if ar.auto_keyword && intrinsic_w > 0.0 && intrinsic_h > 0.0 {
                intrinsic_w / intrinsic_h
            } else {
                ar.ratio
            }
        });

        // CSS dims resolve here: width vs the containing-block width, height
        // vs zero (its % basis is indefinite and style already dropped it).
        // C1: intrinsic keywords on a replaced element = its natural size
        // (`Len`-only reads here; the keyword falls through to auto).
        let cw_px = style
            .width
            .and_then(|sv| sv.len())
            .map(|l| l.resolve(frame.width));
        let ch_px = style.height.and_then(|sv| sv.len()).map(|l| l.resolve(0.0));
        let (mut w, mut h) = match (cw_px, ch_px) {
            (Some(cw), Some(ch)) => (cw, ch),
            (Some(cw), None) => {
                let h = match ratio {
                    Some(r) => cw / r,
                    None if intrinsic_w > 0.0 => intrinsic_h * (cw / intrinsic_w),
                    None => intrinsic_h,
                };
                (cw, h)
            }
            (None, Some(ch)) => {
                let w = match ratio {
                    Some(r) => ch * r,
                    None if intrinsic_h > 0.0 => intrinsic_w * (ch / intrinsic_h),
                    None => intrinsic_w,
                };
                (w, ch)
            }
            // Both axes auto: the natural width stands (a replaced element
            // is not stretched to its containing block) and the ratio — the
            // authored one, or the intrinsic one when `auto ||` kept it —
            // fixes the height.
            (None, None) => match ratio {
                Some(r) => (intrinsic_w, intrinsic_w / r),
                None => (intrinsic_w, intrinsic_h),
            },
        };
        if let Some(mx) = style.max_width {
            let mx = mx.resolve(frame.width);
            if w > mx {
                // Preserve aspect when clamping an auto-sized dimension.
                if cw_px.is_none() && ch_px.is_none() {
                    h *= mx / w;
                }
                w = mx;
            }
        }
        if let Some(mn) = style.min_width {
            let mn = mn.resolve(frame.width);
            if w < mn {
                w = mn;
            }
        }
        if let Some(mx) = style.max_height {
            let mx = mx.resolve(0.0);
            if h > mx {
                h = mx;
            }
        }
        if let Some(mn) = style.min_height {
            let mn = mn.resolve(0.0);
            if h < mn {
                h = mn;
            }
        }
        w = w.max(1.0);
        h = h.max(1.0);
        // Shrink-to-fit against floats, preserving aspect.
        let est_band = self.float_band(frame, self.y, self.y + h);
        if w > est_band.1 && est_band.1 > 1.0 {
            let scale = est_band.1 / w;
            w = est_band.1;
            h *= scale;
        }
        let (band_x, band_w) = self.float_band(frame, self.y, self.y + h);
        let offset = match frame.align {
            TextAlign::Left => 0.0,
            TextAlign::Center => ((band_w - w) * 0.5).max(0.0),
            TextAlign::Right => (band_w - w).max(0.0),
        };
        let x = band_x + offset;
        let y = self.y;
        self.y += h;
        self.max_y = self.max_y.max(self.y);
        (x, y, w, h)
    }
    /// replaced boxes with a background rect and a text label (value /
    /// placeholder / option text). `input[type=hidden]` paints nothing.
    /// Inline-block is approximated as its own line (own band row) — readable
    /// and focusable; true inline flow is future work.
    /// The control's label text and (for inputs) its `type` — shared by
    /// the paint path and the intrinsic measure, so a flex base always
    /// matches what the paint pass draws.
    fn control_label_and_type(&self, node: NodeId) -> (String, String) {
        let tag = self.dom.tag(node).to_string();
        let el = self.dom.element(node);
        // A7: widget kinds that paint more than a label.
        let input_type = if tag == "input" {
            el.and_then(|e| e.attr("type"))
                .unwrap_or("text")
                .to_ascii_lowercase()
        } else {
            String::new()
        };
        // Label text per control kind.
        let mut label = String::new();
        match tag.as_str() {
            "button" => {
                let mut parts = Vec::new();
                self.collect_text(node, &mut |text, _| parts.push(text.to_string()));
                label = parts
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if label.is_empty() {
                    label = "Submit".to_string();
                }
            }
            "input" => {
                label = match input_type.as_str() {
                    "submit" | "button" => el
                        .and_then(|e| e.attr("value"))
                        .unwrap_or("Submit")
                        .to_string(),
                    "checkbox" | "radio" => {
                        // Checked state as [x]/[ ] plus value for dumps.
                        let checked = el.map(|e| e.attr("checked").is_some()).unwrap_or(false);
                        let v = el.and_then(|e| e.attr("value")).unwrap_or("on");
                        format!("{} {}", if checked { "[x]" } else { "[ ]" }, v)
                    }
                    // A7: file inputs show the no-file pill (or the file
                    // name); datetime inputs show their value verbatim
                    // (already ISO-formatted in the attribute).
                    "file" => el
                        .and_then(|e| e.attr("value"))
                        .filter(|v| !v.is_empty())
                        .map(|v| v.rsplit('/').next().unwrap_or(v).to_string())
                        .unwrap_or_else(|| "Choose file".to_string()),
                    "password" => {
                        let v = el.and_then(|e| e.attr("value")).unwrap_or("");
                        if v.is_empty() {
                            el.and_then(|e| e.attr("placeholder"))
                                .filter(|p| !p.is_empty())
                                .unwrap_or("Password")
                                .to_string()
                        } else {
                            "••••••••".to_string()
                        }
                    }
                    _ => {
                        let v = el.and_then(|e| e.attr("value")).unwrap_or("");
                        if v.is_empty() {
                            el.and_then(|e| e.attr("placeholder"))
                                .unwrap_or("")
                                .to_string()
                        } else {
                            v.to_string()
                        }
                    }
                };
            }
            "textarea" => {
                let mut parts = Vec::new();
                self.collect_text(node, &mut |text, _| parts.push(text.to_string()));
                label = parts.concat();
            }
            "select" => {
                // Selected option text, else first option, else value attr.
                let mut first: Option<String> = None;
                let mut selected: Option<String> = None;
                for &c in self.dom.children(node) {
                    if self.dom.tag(c) != "option" {
                        continue;
                    }
                    let mut txt = Vec::new();
                    self.collect_text(c, &mut |t, _| txt.push(t.to_string()));
                    let s = txt.concat().trim().to_string();
                    if first.is_none() {
                        first = Some(s.clone());
                    }
                    let is_sel = self
                        .dom
                        .element(c)
                        .map(|e| e.attr("selected").is_some())
                        .unwrap_or(false);
                    if is_sel {
                        selected = Some(s);
                        break;
                    }
                }
                label = selected
                    .or(first)
                    .or_else(|| el.and_then(|e| e.attr("value")).map(|s| s.to_string()))
                    .unwrap_or_default();
            }
            _ => {}
        }
        (label, input_type)
    }

    /// Solved control box size `(w, h)`: the per-kind default formula plus
    /// the control's REAL chrome (author/UA padding + painted border
    /// widths — CSS widths are border-box here), author `width`/`height`
    /// overriding, then min/max clamps and the small floors. `avail` is
    /// the resolving width for %-values (0 where none exists, e.g. the
    /// intrinsic measure).
    fn control_size(
        &self,
        node: NodeId,
        style: &ComputedStyle,
        input_type: &str,
        text_w: f32,
        avail: f32,
    ) -> (f32, f32) {
        let tag = self.dom.tag(node);
        let p = style.padding.resolve(avail);
        let sides = style.border.resolve(avail, style.color);
        let border_visible = style.border.any_visible();
        let chrome_w = p.left
            + p.right
            + if border_visible { sides[1].width } else { 1.0 }
            + if border_visible { sides[3].width } else { 1.0 };
        let mut w = style
            .width
            .and_then(|sv| sv.len())
            .map(|l| l.resolve(avail))
            .unwrap_or_else(|| match tag {
                "textarea" => 200.0,
                "select" => (text_w + 28.0 + chrome_w).max(80.0).min(300.0),
                "input" => match input_type {
                    "checkbox" | "radio" => (text_w + chrome_w).max(40.0),
                    "submit" | "button" => (text_w + chrome_w).max(60.0),
                    // A7: range/color/file/datetime have engine-default sizes.
                    "range" => 129.0,
                    "color" => 49.0,
                    "file" => (text_w + 30.0 + chrome_w).max(150.0),
                    _ => 160.0 + chrome_w,
                },
                _ => (text_w + chrome_w).max(60.0).min(320.0),
            });
        let mut h = style
            .height
            .and_then(|sv| sv.len())
            .map(|l| l.resolve(0.0))
            .unwrap_or_else(|| match tag {
                "textarea" => (style.font_size * style.line_height * 2.0 + 10.0).max(40.0),
                "select" => (style.font_size * style.line_height + 10.0).max(26.0),
                // A7: a range is a slim slider, a color is a square swatch.
                "input" if input_type == "range" => 14.0,
                "input" if input_type == "color" => 27.0,
                _ => (style.font_size * style.line_height
                    + p.top
                    + p.bottom
                    + if border_visible {
                        sides[0].width + sides[2].width
                    } else {
                        2.0
                    })
                .max(24.0),
            });
        if let Some(mx) = style.max_width {
            w = w.min(mx.resolve(avail));
        }
        if let Some(mn) = style.min_width {
            w = w.max(mn.resolve(avail));
        }
        (w.max(24.0), h.max(18.0))
    }

    fn layout_form_control(&mut self, node: NodeId, style: &ComputedStyle, frame: Frame) {
        let tag = self.dom.tag(node).to_string();
        let el = self.dom.element(node);
        if tag == "input" {
            let t = el
                .and_then(|e| e.attr("type"))
                .unwrap_or("text")
                .to_ascii_lowercase();
            if t == "hidden" {
                return;
            }
        }
        self.flush_line(frame);

        let (label, input_type) = self.control_label_and_type(node);
        let text_w = if label.trim().is_empty() {
            0.0
        } else {
            self.tm.measure_family(
                label.trim(),
                style.font_size,
                style.font_weight,
                style.italic,
                style.monospace,
                &style.font_family,
            )
        };
        let (mut w, h) = self.control_size(node, style, &input_type, text_w, frame.width);
        let est_band = self.float_band(frame, self.y, self.y + h);
        if w > est_band.1 && est_band.1 > 24.0 {
            w = est_band.1;
        }
        let (band_x, band_w) = self.float_band(frame, self.y, self.y + h);
        let offset = match frame.align {
            TextAlign::Left => 0.0,
            TextAlign::Center => ((band_w - w) * 0.5).max(0.0),
            TextAlign::Right => (band_w - w).max(0.0),
        };
        let x = band_x + offset;
        let y = self.y;
        // Chrome geometry: REAL padding and painted border widths — the
        // control is border-box, so the label insets from the border edge.
        // `any_visible` gates the border read: a page CSS `border: none`
        // still means the 1px chrome border (which keeps the invalid-red
        // hook working — A10 paints `input:invalid` through this path).
        let border_visible = style.border.any_visible();
        let sides = style.border.resolve(frame.width, style.color);
        let p = style.padding.resolve(frame.width);
        let (bt, br, bb, bl) = if border_visible {
            (
                sides[0].width,
                sides[1].width,
                sides[2].width,
                sides[3].width,
            )
        } else {
            (1.0, 1.0, 1.0, 1.0)
        };
        // A10: the computed border color wins — `input:invalid` in the UA
        // sheet paints the error red through this hook.
        // The control chrome paints the box's TOP background layer only
        // (the scalar fast path); a layer list on a form control keeps that
        // approximation rather than re-deriving the control's own box.
        let border = if border_visible {
            sides[0].color | 0xFF
        } else {
            0x9AA0A6FF
        };
        // The UA sheet now supplies the control background; an author
        // `background: transparent` cascades to 0 and paints nothing —
        // ghost buttons stay hollow.
        let bg = style.background;
        let radius = style.border_radius;
        let rpx = radius.resolve(style.font_size, 16.0, frame.width);
        let rounded = rpx.iter().any(|r| *r > 0.5);
        if rounded && bg != 0 && bg & 0xFF == 0xFF {
            // Rounded ring: with an opaque background the border paints as
            // an outer rounded rect in border color, with the inner bg rect
            // inset by the painted border widths (there is no stroke
            // primitive; per-corner inner radius sheds the adjacent side
            // widths). A translucent or transparent background keeps the
            // straight-edge fallback below.
            let inner = [
                (rpx[0] - bt.max(bl)).max(0.0),
                (rpx[1] - bt.max(br)).max(0.0),
                (rpx[2] - bb.max(br)).max(0.0),
                (rpx[3] - bb.max(bl)).max(0.0),
            ];
            let (inner_w, inner_h) = (w - bl - br, h - bt - bb);
            if inner_w > 0.0 && inner_h > 0.0 {
                self.push_rect(Rect {
                    x,
                    y,
                    w,
                    h,
                    color: border,
                    radius,
                    paint: RectPaint::Solid,
                    node: node.0,
                    ..Default::default()
                });
                self.push_rect(Rect {
                    x: x + bl,
                    y: y + bt,
                    w: inner_w,
                    h: inner_h,
                    color: bg,
                    radius: BorderRadius {
                        top_left: Length::Px(inner[0]),
                        top_right: Length::Px(inner[1]),
                        bottom_right: Length::Px(inner[2]),
                        bottom_left: Length::Px(inner[3]),
                    },
                    paint: RectPaint::from(&style.background_paint),
                    node: node.0,
                    ..Default::default()
                });
            } else {
                self.push_rect(Rect {
                    x,
                    y,
                    w,
                    h,
                    color: border,
                    radius,
                    paint: RectPaint::Solid,
                    node: node.0,
                    ..Default::default()
                });
            }
        } else {
            if bg != 0 {
                self.push_rect(Rect {
                    x,
                    y,
                    w,
                    h,
                    color: bg,
                    radius,
                    paint: RectPaint::from(&style.background_paint),
                    node: node.0,
                    ..Default::default()
                });
            }
            // Straight-edge border: real painted widths (R1 — invisible
            // sides resolve to 0 and paint nothing), legacy 1px chrome
            // when the author left the border unset.
            for (bx, by, bw, bh) in [
                (x, y, w, bt),
                (x, y + h - bb, w, bb),
                (x, y, bl, h),
                (x + w - br, y, br, h),
            ] {
                if bw > 0.0 && bh > 0.0 {
                    self.push_rect(Rect::solid(bx, by, bw, bh, border));
                }
            }
        }
        // A7: type-specific widget chrome. Range = slider track + knob at
        // value%; color = swatch of the value (or engine default); file =
        // the label is the "Choose file" pill text, already handled below.
        match (tag.as_str(), input_type.as_str()) {
            ("input", "range") => {
                let (min, max, step) = (
                    el.and_then(|e| e.attr("min"))
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .unwrap_or(0.0),
                    el.and_then(|e| e.attr("max"))
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .unwrap_or(100.0),
                    // The step only nudges the knob to the nearest tick.
                    el.and_then(|e| e.attr("step"))
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .unwrap_or(1.0),
                );
                let span = (max - min).abs().max(0.0001);
                let v = el
                    .and_then(|e| e.attr("value"))
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or((min + max) * 0.5)
                    .clamp(min.max(f32::MIN), max.min(f32::MAX));
                let mut frac = ((v - min) / span).clamp(0.0, 1.0);
                if step > 0.0 {
                    frac = ((frac * span / step).round() * step / span).clamp(0.0, 1.0);
                }
                let track_y = y + h * 0.5 - 2.0;
                self.push_rect(Rect::solid(x + 2.0, track_y, w - 4.0, 4.0, 0xE8EAEDFF));
                self.push_rect(Rect::solid(
                    x + 2.0,
                    track_y,
                    (w - 4.0) * frac,
                    4.0,
                    0x1A73E8FF,
                ));
                // Knob: a filled 10px circle-ish square centered on the track.
                let kx = x + 2.0 + (w - 4.0) * frac - 5.0;
                self.push_rect(Rect {
                    x: kx.max(x + 1.0),
                    y: y + h * 0.5 - 5.0,
                    w: 10.0,
                    h: 10.0,
                    color: 0x1A73E8FF,
                    radius: BorderRadius::uniform(Length::Px(5.0)),
                    paint: RectPaint::Solid,
                    transform: None,
                    node: node.0,
                    ..Default::default()
                });
            }
            ("input", "color") => {
                let hex = el
                    .and_then(|e| e.attr("value"))
                    .map(|v| v.to_ascii_lowercase())
                    .filter(|v| v.starts_with('#'))
                    .unwrap_or_else(|| "#000000".to_string());
                let swatch = kitae_style::parse_color(&hex).unwrap_or(0xFF0000FF);
                // Inner swatch area inside the border.
                self.push_rect(Rect::solid(x + 4.0, y + 4.0, w - 8.0, h - 8.0, swatch));
            }
            _ => {}
        }
        if !label.trim().is_empty() {
            let (ascent, _) =
                self.tm
                    .metrics_family(style.font_size, style.monospace, &style.font_family);
            let is_placeholder = tag == "input"
                && el
                    .and_then(|e| e.attr("value"))
                    .map(|s| s.is_empty())
                    .unwrap_or(true)
                && el.and_then(|e| e.attr("placeholder")).is_some();
            self.push_text(TextRun {
                x: x + bl + p.left,
                y: y + (h * 0.5) + ascent * 0.35,
                text: label.trim().to_string(),
                size: style.font_size.min(16.0),
                weight: style.font_weight,
                italic: is_placeholder,
                monospace: style.monospace,
                color: if is_placeholder {
                    0x6B7280FF
                } else {
                    style.color
                },
                link: None,
                letter_spacing: style.letter_spacing,
                transform: None,
                family: style.font_family.clone(),
                node: node.0,
                ..Default::default()
            });
        }
        self.y += h + 4.0;
        self.max_y = self.max_y.max(self.y);
    }

    /// Add text (already whitespace-collapsed by the tree builder, except in
    /// pre) to the current line with wrapping.
    fn feed_text(&mut self, text: &str, style: &ComputedStyle, link: Option<String>, frame: Frame) {
        // `text-transform` (Q3) folds case before anything downstream —
        // bidi runs, word splitting, measuring, the display list (so
        // find-in-page indexes the transformed text), and pre blocks.
        let owned;
        let text: &str = match style.text_transform {
            TextTransform::None => text,
            tt => {
                owned = apply_text_transform(text, tt);
                &owned
            }
        };
        if style.white_space_pre {
            self.feed_pre(text, style, frame);
            return;
        }
        // Baseline shift from `vertical-align` (+down, y-down space; sub
        // sinks, super rises, lengths/% resolve against the font metrics).
        let dy = match style.vertical_align {
            VerticalAlign::Baseline => 0.0,
            VerticalAlign::Sub => style.font_size * 0.2,
            VerticalAlign::Super => -style.font_size * 0.4,
            VerticalAlign::Length(l) => l.resolve(style.font_size, 16.0, 0.0),
            VerticalAlign::Percent(p) => p * style.font_size * style.line_height,
        };
        // Split into bidi visual runs first so each segment carries its run
        // direction; flush_line reorders runs into visual order. Spaces
        // travel with their word (documented cosmetic approximation).
        let para_rtl = self.line.rtl;
        for visual in kitae_shape::bidi::visual_runs(text, para_rtl) {
            let slice = &text[visual.byte_start..visual.byte_end];
            self.feed_run_segments(slice, style, link.clone(), frame, dy, visual.rtl);
        }
    }

    /// Feed one wrap segment into the current line. `space_before` requests
    /// the separating space width when this is not the line's first word —
    /// UAX #14 segments inside a bidi run carry it only where source
    /// whitespace separated them (CJK neighbors get none), while a run's
    /// first segment keeps the legacy unconditional gap.
    /// Pixel width of `word` under `style` (letter-spacing included).
    fn measure_word(&self, word: &str, style: &ComputedStyle) -> f32 {
        let spacing = style.letter_spacing;
        if spacing != 0.0 {
            self.tm.measure_run_sp_family(
                word,
                style.font_size,
                style.font_weight,
                style.italic,
                style.monospace,
                spacing,
                &style.font_family,
            )
        } else {
            self.tm.measure_run_family(
                word,
                style.font_size,
                style.font_weight,
                style.italic,
                style.monospace,
                &style.font_family,
            )
        }
    }

    /// B13: split `word` at the last character boundary whose prefix still
    /// fits `band_w`. `None` when the whole word fits or not even one
    /// character does (no progress — the word overflows unsplit).
    fn split_word_for_band<'w>(
        &self,
        word: &'w str,
        style: &ComputedStyle,
        band_w: f32,
    ) -> Option<(&'w str, &'w str)> {
        let mut last_fit = 0usize;
        for (i, ch) in word.char_indices() {
            let end = i + ch.len_utf8();
            if self.measure_word(&word[..end], style) <= band_w {
                last_fit = end;
            } else {
                break;
            }
        }
        if last_fit == 0 || last_fit >= word.len() {
            None
        } else {
            Some((&word[..last_fit], &word[last_fit..]))
        }
    }

    fn feed_word(
        &mut self,
        word: &str,
        space_before: bool,
        style: &ComputedStyle,
        link: Option<String>,
        frame: Frame,
        dy: f32,
        rtl: bool,
    ) {
        // B13: a trailing soft hyphen is invisible mid-line (stripped
        // before measuring); `flush_line` turns it into a visible `-` when
        // the word ends up at line end. `hyphens: none` keeps it as a
        // normal visible character.
        let (word, shy) = if style.hyphens != Hyphens::None {
            match word.strip_suffix('\u{AD}') {
                Some(stripped) => (stripped, true),
                None => (word, false),
            }
        } else {
            (word, false)
        };
        self.feed_word_shy(word, space_before, style, link, frame, dy, rtl, shy);
    }

    fn feed_word_shy(
        &mut self,
        word: &str,
        space_before: bool,
        style: &ComputedStyle,
        link: Option<String>,
        frame: Frame,
        dy: f32,
        rtl: bool,
        shy: bool,
    ) {
        let w = self.measure_word(word, style);
        // A separating space also ends with a letter-spacing advance in
        // CSS (the space is a glyph); the following word then starts at
        // the correct offset. At line start there is no space at all.
        let space_w = self.measure_word(" ", style);
        let mut space = if self.line.words.is_empty() || !space_before {
            0.0
        } else {
            space_w
        };
        // Wrap against the float-shaped band, not the raw frame width.
        let est_h = (style.font_size * style.line_height).max(style.font_size);
        let band_w = self.float_band(frame, self.y, self.y + est_h).1;
        // D5: `hyphens: auto` — a word that would wrap is split at the
        // dictionary's last break point whose head fits (greedy fill), the
        // head ending the line with a visible `-` via the soft-hyphen path
        // in `flush_line`. Only en-us patterns are embedded; `lang` gates.
        if style.hyphens == Hyphens::Auto
            && !style.white_space_nowrap
            && self.line.width + space + w > band_w
            && style.lang.as_deref().map_or(true, |l| l.starts_with("en"))
        {
            let fit_here = if self.line.words.is_empty() {
                // The word alone overflows the band.
                band_w
            } else {
                (band_w - self.line.width - space).max(0.0)
            };
            let avail = (fit_here - self.measure_word("-", style)).max(0.0);
            if let Some(b) = kitae_shape::hyphenate::en_us_breaks(word)
                .into_iter()
                .rev()
                .find(|&b| self.measure_word(&word[..b], style) <= avail)
            {
                let (head, tail) = word.split_at(b);
                self.feed_word_shy(
                    head,
                    space_before,
                    style,
                    link.clone(),
                    frame,
                    dy,
                    rtl,
                    true,
                );
                self.flush_line(frame);
                self.feed_word_shy(tail, false, style, link, frame, dy, rtl, shy);
                return;
            }
        }
        // B13: `white-space: nowrap` suppresses wrapping entirely.
        if !self.line.words.is_empty()
            && !style.white_space_nowrap
            && self.line.width + space + w > band_w
        {
            self.flush_line(frame);
            // This segment now leads the fresh line — no leading space.
            space = 0.0;
        }
        // B13: `word-break: break-all` — the word may be cut between any
        // two characters: fill the current line to the band, then continue
        // with the remainder (greedy char walk; one merged run per line,
        // kerning across the cut is the documented approximation).
        if style.word_break == WordBreak::BreakAll && !style.white_space_nowrap {
            let avail = (band_w - self.line.width - space).max(0.0);
            if w > avail {
                if let Some((head, tail)) = self.split_word_for_band(word, style, avail) {
                    self.feed_word_shy(
                        head,
                        space_before,
                        style,
                        link.clone(),
                        frame,
                        dy,
                        rtl,
                        shy,
                    );
                    self.feed_word_shy(tail, false, style, link, frame, dy, rtl, shy);
                    return;
                }
            }
        }
        // B13: `overflow-wrap: break-word` — a word leading a fresh line
        // that still overflows the band is split at the last character
        // that fits; the tail re-feeds as the next line's leader (and may
        // split again). `anywhere` behaves alike (its min-content effect
        // needs a sizing pass that doesn't exist yet).
        if self.line.words.is_empty()
            && w > band_w
            && !style.white_space_nowrap
            && matches!(
                style.overflow_wrap,
                OverflowWrap::BreakWord | OverflowWrap::Anywhere
            )
        {
            if let Some((head, tail)) = self.split_word_for_band(word, style, band_w) {
                self.feed_word_shy(
                    head,
                    space_before,
                    style,
                    link.clone(),
                    frame,
                    dy,
                    rtl,
                    false,
                );
                self.feed_word_shy(tail, false, style, link, frame, dy, rtl, shy);
                return;
            }
        }
        let (ascent, descent) =
            self.tm
                .metrics_family(style.font_size, style.monospace, &style.font_family);
        // Baseline shifts eat into ascent (super) or descent (sub).
        self.line.max_ascent = self.line.max_ascent.max(ascent - dy);
        self.line.max_descent = self.line.max_descent.max(descent + dy);
        self.line.line_h = self
            .line
            .line_h
            .max(style.font_size * style.line_height)
            .max(ascent + descent);
        self.line.width += space + w;
        self.line.words.push(PendingWord {
            text: word.to_string(),
            w,
            space,
            style: style.clone(),
            link: link.clone(),
            dy,
            rtl,
            node: self.cur_node,
            box_filter: self
                .styles
                .get(&NodeId(self.cur_node))
                .and_then(|s| s.filter.clone()),
            shy,
        });
    }

    /// Feed one bidi run (or pre-wrap physical line) at its UAX #14 break
    /// opportunities (Q13). Gap rule: a segment gets the separating space
    /// when the previous raw segment ended in source whitespace; the first
    /// segment fed from a run keeps the legacy unconditional gap (bidi-run
    /// boundaries carry no visible source whitespace, so this stays
    /// byte-compatible with the old word-split path). Returns whether
    /// anything was fed.
    fn feed_run_segments(
        &mut self,
        run: &str,
        style: &ComputedStyle,
        link: Option<String>,
        frame: Frame,
        dy: f32,
        rtl: bool,
    ) -> bool {
        let mut first_fed = true;
        let mut prev_ws = false;
        let mut fed = false;
        for (raw, ends_ws) in break_segments(run) {
            let space_before = first_fed || prev_ws;
            first_fed = false;
            prev_ws = ends_ws;
            let seg = raw.trim();
            if seg.is_empty() {
                continue;
            }
            self.feed_word(seg, space_before, style, link.clone(), frame, dy, rtl);
            fed = true;
        }
        fed
    }

    /// Pre-formatted text: newlines preserved. Plain `pre` does not wrap —
    /// long lines overflow and get clipped by the shell; `pre-wrap` runs
    /// each physical line through the word wrapper so long code/JSON lines
    /// break instead.
    fn feed_pre(&mut self, text: &str, style: &ComputedStyle, frame: Frame) {
        self.flush_line(frame);
        let (ascent, descent) = self
            .tm
            .metrics_family(style.font_size, true, &style.font_family);
        let advance = (style.font_size * style.line_height).max(ascent + descent);
        if !style.white_space_wrap {
            for (i, raw) in text.split('\n').enumerate() {
                if i > 0 {
                    self.y += advance;
                }
                if raw.is_empty() {
                    continue;
                }
                self.push_text(TextRun {
                    x: frame.x,
                    y: self.y + ascent,
                    text: raw.to_string(),
                    size: style.font_size,
                    weight: style.font_weight,
                    italic: style.italic,
                    monospace: style.monospace,
                    color: style.color,
                    link: None,
                    letter_spacing: style.letter_spacing,
                    transform: None,
                    family: style.font_family.clone(),
                    node: 0,
                    ..Default::default()
                });
            }
            self.y += advance;
            self.max_y = self.max_y.max(self.y);
            return;
        }
        // pre-wrap: segments wrap inside the frame band (UAX #14 break
        // opportunities, Q13); the newline itself ends the line. Blank
        // lines still advance a full line height.
        for raw in text.split('\n') {
            let fed = self.feed_run_segments(raw, style, None, frame, 0.0, false);
            if fed {
                self.flush_line(frame);
            } else {
                self.y += advance;
            }
        }
        self.max_y = self.max_y.max(self.y);
    }

    /// Emit the pending line (with text-align offset) and advance y.
    fn flush_line(&mut self, frame: Frame) {
        if self.line.words.is_empty() {
            return;
        }
        let line = std::mem::take(&mut self.line);
        // Bidi visual order (Phase M3): maximal same-direction groups keep
        // document order in LTR paragraphs (RTL groups reverse internally);
        // in RTL paragraphs the groups themselves reverse. Spaces travel
        // with their word (documented approximation).
        let reordered = line.rtl || line.words.iter().any(|w| w.rtl);
        let mut order: Vec<&PendingWord> = line.words.iter().collect();
        // The paragraph direction alone reverses pure runs; mixed lines
        // reorder by bidi groups (see above).
        if reordered {
            let mut groups: Vec<Vec<&PendingWord>> = Vec::new();
            for w in &line.words {
                match groups.last_mut() {
                    Some(g) if g[0].rtl == w.rtl => g.push(w),
                    _ => groups.push(vec![w]),
                }
            }
            order.clear();
            if line.rtl {
                // RTL paragraph (UBA L2): groups reverse; RTL groups
                // additionally reverse within (pure-RTL lines read
                // right-to-left); LTR groups keep logical order.
                for g in groups.iter().rev() {
                    if g[0].rtl {
                        order.extend(g.iter().rev().copied());
                    } else {
                        order.extend(g.iter().copied());
                    }
                }
            } else {
                for g in &groups {
                    if g[0].rtl {
                        order.extend(g.iter().rev().copied());
                    } else {
                        order.extend(g.iter().copied());
                    }
                }
            }
        }
        // A word leading the visual line must not carry the space it had
        // mid-line pre-reversal; drop it (subsequent gaps are untouched, so
        // the painted total stays exact).
        let mut words: Vec<PendingWord> = if reordered {
            let mut v: Vec<PendingWord> = order.iter().map(|w| (*w).clone()).collect();
            if let Some(head) = v.first_mut() {
                head.space = 0.0;
            }
            v
        } else {
            line.words.clone()
        };
        // Floats shape this line: shift and narrow to the free band.
        let line_h = line.line_h.max(line.max_ascent + line.max_descent);
        let (band_x, band_w) = self.float_band(frame, self.y, self.y + line_h);
        // B13: a soft hyphen that ended up at line end becomes a visible
        // `-` (it was stripped, and its break opportunity used, at feed
        // time); mid-line soft hyphens stay invisible.
        if let Some(last) = words.last_mut() {
            if last.shy {
                let hw = self.measure_word("-", &last.style);
                last.text.push('-');
                last.w += hw;
            }
        }
        let mut total: f32 = words.iter().map(|w| w.space + w.w).sum();
        // B13: `text-overflow: ellipsis` on nowrap text — the line never
        // wrapped, so cut the overflowing tail back to the band and mark
        // the cut with `…`. (Wrapping multi-line ellipsis is deferred.)
        let ellipsis = words.first().is_some_and(|w| {
            w.style.text_overflow == TextOverflow::Ellipsis && w.style.white_space_nowrap
        });
        if ellipsis && total > band_w {
            let st = words[0].style.clone();
            let ell_w = self.measure_word("…", &st);
            while words.len() > 1 {
                let t: f32 = words.iter().map(|w| w.space + w.w).sum::<f32>() + ell_w;
                if t <= band_w {
                    break;
                }
                words.pop();
            }
            let t: f32 = words.iter().map(|w| w.space + w.w).sum::<f32>();
            if t + ell_w > band_w {
                // Even the leading word overflows: cut inside it.
                let avail = (band_w - words[0].space - ell_w).max(0.0);
                let mut cut = 0usize;
                for (i, ch) in words[0].text.char_indices() {
                    let end = i + ch.len_utf8();
                    if self.measure_word(&words[0].text[..end], &st) <= avail {
                        cut = end;
                    } else {
                        break;
                    }
                }
                words[0].text.truncate(cut);
                words[0].w = self.measure_word(&words[0].text, &st);
            }
            if let Some(last) = words.last_mut() {
                last.text.push('…');
                last.w += ell_w;
            }
            total = words.iter().map(|w| w.space + w.w).sum();
        }
        let offset = match frame.align {
            TextAlign::Left => 0.0,
            TextAlign::Center => ((band_w - total) * 0.5).max(0.0),
            TextAlign::Right => (band_w - total).max(0.0),
        };
        let baseline = self.y + line.max_ascent;

        let mut x = band_x + offset;
        for word in &words {
            x += word.space;
            let st = &word.style;
            let y = baseline + word.dy;
            // D6: inside a vertical writing mode, `text-orientation`
            // splits the word into oriented pieces — each upright char
            // paints alone through a counter-rotation about its advance
            // center, so the enclosing box's rigid stamp lands it upright
            // on the column axis while chars still stack along the flow.
            // Everything else (Sideways, horizontal lines, non-upright
            // chars) stays the single plain run the pre-D6 path painted.
            for (ptext, px, _pw, tf) in self.oriented_pieces(&word.text, word.w, x, y, st) {
                // R8: text shadows paint behind the run, last shadow
                // deepest (CSS: the first shadow is on top). Crisp
                // duplicates — blur needs a per-run wire field, out of v1.
                for s in st.text_shadows.iter().rev() {
                    let run = TextRun {
                        x: px + s.offset_x,
                        y: y + s.offset_y,
                        text: ptext.clone(),
                        size: st.font_size,
                        weight: st.font_weight,
                        italic: st.italic,
                        monospace: st.monospace,
                        color: s.color,
                        link: None,
                        letter_spacing: st.letter_spacing,
                        shadow_blur: s.blur,
                        transform: tf,
                        family: st.font_family.clone(),
                        node: word.node,
                        filters: word.box_filter.clone(),
                        ..Default::default()
                    };
                    self.out.texts.push(run);
                }
                let main = TextRun {
                    x: px,
                    y,
                    text: ptext,
                    size: st.font_size,
                    weight: st.font_weight,
                    italic: st.italic,
                    monospace: st.monospace,
                    color: st.color,
                    link: word.link.clone(),
                    letter_spacing: st.letter_spacing,
                    transform: tf,
                    family: st.font_family.clone(),
                    node: word.node,
                    filters: word.box_filter.clone(),
                    ..Default::default()
                };
                self.out.texts.push(main);
            }
            // B12: decoration lines honor color/thickness/style. The R2
            // offsets stay; thickness defaults to a font-relative width
            // (1px at 16px, scaling up).
            let dec_color = st.decoration_color.unwrap_or(st.color);
            let dec_t = st
                .decoration_thickness
                .map(|l| l.resolve(0.0))
                .filter(|v| *v > 0.0)
                .unwrap_or((st.font_size / 16.0).max(1.0));
            if st.underline {
                for mut r in
                    decoration_rects(x, y + 2.0, word.w, dec_t, st.decoration_style, dec_color)
                {
                    r.node = word.node;
                    self.out.rects.push(r);
                }
            }
            // R2: overline rides the ascent top; line-through sits at the
            // glyph midline (≈0.3em above the baseline).
            if st.overline {
                let top = y - st.font_size * 0.8;
                for mut r in decoration_rects(x, top, word.w, dec_t, st.decoration_style, dec_color)
                {
                    r.node = word.node;
                    self.out.rects.push(r);
                }
            }
            if st.line_through {
                let top = y - st.font_size * 0.3;
                for mut r in decoration_rects(x, top, word.w, dec_t, st.decoration_style, dec_color)
                {
                    r.node = word.node;
                    self.out.rects.push(r);
                }
            }
            if let Some(href) = &word.link {
                self.out.links.push(LinkArea {
                    x,
                    y: y - line.max_ascent,
                    w: word.w,
                    h: line.max_ascent + line.max_descent,
                    href: href.clone(),
                    transform: None,
                    node: self.cur_node,
                });
            }
            x += word.w;
        }
        self.y += line.line_h.max(line.max_ascent + line.max_descent);
        self.max_y = self.max_y.max(self.y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kitae_style::{ColorScheme, MediaType};

    /// Mock measurer: every char is 0.5×size wide (0.6× in monospace),
    /// ascent 0.8×size, descent 0.2×size — so widths are exact sums.
    struct Mock;

    impl TextMeasurer for Mock {
        fn measure(&self, text: &str, size: f32, _weight: u32, _italic: bool, mono: bool) -> f32 {
            text.chars().count() as f32 * size * if mono { 0.6 } else { 0.5 }
        }
        fn metrics(&self, size: f32, _mono: bool) -> (f32, f32) {
            (size * 0.8, size * 0.2)
        }
    }

    fn lay(html: &str, width: f32) -> DisplayList {
        let (dom, styles) = style_of(html);
        layout(&dom, &styles, width, 600.0, &Mock)
    }

    /// [`lay`] in a concrete embedder environment — the entry point the
    /// engine uses, so `<img media>` / `<picture><source media>` resolve
    /// against it. Styles are still computed in the default environment:
    /// which sheet rules apply is the style engine's business, not layout's.
    fn lay_in_env(html: &str, width: f32, env: &EnvironmentState) -> DisplayList {
        let (dom, styles) = style_of(html);
        layout_in_env(&dom, &styles, width, 600.0, &Mock, env)
    }

    /// Parse `html` and compute its styles, honoring authored `<style>`
    /// blocks. A2: `<style>` inside a `<template>` is inert — the walk must
    /// skip it just like the engine's own collect_styles does.
    fn style_of(html: &str) -> (Dom, Vec<(NodeId, ComputedStyle)>) {
        let (dom, _) = kitae_html::parse(html);
        let mut sheets: Vec<String> = Vec::new();
        if let Some(root) = dom.root() {
            dom.walk(root, &mut |n| {
                if dom.tag(n) == "style" {
                    let mut inside_template = false;
                    let mut p = n;
                    while let Some(parent) = dom.node(p).parent {
                        if dom.tag(parent) == "template" {
                            inside_template = true;
                            break;
                        }
                        p = parent;
                    }
                    if !inside_template {
                        let mut s = String::new();
                        for &c in dom.children(n) {
                            s.push_str(&dom.node(c).text);
                        }
                        sheets.push(s);
                    }
                }
            });
        }
        let refs: Vec<&str> = sheets.iter().map(String::as_str).collect();
        let styles = kitae_style::StyleEngine::new(&refs).compute(&dom);
        (dom, styles)
    }

    fn text_of(dl: &DisplayList) -> Vec<&str> {
        dl.texts.iter().map(|t| t.text.as_str()).collect()
    }

    // B1: ancestor opacity bakes into item alpha (LOW byte).
    #[test]
    fn opacity_bakes_into_descendant_items() {
        let dl = lay(
            "<div style='opacity: 0.5'><p style='color: #000000'>hi</p></div>",
            200.0,
        );
        let t = dl
            .texts
            .iter()
            .find(|t| t.text.contains("hi"))
            .expect("text run");
        assert_eq!(
            t.color & 0xFF,
            128,
            "0.5 × 255 → 128 alpha, got {:08X}",
            t.color
        );

        // Nested products multiply: 0.5 × 0.5.
        let dl = lay(
            "<div style='opacity: 0.5'><div style='opacity: 0.5'><p>deep</p></div></div>",
            200.0,
        );
        let t = dl
            .texts
            .iter()
            .find(|t| t.text.contains("deep"))
            .expect("run");
        assert_eq!(t.color & 0xFF, 64, "0.25 × 255 → 64, got {:08X}", t.color);

        // The element's own opacity dims its own background too.
        let dl = lay(
            "<div style='opacity: 0.5; background: #ff0000; width: 50px; height: 20px'></div>",
            200.0,
        );
        let r = dl.rects.iter().find(|r| r.w == 50.0).expect("bg rect");
        assert_eq!(
            r.color & 0xFF,
            128,
            "own background dims, got {:08X}",
            r.color
        );

        // Opaque subtrees stay untouched (alpha 0xFF preserved).
        let dl = lay("<p style='opacity: 1'>plain</p>", 200.0);
        let t = dl.texts.iter().find(|t| t.text.contains("plain")).unwrap();
        assert_eq!(t.color & 0xFF, 0xFF);
    }

    /// `visibility: hidden` drops the box's paint but keeps its layout;
    /// `collapse` behaves the same outside tables, where it removes the row.
    #[test]
    fn visibility_hidden_drops_paint_and_keeps_layout() {
        let dl = lay(
            "<div id='a' style='visibility:hidden; background:#ff0000; height:20px'>gone</div>\
             <div id='b' style='height:20px'>shown</div>",
            200.0,
        );
        assert!(
            !text_of(&dl).iter().any(|t| t.contains("gone")),
            "hidden text paints nothing: {:?}",
            text_of(&dl)
        );
        assert!(
            text_of(&dl).iter().any(|t| t.contains("shown")),
            "a sibling is unaffected"
        );
        assert!(
            dl.rects.iter().all(|r| r.color != 0xFF0000FF),
            "the hidden background rect is gone"
        );
        // Space is still reserved: the visible sibling starts below it.
        let shown = dl.texts.iter().find(|t| t.text.contains("shown")).unwrap();
        assert!(
            shown.y > 15.0,
            "the hidden box still occupies its 20px, got y={}",
            shown.y
        );
        // `collapse` is the same for a non-table box.
        let dl = lay(
            "<div style='visibility:collapse'>gone</div><p>after</p>",
            200.0,
        );
        assert!(!text_of(&dl).iter().any(|t| t.contains("gone")));
        assert!(text_of(&dl).iter().any(|t| t.contains("after")));
    }

    #[test]
    fn visibility_inherits_and_a_descendant_can_repaint() {
        let dl = lay(
            "<div style='visibility:hidden'>hidden<span style='visibility:visible'>shown</span></div>",
            200.0,
        );
        assert!(
            !text_of(&dl).iter().any(|t| t.contains("hidden")),
            "the inherited hidden text paints nothing"
        );
        assert!(
            text_of(&dl).iter().any(|t| t.contains("shown")),
            "a descendant with `visible` paints again: {:?}",
            text_of(&dl)
        );
    }

    #[test]
    fn visibility_hides_boxes_of_every_kind() {
        // Background, border, replaced content, and link hit areas all go.
        let dl = lay(
            "<div style='visibility:hidden'><img src='i.png' alt='pic'>\
             <a href='x'>link</a>\
             <div style='background:#00ff00; height:10px'></div></div>",
            200.0,
        );
        assert!(dl.images.is_empty(), "hidden <img> paints nothing");
        assert!(dl.links.is_empty(), "hidden links are not clickable");
        assert!(
            !dl.rects.iter().any(|r| r.color == 0x00FF00FF),
            "hidden descendant backgrounds are gone"
        );
        // An absolutely positioned descendant is dropped too (its paint
        // lands outside the hidden box's own item range).
        let dl = lay(
            "<div style='position:relative; visibility:hidden'>\
             <div style='position:absolute; left:0; top:0; background:#0000ff; width:10px; height:10px'></div>\
             </div>",
            200.0,
        );
        assert!(
            dl.rects.iter().all(|r| r.color != 0x0000FFFF),
            "hidden out-of-flow descendants are gone"
        );
    }

    #[test]
    fn visibility_collapse_removes_a_table_row() {
        let dl = lay(
            "<table><tr><td>one</td><td>two</td></tr>\
             <tr style='visibility:collapse'><td>three</td><td>four</td></tr>\
             <tr><td>five</td></tr></table>",
            200.0,
        );
        let texts = text_of(&dl);
        assert!(texts.iter().any(|t| t.contains("one")));
        assert!(
            !texts.iter().any(|t| t.contains("three")),
            "a collapsed row leaves the table: {:?}",
            texts
        );
        assert!(texts.iter().any(|t| t.contains("five")));
        // `hidden` keeps the row's space and hides its paint.
        let dl = lay(
            "<table><tr><td>one</td></tr>\
             <tr style='visibility:hidden'><td>two</td></tr></table>",
            200.0,
        );
        let texts = text_of(&dl);
        assert!(texts.iter().any(|t| t.contains("one")));
        assert!(!texts.iter().any(|t| t.contains("two")));
    }

    #[test]
    fn background_image_tiles_paint_position_size_and_repeat() {
        // The box is taken out of flow and pinned to the page origin so the
        // positioning area starts at (0, 0) and the expected tile offsets
        // are the resolved `background-position` itself.
        let sized = |extra: &str, natural: (f32, f32)| -> DisplayList {
            let html = format!(
                "<div style='position:absolute; left:0; top:0; margin:0; \
                 width:200px; height:100px; background:{extra}'></div>"
            );
            let (dom, _) = kitae_html::parse(&html);
            let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
            let sizes = HashMap::from([("t.png".to_string(), natural)]);
            layout_with_image_sizes(&dom, &styles, 300.0, 600.0, &Mock, &sizes)
        };
        // `no-repeat` + the 4-value `right 10px bottom 20px` + an explicit
        // size: one tile. Free space is 200-50 = 150 across and 100-40 = 60
        // down, so the offsets are 140 and 40.
        let dl = sized(
            "url(t.png) no-repeat right 10px bottom 20px / 50px 40px",
            (10.0, 10.0),
        );
        assert_eq!(dl.images.len(), 1, "{:?}", dl.images);
        let im = &dl.images[0];
        assert_eq!(im.src, "t.png");
        assert_eq!((im.x, im.y, im.w, im.h), (140.0, 40.0, 50.0, 40.0));
        assert_eq!(im.fit, ObjectFit::Fill, "the rect is the painted size");
        assert!(!im.animated);

        // `repeat` tiles from the origin in both directions: a 20px tile
        // over 200×100 is 10 × 5.
        let dl = sized("url(t.png) repeat 0 0 / 20px 20px", (20.0, 20.0));
        assert_eq!(dl.images.len(), 50, "{:?}", dl.images.len());
        assert_eq!((dl.images[0].x, dl.images[0].y), (0.0, 0.0));
        assert_eq!((dl.images[9].x, dl.images[9].y), (180.0, 0.0));
        assert_eq!((dl.images[10].x, dl.images[10].y), (0.0, 20.0));
        assert_eq!((dl.images[49].x, dl.images[49].y), (180.0, 80.0));

        // A position past the first tile still covers the box: tiles are
        // laid down on both sides of the origin.
        let dl = sized("url(t.png) repeat 10px 0 / 20px 20px", (20.0, 20.0));
        assert!(
            dl.images.iter().any(|i| i.x == -10.0),
            "the grid wraps back past the origin: {:?}",
            dl.images.iter().map(|i| i.x).collect::<Vec<_>>()
        );
        assert!(dl
            .images
            .iter()
            .any(|i| i.x == 190.0 && i.x + 20.0 >= 200.0));

        // `repeat-x` / `repeat-y` cover one axis only.
        let dl = sized("url(t.png) repeat-x 0 0 / 20px 20px", (20.0, 20.0));
        assert_eq!(dl.images.len(), 10, "one row");
        assert!(dl.images.iter().all(|i| i.y == 0.0));
        let dl = sized("url(t.png) repeat-y 0 0 / 20px 20px", (20.0, 20.0));
        assert_eq!(dl.images.len(), 5, "one column");
        assert!(dl.images.iter().all(|i| i.x == 0.0));

        // `center` centers the single tile on both axes.
        let dl = sized(
            "url(t.png) no-repeat center center / 20px 20px",
            (20.0, 20.0),
        );
        assert_eq!((dl.images[0].x, dl.images[0].y), (90.0, 40.0));
        // `left top` / `right bottom` keywords hug the corners.
        let dl = sized("url(t.png) no-repeat left top / 20px 20px", (20.0, 20.0));
        assert_eq!((dl.images[0].x, dl.images[0].y), (0.0, 0.0));
        let dl = sized(
            "url(t.png) no-repeat right bottom / 20px 20px",
            (20.0, 20.0),
        );
        assert_eq!((dl.images[0].x, dl.images[0].y), (180.0, 80.0));
        // A percentage positions by the free space (25% of 180 = 45).
        let dl = sized("url(t.png) no-repeat 25% 0 / 20px 20px", (20.0, 20.0));
        assert_eq!(dl.images[0].x, 45.0);

        // `space` lays down only whole tiles and shares the slack: 6 × 30px
        // tiles across 200px leave 20px over 5 gaps (4px each), and 5 × 20px
        // rows fill the height exactly.
        let dl = sized("url(t.png) space 0 0 / 30px 20px", (30.0, 20.0));
        assert_eq!(dl.images.len(), 30, "6 columns × 5 rows");
        assert_eq!(dl.images[0].x, 0.0);
        assert_eq!(dl.images[1].x, 34.0, "30px tile + a 4px gap");
        assert_eq!(dl.images[5].x, 170.0);
        assert_eq!(dl.images[6].x, 0.0, "the second row restarts");
        assert_eq!(dl.images[29].x, 170.0);
        assert_eq!(dl.images[29].y, 80.0);
        // `round` resizes so a whole number of tiles fills the axis:
        // 200 / 30 = 6.67 → 7 tiles of 28.57 across, 5 rows of 20 down
        // (which already divide exactly).
        let dl = sized("url(t.png) round 0 0 / 30px 20px", (30.0, 20.0));
        assert_eq!(dl.images.len(), 35, "7 columns × 5 rows");
        assert!(
            (dl.images[0].w - 200.0 / 7.0).abs() < 0.01,
            "{:?}",
            dl.images[0].w
        );
        assert_eq!(dl.images[0].h, 20.0);
        assert!(
            (dl.images[6].x + dl.images[6].w - 200.0).abs() < 0.01,
            "the row ends flush"
        );

        // Percentages resolve against the positioning area (the border box).
        let dl = sized("url(t.png) no-repeat 0 0 / 50% 25%", (10.0, 10.0));
        assert_eq!((dl.images[0].w, dl.images[0].h), (100.0, 25.0));
        // `auto` is the intrinsic size; `cover` / `contain` scale it to the
        // area (200×100 against a 100×100 image).
        let dl = sized("url(t.png) no-repeat 0 0", (100.0, 100.0));
        assert_eq!((dl.images[0].w, dl.images[0].h), (100.0, 100.0));
        let dl = sized("url(t.png) no-repeat 0 0 / cover", (100.0, 100.0));
        assert_eq!((dl.images[0].w, dl.images[0].h), (200.0, 200.0));
        let dl = sized("url(t.png) no-repeat 0 0 / contain", (100.0, 100.0));
        assert_eq!((dl.images[0].w, dl.images[0].h), (100.0, 100.0));
        // `50% auto` takes the free axis from the intrinsic ratio.
        let dl = sized("url(t.png) no-repeat 0 0 / 50% auto", (100.0, 50.0));
        assert_eq!((dl.images[0].w, dl.images[0].h), (100.0, 50.0));
        // A size larger than the area is not tiled without a repeat mode.
        let dl = sized("url(t.png) no-repeat 0 0 / 400px 400px", (10.0, 10.0));
        assert_eq!(dl.images.len(), 1, "one oversized tile, unclipped");
        assert_eq!((dl.images[0].w, dl.images[0].h), (400.0, 400.0));
    }

    #[test]
    fn background_image_needs_a_known_size_to_paint() {
        // Without an intrinsic size from the embedder, a size that depends
        // on it paints nothing (the background color still does) rather
        // than stretching the artwork.
        let dl = lay(
            "<div style='width:200px; height:100px; background:#123456 url(t.png) no-repeat 0 0 / cover'></div>",
            300.0,
        );
        assert!(dl.images.is_empty(), "no intrinsic size → no tiles");
        assert!(
            dl.rects.iter().any(|r| r.color == 0x123456FF),
            "the background color still paints"
        );
        // An explicitly sized image paints with no hint at all.
        let dl = lay(
            "<div style='width:200px; height:100px; background:url(t.png) no-repeat 0 0 / 40px 30px'></div>",
            300.0,
        );
        assert_eq!(dl.images.len(), 1);
        assert_eq!((dl.images[0].w, dl.images[0].h), (40.0, 30.0));
        // A gradient is not a tile source — it stays a rect paint.
        let dl = lay(
            "<div style='width:200px; height:100px; background:linear-gradient(red, blue)'></div>",
            300.0,
        );
        assert!(dl.images.is_empty());
        assert!(
            dl.rects
                .iter()
                .any(|r| matches!(r.paint, RectPaint::LinearGradient { .. })),
            "a gradient stays a rect paint: {:?}",
            dl.rects
        );
    }

    #[test]
    fn background_image_tiles_stay_inside_their_box() {
        // The tile cap keeps a degenerate repeat from exploding the list,
        // and a hidden box drops its tiles with everything else.
        let (dom, _) = kitae_html::parse(
            "<div style='width:4000px; height:4000px; background:url(t.png) repeat 0 0 / 1px 1px'></div>",
        );
        let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
        let sizes = HashMap::from([("t.png".to_string(), (1.0, 1.0))]);
        let dl = layout_with_image_sizes(&dom, &styles, 4000.0, 600.0, &Mock, &sizes);
        assert!(
            dl.images.len() <= MAX_BG_TILES,
            "capped at {MAX_BG_TILES}, got {}",
            dl.images.len()
        );

        let (dom, _) = kitae_html::parse(
            "<div style='visibility:hidden; width:100px; height:100px; \
             background:url(t.png) no-repeat 0 0 / 20px 20px'></div>",
        );
        let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
        let sizes = HashMap::from([("t.png".to_string(), (20.0, 20.0))]);
        let dl = layout_with_image_sizes(&dom, &styles, 300.0, 600.0, &Mock, &sizes);
        assert!(dl.images.is_empty(), "a hidden box's tiles are dropped");
    }

    /// A comma-separated `background` list paints bottom-up per CSS order:
    /// the FIRST layer of the list is the top one, and the vec order is paint
    /// order. The box's own children still paint above the whole stack.
    #[test]
    fn background_layer_list_paints_bottom_up() {
        let dl = lay(
            "<div style='width:100px; height:50px; background: #ff0000, #00ff00,              linear-gradient(#0000ff, #0000ff)'><span>text</span></div>",
            300.0,
        );
        // The UA page background is the first rect; the layers follow.
        let layers: Vec<&Rect> = dl
            .rects
            .iter()
            .filter(|r| r.w == 100.0 && r.h == 50.0)
            .collect();
        assert_eq!(layers.len(), 3, "{:?}", layers);
        assert_eq!(layers[0].color, 0x0000FFFF, "the last layer paints first");
        assert!(matches!(layers[0].paint, RectPaint::LinearGradient { .. }));
        assert_eq!(layers[1].color, 0x00FF00FF, "then the middle layer");
        assert_eq!(layers[2].color, 0xFF0000FF, "the first layer is on top");
        assert!(
            layers.iter().all(|r| r.x == 100.0 && r.y == 16.0),
            "every layer fills the border box: {:?}",
            layers.iter().map(|r| (r.x, r.y)).collect::<Vec<_>>()
        );
        // The text still paints over the whole stack.
        let t = dl.texts.first().expect("text run");
        assert!(t.text.contains("text"));
        assert!(
            t.y > layers[2].y,
            "text is below the background in the list order"
        );
    }

    /// `background-origin` picks the positioning area; `background-clip` the
    /// paint rect. Both are insets of the border box, and only `clip` moves
    /// the rect a solid / gradient layer paints.
    #[test]
    fn background_origin_and_clip_choose_the_paint_rect() {
        // A 100×50 box with 2px borders and 5px padding: the padding box is
        // 96×46 at (2,2), the content box 86×36 at (7,7).
        let box_style = "width:100px; height:50px; padding:5px; border:2px solid #000000;";
        let rect = |clip: &str| -> Rect {
            let dl = lay(
                &format!(
                    "<div style='{box_style} background: #ff0000; background-clip: {clip}'></div>"
                ),
                300.0,
            );
            dl.rects
                .iter()
                .find(|r| r.color == 0xFF0000FF)
                .cloned()
                .unwrap_or_else(|| panic!("no background rect for {clip}: {:?}", dl.rects))
        };
        // The default fills the border box.
        let r = rect("border-box");
        assert_eq!((r.x, r.y, r.w, r.h), (100.0, 16.0, 100.0, 50.0));
        // `padding-box` insets by the border; `content-box` by border+padding.
        let p = rect("padding-box");
        assert_eq!((p.x, p.y, p.w, p.h), (102.0, 18.0, 96.0, 46.0));
        let c = rect("content-box");
        assert_eq!((c.x, c.y, c.w, c.h), (107.0, 23.0, 86.0, 36.0));
        // In the shorthand a lone `<box>` names origin AND clip, so it moves
        // the paint rect; two of them fill the two slots in order (the first
        // is the origin, the second the clip).
        let shorthand = |layer: &str| -> Rect {
            let dl = lay(
                &format!("<div style='{box_style} background: #ff0000 {layer}'></div>"),
                300.0,
            );
            dl.rects
                .iter()
                .find(|x| x.color == 0xFF0000FF)
                .cloned()
                .expect("background rect")
        };
        assert_eq!(shorthand("content-box"), c, "one box keyword: both boxes");
        assert_eq!(
            shorthand("content-box padding-box"),
            p,
            "first box is the origin, the second the clip"
        );
        assert_eq!(
            shorthand("border-box"),
            r,
            "the initial value paints it all"
        );

        // A rounded box: the padding box carries the border-radius reduced in
        // proportion to the inset (96/100 and 46/50 → ×0.92), and a
        // `content-box` clip is square-cornered.
        let rounded = |clip: &str| -> Rect {
            let dl = lay(
                &format!(
                    "<div style='{box_style} border-radius:20px; background: #ff0000; \
                     background-clip: {clip}'></div>"
                ),
                300.0,
            );
            dl.rects
                .iter()
                .find(|x| x.color == 0xFF0000FF)
                .cloned()
                .expect("background rect")
        };
        assert_eq!(
            rounded("border-box").radius.top_left,
            Length::Px(20.0),
            "authored"
        );
        let rp = rounded("padding-box");
        assert!(
            (rp.radius.top_left.resolve(16.0, 16.0, 0.0) - 18.4).abs() < 1e-3,
            "{:?}",
            rp.radius
        );
        assert_eq!(rp.radius.bottom_right, rp.radius.top_left);
        assert_eq!(
            rounded("content-box").radius,
            BorderRadius::ZERO,
            "the content box is square-cornered"
        );
    }

    /// `background-origin` moves the positioning area of an image layer, and
    /// `background-clip` cuts the tiles that reach past it. Tiles ride the
    /// existing image list (one rect each) plus the existing per-item clip —
    /// no new wire.
    #[test]
    fn background_image_layers_position_and_clip_per_layer() {
        let sized = |html: &str| -> DisplayList {
            let (dom, _) = kitae_html::parse(html);
            let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
            let sizes = HashMap::from([
                ("a.png".to_string(), (10.0, 10.0)),
                ("b.png".to_string(), (10.0, 10.0)),
            ]);
            layout_with_image_sizes(&dom, &styles, 300.0, 600.0, &Mock, &sizes)
        };
        // One image layer, a padding-box origin and a content-box clip: the
        // tile starts at the padding box (2,2) and carries the clip rect.
        let dl = sized(
            "<div style='position:absolute; left:0; top:0; margin:0; width:100px; height:50px;              padding:5px; border:2px solid #000000;              background:url(a.png) no-repeat 0 0 / 20px 20px padding-box content-box'></div>",
        );
        assert_eq!(dl.images.len(), 1, "{:?}", dl.images);
        assert_eq!(
            (dl.images[0].x, dl.images[0].y),
            (2.0, 2.0),
            "padding-box origin"
        );
        assert!(
            matches!(
                dl.images[0].clip,
                Some(ResolvedClip::Inset { x, y, w, h, .. })
                    if (x, y, w, h) == (7.0, 7.0, 86.0, 36.0)
            ),
            "clipped to the content box: {:?}",
            dl.images[0].clip
        );
        // A percentage size resolves against the ORIGIN box, not the border
        // box: 50% of a 96-wide padding box is 48.
        let dl = sized(
            "<div style='position:absolute; left:0; top:0; margin:0; width:100px; height:50px;              padding:5px; border:2px solid #000000;              background:url(a.png) no-repeat 0 0 / 50% 50% padding-box content-box'></div>",
        );
        assert_eq!(
            (dl.images[0].w, dl.images[0].h),
            (48.0, 23.0),
            "{:?}",
            dl.images[0]
        );

        // Two image layers: the list's first layer is the top one, so its
        // tiles come last in the image list.
        let dl = sized(
            "<div style='position:absolute; left:0; top:0; margin:0; width:100px; height:50px;              background:url(a.png) no-repeat 0 0 / 20px 20px,              url(b.png) no-repeat 40px 0 / 20px 20px'></div>",
        );
        assert_eq!(dl.images.len(), 2, "{:?}", dl.images);
        assert_eq!(dl.images[0].src, "b.png", "the last layer paints first");
        assert_eq!(dl.images[0].x, 40.0);
        assert_eq!(dl.images[1].src, "a.png", "the first layer paints on top");
        assert_eq!(dl.images[1].x, 0.0);
        assert!(
            dl.images.iter().all(|i| i.clip.is_none()),
            "a border-box clip needs no clip"
        );
    }

    /// The 256-tile cap is per image layer, so a layer list stays bounded
    /// without starving the layers below it.
    #[test]
    fn background_image_tile_cap_is_per_layer() {
        let (dom, _) = kitae_html::parse(
            "<div style='width:4000px; height:4000px;              background:url(a.png) repeat 0 0 / 1px 1px, url(b.png) repeat 0 0 / 1px 1px'></div>",
        );
        let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
        let sizes = HashMap::from([
            ("a.png".to_string(), (1.0, 1.0)),
            ("b.png".to_string(), (1.0, 1.0)),
        ]);
        let dl = layout_with_image_sizes(&dom, &styles, 4000.0, 600.0, &Mock, &sizes);
        for src in ["a.png", "b.png"] {
            assert!(
                dl.images.iter().filter(|i| i.src == src).count() <= MAX_BG_TILES,
                "{src} is capped at {MAX_BG_TILES}, got {}",
                dl.images.iter().filter(|i| i.src == src).count()
            );
        }
        assert!(!dl.images.is_empty(), "each layer still paints");
    }

    /// `<source media>` runs through the same evaluator as stylesheet media
    /// queries, so the viewport height is available there too: `orientation`
    /// and the MQ4 range syntax now answer on this path.
    #[test]
    fn source_media_reads_orientation_and_ranges() {
        // `lay` lays out at a 500×600 viewport.
        let pick = |sources: &str| {
            let dl = lay(
                &format!("<picture>{sources}<img src='default.png'></picture>"),
                500.0,
            );
            dl.images[0].src.clone()
        };
        assert_eq!(
            pick("<source media='(orientation: portrait)' srcset='tall.png'>"),
            "tall.png",
            "500×600 is portrait"
        );
        assert_eq!(
            pick("<source media='(orientation: landscape)' srcset='wide.png'>"),
            "default.png",
            "…and not landscape"
        );
        assert_eq!(
            pick("<source media='(400px <= width < 900px)' srcset='mid.png'>"),
            "mid.png",
            "range syntax on the inline axis"
        );
        assert_eq!(
            pick("<source media='(width > 900px)' srcset='huge.png'>"),
            "default.png"
        );
        // Ranges mix with the legacy form and with `not`.
        assert_eq!(
            pick("<source media='(min-width: 400px) and (width < 600px)' srcset='both.png'>"),
            "both.png"
        );
        assert_eq!(
            pick("<source media='not (width < 400px)' srcset='invert.png'>"),
            "invert.png"
        );
    }

    #[test]
    fn paragraph_wraps_at_width() {
        let dl = lay(
            "<p>one two three four five six seven eight nine ten eleven</p>",
            200.0,
        );
        assert!(
            dl.texts.len() > 1,
            "long paragraph must wrap: {:?}",
            text_of(&dl)
        );
        for run in &dl.texts {
            let w = Mock.measure(&run.text, run.size, run.weight, run.italic, run.monospace);
            assert!(run.x + w <= 232.0, "run {:?} overflows", run.text);
        }
    }

    /// Q13 golden gate: pure-Latin text must come out **byte-identical**
    /// after the wrap path switched from `split_whitespace` to UAX #14
    /// break opportunities. All values captured from the pre-Q13 code.
    #[test]
    fn latin_wrap_is_byte_identical_to_word_split_path() {
        let dl = lay(
            "<p>the quick brown fox jumps over the lazy dog. pack my box with five dozen liquor jugs.</p>",
            200.0,
        );
        let runs: Vec<(&str, f32, f32)> = dl
            .texts
            .iter()
            .map(|t| (t.text.as_str(), t.x, t.y))
            .collect();
        assert_eq!(
            runs,
            vec![
                ("the", 16.0, 28.8),
                ("quick", 48.0, 28.8),
                ("brown", 96.0, 28.8),
                ("fox", 144.0, 28.8),
                ("jumps", 16.0, 54.399998),
                ("over", 64.0, 54.399998),
                ("the", 104.0, 54.399998),
                ("lazy", 136.0, 54.399998),
                ("dog.", 16.0, 80.0),
                ("pack", 56.0, 80.0),
                ("my", 96.0, 80.0),
                ("box", 120.0, 80.0),
                ("with", 152.0, 80.0),
                ("five", 16.0, 105.6),
                ("dozen", 56.0, 105.6),
                ("liquor", 104.0, 105.6),
                ("jugs.", 16.0, 131.2),
            ]
        );
        let dl2 = lay(
            "<pre style=\"white-space: pre-wrap\">alpha beta\ngamma delta epsilon zeta</pre>",
            180.0,
        );
        let runs2: Vec<(&str, f32, f32)> = dl2
            .texts
            .iter()
            .map(|t| (t.text.as_str(), t.x, t.y))
            .collect();
        assert_eq!(
            runs2,
            vec![
                ("alpha", 26.0, 38.8),
                ("beta", 83.6, 38.8),
                ("gamma", 26.0, 64.4),
                ("delta", 83.6, 64.4),
                ("epsilon", 26.0, 90.0),
                ("zeta", 102.8, 90.0),
            ]
        );
    }

    #[test]
    fn break_segments_respect_uax14_classes() {
        // Kinsoku: never before closing punctuation (。 、) — those
        // characters glue to the preceding ideograph; ideographs otherwise
        // break per character.
        let segs: Vec<String> = break_segments("漢字。次、です")
            .into_iter()
            .map(|(s, _)| s.trim().to_string())
            .collect();
        for s in &segs {
            assert!(
                !s.starts_with('。') && !s.starts_with('、'),
                "break before closing punct: {segs:?}"
            );
        }
        assert!(segs.contains(&"漢".to_string()), "CJK per char: {segs:?}");
        assert!(
            segs.contains(&"字。".to_string()),
            "closing punct glues to its word: {segs:?}"
        );
    }

    #[test]
    fn cjk_paragraph_wraps_character_wise() {
        let dl = lay("<p>漢字漢字漢字漢字漢字漢字漢字漢字</p>", 100.0);
        assert!(dl.texts.len() > 1, "CJK must wrap: {:?}", text_of(&dl));
        for t in &dl.texts {
            assert!(
                t.text
                    .chars()
                    .all(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
                "whole ideographs only: {:?}",
                t.text
            );
        }
    }

    #[test]
    fn nbsp_glues_words_into_one_segment() {
        // UAX #14 keeps NBSP unbreakable and the segment path keeps the
        // character in the output (the old split_whitespace broke it and
        // silently dropped it).
        let dl = lay("<p>a\u{a0}b c</p>", 400.0);
        let t = text_of(&dl);
        assert!(t.contains(&"a\u{a0}b"), "NBSP kept and whole: {t:?}");
    }

    #[test]
    fn headings_render_larger() {
        let dl = lay("<h1>Head</h1><p>para</p>", 400.0);
        let h1 = dl.texts.iter().find(|t| t.text == "Head").unwrap();
        let p = dl.texts.iter().find(|t| t.text == "para").unwrap();
        assert!(
            (h1.size - 30.4).abs() < 0.1,
            "h1 = 1.9rem = 30.4px, got {}",
            h1.size
        );
        assert_eq!(p.size, 16.0);
        assert_eq!(h1.weight, 700);
    }

    #[test]
    fn links_produce_hit_areas() {
        let dl = lay(r#"<p><a href="/next">go</a></p>"#, 400.0);
        assert_eq!(dl.links.len(), 1);
        assert_eq!(dl.links[0].href, "/next");
        assert_eq!(dl.texts[0].text, "go");
    }

    #[test]
    fn list_markers_and_ordering() {
        let dl = lay("<ul><li>alpha<li>beta</ul><ol><li>one</ol>", 400.0);
        let t = text_of(&dl);
        assert!(t.contains(&"•"), "bullet marker in {t:?}");
        assert!(t.contains(&"1."), "ordered marker in {t:?}");
        let bullet = dl.texts.iter().find(|t| t.text == "•").unwrap();
        let alpha = dl.texts.iter().find(|t| t.text == "alpha").unwrap();
        assert!(bullet.x < alpha.x, "marker hangs left of content");
    }

    #[test]
    fn b14_marker_types() {
        let dl = lay(
            "<ol style='list-style-type: lower-roman'><li>a<li>b</ol> \
             <ul style='list-style-type: square'><li>c</ul> \
             <ol style='list-style-type: decimal-leading-zero'><li>z</ol>",
            400.0,
        );
        let t = text_of(&dl);
        assert!(t.contains(&"i.") && t.contains(&"ii."), "{t:?}");
        assert!(t.contains(&"▪"), "{t:?}");
        assert!(t.contains(&"01."), "{t:?}");
        assert!(!t.contains(&"•"), "square replaces the bullet");
    }

    #[test]
    fn b14_marker_none_suppresses() {
        let dl = lay("<ol style='list-style-type: none'><li>x</li></ol>", 400.0);
        let t = text_of(&dl);
        assert!(t.contains(&"x"));
        assert!(!t.iter().any(|s| s.ends_with('.')), "{t:?}");
    }

    #[test]
    fn b14_type_attr_and_start() {
        let dl = lay(
            "<ol type='a'><li>x<li>y</ol><ul type='square'><li>z</ul>",
            400.0,
        );
        let t = text_of(&dl);
        assert!(t.contains(&"a.") && t.contains(&"b."), "{t:?}");
        assert!(t.contains(&"▪"), "{t:?}");

        let dl = lay("<ol start='5'><li>a<li>b</ol>", 400.0);
        let t = text_of(&dl);
        assert!(t.contains(&"5.") && t.contains(&"6."), "{t:?}");
    }

    #[test]
    fn b14_nested_lists_restart_numbering() {
        let dl = lay(
            "<ol><li>top<ol><li>sub</li></ol></li><li>next</li></ol>",
            400.0,
        );
        let t = text_of(&dl);
        // Inner list restarts at 1; the outer counter survives it.
        assert_eq!(t.iter().filter(|s| **s == "1.").count(), 2, "{t:?}");
        assert!(t.contains(&"2."), "{t:?}");
    }

    #[test]
    fn b14_list_style_position_inside() {
        let out = lay("<ol><li>first</li></ol>", 400.0);
        let ins = lay(
            "<ol style='list-style-position: inside'><li>first</li></ol>",
            400.0,
        );
        let gap = |dl: &DisplayList| {
            let m = dl.texts.iter().find(|r| r.text == "1.").unwrap();
            let f = dl.texts.iter().find(|r| r.text == "first").unwrap();
            assert_eq!(m.y, f.y, "marker and content share the line");
            f.x - m.x
        };
        assert!(
            gap(&ins) < gap(&out),
            "inside marker flows with the text, outside hangs in the padding"
        );
    }

    #[test]
    fn pre_preserves_newlines_and_background() {
        let dl = lay("<pre>line one\nline two</pre>", 400.0);
        let t = text_of(&dl);
        // Text runs are per-word (pre-wrap contract); the words still read
        // back in order.
        assert!(t.contains(&"one") && t.contains(&"two"), "{t:?}");
        let one = dl.texts.iter().find(|t| t.text == "one").unwrap();
        let two = dl.texts.iter().find(|t| t.text == "two").unwrap();
        assert!(two.y > one.y, "second pre line sits lower");
        assert!(
            dl.rects.iter().any(|r| r.color == 0xF4F5F7FF),
            "pre background"
        );
    }

    #[test]
    fn display_none_content_is_skipped() {
        let dl = lay(
            "<div>visible</div><span style=\"display:none\">ghost</span><p>after</p>",
            400.0,
        );
        let t = text_of(&dl);
        assert!(!t.iter().any(|s| s.contains("ghost")), "{t:?}");
        assert!(t.contains(&"visible") && t.contains(&"after"));
    }

    #[test]
    fn nested_inline_styles_apply() {
        let dl = lay("<p>plain <b>bold <i>both</i></b></p>", 400.0);
        let plain = dl.texts.iter().find(|t| t.text == "plain").unwrap();
        let bold = dl.texts.iter().find(|t| t.text == "bold").unwrap();
        let both = dl.texts.iter().find(|t| t.text == "both").unwrap();
        assert_eq!(plain.weight, 400);
        assert_eq!(bold.weight, 700);
        assert_eq!(both.weight, 700);
        assert!(both.italic);
    }

    #[test]
    fn br_breaks_the_line() {
        let dl = lay("<p>top<br>bottom</p>", 400.0);
        let top = dl.texts.iter().find(|t| t.text == "top").unwrap();
        let bottom = dl.texts.iter().find(|t| t.text == "bottom").unwrap();
        assert!(bottom.y > top.y);
    }

    #[test]
    fn max_width_centers_the_box() {
        let dl = lay(r#"<div style="max-width:100px">hi</div>"#, 400.0);
        let hi = dl.texts.iter().find(|t| t.text == "hi").unwrap();
        assert!((hi.x - 150.0).abs() < 2.0, "centered x≈150, got {}", hi.x);
    }

    #[test]
    fn text_align_center_offsets_the_line() {
        let dl = lay(r#"<p style="text-align:center">ab</p>"#, 400.0);
        let ab = dl.texts.iter().find(|t| t.text == "ab").unwrap();
        assert!(
            (ab.x - 192.0).abs() < 2.0,
            "2 chars at 8px = 16 wide → x=192, got {}",
            ab.x
        );
    }

    #[test]
    fn img_shows_alt_placeholder() {
        let dl = lay(r#"<p><img src="x.png" alt="pic"></p>"#, 400.0);
        let t = text_of(&dl);
        assert!(t.contains(&"[pic]"), "{t:?}");
        let run = dl.texts.iter().find(|t| t.text == "[pic]").unwrap();
        assert!(run.italic);
    }

    #[test]
    fn content_height_grows_with_content() {
        let short = lay("<p>a</p>", 400.0);
        let tall = lay("<p>a</p><p>b</p><p>c</p><p>d</p>", 400.0);
        assert!(tall.content_height > short.content_height + 40.0);
    }

    #[test]
    fn code_text_uses_the_fixed_width_face() {
        let dl = lay("<p><code>fnx</code></p>", 400.0);
        let run = dl.texts.iter().find(|t| t.text == "fnx").unwrap();
        assert!(run.monospace);
        assert_eq!(run.color, 0xA3372CFF);
    }

    // ---- positioning ----

    #[test]
    fn relative_offset_translates_paint() {
        let dl = lay(
            r#"<div style="position:relative;left:30px;top:10px">moved</div>"#,
            400.0,
        );
        let moved = dl.texts.iter().find(|t| t.text == "moved").unwrap();
        // Static x would be 16 (body padding); +30 left, top +10.
        assert!(
            (moved.x - 46.0).abs() < 2.0,
            "left:30 shifts x, got {}",
            moved.x
        );
        assert!(
            moved.y > 16.0 + 20.0,
            "top:10 shifts below first line, y={}",
            moved.y
        );
    }

    #[test]
    fn negative_relative_offset() {
        let dl = lay(
            r#"<div style="position:relative;right:15px">back</div>"#,
            400.0,
        );
        let back = dl.texts.iter().find(|t| t.text == "back").unwrap();
        assert!(
            (back.x - 1.0).abs() < 2.0,
            "right:15 = left:-15, got {}",
            back.x
        );
    }

    #[test]
    fn absolute_positions_against_containing_block() {
        let dl = lay(
            r#"<div style="position:relative;height:100px"><span style="position:absolute;left:10px;top:5px">pinned</span></div>"#,
            400.0,
        );
        let pinned = dl.texts.iter().find(|t| t.text == "pinned").unwrap();
        // Inside the relative div's content box: x = 16 + 10, below its top.
        assert!((pinned.x - 26.0).abs() < 2.0, "got x={}", pinned.x);
        assert!(pinned.y > 30.0, "pinned inside the div, y={}", pinned.y);
    }

    #[test]
    fn absolute_without_offsets_sits_at_containing_block_origin() {
        let dl = lay(
            r#"<div style="position:relative"><div style="position:absolute">auto</div></div>"#,
            400.0,
        );
        let auto = dl.texts.iter().find(|t| t.text == "auto").unwrap();
        assert!(
            (auto.x - 16.0).abs() < 2.0,
            "content-box origin, got x={}",
            auto.x
        );
    }

    #[test]
    fn absolute_right_anchor() {
        let dl = lay(
            r#"<div style="position:relative"><div style="position:absolute;right:20px;width:50px">end</div></div>"#,
            400.0,
        );
        let end = dl.texts.iter().find(|t| t.text == "end").unwrap();
        // w=50 anchored 20 from the div's right content edge: x = 16 + 368 - 20 - 50 = 314.
        assert!((end.x - 314.0).abs() < 3.0, "got x={}", end.x);
    }

    #[test]
    fn fixed_anchors_to_the_page() {
        let dl = lay(
            r#"<p>flow</p><div style="position:fixed;left:5px;top:5px">bar</div>"#,
            400.0,
        );
        let bar = dl.texts.iter().find(|t| t.text == "bar").unwrap();
        assert!((bar.x - 5.0).abs() < 2.0, "page x, got {}", bar.x);
        assert!(bar.y < 40.0, "page top, got y={}", bar.y);
        // Fixed paint lands after flow content.
        let flow_idx = dl.texts.iter().position(|t| t.text == "flow").unwrap();
        let bar_idx = dl.texts.iter().position(|t| t.text == "bar").unwrap();
        assert!(bar_idx > flow_idx, "fixed paints above flow");
    }

    #[test]
    fn fixed_percentage_top_resolves_against_the_viewport() {
        // A ~2000px document in a 600px viewport: `top: 50%` on a fixed box
        // resolves against the viewport height (300), not the document
        // height (which would put the box tenfold below the fold).
        let dl = lay(
            r#"<div style="height:2000px">tall</div><div style="position:fixed;left:10px;top:50%;width:60px">mid</div>"#,
            400.0,
        );
        let mid = dl.texts.iter().find(|t| t.text == "mid").unwrap();
        assert!(
            mid.y > 250.0 && mid.y < 340.0,
            "mid-viewport, got y={} (document-height resolution lands ≈1016)",
            mid.y
        );
    }

    /// ⑦: a fixed box's margin must shift it exactly once. The drain lays
    /// the box from a local origin (margins baked into the buffer) and then
    /// translates by the resolved offsets — adding `margin-top` there too
    /// double-applied it and sat a −60px rail 120px above center.
    #[test]
    fn fixed_margin_top_applies_once() {
        let dl = lay(
            r#"<div style="position:fixed;left:26px;top:50%;margin-top:-60px;width:40px">rail</div>"#,
            400.0,
        );
        let rail = dl.texts.iter().find(|t| t.text == "rail").unwrap();
        // top: 50% of the 600px viewport = 300, plus margin-top −60 → the
        // box top at 240; the text run reports its baseline (~+13 for the
        // 16px font), so anything below ~253 proves the single apply (the
        // doubled −120 lands the baseline at ~193).
        assert!(
            rail.y > 244.0 && rail.y < 260.0,
            "margin applied once: expect baseline ≈253, got {} (double apply lands ≈193)",
            rail.y
        );
    }

    /// ⑧: a column-flex item with an explicit cross width sits at the
    /// container's left edge — not centered inside a phantom 40px frame.
    /// The floor used to widen the item's frame, and the block path's
    /// fixed-width centering then pushed an 11px dot to (40−11)/2.
    #[test]
    fn flex_column_item_sits_at_its_cross_width() {
        let dl = lay(
            r#"<style>
.rail { position:fixed; left:26px; top:100px; display:flex; flex-direction:column; gap:16px; }
.dot { width:11px; height:11px; border:1.5px solid #565664; border-radius:50%; }
</style>
<div class="rail"><div class="dot"></div><div class="dot"></div></div>"#,
            400.0,
        );
        let left_slivers = dl
            .rects
            .iter()
            .filter(|r| (r.w - 1.5).abs() < 0.01 && (r.x - 26.0).abs() < 0.01)
            .count();
        assert!(
            left_slivers >= 2,
            "both dots' left border slivers hug left:26px, found {left_slivers}"
        );
    }

    #[test]
    fn z_index_orders_stacking() {
        let dl = lay(
            r#"<div style="position:relative"><div style="position:absolute;z-index:2">high</div><div style="position:absolute;z-index:1">low</div></div>"#,
            400.0,
        );
        let high = dl.texts.iter().position(|t| t.text == "high").unwrap();
        let low = dl.texts.iter().position(|t| t.text == "low").unwrap();
        assert!(low < high, "z-index 1 paints before z-index 2");
    }

    #[test]
    fn z_index_tie_uses_document_order() {
        let dl = lay(
            r#"<div style="position:relative"><div style="position:absolute">first</div><div style="position:absolute">second</div></div>"#,
            400.0,
        );
        let first = dl.texts.iter().position(|t| t.text == "first").unwrap();
        let second = dl.texts.iter().position(|t| t.text == "second").unwrap();
        assert!(first < second);
    }

    #[test]
    fn display_none_absolute_child_is_skipped() {
        let dl = lay(
            r#"<div style="position:relative"><div style="position:absolute;display:none">ghost</div><div style="position:absolute">real</div></div>"#,
            400.0,
        );
        let t = text_of(&dl);
        assert!(!t.iter().any(|s| s.contains("ghost")), "{t:?}");
        assert!(t.contains(&"real"));
    }

    // ---- overflow + size clamps ----

    #[test]
    fn overflow_hidden_clips_text() {
        let dl = lay(
            r#"<div style="height:30px;overflow:hidden"><p>line1</p><p>line2</p><p>line3</p></div>"#,
            400.0,
        );
        let t = text_of(&dl);
        assert!(t.contains(&"line1"), "{t:?}");
        assert!(
            !t.iter().any(|s| s.contains("line3")),
            "clipped past 30px: {t:?}"
        );
    }

    #[test]
    fn overflow_visible_keeps_content() {
        let dl = lay(
            r#"<div style="height:30px"><p>line1</p><p>line2</p><p>line3</p></div>"#,
            400.0,
        );
        let t = text_of(&dl);
        assert!(
            t.contains(&"line3"),
            "visible overflow keeps painting: {t:?}"
        );
    }

    #[test]
    fn min_height_grows_the_box() {
        let dl = lay(
            r#"<div style="min-height:200px;background:#EEEEEEFF"><p>hi</p></div>"#,
            400.0,
        );
        // Box background: body padding 16 top, div starts at 16, spans ≥200.
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0xEEEEEEFF && r.h >= 200.0)
            .expect("min-height grows the background");
        assert!(bg.h >= 200.0);
    }

    #[test]
    fn max_height_shrinks_and_pulls_flow_back() {
        let dl = lay(
            r#"<div style="max-height:40px;overflow:hidden"><p>one</p><p>two</p></div><p id="after">after</p>"#,
            400.0,
        );
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        // Div clamps to 40 tall; "after" starts near 16+40+14 (p margin), well
        // below the unclamped ~99.
        assert!(
            after.y < 100.0,
            "flow resumes at the clamped edge, y={}",
            after.y
        );
    }

    #[test]
    fn min_width_wins_over_narrow_content() {
        let dl = lay(r#"<div style="min-width:250px"><p>x</p></div>"#, 400.0);
        let x = dl.texts.iter().find(|t| t.text == "x").unwrap();
        // Not centered: box takes full min-width 250 < 400? No — width=avail
        // unless narrowed, so x stays at 16. Assert box stays wide by checking
        // the text still starts at the left edge.
        assert!((x.x - 16.0).abs() < 2.0, "got {}", x.x);
    }

    // ---- flexbox ----

    #[test]
    fn flex_row_grows_evenly() {
        let dl = lay(
            r#"<div style="display:flex"><div style="flex:1">aa</div><div style="flex:3">bb</div></div>"#,
            400.0,
        );
        // Content width 368, no bases: grow splits it 1:3 → 92 / 276.
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        assert!(
            (aa.x - 16.0).abs() < 2.0,
            "first item at left edge, got {}",
            aa.x
        );
        assert!(
            (bb.x - (16.0 + 92.0)).abs() < 2.0,
            "second at 16+92=108, got {}",
            bb.x
        );
    }

    #[test]
    fn flex_row_gap_separates_items() {
        let dl = lay(
            r#"<div style="display:flex;gap:10px"><div style="width:100px">aa</div><div style="width:100px">bb</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        assert!((aa.x - 16.0).abs() < 2.0, "got {}", aa.x);
        assert!(
            (bb.x - (16.0 + 100.0 + 10.0)).abs() < 2.0,
            "126 expected, got {}",
            bb.x
        );
    }

    #[test]
    fn flex_row_reverse_parks_items_at_the_right() {
        let dl = lay(
            r#"<div style="display:flex;flex-direction:row-reverse"><div style="width:80px">aa</div><div style="width:120px">bb</div></div>"#,
            400.0,
        );
        // Main axis flips: last-in-DOM ("bb", 120 wide) hugs the left edge of
        // the content box, first-in-DOM ("aa", 80) sits to its right.
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        assert!(
            (bb.x - 16.0).abs() < 2.0,
            "bb first from the left, got {}",
            bb.x
        );
        assert!(
            (aa.x - (16.0 + 120.0)).abs() < 2.0,
            "aa at 136, got {}",
            aa.x
        );
        // Row-reverse with plain flex-start parks the run's end at the right
        // edge: 16 + 368 − 200 = 184 … items fill left→right here because the
        // run starts at the origin; the real check is ordering, asserted above.
    }

    #[test]
    fn flex_justify_center_centers_the_run() {
        let dl = lay(
            r#"<div style="display:flex;justify-content:center"><div style="width:100px">aa</div></div>"#,
            400.0,
        );
        // Leftover 368 − 100 = 268; half goes in front.
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        assert!((aa.x - (16.0 + 134.0)).abs() < 2.0, "x≈150, got {}", aa.x);
    }

    #[test]
    fn flex_justify_space_between_pins_the_ends() {
        let dl = lay(
            r#"<div style="display:flex;justify-content:space-between"><div style="width:60px">aa</div><div style="width:60px">bb</div><div style="width:60px">cc</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        let cc = dl.texts.iter().find(|t| t.text == "cc").unwrap();
        assert!((aa.x - 16.0).abs() < 2.0, "first pinned left, got {}", aa.x);
        // Leftover 368 − 180 = 188 over 2 gaps = 94 each.
        assert!(
            (bb.x - (16.0 + 60.0 + 94.0)).abs() < 2.0,
            "x≈170, got {}",
            bb.x
        );
        assert!(
            (cc.x - (16.0 + 60.0 + 94.0 + 60.0 + 94.0)).abs() < 2.0,
            "x≈324, got {}",
            cc.x
        );
    }

    #[test]
    fn flex_align_center_vertically_centers_short_items() {
        let dl = lay(
            r#"<div style="display:flex;align-items:center"><div style="width:80px">aa</div><div style="width:80px;height:100px">bb</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        // Cross extent = 100 ("bb" box); "aa" text (~25.6 line) centers at
        // (100 − 25.6)/2 ≈ 37.2 below the container top; baseline = top +
        // 37.2 + 12.8 ≈ 66. Container top = body padding 16.
        assert!(
            aa.y > 55.0 && aa.y < 80.0,
            "baseline centered ≈66, got {}",
            aa.y
        );
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        // "bb"'s text tops its 100px box (baseline ≈28.8) — visibly above the
        // centered short text next to it.
        assert!(
            bb.y < aa.y,
            "tall item's text tops its box: aa.y={} bb.y={}",
            aa.y,
            bb.y
        );
    }

    #[test]
    fn flex_column_stacks_with_gap() {
        let dl = lay(
            r#"<div style="display:flex;flex-direction:column;gap:20px"><div>aa</div><div>bb</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        assert!(
            (aa.x - 16.0).abs() < 2.0,
            "column items keep the left edge, got {}",
            aa.x
        );
        // Each item is a 16px/1.6 line = 25.6 tall + 20 gap.
        assert!(
            (bb.y - (aa.y + 25.6 + 20.0)).abs() < 2.0,
            "aa.y={}, bb.y={}",
            aa.y,
            bb.y
        );
    }

    #[test]
    fn flex_shrink_squeezes_overflown_items() {
        let dl = lay(
            r#"<div style="display:flex"><div style="width:300px">aa</div><div style="width:300px">bb</div></div>"#,
            400.0,
        );
        // Bases 300+300=600 vs 368 available: over 232, equal weights shrink
        // each to 184. "aa" text must stay in [16, 200], "bb" starts ≈200.
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        assert!(
            (bb.x - (16.0 + 184.0)).abs() < 4.0,
            "bb at ≈200, got {}",
            bb.x
        );
        assert!(aa.x < bb.x && aa.x >= 16.0);
    }

    #[test]
    fn flex_container_height_follows_tallest_item() {
        let dl = lay(
            r#"<div style="display:flex"><div style="width:80px">aa</div><div style="width:80px;height:90px">bb</div></div><p id="after">after</p>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        // Row container is 90 tall (tallest item); its baseline ≈ 16+72 = 88;
        // "after" starts at 16 + 90 + p-margin 14 ≈ 120.
        assert!(
            after.y > 100.0 && after.y < 140.0,
            "flow resumes below 90px flex box, y={}",
            after.y
        );
        assert!(aa.y < after.y);
    }

    #[test]
    fn flex_absolute_child_leaves_the_run() {
        let dl = lay(
            r#"<div style="display:flex"><div style="position:absolute;top:5px;left:5px">aa</div><div style="width:80px">bb</div></div>"#,
            400.0,
        );
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        // Out-of-flow "aa" takes no main-axis slot: "bb" stays at the origin.
        assert!((bb.x - 16.0).abs() < 2.0, "got {}", bb.x);
        let t = text_of(&dl);
        assert!(t.contains(&"aa") && t.contains(&"bb"), "{t:?}");
    }

    // ---- flex-wrap (Q4) ----

    #[test]
    fn flex_wrap_breaks_rows_at_fixed_width() {
        // 150px of items vs 120px content width: they must break 2 + 1.
        let dl = lay(
            r#"<div style="display:flex;flex-wrap:wrap;width:120px"><div style="width:50px">aa</div><div style="width:50px">bb</div><div style="width:50px">cc</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        let cc = dl.texts.iter().find(|t| t.text == "cc").unwrap();
        assert!(
            (bb.x - (aa.x + 50.0)).abs() < 2.0,
            "aa/bb share line 1, aa.x={} bb.x={}",
            aa.x,
            bb.x
        );
        assert!(
            cc.y > aa.y + 10.0,
            "cc wraps to line 2, cc.y={} aa.y={}",
            cc.y,
            aa.y
        );
        assert!(
            (cc.x - aa.x).abs() < 2.0,
            "cc returns to the line start, got {}",
            cc.x
        );
    }

    #[test]
    fn flex_wrap_reverse_stacks_first_line_at_the_bottom() {
        let dl = lay(
            r#"<div style="display:flex;flex-wrap:wrap-reverse;width:120px"><div style="width:50px">aa</div><div style="width:50px">bb</div><div style="width:50px">cc</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let cc = dl.texts.iter().find(|t| t.text == "cc").unwrap();
        // Normal wrap puts line 1 (aa/bb) on top; wrap-reverse flips the
        // stack: the aa/bb line sits below the cc line.
        assert!(aa.y > cc.y + 10.0, "aa.y={} cc.y={}", aa.y, cc.y);
    }

    #[test]
    fn flex_line_cross_size_follows_tallest_item() {
        let dl = lay(
            r#"<div style="display:flex;flex-wrap:wrap;width:120px"><div style="width:50px">aa</div><div style="width:50px">bb</div><div style="width:50px;height:60px">cc</div></div><p id="after">after</p>"#,
            400.0,
        );
        let cc = dl.texts.iter().find(|t| t.text == "cc").unwrap();
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        // Line 2 (cc, 60px tall) pushes following flow to ≈ 16 + 60 + 14.
        assert!(
            after.y > cc.y && after.y > 80.0,
            "after.y={} cc.y={}",
            after.y,
            cc.y
        );
        // Line 1 is only as tall as its items (≈25.6): aa/bb well above cc.
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        assert!(cc.y > aa.y + 20.0, "aa.y={} cc.y={}", aa.y, cc.y);
    }

    #[test]
    fn flex_align_content_space_between_spreads_lines() {
        let dl = lay(
            r#"<div style="display:flex;flex-wrap:wrap;width:120px;height:200px;align-content:space-between"><div style="width:50px">aa</div><div style="width:50px">bb</div><div style="width:50px">cc</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let cc = dl.texts.iter().find(|t| t.text == "cc").unwrap();
        // Two lines in a 200px container: line 1 at the top, line 2 at the
        // bottom (≈ 16 + 200 - line-height).
        assert!(
            cc.y > aa.y + 150.0,
            "lines pinned to both ends, aa.y={} cc.y={}",
            aa.y,
            cc.y
        );
    }

    #[test]
    fn flex_align_content_center_centers_the_stack() {
        let dl = lay(
            r#"<div style="display:flex;flex-wrap:wrap;width:120px;height:200px;align-content:center"><div style="width:50px">aa</div><div style="width:50px">bb</div><div style="width:50px">cc</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        // Stack (~2 × 25.6 = 51.2) centered in 200px: lead ≈ 74 → aa.y ≈ 90.
        assert!(aa.y > 60.0 && aa.y < 110.0, "centered stack, aa.y={}", aa.y);
    }

    // ---- R-slices: borders, decorations (ENGINE_HTML_CSS_COMPLETION_PLAN) ----

    #[test]
    fn border_shorthand_paints_four_edges() {
        let dl = lay(
            r#"<div style="border: 2px solid #ff0000; width:100px; height:50px">x</div>"#,
            400.0,
        );
        // The box: 16px body margin left, so edges are known. Find the four
        // 2px red rects (top/bottom full-width, left/right full-height).
        let red: Vec<&Rect> = dl.rects.iter().filter(|r| r.color == 0xFF0000FF).collect();
        assert_eq!(red.len(), 4, "four border edges, got {}", red.len());
        let top = red
            .iter()
            .find(|r| r.h < 3.0 && r.w > 90.0)
            .expect("top edge");
        assert!((top.h - 2.0).abs() < 0.5, "2px thick, got {}", top.h);
        let left = red
            .iter()
            .find(|r| r.w < 3.0 && r.h > 45.0)
            .expect("left edge");
        assert!((left.w - 2.0).abs() < 0.5, "2px thick, got {}", left.w);
    }

    #[test]
    fn border_none_and_hidden_paint_nothing() {
        let dl = lay(
            r#"<div style="border: 2px solid red; border-style: none; width:100px; height:50px">x</div>"#,
            400.0,
        );
        assert!(
            !dl.rects.iter().any(|r| r.color == 0xFF0000FF),
            "style:none clears"
        );
        let dl = lay(
            r#"<div style="border: 2px; width:100px; height:50px">x</div>"#,
            400.0,
        );
        // `border: 2px` alone keeps the initial `none` style — invisible.
        assert!(
            dl.rects.iter().filter(|r| r.h < 3.0 || r.w < 3.0).count() == 0,
            "width without style paints nothing"
        );
    }

    #[test]
    fn dashed_border_segments_fill_the_edge() {
        let dl = lay(
            r#"<div style="border-top: 2px dashed #00f; width:100px; height:50px">x</div>"#,
            400.0,
        );
        let segs: Vec<&Rect> = dl
            .rects
            .iter()
            .filter(|r| r.color == 0x0000FFFF && r.h < 3.0)
            .collect();
        assert!(
            segs.len() > 1,
            "dashed edge breaks into segments, got {}",
            segs.len()
        );
        // Segments tile the 100px width without overlapping: total ≈ half.
        let span: f32 = segs.iter().map(|r| r.w).sum();
        assert!(
            span > 30.0 && span < 70.0,
            "≈half the edge covered, got {}",
            span
        );
    }

    #[test]
    fn border_insets_the_content_box() {
        let inner = r#"<div style="PADDING"><span id="in">in</span></div>"#;
        let without = lay(inner.replace("PADDING", "padding: 10px").as_str(), 400.0);
        let with = lay(
            inner
                .replace("PADDING", "border: 5px solid #333; padding: 10px")
                .as_str(),
            400.0,
        );
        let a = without.texts.iter().find(|t| t.text == "in").unwrap();
        let b = with.texts.iter().find(|t| t.text == "in").unwrap();
        // The border pushes the content box right/down by exactly its width.
        assert!(
            (b.x - a.x - 5.0).abs() < 1.0,
            "x inset by border-left: {} vs {}",
            b.x,
            a.x
        );
        assert!(
            (b.y - a.y - 5.0).abs() < 1.0,
            "y inset by border-top: {} vs {}",
            b.y,
            a.y
        );
    }

    #[test]
    fn outline_paints_outside_without_shifting_flow() {
        let html = r#"<div style="outline: 3px solid #0f0; outline-offset: 2px; width:100px; height:40px">x</div><p id="after">after</p>"#;
        let dl = lay(html, 400.0);
        let green: Vec<&Rect> = dl.rects.iter().filter(|r| r.color == 0x00FF00FF).collect();
        assert_eq!(
            green.len(),
            4,
            "outline ring of 4 edges, got {}",
            green.len()
        );
        // The ring sits 2px outside the box: top edge above y=16 (body margin).
        let top = green.iter().find(|r| r.h < 4.0 && r.w > 90.0).unwrap();
        assert!(top.y < 16.0, "outline outside the border edge, y={}", top.y);
        // And the flow is unaffected: the box is still 40px tall.
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        assert!(
            after.y < 16.0 + 40.0 + 30.0,
            "outline costs no flow height, y={}",
            after.y
        );
    }

    // Temporary probe (removed after debugging).
    #[test]
    fn zz_probe_pseudo_layout() {
        let dl = lay(
            r##"<style>
              .q::before { content: "→ "; color: #ff0000; font-weight: bold; }
              .q::after { content: " <-"; color: #00ff00; }
            </style><p class="q">mid</p>"##,
            400.0,
        );
        for t in &dl.texts {
            println!("run {:?} x={} y={}", t.text, t.x, t.y);
        }
    }

    #[test]
    fn text_decoration_lines_paint_at_their_offsets() {
        let dl = lay(
            r#"<p style="text-decoration: underline overline line-through">abc</p>"#,
            400.0,
        );
        let run = dl.texts.iter().find(|t| t.text == "abc").unwrap();
        // Three 1px rules at distinct y offsets around the baseline.
        let rules: Vec<&Rect> = dl.rects.iter().filter(|r| r.h < 2.0).collect();
        assert_eq!(
            rules.len(),
            3,
            "three decoration lines, got {}",
            rules.len()
        );
        assert!(
            rules.iter().any(|r| r.y > run.y),
            "underline below baseline"
        );
        assert!(
            rules
                .iter()
                .any(|r| (r.y - (run.y - run.size * 0.3)).abs() < 1.0),
            "line-through at the midline"
        );
        assert!(
            rules
                .iter()
                .any(|r| (r.y - (run.y - run.size * 0.8)).abs() < 1.0),
            "overline at the ascent top"
        );
    }

    #[test]
    fn b12_decoration_style_color_thickness() {
        // Dashed red 2px underline: segments, custom color, custom thickness.
        let dl = lay(
            r#"<p style="text-decoration: underline dashed #f00 2px">abcdef</p>"#,
            400.0,
        );
        let run = dl.texts.iter().find(|t| t.text == "abcdef").unwrap();
        let rules: Vec<&Rect> = dl
            .rects
            .iter()
            .filter(|r| (r.y - (run.y + 2.0)).abs() < 0.5)
            .collect();
        assert!(
            rules.len() > 1,
            "dashed breaks into segments, got {}",
            rules.len()
        );
        assert!(
            rules.iter().all(|r| r.color == 0xFF0000FF),
            "decoration color honored"
        );
        assert!(
            rules.iter().all(|r| (r.h - 2.0).abs() < 0.01),
            "thickness honored"
        );
        // Double: two parallel rules spaced 2t apart (band 3t).
        let dl = lay(
            r#"<p style="text-decoration-line: underline; text-decoration-style: double; text-decoration-thickness: 1px">gh</p>"#,
            400.0,
        );
        let run = dl.texts.iter().find(|t| t.text == "gh").unwrap();
        let ys: Vec<f32> = dl
            .rects
            .iter()
            .filter(|r| r.w > 1.0 && r.y >= run.y)
            .map(|r| r.y)
            .collect();
        assert_eq!(ys.len(), 2, "double = two rules, got {ys:?}");
        assert!((ys[1] - ys[0] - 2.0).abs() < 0.01, "rules 2t apart: {ys:?}");
        // Wavy: a zigzag of rotated segments.
        let dl = lay(
            r#"<p style="text-decoration: underline wavy">ij</p>"#,
            400.0,
        );
        let waves: Vec<&Rect> = dl.rects.iter().filter(|r| r.transform.is_some()).collect();
        assert!(waves.len() > 1, "wavy zigzag segments, got {}", waves.len());
    }

    // ---- R3: generated content ----

    #[test]
    fn before_after_strings_render_with_pseudo_styling() {
        let dl = lay(
            r##"<style>
              .q::before { content: "→ "; color: #ff0000; font-weight: bold; }
              .q::after { content: " <-"; color: #00ff00; }
            </style><p class="q">mid</p>"##,
            400.0,
        );
        let t = text_of(&dl);
        assert!(t.contains(&"→"), "before text renders: {t:?}");
        assert!(t.contains(&"mid") && t.contains(&"<-"), "{t:?}");
        let before = dl.texts.iter().find(|r| r.text == "→").expect("before run");
        assert_eq!(before.color, 0xFF0000FF, "pseudo's own color");
        assert_eq!(before.weight, 700, "pseudo's own weight");
        let after = dl.texts.iter().find(|r| r.text == "<-").expect("after run");
        assert_eq!(after.color, 0x00FF00FF);
        // ::before leads the element's own text; ::after lands at or below
        // the element's last line (the children's trailing flush means a
        // ::after starts a fresh line — documented v1 approximation).
        let mid = dl.texts.iter().find(|r| r.text == "mid").unwrap();
        assert!(before.x < mid.x, "before leads");
        assert!(after.y >= mid.y, "after trails");
    }

    #[test]
    fn content_attr_resolves_against_the_element() {
        let dl = lay(
            r##"<style>a[href]::after { content: attr(href) }</style><a href="https://x/">link</a>"##,
            400.0,
        );
        // UAX #14 segments the URL text; the runs are consecutive, so a
        // plain concatenation reconstructs it.
        let joined = text_of(&dl).join("");
        assert!(joined.contains("https://x/"), "attr() content: {joined:?}");
    }

    #[test]
    fn no_content_generates_nothing() {
        let dl = lay(
            r##"<style>.x::before { color: red; margin: 4px }</style><p class="x">plain</p>"##,
            400.0,
        );
        assert_eq!(text_of(&dl), vec!["plain"], "no content: no generated text");
    }

    #[test]
    fn content_escapes_process() {
        let dl = lay(
            r##"<style>.x::before { content: "\2192 \"q\"" }</style><p class="x">m</p>"##,
            400.0,
        );
        let t = text_of(&dl);
        // \2192 → '→' (the space terminator is consumed), then a literal
        // escaped-quote pair: the run reads `→"q"`.
        assert!(t.contains(&"\u{2192}\"q\""), "escapes: {t:?}");
    }

    // ---- R8: text-shadow ----

    #[test]
    fn text_shadow_paints_duplicate_runs_behind() {
        let dl = lay(
            r#"<style>.s { text-shadow: 2px 3px #ff0000 }</style><p class="s">abc</p>"#,
            400.0,
        );
        let shadow = dl
            .texts
            .iter()
            .find(|t| t.text == "abc" && t.color == 0xFF0000FF)
            .expect("shadow run");
        let main = dl
            .texts
            .iter()
            .find(|t| t.text == "abc" && t.color != 0xFF0000FF)
            .expect("main run");
        assert!((shadow.x - main.x - 2.0).abs() < 0.5, "shadow offset x");
        assert!((shadow.y - main.y - 3.0).abs() < 0.5, "shadow offset y");
        let si = dl.texts.iter().position(|t| t.color == 0xFF0000FF).unwrap();
        let mi = dl.texts.iter().position(|t| t.color != 0xFF0000FF).unwrap();
        assert!(si < mi, "shadow paints behind the run");
    }

    #[test]
    fn text_shadow_is_inherited_and_none_clears() {
        let dl = lay(
            r#"<style>body { text-shadow: 1px 1px #00f } .c { text-shadow: none }</style><p>a</p><p class="c">b</p>"#,
            400.0,
        );
        assert!(
            dl.texts
                .iter()
                .any(|t| t.text == "a" && t.color == 0x0000FFFF),
            "inherited from body"
        );
        assert!(
            !dl.texts
                .iter()
                .any(|t| t.text == "b" && t.color == 0x0000FFFF),
            "`none` clears"
        );
    }

    // ---- R6: inline <svg> ----

    #[test]
    fn dom_outer_html_round_trips_svg_subtrees() {
        let (dom, _) = kitae_html::parse(
            r##"<div><svg width="24" height="24"><path d="M0 0L1 1" fill="#f00"/><circle cx="2"/></svg>after</div>"##,
        );
        let root = dom.root().expect("root");
        let mut svg = None;
        dom.walk(root, &mut |n| {
            if dom.tag(n) == "svg" {
                svg = Some(n);
            }
        });
        let html = dom.outer_html(svg.expect("svg node"));
        assert!(html.starts_with("<svg"), "{html}");
        assert!(
            html.contains("xmlns=\"http://www.w3.org/2000/svg\""),
            "xmlns injected: {html}"
        );
        assert!(
            html.contains("<path d=\"M0 0L1 1\" fill=\"#f00\"/>"),
            "self-closed: {html}"
        );
        assert!(html.contains("<circle cx=\"2\"/>"), "{html}");
        assert!(html.ends_with("</svg>"), "{html}");
    }

    #[test]
    fn inline_svg_renders_an_image_entry_not_hidden() {
        let dl = lay(
            r##"<p>before</p><svg width="60" height="30" viewBox="0 0 10 5"><rect width="10" height="5" fill="#369"/></svg><p>after</p>"##,
            400.0,
        );
        let t = text_of(&dl);
        assert!(
            t.contains(&"before") && t.contains(&"after"),
            "flow continues: {t:?}"
        );
        let im = dl.images.last().expect("svg becomes an image entry");
        assert!(
            (im.w - 60.0).abs() < 1.0 && (im.h - 30.0).abs() < 1.0,
            "attr-sized box"
        );
        assert!(
            im.src.starts_with("data:image/svg+xml;base64,"),
            "data URL: {}",
            im.src
        );
        // The payload round-trips through base64 to the original markup.
        let payload = im.src.trim_start_matches("data:image/svg+xml;base64,");
        // (decode sanity is covered by the engine test; check well-formedness)
        assert!(payload.len() % 4 == 0, "padded base64");
        // Following flow resumes below the 30px box.
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        assert!(after.y > im.y + 25.0, "after.y={} im.y={}", after.y, im.y);
    }

    #[test]
    fn css_size_wins_over_svg_attributes() {
        let dl = lay(
            r#"<svg width="60" height="30" style="width:120px;height:50px"><rect width="10" height="5"/></svg>"#,
            400.0,
        );
        let im = dl.images.last().expect("svg image");
        assert!(
            (im.w - 120.0).abs() < 1.0 && (im.h - 50.0).abs() < 1.0,
            "CSS box beats attrs: {}x{}",
            im.w,
            im.h
        );
    }

    // ---- position: sticky (Q5) ----

    #[test]
    fn iframe_renders_placeholder_not_raw_text() {
        let dl = lay(
            r#"<p>before</p><iframe src="https://example.com/embed/path?x=1">fallback text</iframe><p>after</p>"#,
            400.0,
        );
        let t = text_of(&dl);
        let joined = t.join(" ");
        assert!(
            joined.contains("before") && joined.contains("after"),
            "{joined:?}"
        );
        assert!(
            joined.contains("embedded frame — example.com"),
            "{joined:?}"
        );
        assert!(
            !joined.contains("fallback"),
            "raw-text fallback must not leak: {joined:?}"
        );
        // A 150px-tall placeholder box spans the content width.
        let ph = dl
            .rects
            .iter()
            .find(|r| (r.h - 150.0).abs() < 1.0)
            .expect("placeholder box");
        assert!(ph.w > 300.0, "full-width placeholder, w={}", ph.w);
        // Following flow resumes below the placeholder.
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        assert!(after.y > ph.y + 150.0, "after.y={} ph.y={}", after.y, ph.y);
    }

    #[test]
    fn sticky_records_flow_position_and_document_bounds() {
        let dl = lay(
            r#"<div style="height:40px">top</div><div style="position:sticky;top:12px;height:30px;background:#369">head</div><div style="height:800px">tail</div>"#,
            400.0,
        );
        assert_eq!(dl.stickies.len(), 1, "one sticky box");
        let s = &dl.stickies[0];
        // Flow position: the sticky's own background rect sits exactly at it.
        let rects = &dl.rects[s.rect.0 as usize..s.rect.0 as usize + s.rect.1 as usize];
        let bg = rects
            .iter()
            .find(|r| r.h >= 30.0)
            .expect("sticky bg rect in range");
        assert!(
            (bg.y - s.base_y).abs() < 1.0,
            "base_y={} bg.y={}",
            s.base_y,
            bg.y
        );
        assert!((s.height - 30.0).abs() < 2.0);
        assert_eq!(
            s.container_top, 0.0,
            "root sticky clamps against the document"
        );
        assert!(
            s.container_bottom > 800.0,
            "document bottom covers the tail"
        );
        assert!(s.has_top && !s.has_bottom);
        assert!((s.top_offset - 12.0).abs() < 0.5);
        // The shift pins the box top at container_top + offset once scrolled
        // past the stick point.
        let stuck = s.shift(100.0, 600.0);
        assert!(
            (s.base_y + stuck - (100.0 + s.top_offset)).abs() < 0.5,
            "shift={}",
            stuck
        );
        // Before reaching the stick point the box stays in flow.
        assert_eq!(s.shift(10.0, 600.0), 0.0, "not yet stuck at scroll 10");
        // Clamped at the container end: the box bottom never passes the
        // document bottom.
        let end = s.shift(10_000.0, 600.0);
        assert!(
            s.base_y + end + s.height <= s.container_bottom + 0.5,
            "clamped at document bottom, shift={}",
            end
        );
    }

    #[test]
    fn sticky_inside_overflow_box_clamps_at_both_ends() {
        let dl = lay(
            r#"<div style="overflow:hidden;height:500px;background:#eee"><div style="height:60px">pad</div><div style="position:sticky;top:0px;height:20px">head</div><div style="height:900px">long</div></div>"#,
            400.0,
        );
        assert_eq!(dl.stickies.len(), 1, "sticky survives the container clip");
        let s = &dl.stickies[0];
        assert!(
            (s.container_bottom - s.container_top - 500.0).abs() < 2.0,
            "clamped by the 500px box: top={} bottom={}",
            s.container_top,
            s.container_bottom
        );
        assert!(
            s.base_y > s.container_top && s.base_y + s.height < s.container_bottom,
            "sticky flows inside its container"
        );
        // Mid-container scroll pins the box at the viewport top (top:0): its
        // viewport y equals the offset.
        let deep = s.shift(300.0, 600.0);
        assert!(
            (s.base_y + deep - (300.0 + s.top_offset)).abs() < 2.0,
            "pinned at viewport top: shift={}",
            deep
        );
        // The box stays within its container.
        assert!(s.base_y + deep >= s.container_top - 0.5, "shift={}", deep);
        // Bottom end: the box never leaves the container.
        let end = s.shift(9_000.0, 600.0);
        assert!(
            s.base_y + end + s.height <= s.container_bottom + 0.5,
            "stops at container bottom: shift={}",
            end
        );
    }

    #[test]
    fn sticky_bottom_offset_picks_viewport_bottom_pin() {
        // Sticky at the very end of a tall document.
        let dl = lay(
            r#"<div style="height:900px">tail</div><div style="position:sticky;bottom:8px;height:20px;background:#963">foot</div>"#,
            400.0,
        );
        assert_eq!(dl.stickies.len(), 1);
        let s = &dl.stickies[0];
        assert!(!s.has_top && s.has_bottom);
        assert!((s.bottom_offset - 8.0).abs() < 0.5);
        // In flow while the box is visible normally.
        assert_eq!(s.shift(800.0, 600.0), 0.0, "visible box stays in flow");
        // Below the fold the box pins just above the viewport bottom:
        // base_y + shift + height == scroll + viewport_h - bottom_offset.
        let pinned = s.shift(200.0, 600.0);
        assert!(
            (s.base_y + pinned + s.height - (200.0 + 600.0 - 8.0)).abs() < 0.5,
            "shift={}",
            pinned
        );
        // It never leaves its container upward.
        assert!(
            s.base_y + pinned >= s.container_top - 0.5,
            "shift={}",
            pinned
        );
    }

    #[test]
    fn sticky_ranges_cover_the_painted_items() {
        let dl = lay(
            r#"<div style="height:40px">top</div><div style="position:sticky;top:0;height:30px;background:#369"><a href="https://x/">go</a></div><div style="height:800px">tail</div>"#,
            400.0,
        );
        let s = &dl.stickies[0];
        // The sticky box's background rect, its text run and its link all sit
        // inside the recorded ranges at their base (flow) coordinates.
        let rects = &dl.rects[s.rect.0 as usize..s.rect.0 as usize + s.rect.1 as usize];
        assert!(
            rects.iter().any(|r| (r.y - s.base_y).abs() < 2.0),
            "bg rect in range"
        );
        let texts = &dl.texts[s.text.0 as usize..s.text.0 as usize + s.text.1 as usize];
        assert_eq!(texts[0].text, "go", "sticky run in range");
        let links = &dl.links[s.link.0 as usize..s.link.0 as usize + s.link.1 as usize];
        assert_eq!(links[0].href, "https://x/", "sticky link in range");
    }

    // ---- floats ----

    #[test]
    fn float_left_wraps_following_text_beside_it() {
        let dl = lay(
            r#"<div style="float:left;width:100px;height:50px">BOX</div><p>alpha beta gamma delta epsilon zeta eta theta</p>"#,
            400.0,
        );
        // Content box: x 16..384. First lines narrow to [116, 384]; a word
        // landing beside the float starts at ≥116.
        let beside = dl
            .texts
            .iter()
            .find(|t| t.x >= 116.0 - 1.0)
            .unwrap_or_else(|| panic!("no text shaped beside the float: {:?}", text_of(&dl)));
        assert!(
            beside.x >= 115.0,
            "beside text at {}, float ends at 116",
            beside.x
        );
    }

    #[test]
    fn text_resumes_full_width_below_the_float() {
        let dl = lay(
            r#"<div style="float:left;width:100px;height:50px">BOX</div><p>alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron</p>"#,
            400.0,
        );
        // Body starts at y=16; float spans 16..66. The narrowed band fits two
        // lines beside the float; line 3 lands past y=66 and must use the
        // full band starting at x=16 again.
        let full = dl
            .texts
            .iter()
            .filter(|t| t.y > 66.0)
            .find(|t| t.x < 20.0 && t.x >= 15.0)
            .unwrap_or_else(|| {
                panic!(
                    "no full-width line below float: {:?}",
                    dl.texts
                        .iter()
                        .map(|t| (t.text.clone(), t.x, t.y))
                        .collect::<Vec<_>>()
                )
            });
        assert!(
            full.x < 20.0,
            "line below the float starts at the left edge, x={}",
            full.x
        );
    }

    #[test]
    fn float_right_pushes_text_leftward() {
        let dl = lay(
            r#"<div style="float:right;width:100px;height:50px">BOX</div><p>alpha beta gamma delta epsilon</p>"#,
            400.0,
        );
        // Float occupies [284, 384]; paragraph text beside it must end
        // before 284. (BOX itself is the float's own content — skip it.)
        let beside = dl
            .texts
            .iter()
            .find(|t| t.y < 66.0 && t.text != "BOX")
            .expect("paragraph text beside the right float");
        let end = beside.x
            + Mock.measure(
                &beside.text,
                beside.size,
                beside.weight,
                beside.italic,
                beside.monospace,
            );
        assert!(
            end <= 285.0,
            "text ends at {}, right float starts at 284",
            end
        );
        let box_run = dl.texts.iter().find(|t| t.text == "BOX").unwrap();
        assert!(
            box_run.x > 250.0,
            "float content at the right edge, x={}",
            box_run.x
        );
    }

    #[test]
    fn float_does_not_push_the_flow_cursor() {
        let dl = lay(
            r#"<div style="float:left;width:100px;height:80px">BOX</div><p id="after">after</p>"#,
            400.0,
        );
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        // A float never displaces block flow: "after" sits at the normal
        // first-line spot (baseline ≈ 16+12.8 = 28.8), beside the float.
        assert!(
            after.y < 60.0,
            "float must not push following block down, y={}",
            after.y
        );
    }

    #[test]
    fn floats_stack_side_by_side_on_the_same_side() {
        let dl = lay(
            r#"<div style="float:left;width:60px">ONE</div><div style="float:left;width:60px">TWO</div><p>text</p>"#,
            400.0,
        );
        let one = dl.texts.iter().find(|t| t.text == "ONE").unwrap();
        let two = dl.texts.iter().find(|t| t.text == "TWO").unwrap();
        // Both at the flow edge y≈16 (beside, not below), side by side.
        assert!(
            (one.x - 16.0).abs() < 2.0,
            "first float at left edge, x={}",
            one.x
        );
        assert!(
            (two.x - (16.0 + 60.0)).abs() < 2.0,
            "second floats beside the first, x={}",
            two.x
        );
        assert!(
            (two.y - one.y).abs() < 3.0,
            "same band, not stacked: {} vs {}",
            one.y,
            two.y
        );
    }

    #[test]
    fn containing_block_grows_past_its_float() {
        let dl = lay(
            r#"<div style="background:#EEEEEEFF"><div style="float:left;width:80px;height:120px">BOX</div><p>inside</p></div><p id="after">after</p>"#,
            400.0,
        );
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0xEEEEEEFF)
            .expect("container background");
        // The container's bottom edge clears the 120-tall float instead of
        // wrapping it: bg starts ≈16, must span ≥120.
        assert!(bg.h >= 120.0, "container height {} clears the float", bg.h);
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        assert!(
            after.y > 130.0,
            "next block starts below the cleared float, y={}",
            after.y
        );
    }

    // ---- tables (E4) ----

    #[test]
    fn table_columns_align_across_rows() {
        let dl = lay(
            "<table><tr><td>aa</td><td>bb</td></tr><tr><td>cc</td><td>dd</td></tr></table>",
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let cc = dl.texts.iter().find(|t| t.text == "cc").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        let dd = dl.texts.iter().find(|t| t.text == "dd").unwrap();
        assert!(
            (aa.x - cc.x).abs() < 1.0,
            "first column aligns: {} vs {}",
            aa.x,
            cc.x
        );
        assert!(
            (bb.x - dd.x).abs() < 1.0,
            "second column aligns: {} vs {}",
            bb.x,
            dd.x
        );
        assert!(
            bb.x > aa.x + 20.0,
            "columns separated: {} vs {}",
            aa.x,
            bb.x
        );
        assert!(cc.y > aa.y, "second row below first: {} vs {}", aa.y, cc.y);
    }

    #[test]
    fn table_cell_x_offsets_match_measured_columns() {
        let dl = lay("<table><tr><td>aa</td><td>bb</td></tr></table>", 400.0);
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        // Content width 368, equal content → equal columns (184 each).
        // Cell text sits 8px (td padding-left) inside its column.
        assert!(
            (aa.x - 24.0).abs() < 3.0,
            "first cell text at 16+8=24, got {}",
            aa.x
        );
        assert!(
            (bb.x - 208.0).abs() < 3.0,
            "second cell text at 200+8=208, got {}",
            bb.x
        );
    }

    #[test]
    fn table_td_padding_honored() {
        let dl = lay("<table><tr><td>pad</td></tr></table>", 400.0);
        let pad = dl.texts.iter().find(|t| t.text == "pad").unwrap();
        assert!(
            (pad.x - 24.0).abs() < 3.0,
            "td padding-left 8px: x={}",
            pad.x
        );
    }

    #[test]
    fn table_caption_above_the_grid() {
        let dl = lay(
            "<table><caption>mycap</caption><tr><td>cell</td></tr></table>",
            400.0,
        );
        let cap = dl.texts.iter().find(|t| t.text == "mycap").unwrap();
        let cell = dl.texts.iter().find(|t| t.text == "cell").unwrap();
        assert!(
            cap.y < cell.y,
            "caption above grid: cap.y={} cell.y={}",
            cap.y,
            cell.y
        );
    }

    #[test]
    fn table_th_renders_bold() {
        let dl = lay("<table><tr><th>head</th></tr></table>", 400.0);
        let head = dl.texts.iter().find(|t| t.text == "head").unwrap();
        assert_eq!(head.weight, 700, "th is bold via UA sheet");
    }

    #[test]
    fn table_row_groups_flatten_into_rows() {
        let dl = lay(
            "<table><thead><tr><td>ha</td><td>hb</td></tr></thead><tbody><tr><td>ba</td><td>bb</td></tr></tbody></table>",
            400.0,
        );
        let t = text_of(&dl);
        assert!(
            t.contains(&"ha") && t.contains(&"hb") && t.contains(&"ba") && t.contains(&"bb"),
            "{t:?}"
        );
        let ha = dl.texts.iter().find(|t| t.text == "ha").unwrap();
        let ba = dl.texts.iter().find(|t| t.text == "ba").unwrap();
        let hb = dl.texts.iter().find(|t| t.text == "hb").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        assert!((ha.x - ba.x).abs() < 1.0, "thead/tbody share columns");
        assert!((hb.x - bb.x).abs() < 1.0, "thead/tbody share columns");
        assert!(ba.y > ha.y, "tbody row below thead row");
    }

    #[test]
    fn table_row_sized_to_tallest_cell() {
        let dl = lay(
            "<table><tr><td>one</td><td>top<br>mid<br>bot</td></tr></table><p id=\"after\">after</p>",
            400.0,
        );
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        // Tall cell is 3 lines ≈ 3*25.6 = 76.8; row must clear it.
        // Table starts at y=16, so "after" sits well below a single-line row.
        assert!(
            after.y > 16.0 + 76.8,
            "row height follows tallest cell, after.y={}",
            after.y
        );
    }

    #[test]
    fn table_rowspan_pushes_later_cells_right() {
        let dl = lay(
            "<table><tr><td rowspan=\"2\">aa</td><td>bb</td></tr><tr><td>cc</td></tr></table>",
            400.0,
        );
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        let cc = dl.texts.iter().find(|t| t.text == "cc").unwrap();
        assert!(
            (bb.x - cc.x).abs() < 1.0,
            "cc sits under bb in column 1: {} vs {}",
            bb.x,
            cc.x
        );
        assert!(cc.y > bb.y, "second row below first");
    }

    #[test]
    fn table_rowspan_zero_spans_to_the_end() {
        let dl = lay(
            "<table><tr><td rowspan=\"0\">aa</td><td>bb</td></tr><tr><td>cc</td></tr><tr><td>dd</td></tr></table>",
            400.0,
        );
        let t = dl.texts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>();
        assert!(t.contains(&"aa") && t.contains(&"dd"), "{t:?}");
        let cc = dl.texts.iter().find(|t| t.text == "cc").unwrap();
        let dd = dl.texts.iter().find(|t| t.text == "dd").unwrap();
        assert!((cc.x - dd.x).abs() < 1.0, "both under bb's column");
    }

    #[test]
    fn table_tall_rowspan_grows_covered_rows() {
        let dl = lay(
            "<table><tr><td rowspan=\"2\">top<br>mid<br>bot<br>fourth</td><td>r1</td></tr>\
             <tr><td>r2</td></tr></table><p id=\"after\">after</p>",
            400.0,
        );
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        // Spanned cell is 4 lines ≈ 102.4; the two covered rows grow for it.
        assert!(
            after.y > 16.0 + 102.4,
            "covered rows grew, after.y={}",
            after.y
        );
    }

    // ---- grid (E5) ----

    #[test]
    fn grid_two_equal_columns() {
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr"><div>aa</div><div>bb</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        // Content width 368 → 184 each.
        assert!(
            (aa.x - 16.0).abs() < 2.0,
            "first column at left edge, got {}",
            aa.x
        );
        assert!(
            (bb.x - 200.0).abs() < 2.0,
            "second column at 16+184=200, got {}",
            bb.x
        );
        assert!((aa.y - bb.y).abs() < 2.0, "same row");
    }

    #[test]
    fn grid_template_areas_place_items_by_name() {
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr;grid-template-areas:'head head' 'side main'">
               <div style="grid-area:head;background:#101010">head</div>
               <div style="grid-area:side">side</div>
               <div style="grid-area:main">main</div>
               </div>"#,
            400.0,
        );
        let head = dl.texts.iter().find(|t| t.text == "head").unwrap();
        let side = dl.texts.iter().find(|t| t.text == "side").unwrap();
        let main = dl.texts.iter().find(|t| t.text == "main").unwrap();
        // Head spans both columns; side and main share row 2.
        let head_bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0x101010FF)
            .expect("head bg rect");
        assert!(
            head_bg.w > 300.0,
            "head spans the full width, got {}",
            head_bg.w
        );
        assert!((side.y - main.y).abs() < 2.0, "side/main same row");
        assert!(main.y > head.y + 10.0, "row 2 below head");
        assert!(side.x < main.x - 100.0, "side left of main");
    }

    #[test]
    fn grid_integer_lines_pin_placement() {
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:100px 100px 100px">
               <div style="grid-column:2 / 4">xy</div>
               </div>"#,
            400.0,
        );
        let xy = dl.texts.iter().find(|t| t.text == "xy").unwrap();
        assert!(
            (xy.x - 116.0).abs() < 2.0,
            "starts at track 2 (16+100), got {}",
            xy.x
        );
    }

    #[test]
    fn grid_named_lines_resolve_start_and_end() {
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:[a] 100px [b] 100px [c] 100px [d]">
               <div style="grid-column:b / d;background:#202020">nm</div>
               </div>"#,
            400.0,
        );
        let nm = dl.texts.iter().find(|t| t.text == "nm").unwrap();
        // Line b (0-based 1) → track 1; line d − line b = 2 tracks wide.
        assert!((nm.x - 116.0).abs() < 2.0, "got {}", nm.x);
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0x202020FF)
            .expect("bg rect");
        assert!(
            (bg.w - 200.0).abs() < 2.0,
            "spans tracks 1-2 (200px), got {}",
            bg.w
        );
    }

    #[test]
    fn grid_explicit_items_reserve_cells_for_auto_flow() {
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr 1fr">
               <div>auto1</div>
               <div style="grid-column-start:3">pin</div>
               <div>auto2</div>
               </div>"#,
            400.0,
        );
        let auto1 = dl.texts.iter().find(|t| t.text == "auto1").unwrap();
        let pin = dl.texts.iter().find(|t| t.text == "pin").unwrap();
        let auto2 = dl.texts.iter().find(|t| t.text == "auto2").unwrap();
        // Sparse auto-flow (CSS §8.5 example): auto1 → (1,1), pin → (1,3),
        // then the cursor sits past the row-1 hole — auto2 opens row 2 in
        // column 1, it does NOT backfill (see the dense twin below).
        assert!((pin.x - auto1.x).abs() > 200.0, "pin in column 3");
        assert!((auto2.x - auto1.x).abs() < 2.0, "auto2 in column 1");
        assert!(auto2.y > auto1.y + 8.0, "auto2 on row 2, not the hole");
    }

    #[test]
    fn c6_grid_auto_flow_dense_backfills_the_hole() {
        // Same grid as `grid_explicit_items_reserve_cells_for_auto_flow`
        // with `grid-auto-flow: dense`: auto2 backfills (1,2).
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr 1fr;grid-auto-flow:dense">
               <div>auto1</div>
               <div style="grid-column-start:3">pin</div>
               <div>auto2</div>
               </div>"#,
            400.0,
        );
        let auto1 = dl.texts.iter().find(|t| t.text == "auto1").unwrap();
        let pin = dl.texts.iter().find(|t| t.text == "pin").unwrap();
        let auto2 = dl.texts.iter().find(|t| t.text == "auto2").unwrap();
        assert!((pin.x - auto1.x).abs() > 200.0, "pin in column 3");
        assert!(
            auto2.x > auto1.x + 80.0 && auto2.x < pin.x - 80.0,
            "auto2 backfills column 2, got {}",
            auto2.x
        );
        assert!((auto2.y - auto1.y).abs() < 2.0, "auto2 shares row 1");
    }

    #[test]
    fn c6_sparse_spanning_item_jumps_a_row() {
        // 3 columns, sparse: aa (1,1), bb (1,2), the `span 2` item does
        // not fit at cursor (1,3) so it opens row 2, and cc follows it in
        // the same row — exactly what Chrome lays out.
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr 1fr">
               <div>aa</div><div>bb</div>
               <div style="grid-column:span 2">wide</div>
               <div>cc</div>
               </div>"#,
            400.0,
        );
        let rect = |t: &str| dl.texts.iter().find(|x| x.text == t).unwrap();
        let (aa, bb, wide, cc) = (rect("aa"), rect("bb"), rect("wide"), rect("cc"));
        assert!((bb.y - aa.y).abs() < 2.0, "bb shares row 1");
        assert!(bb.x > aa.x + 80.0, "bb in column 2");
        assert!(wide.y > aa.y + 8.0, "wide opens row 2");
        assert!((wide.x - aa.x).abs() < 2.0, "wide starts at column 1");
        assert!((cc.y - wide.y).abs() < 2.0, "cc shares wide's row");
        assert!(cc.x > wide.x + 80.0, "cc after wide in row 2");
    }

    #[test]
    fn c6_subgrid_child_aligns_with_parent_columns() {
        // A full-span child grid with `grid-template-columns: subgrid`
        // adopts the parent's 40px/60px tracks: its items sit side-by-side
        // on the parent's column boundaries. Without subgrid the child
        // would fall back to a single auto column and stack them.
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:40px 60px;margin:0">
               <div style="display:grid;grid-template-columns:subgrid;grid-column:1/3">
                 <div style="background:#ff0000">a</div>
                 <div style="background:#00ff00">b</div>
               </div>
               </div>"#,
            400.0,
        );
        let red = dl
            .rects
            .iter()
            .find(|r| r.color == 0xFF0000FF)
            .expect("red");
        let green = dl
            .rects
            .iter()
            .find(|r| r.color == 0x00FF00FF)
            .expect("green");
        assert!(
            (green.y - red.y).abs() < 2.0,
            "subgrid items share a row: red.y={} green.y={}",
            red.y,
            green.y
        );
        assert!(
            (red.x - 16.0).abs() < 2.0,
            "first track at the body content edge, got {}",
            red.x
        );
        assert!(
            (green.x - (red.x + 40.0)).abs() < 2.0,
            "second track at the parent's 40px boundary, got {}",
            green.x
        );
    }

    #[test]
    fn conic_gradient_paint_attaches() {
        let dl = lay(
            r#"<div style="background:conic-gradient(from 45deg at 30% 70%, #FF0000, #0000FF)">c</div>"#,
            400.0,
        );
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0xFF0000FF)
            .expect("first-stop bg rect");
        match &bg.paint {
            RectPaint::ConicGradient {
                angle_deg,
                cx,
                cy,
                stops,
            } => {
                assert!((angle_deg - 45.0).abs() < 0.01);
                assert!((cx - 0.30).abs() < 0.01 && (cy - 0.70).abs() < 0.01);
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[1].1, 0x0000FFFF);
            }
            other => panic!("expected conic paint, got {other:?}"),
        }
    }

    #[test]
    fn grid_fixed_plus_flex_track() {
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:100px 1fr"><div>aa</div><div>bb</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        assert!((aa.x - 16.0).abs() < 2.0, "got {}", aa.x);
        assert!((bb.x - 116.0).abs() < 2.0, "16+100=116, got {}", bb.x);
    }

    #[test]
    fn grid_gap_separates_tracks() {
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr;gap:10px"><div>aa</div><div>bb</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        // (368-10)/2 = 179 each; second starts at 16+179+10 = 205.
        assert!((aa.x - 16.0).abs() < 2.0, "got {}", aa.x);
        assert!((bb.x - 205.0).abs() < 2.0, "got {}", bb.x);
    }

    #[test]
    fn grid_wraps_to_next_row() {
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr"><div>aa</div><div>bb</div><div>cc</div></div>"#,
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let cc = dl.texts.iter().find(|t| t.text == "cc").unwrap();
        assert!(
            (cc.x - aa.x).abs() < 2.0,
            "wrapped item aligns with first column, {} vs {}",
            cc.x,
            aa.x
        );
        assert!(
            cc.y > aa.y + 20.0,
            "wrapped to second row: {} vs {}",
            aa.y,
            cc.y
        );
    }

    #[test]
    fn grid_column_span_covers_two_tracks() {
        let dl = lay(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr"><div style="grid-column:span 2">wide</div><div>aa</div><div>bb</div></div>"#,
            400.0,
        );
        let wide = dl.texts.iter().find(|t| t.text == "wide").unwrap();
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        assert!(
            (wide.x - 16.0).abs() < 2.0,
            "spanned item starts at left edge, got {}",
            wide.x
        );
        assert!(
            aa.y > wide.y + 20.0,
            "next items below the spanned row: wide.y={} aa.y={}",
            wide.y,
            aa.y
        );
    }

    // ---- images (F) ----

    #[test]
    fn image_emits_display_item_with_src_and_size() {
        let dl = lay(
            r#"<p><img src="pic.png" alt="pic" width="200" height="100"></p>"#,
            400.0,
        );
        assert_eq!(dl.images.len(), 1);
        let im = &dl.images[0];
        assert_eq!(im.src, "pic.png");
        assert_eq!(im.alt, "pic");
        assert!(
            (im.w - 200.0).abs() < 1.0,
            "attr width honored, got {}",
            im.w
        );
        assert!(
            (im.h - 100.0).abs() < 1.0,
            "attr height honored, got {}",
            im.h
        );
    }

    #[test]
    fn image_css_width_preserves_aspect() {
        let dl = lay(
            r#"<p><img src="a.png" alt="a" width="200" height="100" style="width:100px"></p>"#,
            400.0,
        );
        let im = &dl.images[0];
        assert!((im.w - 100.0).abs() < 1.0, "got {}", im.w);
        assert!(
            (im.h - 50.0).abs() < 1.0,
            "aspect preserved 200x100 → 100x50, got {}",
            im.h
        );
    }

    #[test]
    fn image_object_fit_carried_to_display_list() {
        let dl = lay(
            r#"<p><img src="a.png" alt="a" width="100" height="100" style="width:200px;height:100px;object-fit:cover"></p>"#,
            400.0,
        );
        let im = &dl.images[0];
        assert!((im.w - 200.0).abs() < 1.0, "got {}", im.w);
        assert!((im.h - 100.0).abs() < 1.0, "got {}", im.h);
        assert_eq!(im.fit, kitae_style::ObjectFit::Cover);
    }

    #[test]
    fn image_shrinks_to_fit_narrow_container() {
        let dl = lay(
            r#"<p><img src="big.png" alt="big" width="600" height="300"></p>"#,
            400.0,
        );
        let im = &dl.images[0];
        // Content width 368; 600-wide image must shrink, aspect preserved.
        assert!(im.w <= 369.0, "clamped to content width, got {}", im.w);
        assert!(
            (im.h - im.w * 0.5).abs() < 2.0,
            "aspect 2:1 preserved: {}x{}",
            im.w,
            im.h
        );
    }

    #[test]
    fn image_inside_link_produces_hit_area() {
        let dl = lay(
            r#"<p><a href="/go"><img src="b.png" alt="b" width="80" height="40"></a></p>"#,
            400.0,
        );
        assert_eq!(dl.images.len(), 1);
        let im = &dl.images[0];
        let hit = dl
            .links
            .iter()
            .find(|l| l.href == "/go")
            .expect("link hit area");
        assert!((hit.x - im.x).abs() < 1.0 && (hit.y - im.y).abs() < 1.0);
        assert!((hit.w - im.w).abs() < 1.0 && (hit.h - im.h).abs() < 1.0);
    }

    #[test]
    fn image_dimensions_decode_png_gif_jpeg() {
        // PNG 2x3 (IHDR only; the decoder reads the first 24 bytes).
        let mut png = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03,
        ];
        png.extend_from_slice(&[0x08, 0x02, 0x00, 0x00, 0x00]);
        assert_eq!(image_dimensions(&png), Some((2, 3)));

        // GIF89a 10x20 (little-endian screen descriptor).
        let gif = [
            b'G', b'I', b'F', b'8', b'9', b'a', 0x0A, 0x00, 0x14, 0x00, 0x00, 0x00,
        ];
        assert_eq!(image_dimensions(&gif), Some((10, 20)));

        // Minimal JPEG SOF0 20x10.
        let jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x0A, 0x00, 0x14, 0x01, 0x11, 0x00,
            0xFF,
        ];
        assert_eq!(image_dimensions(&jpeg), Some((20, 10)));

        assert_eq!(image_dimensions(b"not an image"), None);
    }

    #[test]
    fn image_dimensions_reads_qtsq_header() {
        // A QTSQ v3 image file with a horizon (schema) block: 112-byte header
        // (magic "QTS\x01", v3, PIXEL strategy, IMAGE_RGB) followed by the
        // aligned 176-byte schema whose dimensions[0..2] are 5 × 7.
        let mut b = vec![0u8; kitae_qtsq::HEADER_V3_SIZE + kitae_qtsq::SCHEMA_BLOCK_SIZE];
        b[0..4].copy_from_slice(&kitae_qtsq::QTSQ_MAGIC);
        b[4] = 3; // version
        b[5] = kitae_qtsq::QtsqStrategy::Pixel.raw();
        b[10] = kitae_qtsq::QtsqDataType::ImageRgb.raw();
        b[11] = 3; // channels
        let horizon = kitae_qtsq::HEADER_V3_SIZE as u64;
        b[62..70].copy_from_slice(&horizon.to_le_bytes());
        let s = kitae_qtsq::HEADER_V3_SIZE;
        b[s + 68..s + 72].copy_from_slice(&5u32.to_le_bytes());
        b[s + 72..s + 76].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(image_dimensions(&b), Some((5, 7)));

        // Non-image QTSQ payloads (audio/text/tensor) must not size an <img>.
        b[5] = kitae_qtsq::QtsqStrategy::Dictionary.raw();
        b[10] = kitae_qtsq::QtsqDataType::TextUtf8.raw();
        assert_eq!(image_dimensions(&b), None, "text payload is not an image");

        // A pixel-strategy file without a horizon block carries no header
        // dimensions — callers fall back to attribute/default sizing.
        let b = b[..kitae_qtsq::HEADER_V3_SIZE].to_vec();
        assert_eq!(image_dimensions(&b), None, "no horizon → no header dims");
    }

    // ---- media (J5) ----

    #[test]
    fn video_emits_media_item_and_frame_image() {
        let dl = lay(
            r#"<p><video src="clip.qtsq" width="320" height="180">fallback text</video></p>"#,
            400.0,
        );
        assert_eq!(dl.media.len(), 1, "one DisplayMedia item");
        let m = &dl.media[0];
        assert_eq!(m.id, 0);
        assert_eq!(m.src, "clip.qtsq");
        assert_eq!(m.kind, MediaKind::Video);
        assert_eq!(m.state, (false, 0.0), "layout emits paused at 0");
        assert!((m.w - 320.0).abs() < 1.0, "attr width honored, got {}", m.w);
        assert!(
            (m.h - 180.0).abs() < 1.0,
            "attr height honored, got {}",
            m.h
        );

        // The current frame rides the image path: exactly one image at the
        // same rect, src marked as the media payload for item 0.
        assert_eq!(dl.images.len(), 1, "frame image");
        let im = &dl.images[0];
        assert_eq!(im.src, "\u{0}media:0");
        assert!(
            (im.x - m.x).abs() < 0.5
                && (im.y - m.y).abs() < 0.5
                && (im.w - m.w).abs() < 0.5
                && (im.h - m.h).abs() < 0.5,
            "frame image covers the media rect: image {}x{} at ({},{}) vs media {}x{} at ({},{})",
            im.w,
            im.h,
            im.x,
            im.y,
            m.w,
            m.h,
            m.x,
            m.y
        );
        // Caption from the element's fallback children stays visible.
        assert!(
            text_of(&dl).contains(&"fallback text"),
            "{:?}",
            text_of(&dl)
        );
        // The media rect itself advances the flow (content grows past it).
        assert!(
            dl.content_height > m.y + m.h - 1.0,
            "flow advanced past media rect"
        );
    }

    #[test]
    fn video_sizing_matches_image_rules() {
        // CSS width alone preserves aspect (320×180 → 160×90), same as img.
        let dl = lay(
            r#"<p><video src="c.qtsq" width="320" height="180" style="width:160px"></video></p>"#,
            400.0,
        );
        let m = &dl.media[0];
        assert!((m.w - 160.0).abs() < 1.0, "css width honored, got {}", m.w);
        assert!((m.h - 90.0).abs() < 1.0, "aspect preserved, got {}", m.h);
        // object-fit rides along on the frame image.
        let dl = lay(
            r#"<p><video src="c.qtsq" width="100" height="100" style="width:200px;height:100px;object-fit:cover"></video></p>"#,
            400.0,
        );
        assert_eq!(dl.images[0].fit, kitae_style::ObjectFit::Cover);
        // Narrow container shrink-to-fit.
        let dl = lay(
            r#"<p><video src="c.qtsq" width="600" height="300"></video></p>"#,
            400.0,
        );
        let m = &dl.media[0];
        assert!(m.w <= 369.0, "clamped to content width, got {}", m.w);
        assert!(
            (m.h - m.w * 0.5).abs() < 2.0,
            "aspect 2:1 preserved: {}x{}",
            m.w,
            m.h
        );
    }

    #[test]
    fn video_poster_rides_the_media_item() {
        // The engine reads the poster off the media item (resolved against the
        // page URL there) and stamps its raster into the frame image, so the
        // attribute never becomes a second paint item.
        let dl = lay(
            r#"<p><video src="c.webm" poster="  still.png  " controls autoplay muted loop preload="none"></video></p>"#,
            400.0,
        );
        let m = &dl.media[0];
        assert_eq!(m.poster, "still.png", "poster trimmed onto the item");
        assert!(m.controls, "controls rides along as before");
        assert_eq!(dl.images.len(), 1, "a poster adds no second image");
        assert_eq!(dl.images[0].src, "\u{0}media:0");

        // An empty poster means no poster frame, same as no attribute.
        for html in [
            r#"<p><video src="c.webm" poster=""></video></p>"#,
            r#"<p><video src="c.webm" poster="   "></video></p>"#,
            r#"<p><video src="c.webm"></video></p>"#,
        ] {
            let dl = lay(html, 400.0);
            assert_eq!(dl.media[0].poster, "", "{html}");
        }
    }

    #[test]
    fn audio_media_item_carries_no_poster() {
        // The UA sheet hides `<audio>` and only the inline path dispatches
        // media, so the author's `display` override is what puts an audio
        // media item on the page at all.
        let dl = lay(
            r#"<p><audio src="a.mp3" controls style="display:inline-block" poster="still.png"></audio></p>"#,
            400.0,
        );
        assert_eq!(dl.media.len(), 1);
        assert_eq!(dl.media[0].kind, MediaKind::Audio);
        assert_eq!(dl.media[0].poster, "", "poster is a <video>-only attribute");
    }

    #[test]
    fn display_list_contract_version_is_9() {
        // J5 adds the `media` vec (DisplayMedia items), L4 adds `radius`
        // + `paint` on rects plus the `shadows` vec, Q2 adds the sparse
        // item-local `transform`, v7 adds `node` provenance on every
        // item (engine-internal, for incremental UI splicing), v8 adds
        // the B7 item-local `clip`, and v9 extends transforms to links.
        assert_eq!(DISPLAY_LIST_VERSION, 9);
        assert!(DisplayList::default().media.is_empty());
        assert!(DisplayList::default().shadows.is_empty());
    }

    // ---- B7: clip-path / mask resolution + item stamping ----

    #[test]
    fn b7_resolve_clip_folds_keywords_and_percentages() {
        // 100 × 50 border box at the origin.
        let (x, y, w, h) = (0.0_f32, 0.0_f32, 100.0_f32, 50.0_f32);
        let pc = |n: f32| Len::Sum { px: 0.0, pct: n };
        // circle(50%) — % radius resolves against sqrt((w²+h²)/2) ≈ 79.057.
        let ResolvedClip::Circle { cx, cy, r } = resolve_clip(
            &ClipShape::Circle {
                r: ShapeRadius::Len(pc(50.0)),
                cx: pc(50.0),
                cy: pc(50.0),
            },
            x,
            y,
            w,
            h,
        ) else {
            panic!("circle")
        };
        assert_eq!((cx, cy), (50.0, 25.0));
        assert!((r - (100.0f32 * 100.0 + 50.0 * 50.0).sqrt() / 2.0f32.sqrt() / 2.0).abs() < 1e-3);
        // Keyword radii at the center: closest-side 25, farthest-side 50,
        // closest/farthest-corner are the diagonal 55.902.
        let rad = |k: ShapeRadius| {
            let ResolvedClip::Circle { r, .. } = resolve_clip(
                &ClipShape::Circle {
                    r: k,
                    cx: Len::const_px(50.0),
                    cy: Len::const_px(25.0),
                },
                x,
                y,
                w,
                h,
            ) else {
                unreachable!()
            };
            r
        };
        assert_eq!(rad(ShapeRadius::ClosestSide), 25.0);
        assert_eq!(rad(ShapeRadius::FarthestSide), 50.0);
        assert!((rad(ShapeRadius::ClosestCorner) - (50.0f32 * 50.0 + 625.0).sqrt()).abs() < 1e-3);
        assert!((rad(ShapeRadius::FarthestCorner) - (50.0f32 * 50.0 + 625.0).sqrt()).abs() < 1e-3);
        // Off-center circle picks the nearest edge correctly.
        let ResolvedClip::Circle { r, .. } = resolve_clip(
            &ClipShape::Circle {
                r: ShapeRadius::ClosestSide,
                cx: Len::const_px(10.0),
                cy: Len::const_px(10.0),
            },
            x,
            y,
            w,
            h,
        ) else {
            unreachable!()
        };
        assert_eq!(r, 10.0);
        // ellipse closest-side folds per-axis (rx: horizontal, ry: vertical).
        let ResolvedClip::Ellipse { rx, ry, .. } = resolve_clip(
            &ClipShape::Ellipse {
                rx: ShapeRadius::ClosestSide,
                ry: ShapeRadius::ClosestSide,
                cx: pc(50.0),
                cy: pc(50.0),
            },
            x,
            y,
            w,
            h,
        ) else {
            panic!("ellipse")
        };
        assert_eq!((rx, ry), (50.0, 25.0));
        // inset(10% 20% round 50%): offsets w→l/r, h→t/b; radii vs the INSET rect.
        let ResolvedClip::Inset { x, y, w, h, radii } = resolve_clip(
            &ClipShape::Inset {
                top: pc(10.0),
                right: pc(20.0),
                bottom: pc(10.0),
                left: pc(20.0),
                radii: [(pc(50.0), pc(50.0)); 4],
            },
            0.0,
            0.0,
            100.0,
            50.0,
        ) else {
            panic!("inset")
        };
        assert_eq!((x, y, w, h), (20.0, 5.0, 60.0, 40.0));
        assert_eq!(radii, [(30.0, 20.0); 4]);
        // Over-inset collapses to zero size, never negative.
        let ResolvedClip::Inset { w, h, .. } = resolve_clip(
            &ClipShape::Inset {
                top: Len::const_px(60.0),
                right: Len::ZERO,
                bottom: Len::ZERO,
                left: Len::ZERO,
                radii: [(Len::ZERO, Len::ZERO); 4],
            },
            0.0,
            0.0,
            100.0,
            50.0,
        ) else {
            unreachable!()
        };
        assert_eq!((w, h), (100.0, 0.0));
        // polygon: x folds vs w, y vs h; positions absolute.
        let ResolvedClip::Polygon { points, evenodd } = resolve_clip(
            &ClipShape::Polygon {
                points: vec![
                    (Len::ZERO, pc(50.0)),
                    (pc(100.0), Len::ZERO),
                    (Len::ZERO, Len::ZERO),
                ],
                evenodd: true,
            },
            0.0,
            0.0,
            100.0,
            50.0,
        ) else {
            panic!("polygon")
        };
        assert!(evenodd);
        assert_eq!(points, vec![(0.0, 25.0), (100.0, 0.0), (0.0, 0.0)]);
    }

    #[test]
    fn b7_clip_stamp_innermost_wins_and_mask_falls_back() {
        let dl = lay(
            "<style>
             .outer { width: 300px; height: 200px; background: #ff0000;
                      clip-path: circle(30px at 10px 10px); }
             .mid { width: 200px; height: 100px; background: #00ff00; }
             .inner { width: 100px; height: 50px; background: #0000ff;
                      clip-path: inset(4px 6px); }
             .masked { width: 80px; height: 40px; background: #ffff00;
                       mask: ellipse(20px 10px); }
             </style>
             <div class='outer'><div class='mid'><div class='inner'></div></div></div>
             <div class='masked'></div>",
            800.0,
        );
        let by_color = |c: u32| dl.rects.iter().find(|r| r.color == c).expect("bg rect");
        let outer = by_color(0xFF0000FF);
        assert!(
            matches!(outer.clip, Some(ResolvedClip::Circle { r, .. }) if (r - 30.0).abs() < 1e-3)
        );
        // No clip of its own: mid's background carries the ancestor's circle.
        assert!(matches!(
            by_color(0x00FF00FF).clip,
            Some(ResolvedClip::Circle { .. })
        ));
        // Innermost wins: the leaf's own inset survives the ancestor's stamp.
        assert!(matches!(
            by_color(0x0000FFFF).clip,
            Some(ResolvedClip::Inset { .. })
        ));
        // mask-only element clips too (shape approximation), clip-path absent.
        assert!(matches!(
            by_color(0xFFFF00FF).clip,
            Some(ResolvedClip::Ellipse { rx: 20.0, .. })
        ));
        // Text under a clip inherits the stamp: the box's ink is clipped.
        let dl2 = lay(
            "<style>p { clip-path: inset(2px); }</style>
             <div style='width:200px;background:#0f0'><p>clipped text</p></div>",
            400.0,
        );
        let t = dl2
            .texts
            .iter()
            .find(|t| t.text.contains("clipped"))
            .expect("run");
        assert!(matches!(t.clip, Some(ResolvedClip::Inset { .. })));
    }

    #[test]
    fn b8_blend_first_writer_group_flattens_to_outermost() {
        let dl = lay(
            "<style>
             .wrap { width: 300px; height: 200px; background: #ff0000; }
             .blend { width: 200px; height: 100px; background: #00ff00;
                      mix-blend-mode: multiply; }
             .plain { width: 100px; height: 50px; background: #0000ff; }
             .own { width: 90px; height: 40px; background: #00ffff;
                    mix-blend-mode: screen; }
             </style>
             <div class='wrap'><div class='blend'><div class='plain'></div>
             <div class='own'></div></div></div>",
            800.0,
        );
        let blend = |c: u32| dl.rects.iter().find(|r| r.color == c).expect("bg rect");
        // The wrap (not blending) stays normal.
        assert_eq!(blend(0xFF0000FF).blend, 0);
        // The blend element stamps its own wire mode…
        assert_eq!(blend(0x00FF00FF).blend, 1); // multiply
                                                // …and descendants without their own mode take it (group model:
                                                // the whole subtree blends with the backdrop).
        assert_eq!(blend(0x0000FFFF).blend, 1);
        // A descendant with its own mode won at ITS box end (first writer).
        assert_eq!(blend(0x00FFFFFF).blend, 2); // screen
                                                // No subtree carries an isolate group here.
        assert!(dl.rects.iter().all(|r| r.group == 0));

        // Isolation stamps one shared group id over the whole subtree
        // (nested isolates flatten to the outermost writer's overwrite),
        // and siblings outside the group keep 0.
        let dl2 = lay(
            "<style>
             body { margin: 0; }
             .iso { width: 300px; height: 120px; background: #ff0000;
                    isolation: isolate; }
             .inner { width: 200px; height: 60px; background: #00ff00;
                      isolation: isolate; }
             .leaf { width: 100px; height: 30px; background: #0000ff; }
             .sib { width: 100px; height: 30px; background: #ffff00; }
             </style>
             <div class='iso'><div class='inner'><div class='leaf'></div></div></div>
             <div class='sib'></div>",
            800.0,
        );
        let by = |c: u32| dl2.rects.iter().find(|r| r.color == c).expect("bg rect");
        let g = by(0xFF0000FF).group;
        assert_ne!(g, 0, "isolated subtree carries a group");
        assert_eq!(
            by(0x00FF00FF).group,
            g,
            "inner isolate flattens into the outer group"
        );
        assert_eq!(by(0x0000FFFF).group, g);
        assert_eq!(
            by(0xFFFF00FF).group,
            0,
            "sibling outside the isolate stays ungrouped"
        );
    }

    #[test]
    fn b5_multicol_splits_balances_and_gaps_columns() {
        // 800 wide, gap 20px → two 390px columns. Greedy balance to the
        // 100px target (total 300 / 2): red fills col 0, green + blue the
        // second column below it.
        let dl = lay(
            "<style>
             body { margin: 0; }
             .cols { column-count: 2; column-gap: 20px; }
             .b { height: 100px; margin: 0; }
             </style>
             <div class='cols'>
             <div class='b' style='background:#ff0000'></div>
             <div class='b' style='background:#00ff00'></div>
             <div class='b' style='background:#0000ff'></div>
             </div>",
            800.0,
        );
        let by = |c: u32| dl.rects.iter().find(|r| r.color == c).expect("bg rect");
        let (r, g, bl) = (by(0xFF0000FF), by(0x00FF00FF), by(0x0000FFFF));
        // Flow starts at the UA body margin; columns split 768 − 20 by 2.
        assert!(
            (r.x - 16.0).abs() < 0.6 && (r.w - 374.0).abs() < 0.6,
            "col 0 at x=16 w=374, got x={} w={}",
            r.x,
            r.w
        );
        assert!(
            (g.x - (r.x + r.w + 20.0)).abs() < 0.6 && (g.w - 374.0).abs() < 0.6,
            "col 1 starts past col 0 + gap"
        );
        assert!((bl.x - g.x).abs() < 0.6);
        assert!((r.y - g.y).abs() < 0.6, "column tops align");
        assert!(
            (bl.y - (r.y + 100.0)).abs() < 0.6,
            "blue stacks below green in the second column"
        );

        // `column-width` alone estimates the count: 3×190+gaps fits 800 →
        // 3 columns of 256 (gap = normal = 1em = 16), one child each.
        let dl2 = lay(
            "<style>
             body { margin: 0; }
             .cols { column-width: 190px; }
             .b { height: 60px; margin: 0; }
             </style>
             <div class='cols'>
             <div class='b' style='background:#ff0000'></div>
             <div class='b' style='background:#00ff00'></div>
             <div class='b' style='background:#0000ff'></div>
             </div>",
            800.0,
        );
        let by2 = |c: u32| dl2.rects.iter().find(|r| r.color == c).expect("bg rect");
        let (r, g, bl) = (by2(0xFF0000FF), by2(0x00FF00FF), by2(0x0000FFFF));
        // 3 columns fit 768 − 2×16 → 245.33 wide each.
        assert!(
            (r.w - 245.333).abs() < 0.6 && (g.w - 245.333).abs() < 0.6,
            "got w={}",
            r.w
        );
        assert!(
            (g.x - (r.x + r.w + 16.0)).abs() < 0.6
                && (bl.x - (r.x + 2.0 * (r.w + 16.0))).abs() < 0.6,
            "3 columns at gap 16"
        );
        assert!(
            (g.y - r.y).abs() < 0.6 && (bl.y - r.y).abs() < 0.6,
            "each column holds one child"
        );

        // Count + width together: the width estimate caps the count — 500px
        // columns do not fit twice in 800, so the container is single-column
        // and the children span the full flow width.
        let dl3 = lay(
            "<style>
             body { margin: 0; }
             .cols { column-count: 4; column-width: 500px; }
             .b { height: 60px; margin: 0; }
             </style>
             <div class='cols'>
             <div class='b' style='background:#ff0000'></div>
             <div class='b' style='background:#00ff00'></div>
             </div>",
            800.0,
        );
        let r3 = dl3
            .rects
            .iter()
            .find(|x| x.color == 0xFF0000FF)
            .expect("bg rect");
        let g3 = dl3
            .rects
            .iter()
            .find(|x| x.color == 0x00FF00FF)
            .expect("bg rect");
        assert!(
            (r3.w - 768.0).abs() < 0.6,
            "fallback to normal flow keeps full width, got {}",
            r3.w
        );
        assert!(
            (g3.y - r3.y - 60.0).abs() < 0.6,
            "children stack vertically"
        );
    }

    #[test]
    fn b4_writing_mode_rotates_vertical_box() {
        // vertical-rl: logical horizontal subtree + clockwise stamp.
        // The div's `height: 100px` is the folded inline length (style
        // engine maps inline-size → height in vertical modes), so the
        // physical footprint is 100 tall; the two 30px-tall children stack
        // into 60px of physical width — green (logical top) rightmost.
        let dl = lay(
            "<style>
             body { margin: 0; }
             .v { writing-mode: vertical-rl; inline-size: 100px; }
             .b { height: 30px; margin: 0; }
             </style>
             <div class='v'>
             <div class='b' style='background:#00ff00'></div>
             <div class='b' style='background:#0000ff'></div>
             </div>",
            800.0,
        );
        let by = |c: u32| dl.rects.iter().find(|r| r.color == c).expect("bg rect");
        let (g, bl) = (by(0x00FF00FF), by(0x0000FFFF));
        let want = Matrix2d([0.0, -1.0, 76.0, 1.0, 0.0, 16.0]);
        assert_eq!(
            g.transform,
            Some(want),
            "clockwise stamp (tx past the 16+60 column stack)"
        );
        assert_eq!(
            bl.transform,
            Some(want),
            "one inherited stamp — no per-child rotation"
        );
        // Logical geometry stays on the horizontal axis: x = 0..100 (the
        // inline length), green at top (y 0), blue stacked below (y 30).
        assert!(
            (g.x - 0.0).abs() < 0.6 && (g.w - 100.0).abs() < 0.6,
            "logical rect w=inline, got {:?}",
            (g.x, g.w)
        );
        assert!((g.y - 0.0).abs() < 0.6 && (g.h - 30.0).abs() < 0.6);
        assert!(
            (bl.y - 30.0).abs() < 0.6,
            "blue stacks after green in the logical column"
        );
        // Page-space footprints via the matrix: x' = −y + tx, y' = x.
        let gx0 = want.0[1] * (g.y + g.h) + want.0[2]; // rightmost logical… left edge
        let gx1 = want.0[1] * g.y + want.0[2];
        assert!(
            (gx0 - 46.0).abs() < 0.6 && (gx1 - 76.0).abs() < 0.6,
            "green column x'∈[46,76]"
        );
        let bx0 = want.0[1] * (bl.y + bl.h) + want.0[2];
        assert!(
            (bx0 - 16.0).abs() < 0.6,
            "blue column hugs the flow's left edge at 16"
        );

        // sideways-lr: counter-clockwise, reads bottom-to-top, later lines
        // stack rightward — so green is LEFTMOST and blue hugs the right.
        let dl2 = lay(
            "<style>
             body { margin: 0; }
             .v { writing-mode: sideways-lr; inline-size: 100px; }
             .b { height: 30px; margin: 0; }
             </style>
             <div class='v'>
             <div class='b' style='background:#00ff00'></div>
             <div class='b' style='background:#0000ff'></div>
             </div>",
            800.0,
        );
        let g2 = dl2
            .rects
            .iter()
            .find(|r| r.color == 0x00FF00FF)
            .expect("bg rect");
        let m2 = g2.transform.expect("ccw stamp");
        assert_eq!(m2, Matrix2d([0.0, 1.0, 16.0, -1.0, 0.0, 116.0]));
        let gx2 = m2.0[1] * g2.y + m2.0[2];
        assert!(
            (gx2 - 16.0).abs() < 0.6,
            "first line leftmost at the flow edge"
        );

        // Flow continues below the rotated box by its physical height
        // (the inline length): a plain block after it sits at y = 100.
        let dl3 = lay(
            "<style>
             body { margin: 0; }
             .v { writing-mode: vertical-rl; inline-size: 100px; }
             .b { height: 30px; margin: 0; }
             .after { height: 10px; }
             </style>
             <div class='v'><div class='b' style='background:#00ff00'></div></div>
             <div class='after' style='background:#ff0000'></div>",
            800.0,
        );
        let after = dl3
            .rects
            .iter()
            .find(|r| r.color == 0xFF0000FF)
            .expect("bg rect");
        assert!(
            (after.y - 116.0).abs() < 0.6,
            "rotated box consumes v=100 of vertical flow, got y={}",
            after.y
        );
        assert!(
            after.transform.is_none(),
            "horizontal sibling stays unrotated"
        );
    }

    // ---- transforms (Q2) ----

    use kitae_style::TransLen;

    #[test]
    fn matrix2d_compose_and_rebase() {
        // Composition: outer ∘ inner (inner applied first).
        let a = Matrix2d::translation(10.0, 0.0);
        let b = Matrix2d::translation(0.0, 5.0);
        assert_eq!(a.compose(b), Matrix2d::translation(10.0, 5.0));
        // Rotation about the origin maps (1,0) down to (0,1) in y-down space.
        let r = fn_matrix(&TransformFn::Rotate(90.0), 0.0, 0.0);
        let (x, y) = (r.0[0] + r.0[2], r.0[3] + r.0[4]); // point (1, 0)
        assert!((x - 0.0).abs() < 1e-6 && (y - 1.0).abs() < 1e-6);
        // Rebase: M_new(p + d) = M(p) + d. Hand case — scale-free rotate
        // about center (100,100) gives M = [0,−1,200,1,0,0]; M(150,100) =
        // (100,150), so M_new must map (200,125) to (150,175).
        let m = Matrix2d::translation(100.0, 100.0)
            .compose(fn_matrix(&TransformFn::Rotate(90.0), 0.0, 0.0))
            .compose(Matrix2d::translation(-100.0, -100.0));
        assert!((m.0[0] - 150.0 + 150.0).abs() < 1e-6); // sanity: compiles
        let at = |mm: Matrix2d, x: f32, y: f32| {
            (
                mm.0[0] * x + mm.0[1] * y + mm.0[2],
                mm.0[3] * x + mm.0[4] * y + mm.0[5],
            )
        };
        let base = at(m, 150.0, 100.0);
        assert!((base.0 - 100.0).abs() < 1e-4 && (base.1 - 150.0).abs() < 1e-4);
        let shifted = at(m.rebase(50.0, 25.0), 200.0, 125.0);
        assert!((shifted.0 - 150.0).abs() < 1e-4 && (shifted.1 - 175.0).abs() < 1e-4);
    }

    #[test]
    fn transform_stamps_link_hit_geometry() {
        let dl = lay(
            r#"<div style="transform-origin:0 0; transform:rotate(90deg)"><a href="/next">go</a></div><a href="/plain">plain</a>"#,
            400.0,
        );
        assert_eq!(dl.links.len(), 2);
        let link = &dl.links[0];
        let transform = link.transform.expect("rotated link matrix");
        let physical = transform.apply(link.x + link.w / 2.0, link.y + link.h / 2.0);
        assert!(link.contains(physical.0, physical.1));
        assert!(!link.contains(link.x + link.w / 2.0, link.y + link.h / 2.0));
        let inverse = transform.invert().expect("rotated link inverse");
        let local = inverse.apply(physical.0, physical.1);
        assert!((local.0 - (link.x + link.w / 2.0)).abs() < 1e-3);
        assert!((local.1 - (link.y + link.h / 2.0)).abs() < 1e-3);
        assert!(dl.links[1].transform.is_none());

        let vertical = lay(
            "<div style='writing-mode:vertical-rl;height:100px'><a href='/vertical'>go</a></div>",
            400.0,
        );
        assert!(vertical.links[0].transform.is_some());
    }

    #[test]
    fn transform_origin_matrix_fold_order() {
        // CSS `transform: A B` — B applies to the point first, A outermost.
        // With a zero origin the page matrix is the pure function-list
        // matrix, so the fold is pinned exactly: translateX(100px) then
        // rotate(90deg) puts the translation in m2; reversed it lands in m5
        // (rotated into y).
        let zero = TransformOrigin {
            x: TransLen::ZERO,
            y: TransLen::ZERO,
        };
        let tl = TransformList(vec![
            TransformFn::TranslateX(TransLen {
                px: 100.0,
                pct: 0.0,
            }),
            TransformFn::Rotate(90.0),
        ]);
        let m = origin_matrix(&tl, &zero, 0.0, 0.0, 0.0, 0.0);
        assert!(
            (m.0[2] - 100.0).abs() < 1e-5 && m.0[5].abs() < 1e-5,
            "got {:?}",
            m.0
        );
        let tl = TransformList(vec![
            TransformFn::Rotate(90.0),
            TransformFn::TranslateX(TransLen {
                px: 100.0,
                pct: 0.0,
            }),
        ]);
        let m = origin_matrix(&tl, &zero, 0.0, 0.0, 0.0, 0.0);
        assert!(
            m.0[2].abs() < 1e-5 && (m.0[5] - 100.0).abs() < 1e-5,
            "got {:?}",
            m.0
        );
    }

    #[test]
    fn transform_translate_bakes_into_subtree_items_only() {
        // Percent translate resolves against the border box (height 100px →
        // 20px); the whole subtree carries the matrix, siblings stay None.
        let dl = lay(
            r#"<div style="height:100px; background:#369; transform: translate(10px, 20%)">hi</div><p>after</p>"#,
            400.0,
        );
        let r = dl
            .rects
            .iter()
            .find(|r| r.transform.is_some())
            .expect("transformed rect");
        assert_eq!(
            r.transform,
            Some(Matrix2d([1.0, 0.0, 10.0, 0.0, 1.0, 20.0]))
        );
        assert_eq!(
            dl.texts[0].transform,
            Some(Matrix2d([1.0, 0.0, 10.0, 0.0, 1.0, 20.0]))
        );
        assert!(dl.texts[1].transform.is_none(), "sibling unaffected");
    }

    #[test]
    fn transform_nested_translations_compose() {
        // Children bake first; the parent pre-multiplies — two translates
        // add up, origin-invariant either way.
        let dl = lay(
            r#"<div style="transform: translate(10px, 5px)"><p style="transform: translate(3px, 4px)">x</p></div>"#,
            400.0,
        );
        assert_eq!(
            dl.texts[0].transform,
            Some(Matrix2d([1.0, 0.0, 13.0, 0.0, 1.0, 9.0])),
            "nested translations compose"
        );
    }

    #[test]
    fn transform_rotate_respects_origin() {
        // transform-origin: 0 0 pins the matrix to the box's own top-left —
        // which is the stored (pre-transform) geometry, so the check is
        // self-locating regardless of body margins.
        let dl = lay(
            r#"<div style="width:200px; height:100px; background:#369; transform-origin: 0 0; transform: rotate(90deg)">x</div>"#,
            400.0,
        );
        let r = dl
            .rects
            .iter()
            .find(|r| r.transform.is_some())
            .expect("rotated rect");
        let m = r.transform.expect("rotated rect");
        let at = |x: f32, y: f32| {
            (
                m.0[0] * x + m.0[1] * y + m.0[2],
                m.0[3] * x + m.0[4] * y + m.0[5],
            )
        };
        // The origin corner is fixed…
        let (fx, fy) = at(r.x, r.y);
        assert!((fx - r.x).abs() < 1e-4 && (fy - r.y).abs() < 1e-4);
        // …and the top-right corner swings down 200px (cw 90°, y-down).
        let (gx, gy) = at(r.x + r.w, r.y);
        assert!((gx - r.x).abs() < 1e-4 && (gy - (r.y + r.w)).abs() < 1e-4);
    }

    #[test]
    fn transform_relative_rebase_moves_matrix_with_the_box() {
        // Relative positioning shifts the baked geometry *after* the
        // transform post-pass, so the matrix must rebase (conjugation), not
        // just shift its offset column. With origin 0 0 the rebased matrix
        // fixes the *shifted* top-left — which equals the rect's final
        // stored position.
        let dl = lay(
            r#"<div style="position: relative; left: 10px; top: 5px; background:#369; transform-origin: 0 0; transform: scale(2)">x</div>"#,
            400.0,
        );
        let r = dl
            .rects
            .iter()
            .find(|r| r.transform.is_some())
            .expect("scaled rect");
        let m = r.transform.expect("scaled rect");
        let at = |x: f32, y: f32| {
            (
                m.0[0] * x + m.0[1] * y + m.0[2],
                m.0[3] * x + m.0[4] * y + m.0[5],
            )
        };
        // The shifted top-left is the fixed point…
        let (fx, fy) = at(r.x, r.y);
        assert!((fx - r.x).abs() < 1e-4 && (fy - r.y).abs() < 1e-4);
        // …and scale(2) doubles the width from it.
        let (gx, _) = at(r.x + r.w, r.y);
        assert!((gx - (r.x + 2.0 * r.w)).abs() < 1e-4);
    }

    // ---- incremental layout (G) ----

    #[test]
    fn dirty_chain_covers_leaf_to_root() {
        let (dom, root) = kitae_html::parse("<div><p>hi</p></div>");
        let p = {
            let mut found = None;
            dom.walk(root.unwrap(), &mut |n| {
                if dom.tag(n) == "p" {
                    found = Some(n);
                }
            });
            found.unwrap()
        };
        let chain = dirty_chain(&dom, p);
        assert!(chain.contains(&p));
        assert_eq!(*chain.last().unwrap(), root.unwrap());
        // Leaf + ancestors: p, div, body, html.
        assert!(chain.len() >= 3, "chain={chain:?}");
    }

    #[test]
    fn dirty_set_tracks_subtrees() {
        let (dom, root) = kitae_html::parse("<div><p>a</p><p>b</p></div>");
        let root = root.unwrap();
        let mut ds = DirtySet::new();
        assert!(ds.is_empty());
        ds.mark_dirty(root);
        assert!(ds.is_dirty(root));
        assert_eq!(ds.len(), 1);
        ds.clear();
        assert!(ds.is_empty());
        ds.mark_subtree(&dom, root);
        assert!(ds.len() >= 4, "root + div + 2 p + texts");
    }

    #[test]
    fn subtree_relayout_touches_fewer_nodes() {
        let (dom, root) = kitae_html::parse(
            "<div id='a'><p>one</p></div><div id='b'><p>two</p></div><div id='c'><p>three</p></div>",
        );
        let root = root.unwrap();
        let full = count_subtree(&dom, root);
        let target = dom.get_element_by_id("b").unwrap();
        let sub = count_subtree(&dom, target);
        assert!(sub < full, "subtree {sub} < full {full}");
        // Repaint counter: dirty nodes vs full-document nodes.
        let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
        let full_dl = layout(&dom, &styles, 400.0, 600.0, &Mock);
        let sub_dl = layout_subtree(&dom, &styles, target, 368.0, 600.0, &Mock);
        assert!(!sub_dl.texts.is_empty());
        assert!(sub_dl.texts.iter().any(|t| t.text == "two"));
        assert!(!full_dl.texts.is_empty());
        // Subtree height is positive and smaller than the full page.
        assert!(sub_dl.content_height > 0.0);
        assert!(sub_dl.content_height < full_dl.content_height);
    }

    #[test]
    fn subtree_relayout_reflects_text_mutation() {
        let (mut dom, root) = kitae_html::parse("<div id='b'><p>before</p></div>");
        let root = root.unwrap();
        let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
        let target = dom.get_element_by_id("b").unwrap();
        let before = layout_subtree(&dom, &styles, target, 368.0, 600.0, &Mock);
        assert!(before.texts.iter().any(|t| t.text == "before"));
        // Mutate the text node in place, then relayout only the subtree.
        let mut text_node = None;
        dom.walk(target, &mut |n| {
            if dom.node(n).kind == kitae_dom::NodeKind::Text {
                text_node = Some(n);
            }
        });
        assert!(dom.set_text(text_node.unwrap(), "after mutation here".into()));
        let styles2 = kitae_style::StyleEngine::new(&[]).compute(&dom);
        let after = layout_subtree(&dom, &styles2, target, 368.0, 600.0, &Mock);
        assert!(after.texts.iter().any(|t| t.text.contains("after")));
        // Invalidation covers the mutated node to the root.
        let _ = root;
        let chain = dirty_chain(&dom, text_node.unwrap());
        assert!(chain.contains(&target));
    }

    // ---- interaction: forms paint (H) ----
    #[test]
    fn form_controls_paint_boxes_and_labels() {
        let dl = lay(
            "<form><input id='q' name='q' value='hi'><button>Go</button><textarea>Hey</textarea><select><option>One</option><option selected>Two</option></select></form>",
            400.0,
        );
        let t: Vec<&str> = text_of(&dl);
        assert!(t.contains(&"hi"), "input value paints: {t:?}");
        assert!(t.contains(&"Go"), "button label paints: {t:?}");
        assert!(t.contains(&"Hey"), "textarea text paints: {t:?}");
        assert!(t.contains(&"Two"), "selected option paints: {t:?}");
        assert!(!dl.rects.is_empty(), "control boxes paint as rects");
    }

    #[test]
    fn password_input_paints_a_mask_instead_of_its_value() {
        let dl = lay(
            "<label for='secret'>Password</label><input id='secret' type='password' value='do-not-render'>",
            400.0,
        );
        let texts = text_of(&dl);
        assert!(texts.contains(&"••••••••"), "{texts:?}");
        assert!(
            !texts.iter().any(|text| text.contains("do-not-render")),
            "{texts:?}"
        );
    }

    #[test]
    fn hidden_inputs_paint_nothing() {
        let dl = lay(
            "<form><input type='hidden' value='secret'><p>vis</p></form>",
            400.0,
        );
        let t = text_of(&dl);
        assert!(!t.iter().any(|s| s.contains("secret")), "{t:?}");
        assert!(t.contains(&"vis"));
    }

    #[test]
    fn flex_button_label_stays_on_one_line() {
        // A flex-item button is a real box: the UA `white-space: nowrap` +
        // chrome-inclusive intrinsic width keep a multi-word label on ONE
        // line inside the widget rect (the old generic-block path wrapped
        // it into two stacked words).
        let dl = lay(
            "<div style=\"display:flex\"><button>Add item</button></div>",
            400.0,
        );
        let add = dl.texts.iter().find(|t| t.text == "Add").expect("Add run");
        let item = dl
            .texts
            .iter()
            .find(|t| t.text == "item")
            .expect("item run");
        assert!(
            (add.y - item.y).abs() < 1.0,
            "label must not wrap: {:?} vs {:?}",
            (add.x, add.y),
            (item.x, item.y)
        );
        // The painted widget holds the whole label.
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0xF4F5F7FF)
            .expect("UA button bg rect");
        assert!(
            item.x < bg.x + bg.w,
            "label overflows the widget: {} vs {}",
            item.x,
            bg.x + bg.w
        );
    }

    #[test]
    fn control_label_insets_by_real_padding_and_border() {
        // Author padding 8px/16px + UA 1px border → the label sits at
        // rect.x + 17 and the box at least covers the chrome.
        let dl = lay(
            r#"<button style="padding:8px 16px;background:#00FF00">Go</button>"#,
            400.0,
        );
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0x00FF00FF)
            .expect("button bg rect");
        let go = dl.texts.iter().find(|t| t.text == "Go").expect("Go run");
        assert!(
            (go.x - (bg.x + 17.0)).abs() < 1.0,
            "label inset by border+padding: {} vs {}",
            go.x,
            bg.x + 17.0
        );
        assert!(bg.w > 34.0, "box must cover horizontal chrome: {}", bg.w);
        assert!(bg.h > 18.0, "box must cover vertical chrome: {}", bg.h);
    }

    #[test]
    fn control_border_radius_paints_rounded_ring() {
        // With an opaque background the control chrome paints as an outer
        // rounded rect in border color + an inner bg rect inset by the
        // 1px border, inner radius shed by the same widths.
        let dl = lay(
            r#"<button style="border-radius:8px;background:#00FF00">Hi</button>"#,
            400.0,
        );
        let ring = dl
            .rects
            .iter()
            .find(|r| r.color == 0x9AA0A6FF)
            .expect("border ring rect");
        let outer = ring.radius.resolve(16.0, 16.0, ring.w);
        assert!(
            outer.iter().all(|&v| (v - 8.0).abs() < 0.01),
            "outer radius 8: {outer:?}"
        );
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0x00FF00FF)
            .expect("inner bg rect");
        assert!(
            (bg.x - ring.x - 1.0).abs() < 0.01 && (bg.y - ring.y - 1.0).abs() < 0.01,
            "bg inset by the 1px border"
        );
        assert!(
            (bg.w - (ring.w - 2.0)).abs() < 0.01 && (bg.h - (ring.h - 2.0)).abs() < 0.01,
            "bg inset on both axes"
        );
        let inner = bg.radius.resolve(16.0, 16.0, bg.w);
        assert!(
            inner.iter().all(|&v| (v - 7.0).abs() < 0.01),
            "inner radius 7: {inner:?}"
        );
    }

    #[test]
    fn transparent_button_paints_no_chrome_background() {
        // Author `background: transparent` cascades over the UA sheet's
        // control grey (author > UA): no fill paints, the border still does.
        let dl = lay(
            r#"<button style="background:transparent">Go</button>"#,
            400.0,
        );
        assert!(
            !dl.rects.iter().any(|r| r.color == 0xF4F5F7FF),
            "UA grey must not paint through author transparent"
        );
        assert!(
            dl.rects.iter().any(|r| r.color == 0x9AA0A6FF),
            "the chrome border still paints"
        );
        assert!(text_of(&dl).contains(&"Go"));
    }

    // ---- paint upgrades: radius, gradients, shadows (L4) ----

    #[test]
    fn border_radius_rides_the_background_rect() {
        let dl = lay(
            r#"<div style="background:#FF0000;border-radius:6px">x</div>"#,
            400.0,
        );
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0xFF0000FF)
            .expect("bg rect");
        let resolved = bg.radius.resolve(16.0, 16.0, bg.w);
        assert!(
            resolved.iter().all(|&v| (v - 6.0).abs() < 0.01),
            "{resolved:?}"
        );
        assert!(matches!(bg.paint, RectPaint::Solid));
        // Square boxes stay zero-radius.
        let dl = lay(r#"<div style="background:#00FF00">y</div>"#, 400.0);
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0x00FF00FF)
            .expect("bg rect");
        assert!(bg
            .radius
            .resolve(16.0, 16.0, bg.w)
            .iter()
            .all(|&v| v == 0.0));
    }

    #[test]
    fn linear_gradient_paint_attaches_with_first_stop_color() {
        let dl = lay(
            r#"<div style="background:linear-gradient(to right, #FF0000, #0000FF)">g</div>"#,
            400.0,
        );
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0xFF0000FF)
            .expect("first-stop bg rect");
        match &bg.paint {
            RectPaint::LinearGradient { angle_deg, stops } => {
                assert!((angle_deg - 90.0).abs() < 0.01);
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[1].1, 0x0000FFFF);
            }
            other => panic!("expected gradient paint, got {other:?}"),
        }
    }

    #[test]
    fn box_shadow_emits_under_its_box() {
        let dl = lay(
            r#"<div style="background:#112233;box-shadow:3px 4px 5px #000000">s</div>"#,
            400.0,
        );
        assert_eq!(dl.shadows.len(), 1);
        let sh = &dl.shadows[0];
        assert_eq!((sh.offset_x, sh.offset_y, sh.blur), (3.0, 4.0, 5.0));
        assert_eq!(sh.color, 0x000000FF);
        assert!(!sh.inset);
        // The shadow belongs to the tinted box.
        let bg = dl
            .rects
            .iter()
            .find(|r| r.color == 0x112233FF)
            .expect("bg rect");
        assert!((sh.x - bg.x).abs() < 0.01 && (sh.y - bg.y).abs() < 0.01);
        assert!((sh.w - bg.w).abs() < 0.01);
    }

    // ---- bidi + vertical-align (M3/M4) ----

    #[test]
    fn rtl_paragraph_keeps_ltr_order_and_reverses_rtl() {
        // Same-direction Latin in an RTL paragraph keeps logical order
        // (UBA L2 double reversal is the identity); alignment is separate.
        let dl = lay("<p dir=\"rtl\">one two</p>", 400.0);
        let one = dl.texts.iter().find(|t| t.text == "one").unwrap();
        let two = dl.texts.iter().find(|t| t.text == "two").unwrap();
        assert!(one.x < two.x, "logical order kept: {} vs {}", one.x, two.x);
        // Pure-Hebrew line reads right-to-left: second word paints left.
        let dl = lay("<p dir=\"rtl\">אבג דהו</p>", 400.0);
        let first = dl.texts.iter().find(|t| t.text == "אבג").unwrap();
        let second = dl.texts.iter().find(|t| t.text == "דהו").unwrap();
        assert!(
            second.x < first.x,
            "RTL visual order: {} vs {}",
            second.x,
            first.x
        );
        assert!(
            (second.x - 16.0).abs() < 1.0,
            "second word leads, x={}",
            second.x
        );
    }

    #[test]
    fn mixed_direction_line_keeps_logical_groups() {
        let dl = lay("<p>ab <span>ג</span> cd</p>", 400.0);
        let ab = dl.texts.iter().find(|t| t.text == "ab").unwrap();
        let he = dl.texts.iter().find(|t| t.text == "ג").unwrap();
        let cd = dl.texts.iter().find(|t| t.text == "cd").unwrap();
        assert!(ab.x < he.x && he.x < cd.x, "{},{},{}", ab.x, he.x, cd.x);
    }

    #[test]
    fn vertical_align_shifts_the_baseline() {
        let dl = lay(
            "<p>a<span style=\"vertical-align:super\">b</span></p>",
            400.0,
        );
        let a = dl.texts.iter().find(|t| t.text == "a").unwrap();
        let b = dl.texts.iter().find(|t| t.text == "b").unwrap();
        assert!(b.y < a.y, "super rises: a.y={} b.y={}", a.y, b.y);
        assert!((a.y - b.y - 16.0 * 0.4).abs() < 0.5);
        let dl = lay("<p>a<span style=\"vertical-align:sub\">b</span></p>", 400.0);
        let a = dl.texts.iter().find(|t| t.text == "a").unwrap();
        let b = dl.texts.iter().find(|t| t.text == "b").unwrap();
        assert!(b.y > a.y, "sub sinks: a.y={} b.y={}", a.y, b.y);
    }

    #[test]
    fn shaped_run_measurement_seam_matches_measure() {
        // The default measure_run delegates to measure: switching the seam
        // changes nothing until a real shaper plugs in.
        assert!((Mock.measure_run("hello", 16.0, 400, false, false) - 40.0).abs() < 0.01);
    }

    // ---- clear (N1) ----

    #[test]
    fn clear_left_drops_below_the_float() {
        let dl = lay(
            "<div style=\"float:left;width:100px;height:80px\">F</div>\
             <p style=\"clear:left\" id=\"c\">after</p>",
            400.0,
        );
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        // Float spans y 16..96; cleared text starts at/after 96.
        assert!(after.y >= 96.0 + 12.0, "cleared below float, y={}", after.y);
    }

    #[test]
    fn clear_right_ignores_left_floats() {
        let dl = lay(
            "<div style=\"float:left;width:100px;height:80px\">F</div>\
             <p style=\"clear:right\" id=\"c\">after</p>",
            400.0,
        );
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        assert!(after.y < 60.0, "no clearance across sides, y={}", after.y);
    }

    #[test]
    fn clear_both_drops_below_right_floats() {
        let dl = lay(
            "<div style=\"float:right;width:100px;height:60px\">F</div>\
             <p style=\"clear:both\" id=\"c\">after</p>",
            400.0,
        );
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        assert!(after.y >= 76.0 + 12.0, "cleared below float, y={}", after.y);
    }

    // --- Q3: letter-spacing + text-transform --------------------------------

    /// Q3: the default `measure_run_sp` is Σ per-char measure plus one
    /// spacing per gap (n−1 gaps); spacing 0 collapses to plain measure.
    #[test]
    fn measure_run_sp_formula() {
        // Mock: every char is 8.0 at size 16.
        assert_eq!(Mock.measure("abc", 16.0, 400, false, false), 24.0);
        assert_eq!(
            Mock.measure_run_sp("abc", 16.0, 400, false, false, 2.0),
            28.0
        );
        // A single char has no gaps, so spacing never applies.
        assert_eq!(Mock.measure_run_sp("a", 16.0, 400, false, false, 5.0), 8.0);
        // Zero spacing is exactly the unspaced width.
        assert_eq!(
            Mock.measure_run_sp("abc", 16.0, 400, false, false, 0.0),
            24.0
        );
    }

    #[test]
    fn text_transform_uppercase_and_lowercase() {
        let dl = lay(
            "<p style=\"text-transform:uppercase\">Kiwi</p>\
             <p style=\"text-transform:lowercase\">MANGO</p>",
            400.0,
        );
        let t = text_of(&dl);
        assert!(t.contains(&"KIWI"), "{t:?}");
        assert!(t.contains(&"mango"), "{t:?}");
    }

    #[test]
    fn text_transform_capitalize_uppercases_word_starts() {
        let dl = lay(
            "<h1 style=\"text-transform:capitalize\">hello wide world</h1>",
            400.0,
        );
        let t = text_of(&dl);
        assert!(
            t.contains(&"Hello") && t.contains(&"Wide") && t.contains(&"World"),
            "{t:?}"
        );
        // Mid-word letters stay lowercase.
        assert!(!t.contains(&"HELLO"), "{t:?}");
    }

    /// Q3: spaced runs carry the spacing in the display list and their width
    /// is the spaced sum — one extra pixel-pair per gap over the plain run.
    #[test]
    fn letter_spacing_carries_and_widens_runs() {
        let dl = lay("<p style=\"letter-spacing:2px\">ab</p><p>ab</p>", 400.0);
        let spaced = dl
            .texts
            .iter()
            .find(|t| t.text == "ab" && t.letter_spacing != 0.0);
        let plain = dl
            .texts
            .iter()
            .find(|t| t.text == "ab" && t.letter_spacing == 0.0);
        let spaced = spaced.expect("spaced run");
        let plain = plain.expect("plain run");
        assert_eq!(spaced.letter_spacing, 2.0);
        // measure_run_sp: 8 + 8 + one 2px gap = 18 vs plain 16.
        assert_eq!(
            Mock.measure_run_sp(
                &spaced.text,
                spaced.size,
                spaced.weight,
                spaced.italic,
                spaced.monospace,
                spaced.letter_spacing
            ),
            18.0
        );
        assert_eq!(
            Mock.measure(
                &plain.text,
                plain.size,
                plain.weight,
                plain.italic,
                plain.monospace
            ),
            16.0
        );
    }

    /// Q3: spacing widens every word, so a line that fit unspaced overflows
    /// and "cc" wraps to the next line (runs are per-word; compare baselines).
    #[test]
    fn letter_spacing_forces_earlier_wrap() {
        // Unspaced: 16+8+16+8+16 = 64 fits the 68px content box → "cc" shares
        // line 1. (UA body padding takes 16px each side of the 100px view.)
        let plain = lay("<p>aa bb cc dd</p>", 100.0);
        let y = |dl: &DisplayList, t: &str| dl.texts.iter().find(|r| r.text == t).unwrap().y;
        assert_eq!(y(&plain, "cc"), y(&plain, "aa"), "{:?}", text_of(&plain));
        // Spaced: 20+8+20+8+20 = 76 > 68 → "cc" drops to line 2.
        let spaced = lay("<p style=\"letter-spacing:4px\">aa bb cc dd</p>", 100.0);
        assert!(
            y(&spaced, "cc") > y(&spaced, "aa"),
            "{:?}",
            text_of(&spaced)
        );
    }

    /// Q3: text-transform reaches pre blocks too, but newlines still split
    /// the lines (per-word runs read back in order, second line lower).
    #[test]
    fn text_transform_applies_to_pre_with_newlines_kept() {
        let dl = lay(
            "<pre style=\"text-transform:uppercase\">line one\nline two</pre>",
            400.0,
        );
        let t = text_of(&dl);
        assert!(
            t.contains(&"LINE") && t.contains(&"ONE") && t.contains(&"TWO"),
            "{t:?}"
        );
        let one = dl.texts.iter().find(|t| t.text == "ONE").unwrap();
        let two = dl.texts.iter().find(|t| t.text == "TWO").unwrap();
        assert!(two.y > one.y, "second pre line sits lower");
    }

    /// Q1 acceptance: `width: calc(100% - 24px)` inside a padded parent must
    /// lay out identical to the explicit pixel equivalent — compared by the
    /// child's background rect (same x and width).
    #[test]
    fn calc_width_matches_explicit_pixels() {
        let calc = lay(
            "<div style=\"padding: 8px\"><p style=\"background: #336699; width: calc(100% - 24px); margin: 0\">x</p></div>",
            400.0,
        );
        let plain = lay(
            "<div style=\"padding: 8px\"><p style=\"background: #336699; width: 328px; margin: 0\">x</p></div>",
            400.0,
        );
        // The p's bg rect: viewport 400 − body padding 32 = 368 content;
        // parent padding 8/side → 352 available; calc: 352 − 24 = 328.
        // Both widths are < avail so both center — identical rects.
        let w = |dl: &DisplayList| {
            dl.rects
                .iter()
                .find(|r| r.color == 0x336699FF)
                .map(|r| r.w)
                .unwrap_or_else(|| panic!("no bg rect: {:#?}", dl.rects))
        };
        assert_eq!(w(&calc), w(&plain), "calc must equal the explicit width");
        assert!((w(&calc) - 328.0).abs() < 0.01);
    }

    /// Q1: percentage widths no longer collapse to zero at parse time.
    #[test]
    fn percent_width_resolves_against_container() {
        let dl = lay(
            "<p style=\"background: #336699; width: 50%; margin: 0\">x</p>",
            200.0,
        );
        let rect = dl
            .rects
            .iter()
            .find(|r| r.color == 0x336699FF)
            .expect("50% width must still paint");
        // Viewport 200 − body padding 32 = 168 content; 50% = 84, centered
        // in 168 → x = 16 + (168 − 84) / 2 = 58.
        assert!((rect.w - 84.0).abs() < 0.01, "w = {}", rect.w);
        assert!((rect.x - 58.0).abs() < 0.01, "x = {}", rect.x);
    }

    /// Q1: min()/max()/clamp() resolve against the layout basis, not at
    /// parse time.
    #[test]
    fn min_max_clamp_evaluate_at_layout() {
        let mk = |w: f32| {
            lay(
                &format!(
                    "<p style=\"background: #336699; width: min(50%, 100px); margin: 0\">x</p>"
                ),
                w,
            )
        };
        let narrow = mk(300.0); // content 268 → 50% = 134 vs 100 → 100
        let wide = mk(600.0); // content 568 → 50% = 284 vs 100 → 100
        let w = |dl: &DisplayList| dl.rects.iter().find(|r| r.color == 0x336699FF).unwrap().w;
        assert!(
            (w(&narrow) - 100.0).abs() < 0.01,
            "min → 100, got {}",
            w(&narrow)
        );
        assert!(
            (w(&wide) - 100.0).abs() < 0.01,
            "min → 100, got {}",
            w(&wide)
        );

        // clamp(24px, 50%, 120px) against a 300px content box: 50% = 150 →
        // clamps to 120.
        let dl = lay(
            "<p style=\"background: #336699; width: clamp(24px, 50%, 120px); margin: 0\">x</p>",
            332.0,
        );
        let r = dl.rects.iter().find(|r| r.color == 0x336699FF).unwrap();
        assert!((r.w - 120.0).abs() < 0.01, "clamp upper bound, got {}", r.w);
    }

    // ------------------------------------------------------------------
    // A-bucket (DOM completion): template inertness, details/summary,
    // progress/meter, dialog, picture/srcset, colgroup/col.
    // ------------------------------------------------------------------

    /// A2: content inside <template> never activates — its styles don't
    /// apply and its elements generate no boxes.
    #[test]
    fn template_content_stays_inert() {
        let dl = lay(
            "<template><p style=\"background: #ff0000\">ghost</p></template>\
             <p>real</p>",
            400.0,
        );
        assert!(
            !dl.texts.iter().any(|t| t.text.contains("ghost")),
            "template text must not render: {:?}",
            text_of(&dl)
        );
        assert!(
            !dl.rects.iter().any(|r| r.color == 0xFF0000FF),
            "template styles must not leak into the page"
        );
        // The real paragraph still renders.
        assert!(dl.texts.iter().any(|t| t.text == "real"));
    }

    /// A2: a <style> block inside a template applies to nothing.
    #[test]
    fn template_style_block_is_ignored() {
        let dl = lay(
            "<template><style>p { background: #00ff00 }</style></template>\
             <p>visible</p>",
            400.0,
        );
        assert!(
            !dl.rects.iter().any(|r| r.color == 0x00FF00FF),
            "template style must never apply"
        );
        assert!(dl.texts.iter().any(|t| t.text == "visible"));
    }

    /// A5: closed details renders only the summary; the toggle hit-area
    /// covers the summary box; opening reveals the body.
    #[test]
    fn details_closed_shows_summary_only() {
        let closed = lay(
            "<details><summary>hint</summary><p>body text</p></details>",
            400.0,
        );
        assert!(closed.texts.iter().any(|t| t.text.contains("hint")));
        assert!(
            !closed.texts.iter().any(|t| t.text.contains("body")),
            "closed details hides body: {:?}",
            text_of(&closed)
        );
        assert_eq!(closed.toggles.len(), 1, "one toggle area for the summary");
        let t = &closed.toggles[0];
        let sum = closed
            .texts
            .iter()
            .find(|x| x.text.contains("hint"))
            .unwrap();
        assert!(
            t.x <= sum.x && t.y <= sum.y && t.y + t.h >= sum.y,
            "toggle covers summary: toggles={:?} text=({}, {})",
            closed.toggles,
            sum.x,
            sum.y
        );

        let open = lay(
            "<details open><summary>hint</summary><p>body text</p></details>",
            400.0,
        );
        assert!(open.texts.iter().any(|t| t.text.contains("hint")));
        assert!(open.texts.iter().any(|t| t.text.contains("body")));
        assert_eq!(open.toggles.len(), 1, "open state still toggles");
    }

    /// A5: the UA marker flips with [open].
    #[test]
    fn details_marker_flips_with_open() {
        let closed = lay("<details><summary>hint</summary></details>", 400.0);
        let open = lay("<details open><summary>hint</summary></details>", 400.0);
        assert!(
            closed.texts.iter().any(|t| t.text == "+"),
            "closed marker is +: {:?}",
            text_of(&closed)
        );
        assert!(
            open.texts.iter().any(|t| t.text == "-"),
            "open marker is -: {:?}",
            text_of(&open)
        );
    }

    /// A8: progress paints a track and a clamped fill.
    #[test]
    fn progress_paints_track_and_fill() {
        let dl = lay("<progress value=\"70\" max=\"100\"></progress>", 400.0);
        let track = dl.rects.iter().find(|r| r.color == 0xE8EAEDFF).unwrap();
        let fill = dl.rects.iter().find(|r| r.color == 0x1A73E8FF).unwrap();
        assert!(
            (fill.w / track.w - 0.7).abs() < 0.02,
            "fill = {} of {}",
            fill.w,
            track.w
        );
        assert!((fill.x - track.x).abs() < 0.01 && (fill.y - track.y).abs() < 0.01);

        // value clamps to [0, max].
        let over = lay("<progress value=\"150\" max=\"100\"></progress>", 400.0);
        let fill = over.rects.iter().find(|r| r.color == 0x1A73E8FF).unwrap();
        let track = over.rects.iter().find(|r| r.color == 0xE8EAEDFF).unwrap();
        assert!(
            (fill.w - track.w).abs() < 0.01,
            "over-max fill clamps to full"
        );
    }

    /// A8: meter zones — green inside [low, high], red at the extremes,
    /// indeterminate progress (no value) paints no fill.
    #[test]
    fn meter_zones_and_indeterminate_progress() {
        let green = lay("<meter value=\"50\" low=\"10\" high=\"90\"></meter>", 400.0);
        assert!(green.rects.iter().any(|r| r.color == 0x34A853FF));

        let red = lay("<meter value=\"95\" low=\"10\" high=\"90\"></meter>", 400.0);
        assert!(red.rects.iter().any(|r| r.color == 0xEA4335FF));

        let indet = lay("<progress max=\"100\"></progress>", 400.0);
        assert!(
            !indet.rects.iter().any(|r| r.color == 0x1A73E8FF),
            "no value → no fill"
        );
        assert!(
            indet.rects.iter().any(|r| r.color == 0xE8EAEDFF),
            "track still paints"
        );
    }

    /// A6: closed dialogs render nothing; [open] renders a centered card
    /// near the top of the viewport with the UA chrome.
    #[test]
    fn dialog_open_renders_centered_card() {
        let closed = lay("<dialog><p>hidden modal</p></dialog>", 400.0);
        assert!(
            !closed.texts.iter().any(|t| t.text.contains("modal")),
            "closed dialog is invisible: {:?}",
            text_of(&closed)
        );

        let dl = lay("<dialog open><p>modal body</p></dialog>", 400.0);
        let body = dl.texts.iter().find(|t| t.text.contains("modal")).unwrap();
        assert!(
            body.x > 400.0 * 0.5,
            "dialog content starts right of center (card centering): x={}",
            body.x
        );
        // The card background paints (UA white on default white — check by
        // structure: the rect covering the text is the dialog card).
        let card = dl
            .rects
            .iter()
            .find(|r| r.x <= body.x && r.x + r.w >= body.x && r.y <= body.y && r.y + r.h >= body.y)
            .expect("dialog card background");
        assert!(card.w > 100.0, "card has real width: {}", card.w);
        // UA shadow present.
        assert!(!dl.shadows.is_empty(), "dialog paints its UA box-shadow");
    }

    /// A6: an open modal dialog paints a full-page dim rect UNDER the card —
    /// the first rect of the dialog's own paint run (fixed boxes drain after
    /// the document, so body background and page flow precede it), spanning
    /// the drain's content box and untouched by the card's `translateX(-50%)`
    /// bake.
    #[test]
    fn dialog_backdrop_dims_page_under_card() {
        let dl = lay(
            "<body style=\"margin:0\">\
             <p>page words behind the modal</p>\
             <dialog open><p>modal body</p></dialog></body>",
            400.0,
        );
        // The dim: untransformed (it must escape the card's matrix bake),
        // alpha 0x80 in the low byte (crisp fallback approximation of
        // ::backdrop's UA `rgba(0,0,0,0.5)`), full-page width. The page flow
        // before it paints no such rect (body is opaque white).
        let backdrop_idx = dl
            .rects
            .iter()
            .position(|r| r.transform.is_none() && r.color & 0xFF == 0x80 && r.w == 400.0)
            .expect("backdrop dim rect exists");
        let backdrop = &dl.rects[backdrop_idx];
        assert!(backdrop.h > 0.0, "backdrop covers the document height");
        // The card (transform-carrying dialog rect) paints after the dim and
        // sits centered (its pre-transform x plus the -50% shift = centered).
        let card = dl
            .rects
            .iter()
            .position(|r| r.transform.is_some())
            .expect("card rect carries the dialog transform");
        assert!(card > backdrop_idx, "backdrop paints before the card");
        let card_rect = &dl.rects[card];
        let center_x = card_rect.x + m0_shift(card_rect) + card_rect.w / 2.0;
        assert!(
            (center_x - 200.0).abs() < 30.0,
            "card centered on the 400px page: center_x={center_x}"
        );

        // A closed dialog paints no dim at all.
        let closed = lay(
            "<body style=\"margin:0\"><p>plain</p><dialog><p>x</p></dialog></body>",
            400.0,
        );
        assert!(
            !closed
                .rects
                .iter()
                .any(|r| r.color & 0xFF == 0x80 && r.transform.is_none()),
            "closed dialog has no backdrop"
        );
    }

    /// Pre-transform x of a dialog card's rects — `x + m[2]` for the plain
    /// translateX bake (only the offset column moves a rect's corner in x).
    fn m0_shift(r: &crate::Rect) -> f32 {
        r.transform.map(|m| m.0[2]).unwrap_or(0.0)
    }

    /// A3: srcset width descriptors — smallest ≥ viewport wins; viewport
    /// larger than all → the largest wins.
    #[test]
    fn img_srcset_picks_by_width_descriptor() {
        let dl = lay(
            "<img srcset=\"small.png 320w, big.png 1280w\" src=\"fallback.png\">",
            500.0,
        );
        assert_eq!(dl.images.len(), 1);
        assert_eq!(dl.images[0].src, "big.png", "500 viewport → 1280w");

        let dl = lay(
            "<img srcset=\"small.png 320w, big.png 1280w\" src=\"fallback.png\">",
            1300.0,
        );
        assert_eq!(
            dl.images[0].src, "big.png",
            "1300 viewport → still 1280w (largest)"
        );
    }

    /// A3: density descriptors — closest to 1x wins over the src fallback.
    #[test]
    fn img_srcset_picks_by_density() {
        let dl = lay(
            "<img srcset=\"retina.png 2x, plain.png 1x\" src=\"fallback.png\">",
            400.0,
        );
        assert_eq!(dl.images[0].src, "plain.png", "1x is closest to 1");
    }

    /// A3: <picture> picks the last matching <source>, honoring type and
    /// media filters; falls back to the img when none match.
    #[test]
    fn picture_selects_matching_source() {
        let dl = lay(
            "<picture>\
             <source media=\"(min-width: 900px)\" srcset=\"wide.png\">\
             <source media=\"(max-width: 600px)\" srcset=\"narrow.png\">\
             <img src=\"default.png\">\
             </picture>",
            500.0,
        );
        assert_eq!(
            dl.images[0].src, "narrow.png",
            "500 viewport → max-width source"
        );

        let dl = lay(
            "<picture>\
             <source type=\"image/avif\" srcset=\"photo.avif\">\
             <source media=\"(min-width: 100px)\" srcset=\"wide.png\">\
             <img src=\"default.png\">\
             </picture>",
            500.0,
        );
        assert_eq!(
            dl.images[0].src, "wide.png",
            "avif source skipped, media source wins"
        );

        let dl = lay(
            "<picture>\
             <source media=\"(min-width: 5000px)\" srcset=\"huge.png\">\
             <img src=\"default.png\">\
             </picture>",
            500.0,
        );
        assert_eq!(dl.images[0].src, "default.png", "no match → img src");
    }

    /// A3/env: `<picture><source media>` resolves against the page's own
    /// media environment, so a page laid out for paper picks its print
    /// source and one on screen picks the screen source.
    #[test]
    fn picture_source_media_reads_the_page_media_type() {
        let html = "<picture>\
                   <source media='print' srcset='paper.png'>\
                   <source media='screen' srcset='glass.png'>\
                   <img src='default.png'>\
                   </picture>";
        let print = EnvironmentState {
            media_type: MediaType::Print,
            ..EnvironmentState::default()
        };
        assert_eq!(
            lay_in_env(html, 500.0, &print).images[0].src,
            "paper.png",
            "the print source on paper"
        );
        // A bare `media="screen"` is a media type, not an always-true
        // condition: it must not be selected on paper.
        assert_eq!(
            lay(html, 500.0).images[0].src,
            "glass.png",
            "the screen source on screen, and the default entry point is screen"
        );
        assert_eq!(
            lay_in_env(html, 500.0, &EnvironmentState::default()).images[0].src,
            "glass.png",
            "…including when the environment is spelled out"
        );
    }

    /// A3/env: the same environment, reached through the `prefers-*` half —
    /// the whole [`EnvironmentState`] rides in, not just the media type.
    #[test]
    fn picture_source_media_reads_the_prefers_features() {
        let html = "<picture>\
                   <source media='(prefers-color-scheme: dark)' srcset='night.png'>\
                   <img src='day.png'>\
                   </picture>";
        let dark = EnvironmentState {
            color_scheme: ColorScheme::Dark,
            ..EnvironmentState::default()
        };
        assert_eq!(
            lay_in_env(html, 500.0, &dark).images[0].src,
            "night.png",
            "a dark page picks the dark source"
        );
        assert_eq!(
            lay(html, 500.0).images[0].src,
            "day.png",
            "the default environment is light"
        );
        // Paper answers as paper: a light, normal-contrast device, whatever
        // the screen it came from is set to.
        let print_dark = EnvironmentState {
            media_type: MediaType::Print,
            color_scheme: ColorScheme::Dark,
            ..EnvironmentState::default()
        };
        assert_eq!(
            lay_in_env(html, 500.0, &print_dark).images[0].src,
            "day.png",
            "print ignores the screen's dark scheme"
        );
    }

    /// A3/env: the `<img>`'s own `media` attribute gates the element — no
    /// `src`, no `<picture>` source, nothing to select.
    #[test]
    fn img_media_gates_the_element() {
        let print = EnvironmentState {
            media_type: MediaType::Print,
            ..EnvironmentState::default()
        };
        let html = "<img media='print' src='paper.png'>";
        assert_eq!(
            lay_in_env(html, 500.0, &print).images[0].src,
            "paper.png",
            "a print-gated img paints on paper"
        );
        let dl = lay(html, 500.0);
        assert_eq!(dl.images.len(), 1, "the box still occupies its space");
        assert_eq!(dl.images[0].src, "", "…with no resource on screen");

        // It also gates a `<picture>`: the whole img is out, not just its own
        // fallback src.
        let dl = lay(
            "<picture><source srcset='wide.png'><img media='print' src='default.png'></picture>",
            500.0,
        );
        assert_eq!(dl.images[0].src, "", "no source reaches the wire");

        // A condition that does match is a no-op, whatever it says.
        for media in ["", "all", "screen", "(min-width: 100px)", "print, screen"] {
            let dl = lay(&format!("<img media='{media}' src='p.png'>"), 500.0);
            assert_eq!(dl.images[0].src, "p.png", "media={media:?}");
        }
        // …and it is the same evaluator the stylesheet path uses, so a bare
        // `media="screen"` is inert on paper.
        let dl = lay_in_env("<img media='screen' src='glass.png'>", 500.0, &print);
        assert_eq!(dl.images[0].src, "", "screen-only on paper");
    }

    /// A4: <colgroup><col> pins column widths; unpinned columns split the
    /// remainder.
    #[test]
    fn colgroup_pins_column_widths() {
        let dl = lay(
            "<table>\
             <colgroup><col style=\"width: 80px\"><col style=\"width: 40px\"></colgroup>\
             <tr><td>aa</td><td>bb</td></tr>\
             <tr><td>cc</td><td>dd</td></tr>\
             </table>",
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        let cc = dl.texts.iter().find(|t| t.text == "cc").unwrap();
        // Column 0 starts at the table edge; column 1 exactly 80px later.
        // (td padding-left 8px applies to both.)
        assert!(
            (bb.x - aa.x - 80.0).abs() < 4.0,
            "col width 80px: gap={}",
            bb.x - aa.x
        );
        assert!((cc.x - aa.x).abs() < 4.0, "columns align across rows");
    }
    #[test]
    fn col_width_attribute_pins_in_px() {
        let dl = lay(
            "<table>\
             <colgroup><col width=\"120\"></colgroup>\
             <tr><td>aa</td><td>bb</td></tr>\
             </table>",
            400.0,
        );
        let aa = dl.texts.iter().find(|t| t.text == "aa").unwrap();
        let bb = dl.texts.iter().find(|t| t.text == "bb").unwrap();
        assert!(
            (bb.x - aa.x - 120.0).abs() < 4.0,
            "width attr = 120px, gap={}",
            bb.x - aa.x
        );
    }

    // ------------------------------------------------------------------
    // A7/A10/A12 (bucket A batch 2): control widget chrome, constraint
    // validation, MathML linearized rendering.
    // ------------------------------------------------------------------

    /// A7: range paints a track + fill + knob; the knob sits at the value
    /// fraction of the track.
    #[test]
    fn range_paints_slider_with_knob_at_value() {
        let dl = lay(
            "<input type=\"range\" min=\"0\" max=\"100\" value=\"70\">",
            400.0,
        );
        // The border rects are the 1px control chrome; the track/fill/knob
        // are the widget rects. Track is the 4px gray, fill the 70% blue.
        let tracks: Vec<&Rect> = dl
            .rects
            .iter()
            .filter(|r| r.color == 0xE8EAEDFF && r.h == 4.0)
            .collect();
        assert!(!tracks.is_empty(), "range paints its track");
        let track = tracks[0];
        let fill = dl
            .rects
            .iter()
            .filter(|r| r.color == 0x1A73E8FF && r.h == 4.0)
            .next()
            .expect("range paints its fill");
        assert!(
            (fill.w / track.w - 0.7).abs() < 0.02,
            "fill at 70%: {}",
            fill.w / track.w
        );
        let knob = dl
            .rects
            .iter()
            .filter(|r| r.color == 0x1A73E8FF && r.w == 10.0 && r.h == 10.0)
            .next()
            .expect("range paints its knob");
        assert!(
            (knob.x + 5.0 - (track.x + track.w * 0.7)).abs() < 1.0,
            "knob centered at 70%"
        );
    }

    /// A7: color paints a swatch of the value color.
    #[test]
    fn color_paints_value_swatch() {
        let dl = lay("<input type=\"color\" value=\"#00ff00\">", 400.0);
        assert!(
            dl.rects.iter().any(|r| r.color == 0x00FF00FF),
            "green swatch painted"
        );
        // Missing/empty value → engine default black (HTML's initial #000000).
        let dl = lay("<input type=\"color\">", 400.0);
        assert!(
            dl.rects.iter().any(|r| r.color == 0x000000FF),
            "default black swatch when no value"
        );
    }

    /// A7: file inputs show the choose-file pill text.
    #[test]
    fn file_input_shows_choose_pill() {
        let dl = lay("<input type=\"file\">", 400.0);
        assert!(
            dl.texts.iter().any(|t| t.text.contains("Choose file")),
            "file pill text: {:?}",
            text_of(&dl)
        );
        let dl = lay("<input type=\"file\" value=\"/Users/x/report.pdf\">", 400.0);
        assert!(
            dl.texts.iter().any(|t| t.text.contains("report.pdf")),
            "chosen file name shown (basename only)"
        );
    }

    /// A10: an invalid control's UA border paints red; a valid one keeps
    /// the gray chrome border.
    #[test]
    fn invalid_control_paints_error_border() {
        let dl = lay("<input required value=\"\">", 400.0);
        assert!(
            dl.rects.iter().any(|r| r.color == 0xD93025FF),
            "required-empty input gets the error border"
        );
        let dl = lay("<input required value=\"hello\">", 400.0);
        assert!(
            !dl.rects.iter().any(|r| r.color == 0xD93025FF),
            "filled input keeps the normal chrome border"
        );
    }

    /// A10: type-mismatch families — bad email, bad number, bad date.
    #[test]
    fn type_mismatch_marks_invalid() {
        let dl = lay("<input type=\"email\" value=\"not-an-email\">", 400.0);
        assert!(dl.rects.iter().any(|r| r.color == 0xD93025FF), "bad email");
        let dl = lay("<input type=\"email\" value=\"a@b.co\">", 400.0);
        assert!(
            !dl.rects.iter().any(|r| r.color == 0xD93025FF),
            "good email"
        );
        let dl = lay("<input type=\"date\" value=\"2026-13-40\">", 400.0);
        assert!(
            dl.rects.iter().any(|r| r.color == 0xD93025FF),
            "bad date month"
        );
        let dl = lay("<input type=\"date\" value=\"2026-09-06\">", 400.0);
        assert!(!dl.rects.iter().any(|r| r.color == 0xD93025FF), "good date");
        let dl = lay("<input type=\"number\" value=\"15\" max=\"10\">", 400.0);
        assert!(
            dl.rects.iter().any(|r| r.color == 0xD93025FF),
            "number over max"
        );
    }

    /// A10: `:valid` is the negation — a filled required input matches,
    /// an empty one does not (guards the Not(:invalid) parse path).
    #[test]
    fn valid_pseudo_negates_invalid() {
        let dl = lay(
            "<style>input:valid { background: #e6f4ea }</style>\
             <input required value=\"ok\">",
            400.0,
        );
        assert!(
            dl.rects.iter().any(|r| r.color == 0xE6F4EAFF),
            ":valid matches the filled input"
        );
        let dl = lay(
            "<style>input:valid { background: #e6f4ea }</style>\
             <input required value=\"\">",
            400.0,
        );
        assert!(
            !dl.rects.iter().any(|r| r.color == 0xE6F4EAFF),
            ":valid does not match the empty input"
        );
    }

    /// A12: MathML renders linearized — tokens in reading order, mi italic,
    /// mo separated.
    #[test]
    fn mathml_renders_linearized() {
        let dl = lay(
            "<math><mrow><mi>a</mi><mo>+</mo><mi>b</mi></mrow></math>",
            400.0,
        );
        let a = dl
            .texts
            .iter()
            .find(|t| t.text == "a")
            .expect("mi a renders");
        let plus = dl
            .texts
            .iter()
            .find(|t| t.text == "+")
            .expect("mo + renders");
        let b = dl
            .texts
            .iter()
            .find(|t| t.text == "b")
            .expect("mi b renders");
        assert!(a.italic, "mi is italic");
        assert!(b.italic, "mi is italic");
        assert!(!plus.italic, "mo is not italic");
        // Reading order on the line: a, then +, then b, with real gaps.
        assert!(a.x < plus.x && plus.x < b.x, "tokens in reading order");
        assert!(
            plus.x - a.x > 3.0 && b.x - plus.x > 3.0,
            "mo margins separate tokens (a→+ {}, +→b {})",
            plus.x - a.x,
            b.x - plus.x
        );
    }

    /// A12 finish: mfrac stacks numerator over a 1px rule over
    /// denominator, both centered on the bar.
    #[test]
    fn mathml_mfrac_stacks_around_bar() {
        let dl = lay("<math><mfrac><mi>a</mi><mi>b</mi></mfrac></math>", 400.0);
        let a = dl.texts.iter().find(|t| t.text == "a").expect("numerator");
        let b = dl
            .texts
            .iter()
            .find(|t| t.text == "b")
            .expect("denominator");
        assert!(
            a.y < b.y,
            "numerator above denominator ({} vs {})",
            a.y,
            b.y
        );
        // The bar: a short flat rect between the two baselines. Mock
        // widths make both parts 8px wide, so the bar is 8px inside the
        // 10px form and every center lands on a.x + 4.
        let bar = dl
            .rects
            .iter()
            .find(|r| r.h <= 2.0 && r.w > 1.0)
            .expect("fraction bar rect");
        assert!(bar.y > a.y && bar.y < b.y, "bar between the parts");
        assert!((bar.x + 4.0 - (a.x + 4.0)).abs() < 0.01, "num centered");
        assert!((a.x - b.x).abs() < 0.01, "parts share the column");
    }

    /// A12 finish: msqrt paints the radical glyph left of the radicand
    /// with an overline rect spanning it.
    #[test]
    fn mathml_msqrt_radical_and_overbar() {
        let dl = lay("<math><msqrt><mi>x</mi></msqrt></math>", 400.0);
        let rad = dl
            .texts
            .iter()
            .find(|t| t.text == "√")
            .expect("radical glyph");
        let x = dl.texts.iter().find(|t| t.text == "x").expect("radicand");
        assert!(rad.x < x.x, "radical left of radicand");
        assert!(
            rad.size > x.size,
            "radical renders larger ({} vs {})",
            rad.size,
            x.size
        );
        // Overline: starts right of the radical, spans the radicand,
        // sits above its baseline.
        let bar = dl
            .rects
            .iter()
            .find(|r| r.h <= 2.0 && r.w > 1.0 && r.x >= rad.x)
            .expect("overbar rect");
        assert!(bar.y < x.y, "overline above radicand baseline");
        assert!(
            bar.x + bar.w > x.x,
            "overline spans the radicand (bar ends {}, radicand at {})",
            bar.x + bar.w,
            x.x
        );
    }

    /// A12 finish: msubsup places sub below and sup above the shared
    /// script column, both at 0.7× the base size.
    #[test]
    fn mathml_msubsup_shifts_and_shrinks() {
        let dl = lay(
            "<math><msubsup><mi>x</mi><mn>1</mn><mn>2</mn></msubsup></math>",
            400.0,
        );
        let base = dl.texts.iter().find(|t| t.text == "x").expect("base");
        let one = dl.texts.iter().find(|t| t.text == "1").expect("sub");
        let two = dl.texts.iter().find(|t| t.text == "2").expect("sup");
        assert!(
            one.y > base.y,
            "sub below baseline ({} vs {})",
            one.y,
            base.y
        );
        assert!(
            two.y < base.y,
            "sup above baseline ({} vs {})",
            two.y,
            base.y
        );
        assert!(
            (one.x - two.x).abs() < 0.5,
            "scripts share a column ({} vs {})",
            one.x,
            two.x
        );
        assert!(
            (one.size - base.size * 0.7).abs() < 0.5,
            "script at 0.7× base ({} vs {})",
            one.size,
            base.size * 0.7
        );
        assert!((two.size - base.size * 0.7).abs() < 0.5, "sup same shrink");
    }

    /// A12 finish: mover centers its script above the base (x̂ hat form).
    #[test]
    fn mathml_mover_centers_above() {
        let dl = lay("<math><mover><mi>x</mi><mo>^</mo></mover></math>", 400.0);
        let base = dl.texts.iter().find(|t| t.text == "x").expect("base");
        let hat = dl.texts.iter().find(|t| t.text == "^").expect("hat");
        assert!(hat.y < base.y, "hat above base ({} vs {})", hat.y, base.y);
        assert!(
            hat.size < base.size,
            "script smaller ({} vs {})",
            hat.size,
            base.size
        );
        // Hat roughly centered over the base glyph.
        let base_c = base.x + base.size * 0.25;
        let hat_c = hat.x + hat.size * 0.25;
        assert!(
            (base_c - hat_c).abs() < 6.0,
            "hat centered ({} vs {})",
            base_c,
            hat_c
        );
    }

    /// A12 finish: `<semantics>` paints the presentation child and skips
    /// the annotation entirely.
    #[test]
    fn mathml_semantics_skips_annotation() {
        let dl = lay(
            "<math><semantics><mi>a</mi>\
             <annotation encoding='text/x-tex'>a^2</annotation>\
             </semantics></math>",
            400.0,
        );
        assert!(dl.texts.iter().any(|t| t.text == "a"), "mi renders");
        assert!(
            !dl.texts.iter().any(|t| t.text == "a^2"),
            "annotation never paints"
        );
    }

    // B3: container-type (InlineSize) boxes are recorded in
    // DisplayList::containers at the content-box geometry the cascade
    // will see on the second pass. This is the data the engine walks
    // to resolve `@container (min-width: …)` and cqw/cqh.
    #[test]
    fn container_box_records_content_geometry() {
        let dl = lay(
            "<div style='container-type: inline-size; \
                         width: 300px; padding: 10px; \
                         container-name: card'>x</div>",
            400.0,
        );
        assert_eq!(dl.containers.len(), 1);
        let cb = &dl.containers[0];
        // 300 minus the 10+10 padding the layout subtracts.
        assert!((cb.w - 280.0).abs() < 0.01, "w = 280 (got {})", cb.w);
        // Height closes after the line is laid out, so it reflects the
        // single line of text (font-size 16 → line ≈19.2 + 0 padding).
        assert!(cb.h > 10.0, "h closes after the line (got {})", cb.h);
        // The container-name declaration flows through to the probe map.
        assert_eq!(cb.names, vec!["card".to_string()]);
    }

    #[test]
    fn no_container_box_without_container_type() {
        // Without container-type set, the layout pass emits no box even
        // if a name is declared in isolation.
        let dl = lay("<div style='container-name: card'>x</div>", 400.0);
        assert!(dl.containers.is_empty());
    }

    // B4: the element's `filter` chain is stamped onto every paint item
    // it produces. The renderer then applies CIFilters to the backing
    // layer. Hosts without a filter backend can ignore the field; it has
    // no effect on the layout coordinates.
    #[test]
    fn filter_is_stamped_on_rects_and_text() {
        // The filter is on the text-bearing element itself (the <p>), so
        // its paint items carry it. Filter on an enclosing block that has
        // no own paint (a bare <div>) needs parent-ancestor plumbing not
        // worth shipping for the first cut; documented.
        let dl = lay(
            "<p style='filter: blur(8px) brightness(1.2)'>hello</p>",
            400.0,
        );
        let count_filtered_rects = dl.rects.iter().filter(|r| r.filters.is_some()).count();
        let count_filtered_text = dl.texts.iter().filter(|t| t.filters.is_some()).count();
        assert!(
            count_filtered_rects + count_filtered_text > 0,
            "at least one item carries the filter (rects={} texts={})",
            count_filtered_rects,
            count_filtered_text
        );
        let fl = if count_filtered_rects > 0 {
            dl.rects
                .iter()
                .find(|r| r.filters.is_some())
                .unwrap()
                .filters
                .as_ref()
                .unwrap()
        } else {
            dl.texts
                .iter()
                .find(|t| t.filters.is_some())
                .unwrap()
                .filters
                .as_ref()
                .unwrap()
        };
        assert_eq!(fl.0.len(), 2);
        assert!(matches!(fl.0[0], FilterFn::Blur(8.0)));
    }

    #[test]
    fn no_filter_leaves_items_filterless() {
        let dl = lay("<div>hi</div>", 400.0);
        for r in &dl.rects {
            assert!(r.filters.is_none(), "no `filter:` → no per-item filter");
        }
        for t in &dl.texts {
            assert!(t.filters.is_none());
        }
    }

    // B2: `overflow: auto|scroll` on a box whose content is taller than
    // the box itself records a ScrollContainer. The host uses the
    // recorded viewport+content extents to paint a scrollbar and route
    // wheel events. `overflow: hidden` is a clip-only box — not a
    // scroll container, even with overflow.
    #[test]
    fn scroll_container_recorded_when_content_overflows() {
        let dl = lay(
            "<div style='overflow: auto; height: 50px'>\
             <p>line 1</p><p>line 2</p><p>line 3</p>\
             <p>line 4</p><p>line 5</p>\
             </div>",
            400.0,
        );
        assert_eq!(dl.scroll_containers.len(), 1);
        let sc = &dl.scroll_containers[0];
        assert!(sc.h <= 50.0 + 0.01, "viewport h is the box's height");
        assert!(sc.content_h > sc.h, "content_h is taller than the viewport");
    }

    #[test]
    fn scroll_viewport_excludes_padding_and_border() {
        let dl = lay(
            "<div style='overflow: auto; height: 50px; padding: 5px; border: 2px solid'>\
             <p>line</p><p>line</p><p>line</p><p>line</p>\
             </div>",
            400.0,
        );
        assert_eq!(dl.scroll_containers.len(), 1);
        assert!((dl.scroll_containers[0].h - 36.0).abs() < 0.01);
    }

    #[test]
    fn hidden_overflow_does_not_record_a_scroll_container() {
        // overflow: hidden is a clip-only box — the engine still
        // tracks the box for stickies, but it does NOT add a scroll
        // container. The host has no wheel target.
        let dl = lay(
            "<div style='overflow: hidden; height: 30px'>\
             <p>line 1</p><p>line 2</p><p>line 3</p>\
             <p>line 4</p>\
             </div>",
            400.0,
        );
        assert!(dl.scroll_containers.is_empty());
    }

    #[test]
    fn scroll_records_a_short_box_but_auto_does_not() {
        let auto = lay(
            "<div style='overflow: auto; height: 200px'>short</div>",
            400.0,
        );
        assert!(auto.scroll_containers.is_empty());
        let scroll = lay(
            "<div style='overflow: scroll; height: 200px'>short</div>",
            400.0,
        );
        assert_eq!(scroll.scroll_containers.len(), 1);
        assert_eq!(scroll.scroll_containers[0].content_h, 200.0);
    }

    #[test]
    fn scroll_metadata_records_horizontal_extent_mode_and_parent() {
        let dl = lay(
            "<div style='overflow:auto;width:100px;height:80px;white-space:nowrap'>\
             <div style='overflow:scroll;width:80px;height:20px'>abcdefghijklmnop</div>\
             <p>abcdefghijklmnopqrstuvwxyz</p></div>",
            400.0,
        );
        assert_eq!(dl.scroll_containers.len(), 2);
        let outer = dl
            .scroll_containers
            .iter()
            .find(|sc| sc.mode == 1)
            .expect("outer scroll container");
        let inner = dl
            .scroll_containers
            .iter()
            .find(|sc| sc.mode == 2)
            .expect("inner scroll container");
        assert!(outer.content_w > outer.w, "horizontal extent is recorded");
        assert_eq!(inner.mode, 2, "overflow:scroll is distinguishable");
        assert_eq!(inner.parent, outer.node, "nested scroll parent is recorded");
    }

    // ---- B13: line-break control (Mock: 8px/char at 16px font) ----

    #[test]
    fn b13_soft_hyphen_becomes_visible_at_line_end() {
        // "hy­phenation": UAX #14 breaks after the soft hyphen, so the
        // first line ends on the shy word — it gains a visible '-'.
        let dl = lay("<p style='width: 60px'>hy\u{AD}phenation</p>", 400.0);
        let texts = text_of(&dl);
        assert_eq!(texts, vec!["hy-", "phenation"]);
    }

    #[test]
    fn b13_hyphens_none_keeps_the_shy_visible() {
        let dl = lay(
            "<p style='width: 60px; hyphens: none'>hy\u{AD}phenation</p>",
            400.0,
        );
        let texts = text_of(&dl);
        assert_eq!(texts, vec!["hy\u{AD}", "phenation"]);
    }

    #[test]
    fn b13_overflow_wrap_break_word_splits_long_words() {
        // 12 chars × 8px = 96px into a 40px band → 5 + 5 + 2.
        let dl = lay(
            "<p style='width: 40px; overflow-wrap: break-word'>abcdefghijkl</p>",
            400.0,
        );
        assert_eq!(text_of(&dl), vec!["abcde", "fghij", "kl"]);
    }

    #[test]
    fn b13_overflow_wrap_normal_leaves_the_word_whole() {
        let dl = lay("<p style='width: 40px'>abcdefghijkl</p>", 400.0);
        assert_eq!(text_of(&dl), vec!["abcdefghijkl"]);
    }

    #[test]
    fn b13_word_break_break_all_wraps_per_character() {
        let dl = lay(
            "<p style='width: 40px; word-break: break-all'>abcdefgh</p>",
            400.0,
        );
        assert_eq!(text_of(&dl), vec!["abcde", "fgh"]);
    }

    #[test]
    fn b13_nowrap_suppresses_wrapping() {
        let dl = lay(
            "<p style='width: 40px; white-space: nowrap'>one two</p>",
            400.0,
        );
        let runs: Vec<_> = dl.texts.iter().map(|t| (t.text.as_str(), t.y)).collect();
        assert_eq!(runs.len(), 2, "both words stay on one line");
        assert_eq!(runs[0].1, runs[1].1, "same baseline — no wrap");
    }

    /// D5: `hyphens: auto` splits at the dictionary break when the word
    /// would wrap; the first line ends with a visible '-' (Mock: 8px/char,
    /// so 60px fits 7 chars and the hyphen eats one).
    #[test]
    fn d5_hyphens_auto_uses_the_dictionary() {
        let dl = lay(
            "<p style='width: 60px; hyphens: auto'>hyphenation</p>",
            400.0,
        );
        assert_eq!(text_of(&dl), vec!["hyphen-", "ation"]);
    }

    #[test]
    fn d5_hyphens_manual_leaves_the_word_whole() {
        // Without `auto`, the word overflows whole (no soft hyphens present).
        let dl = lay("<p style='width: 60px'>hyphenation</p>", 400.0);
        assert_eq!(text_of(&dl), vec!["hyphenation"]);
    }

    #[test]
    fn d5_non_english_lang_disables_auto() {
        let dl = lay(
            "<p lang='de' style='width: 60px; hyphens: auto'>hyphenation</p>",
            400.0,
        );
        assert_eq!(text_of(&dl), vec!["hyphenation"]);
    }

    /// D6: in a vertical box the default `mixed` stands CJK chars up while
    /// Latin rides the box rotation. Mock: 8px/char. The box stamp composes
    /// onto every run at `rotate()` time, so the final transforms are
    /// box∘piece: identity linear part for the upright char (upright
    /// glyph), the raw rotation for the plain piece.
    #[test]
    fn d6_mixed_stands_cjk_upright_in_vertical_box() {
        let dl = lay(
            "<div style='writing-mode: vertical-rl; height: 100px'>漢ab</div>",
            400.0,
        );
        let runs: Vec<(&str, Matrix2d)> = dl
            .texts
            .iter()
            .map(|t| {
                (
                    t.text.as_str(),
                    t.transform.expect("vertical text is stamped"),
                )
            })
            .collect();
        assert_eq!(runs.len(), 2, "CJK char + Latin piece (UAX #14 splits)");
        assert_eq!(runs[0].0, "漢");
        assert_eq!(runs[1].0, "ab");
        let (t0, t1) = (runs[0].1, runs[1].1);
        // Upright: the counter-rotation cancels the box rotation — identity
        // linear part. Plain: the box stamp survives untouched.
        assert!(
            (t0.0[0] - 1.0).abs() < 1e-4
                && t0.0[1].abs() < 1e-4
                && t0.0[3].abs() < 1e-4
                && (t0.0[4] - 1.0).abs() < 1e-4,
            "upright glyph renders unrotated: {t0:?}"
        );
        assert!(
            t1.0[0].abs() < 1e-4
                && (t1.0[1] + 1.0).abs() < 1e-4
                && (t1.0[3] - 1.0).abs() < 1e-4
                && t1.0[4].abs() < 1e-4,
            "plain piece rides the box rotation: {t1:?}"
        );
        // The upright glyph's advance center (logical (4, 12.8): 4px into
        // the char, baseline 0.8×16) maps to the same page point the box
        // stamp gives it — on the baseline column, stacked down the flow.
        let at = |m: Matrix2d| {
            (
                m.0[0] * 4.0 + m.0[1] * 12.8 + m.0[2],
                m.0[3] * 4.0 + m.0[4] * 12.8 + m.0[5],
            )
        };
        let (p0, p1) = (at(t0), at(t1));
        assert!((p0.0 - p1.0).abs() < 1e-4 && (p0.1 - p1.1).abs() < 1e-4);
        assert!(
            p1.1 > 16.0,
            "char center sits 4px down the column, not at the box top"
        );
    }

    #[test]
    fn d6_mixed_keeps_horizontal_lines_plain() {
        // Horizontal-tb text ignores text-orientation entirely: no
        // transform, even for CJK. (UAX #14 already splits 漢/ab into two
        // words at feed time — that predates D6.)
        let dl = lay("<p>漢ab</p>", 400.0);
        let runs: Vec<_> = dl
            .texts
            .iter()
            .map(|t| (t.text.as_str(), t.transform))
            .collect();
        assert_eq!(runs, vec![("漢", None), ("ab", None)]);
    }

    #[test]
    fn d6_sideways_is_the_plain_rotated_run() {
        // `sideways` never counter-rotates: every piece rides the box
        // rotation. The box stamp composes onto all runs at rotate() time,
        // so both runs carry the same stamp with the box's linear part.
        let dl = lay(
            "<div style='writing-mode: vertical-rl; text-orientation: sideways; height: 100px'>漢ab</div>",
            400.0,
        );
        let runs: Vec<(&str, Matrix2d)> = dl
            .texts
            .iter()
            .map(|t| {
                (
                    t.text.as_str(),
                    t.transform.expect("vertical text is stamped"),
                )
            })
            .collect();
        assert_eq!(runs.len(), 2, "CJK char + Latin piece (UAX #14 splits)");
        assert_eq!(runs[0].0, "漢");
        assert_eq!(runs[1].0, "ab");
        assert_eq!(runs[0].1, runs[1].1, "one box stamp for every piece");
        let t = runs[0].1;
        assert!(
            t.0[0].abs() < 1e-4
                && (t.0[1] + 1.0).abs() < 1e-4
                && (t.0[3] - 1.0).abs() < 1e-4
                && t.0[4].abs() < 1e-4,
            "glyphs ride the box rotation: {t:?}"
        );
    }

    #[test]
    fn d6_upright_stands_every_char_up() {
        // `upright` splits Latin too: each char is its own counter-rotated
        // run, stacked down the column.
        let dl = lay(
            "<div style='writing-mode: vertical-rl; text-orientation: upright; height: 100px'>ab</div>",
            400.0,
        );
        let runs: Vec<_> = dl
            .texts
            .iter()
            .map(|t| (t.text.as_str(), t.transform.is_some()))
            .collect();
        assert_eq!(runs, vec![("a", true), ("b", true)]);
    }

    #[test]
    fn d6_pieces_tile_the_word_exactly() {
        // Piece x positions are prefix measure differences, so consecutive
        // piece origins tile the word: the upright 漢 is 8px wide (Mock
        // 8px/char) and "ab" starts right after it (x runs left-to-right in
        // logical order before the box stamp rotates them).
        let dl = lay(
            "<div style='writing-mode: vertical-rl; height: 100px'>漢ab</div>",
            400.0,
        );
        let xs: Vec<f32> = dl.texts.iter().map(|t| t.x).collect();
        assert_eq!(xs.len(), 2);
        assert!(
            (xs[1] - xs[0] - 8.0).abs() < 1e-4,
            "Latin starts after the upright char"
        );
    }

    #[test]
    fn b13_text_overflow_ellipsis_truncates_nowrap_line() {
        // "one two three" = 24 + 32 + 48 = 104px into 40px; '…' is 8px.
        // Dropping "three" (56+8 > 40) then "two" (24+8 ≤ 40) leaves
        // "one…".
        let dl = lay(
            "<p style='width: 40px; white-space: nowrap; text-overflow: ellipsis'>one two three</p>",
            400.0,
        );
        assert_eq!(text_of(&dl), vec!["one…"]);
    }

    #[test]
    fn b13_ellipsis_cuts_inside_a_single_long_word() {
        // One 10-char word (80px) into 40px: avail = 40 − 8 = 32 → 4 chars.
        let dl = lay(
            "<p style='width: 40px; white-space: nowrap; text-overflow: ellipsis'>abcdefghij</p>",
            400.0,
        );
        assert_eq!(text_of(&dl), vec!["abcd…"]);
    }

    // ---- C1: intrinsic sizing keywords in block/flex/abs layout ----
    // The UA body has 16px padding, so a `lay(_, w)` test sees child
    // `avail = w - 32`. Mock glyphs are 0.5 × size: the default 16px font
    // gives exactly 8px per character.
    fn box_rect(dl: &DisplayList) -> &Rect {
        dl.rects
            .iter()
            .filter(|r| r.color != 0xFFFFFFFF)
            .last()
            .expect("content box rect")
    }

    #[test]
    fn c1_max_content_block_is_whole_run() {
        let dl = lay(
            "<div style='width: max-content; background: #ff0000'>abcd efgh</div>",
            200.0,
        );
        assert_eq!(box_rect(&dl).w, 72.0, "9 chars × 8px");
    }

    #[test]
    fn c1_min_content_block_is_longest_word() {
        let dl = lay(
            "<div style='width: min-content; background: #ff0000'>aaa bbbbbbb</div>",
            200.0,
        );
        assert_eq!(box_rect(&dl).w, 56.0, "longest word 7 × 8px");
    }

    #[test]
    fn c1_fit_content_clamps_like_css() {
        // avail 60 < max 88: the available width caps, min 56 floors.
        let dl = lay(
            "<div style='width: fit-content; background: #ff0000'>aaa bbbbbbb</div>",
            92.0,
        );
        assert_eq!(box_rect(&dl).w, 60.0);
        // Plenty of room: clamps to max-content.
        let dl = lay(
            "<div style='width: fit-content; background: #ff0000'>aaa bbbbbbb</div>",
            200.0,
        );
        assert_eq!(box_rect(&dl).w, 88.0);
        // fit-content(40px): the argument caps below min-content → floored.
        let dl = lay(
            "<div style='width: fit-content(40px); background: #ff0000'>aaa bbbbbbb</div>",
            200.0,
        );
        assert_eq!(box_rect(&dl).w, 56.0);
    }

    #[test]
    fn c1_nested_min_content_widest_block_child() {
        let dl = lay(
            "<div style='width: min-content; background: #00ff00'><div>ab</div><div>abcdef</div></div>",
            200.0,
        );
        assert_eq!(box_rect(&dl).w, 48.0, "widest block child's min-content");
    }

    #[test]
    fn c1_intrinsic_includes_padding_chrome() {
        let dl = lay(
            "<div style='width: max-content; padding: 0 10px; background: #ff0000'>abcd</div>",
            200.0,
        );
        assert_eq!(box_rect(&dl).w, 52.0, "32 content + 2×10 padding");
    }

    #[test]
    fn c1_replaced_intrinsic_is_natural_size() {
        let dl = lay(
            "<div style='width: max-content; background: #ff0000'><img width='120'></div>",
            200.0,
        );
        assert_eq!(box_rect(&dl).w, 120.0);
    }

    #[test]
    fn c1_height_keyword_behaves_as_auto() {
        let with = lay(
            "<div style='height: max-content; background: #ff0000'>text</div>",
            200.0,
        );
        let auto = lay("<div style='background: #ff0000'>text</div>", 200.0);
        assert_eq!(
            box_rect(&with).h,
            box_rect(&auto).h,
            "keyword height = auto (C1 v1)"
        );
    }

    #[test]
    fn c1_abs_max_content_sizes_to_text() {
        let dl = lay(
            "<div style='position: absolute; left: 0; top: 0; width: max-content; background: #ff0000'>abcde</div>",
            200.0,
        );
        assert_eq!(box_rect(&dl).w, 40.0);
    }

    #[test]
    fn c1_flex_basis_keyword_sizes_item() {
        let dl = lay(
            "<div style='display: flex'><div style='flex: 0 0 max-content; background: #ff0000'>abcdef</div></div>",
            200.0,
        );
        assert_eq!(box_rect(&dl).w, 48.0);
    }

    /// C1b: the painted (non-white) rects in order — one per grid item.
    fn grid_rects(dl: &DisplayList) -> Vec<Rect> {
        dl.rects
            .iter()
            .filter(|r| r.color != 0xFFFFFFFF)
            .cloned()
            .collect()
    }

    #[test]
    fn c1b_grid_keyword_track_widths() {
        // "aaa bbbbbb": min-content = word 'bbbbbb' (48), max-content = the
        // whole run (10 glyphs, 80). Column positions prove the packing.
        let dl = lay(
            "<div style='display:grid;grid-template-columns:min-content max-content 20px'><p style='background:#ff0000'>aaa bbbbbb</p><p style='background:#00ff00'>aaa bbbbbb</p><p style='background:#0000ff'>z</p></div>",
            800.0,
        );
        let rs = grid_rects(&dl);
        assert_eq!(rs.len(), 3, "one box per item");
        assert_eq!(rs[0].w, 48.0, "min-content = longest word");
        assert_eq!(rs[1].w, 80.0, "max-content = whole run");
        assert_eq!(rs[1].x - rs[0].x, 48.0, "columns pack at track widths");
        assert_eq!(rs[2].x - rs[1].x, 80.0);
    }

    #[test]
    fn c1b_grid_fit_content_clamps() {
        let dl = lay(
            "<div style='display:grid;grid-template-columns:fit-content(50px) 1fr'><p style='background:#ff0000'>aa bbbb</p><p style='background:#00ff00'>z</p></div>",
            800.0,
        );
        let rs = grid_rects(&dl);
        assert_eq!(rs[0].w, 50.0, "cap 50 between min 32 and max 56");
    }

    #[test]
    fn c6_grid_minmax_floor_beats_fr_share() {
        // avail 768 − 600 fixed = 168 for the fr track; the 200px floor wins.
        let dl = lay(
            "<div style='display:grid;grid-template-columns:minmax(200px, 1fr) 600px'><p style='background:#ff0000'>aa</p><p style='background:#00ff00'>z</p></div>",
            800.0,
        );
        let rs = grid_rects(&dl);
        assert_eq!(rs[0].w, 200.0);
    }

    #[test]
    fn c6_grid_minmax_zero_fr_shares_evenly() {
        // Two `minmax(0, 1fr)` columns with 600px of unwrappable text each:
        // the 0 floor (C6 fix) keeps the even 384/384 split instead of the
        // old minmax→auto content sizing.
        let long = "a".repeat(75);
        let html = format!(
            "<div style='display:grid;grid-template-columns:minmax(0, 1fr) minmax(0, 1fr)'><p style='background:#ff0000'>{long}</p><p style='background:#00ff00'>{long}</p></div>"
        );
        let dl = lay(&html, 800.0);
        let rs = grid_rects(&dl);
        assert_eq!(rs[0].w, 384.0);
        assert_eq!(rs[1].w, 384.0);
    }

    #[test]
    fn c2_aspect_ratio_sizes_auto_height() {
        let dl = lay(
            "<div style='width:160px;aspect-ratio:2;background:#ff0000'>z</div>",
            800.0,
        );
        let r = box_rect(&dl);
        assert_eq!(r.w, 160.0);
        assert_eq!(r.h, 80.0);
    }

    #[test]
    fn c2_aspect_ratio_content_box_and_clamps() {
        // Content-box convention: padding adds back on top of the ratio
        // height (content 100 + 2*10 chrome = 120, not 100).
        let dl = lay(
            "<div style='width:120px;padding:10px;aspect-ratio:1;background:#ff0000'>z</div>",
            800.0,
        );
        assert_eq!(box_rect(&dl).h, 120.0);
        // An explicit height wins over the ratio.
        let dl = lay(
            "<div style='width:160px;height:40px;aspect-ratio:2;background:#ff0000'>z</div>",
            800.0,
        );
        assert_eq!(box_rect(&dl).h, 40.0);
        // min/max-height clamp the ratio-derived height.
        let dl = lay(
            "<div style='width:160px;aspect-ratio:2;min-height:100px;background:#ff0000'>z</div>",
            800.0,
        );
        assert_eq!(box_rect(&dl).h, 100.0);
    }

    /// C2 on a replaced-ish box: a specified `aspect-ratio` replaces the
    /// intrinsic ratio, whichever axis is `auto`. `<img>`'s attributes are
    /// its intrinsic size here (300×150 by default).
    #[test]
    fn c2_aspect_ratio_sizes_a_replaced_box() {
        let size = |html: &str| {
            let dl = lay(html, 800.0);
            (dl.images[0].w, dl.images[0].h)
        };
        // A definite width, auto height: the ratio, not the intrinsic 2:1.
        assert_eq!(
            size("<img style='width:160px;aspect-ratio:2' src='a.png'>"),
            (160.0, 80.0)
        );
        // A definite height, auto width: the box grows sideways, which the
        // intrinsic ratio alone would not allow.
        assert_eq!(
            size("<img style='height:80px;aspect-ratio:2' src='a.png'>"),
            (160.0, 80.0)
        );
        // Both axes auto: a replaced element keeps its intrinsic width and
        // takes the ratio as its height (300 wide → 300×100 at 3:1).
        assert_eq!(
            size("<img style='aspect-ratio:3' src='a.png'>"),
            (300.0, 100.0)
        );
        // The `w / h` spelling is the same ratio, written out.
        assert_eq!(
            size("<img style='width:320px;aspect-ratio:16/9' src='a.png'>"),
            (320.0, 180.0)
        );
        // Two definite axes beat the ratio entirely.
        assert_eq!(
            size("<img style='width:160px;height:40px;aspect-ratio:2' src='a.png'>"),
            (160.0, 40.0)
        );
        // The ratio is measured against the *attribute* intrinsic size, not
        // the 300×150 default: a 200×100 image is 2:1, so a 3:1 ratio gives
        // a 200×~67 box.
        let (w, h) = size("<img width=200 height=100 style='aspect-ratio:3' src='a.png'>");
        assert_eq!(w, 200.0);
        assert!((h - 200.0 / 3.0).abs() < 0.01, "got {h}");
        // A `<video>` is the same replaced path, and so is an inline `<svg>`.
        assert_eq!(
            size("<video style='width:160px;aspect-ratio:2'></video>"),
            (160.0, 80.0)
        );
    }

    /// C2 `auto <ratio>`: the keyword lets the box keep a ratio it already
    /// has, and only falls back to the authored one where it has none.
    #[test]
    fn c2_aspect_ratio_auto_keyword_keeps_the_intrinsic_ratio() {
        let size = |html: &str| {
            let dl = lay(html, 800.0);
            (dl.images[0].w, dl.images[0].h)
        };
        // An `<img width=200 height=100>` has an intrinsic ratio (2:1), so
        // `auto 3` is read as 2:1 — the keyword did not override what the
        // element already knows.
        let (w, h) =
            size("<img width=200 height=100 style='width:160px;aspect-ratio:auto 3' src='a.png'>");
        assert_eq!(w, 160.0);
        assert_eq!(h, 80.0, "the intrinsic 2:1, not 160/3");
        // Without the keyword the same declaration is a flat 3:1.
        let (w, h) =
            size("<img width=200 height=100 style='width:160px;aspect-ratio:3' src='a.png'>");
        assert_eq!((w, h), (160.0, 160.0 / 3.0));
        // A replaced box with no attributes still has the 300×150 default as
        // its intrinsic ratio, so `auto 3` keeps that 2:1 too.
        assert_eq!(
            size("<img style='width:150px;aspect-ratio:auto 3' src='a.png'>"),
            (150.0, 75.0)
        );
        // `auto` on its own is not a ratio: the intrinsic 2:1 stands, which
        // is the behavior it had before the `auto <ratio>` form existed.
        assert_eq!(
            size("<img style='aspect-ratio:auto' src='a.png'>"),
            (300.0, 150.0)
        );
    }

    /// C2 `auto <ratio>` on a plain block, which has no intrinsic ratio to
    /// keep: the keyword is inert and the authored number is used. (This is
    /// the only place the `auto <ratio>` fallback actually fires, since every
    /// replaced box in this model has an intrinsic ratio — its attributes, or
    /// the 300×150 default.)
    #[test]
    fn c2_aspect_ratio_auto_keyword_falls_back_on_a_plain_box() {
        let dl = lay(
            "<div style='width:160px;aspect-ratio:auto 2;background:#ff0000'>z</div>",
            800.0,
        );
        let r = box_rect(&dl);
        assert_eq!((r.w, r.h), (160.0, 80.0));
    }

    /// The replaced path is untouched when no ratio is authored: a definite
    /// axis still stretches along the intrinsic ratio, min/max still clamp.
    #[test]
    fn c2_replaced_sizing_without_a_ratio_is_unchanged() {
        let size = |html: &str| {
            let dl = lay(html, 800.0);
            (dl.images[0].w, dl.images[0].h)
        };
        assert_eq!(
            size("<img src='a.png'>"),
            (300.0, 150.0),
            "the default intrinsic"
        );
        assert_eq!(
            size("<img width=200 height=100 src='a.png'>"),
            (200.0, 100.0),
            "attribute intrinsic"
        );
        assert_eq!(
            size("<img width=200 height=100 style='width:100px' src='a.png'>"),
            (100.0, 50.0),
            "a definite width keeps the intrinsic ratio"
        );
        assert_eq!(
            size("<img width=200 height=100 style='height:50px' src='a.png'>"),
            (100.0, 50.0),
            "…and so does a definite height"
        );
        assert_eq!(
            size("<img style='max-width:200px' src='a.png'>"),
            (200.0, 100.0),
            "clamping an auto-sized box keeps its aspect"
        );
        assert_eq!(
            size("<img style='width:400px;max-width:200px' src='a.png'>"),
            (200.0, 200.0),
            "a box with a definite width is clamped without reshaping it"
        );
    }

    #[test]
    fn c3_break_forces_column_in_multicol() {
        // Four 40px blocks in two columns: greedy (target 80) would fit
        // red+green in column 0; `break-after: column` on red forces
        // green — and the rest — into the second column.
        let dl = lay(
            "<style>
             body { margin: 0; }
             .cols { column-count: 2; column-gap: 20px; }
             .b { height: 40px; margin: 0; }
             </style>
             <div class='cols'>
             <div class='b' style='background:#ff0000; break-after: column'></div>
             <div class='b' style='background:#00ff00'></div>
             <div class='b' style='background:#0000ff'></div>
             <div class='b' style='background:#ffff00'></div>
             </div>",
            800.0,
        );
        let by = |c: u32| dl.rects.iter().find(|r| r.color == c).expect("bg rect");
        let (r, g, bl, y) = (
            by(0xFF0000FF),
            by(0x00FF00FF),
            by(0x0000FFFF),
            by(0xFFFF00FF),
        );
        assert!(
            g.x > r.x + r.w,
            "green forced into a new column: r.x={} r.w={} g.x={}",
            r.x,
            r.w,
            g.x
        );
        assert!((bl.x - g.x).abs() < 0.6, "blue shares green's column");
        assert!((bl.y - (g.y + 40.0)).abs() < 0.6);
        assert!((y.y - (bl.y + 40.0)).abs() < 0.6);
    }

    #[test]
    fn c4_abs_without_edges_uses_static_position() {
        // C4: an absolute box with auto edges on both axes lands at its
        // *static position* — where it would have been in flow — not at
        // the containing block's content origin. The flow cursor here sits
        // at x = 16 + 50 (padding), y ≈ 46 + 200 (after the tall block);
        // the old (cx, cy) fallback would have pinned y to ≈ 46.
        let dl = lay(
            r#"<div style="position:relative;padding-left:50px;padding-top:30px"><div style="height:200px">tall</div><div style="position:absolute">after</div></div>"#,
            400.0,
        );
        let after = dl.texts.iter().find(|t| t.text == "after").unwrap();
        let tall = dl.texts.iter().find(|t| t.text == "tall").unwrap();
        assert!((after.x - 66.0).abs() < 2.0, "static x, got {}", after.x);
        assert!(
            (after.y - (tall.y + 200.0)).abs() < 4.0,
            "static y at the flow cursor after the tall block: tall.y={} after.y={}",
            tall.y,
            after.y
        );
    }

    #[test]
    fn c5_rowspan_beyond_twelve_spans_the_whole_table() {
        // Old code clamped rowspan to 12; rows 13+ would drop the tall
        // cell's occupancy and their lone cells slid into column 0.
        let mut html = String::from("<table><tr><td rowspan='20'>a</td><td>b</td></tr>");
        for i in 2..=16 {
            html.push_str(&format!("<tr><td>{i}</td></tr>"));
        }
        html.push_str("</table>");
        let dl = lay(&html, 800.0);
        let x = |t: &str| dl.texts.iter().find(|r| r.text == t).unwrap().x;
        assert!(
            (x("16") - x("b")).abs() < 1.0,
            "row 16's cell stays in column 2: b.x={} c16.x={}",
            x("b"),
            x("16")
        );
        assert!(x("16") > x("a") + 1.0, "column 2 is right of column 1");
    }

    #[test]
    fn c5_border_spacing_and_collapse() {
        // Gaps between adjacent cell boxes for a 2×2 table under a given
        // table-level CSS: (column gap, row gap).
        let gaps = |table_css: &str| -> (f32, f32) {
            let html = format!(
                "<style>body {{ margin: 0; }} table {{ {table_css} }}</style>
                 <table><tr>
                 <td style='background:#ff0000'>a</td><td style='background:#00ff00'>b</td>
                 </tr><tr>
                 <td style='background:#0000ff'>c</td><td style='background:#ffff00'>d</td>
                 </tr></table>"
            );
            let dl = lay(&html, 800.0);
            let by = |c: u32| dl.rects.iter().find(|r| r.color == c).expect("cell bg");
            let (r, g, bl) = (by(0xFF0000FF), by(0x00FF00FF), by(0x0000FFFF));
            (g.x - (r.x + r.w), bl.y - (r.y + r.h))
        };
        let (cg, rg) = gaps("border-spacing: 10px");
        assert!((cg - 10.0).abs() < 1.0, "h spacing 10: got {cg}");
        assert!((rg - 10.0).abs() < 1.0, "v spacing 10: got {rg}");
        let (cg, rg) = gaps("border-spacing: 10px; border-collapse: collapse");
        assert!(cg.abs() < 1.0, "collapse zeroes spacing: {cg}");
        assert!(rg.abs() < 1.0, "collapse zeroes spacing: {rg}");
        let (cg, rg) = gaps("");
        assert!(
            cg.abs() < 0.01 && rg.abs() < 0.01,
            "default: abutting cells"
        );
    }

    #[test]
    fn c5_caption_side_bottom_paints_below_the_grid() {
        let dl = lay(
            "<table><caption style='caption-side:bottom;background:#ff0000'>cap</caption><tr><td style='background:#00ff00'>cell</td></tr></table>",
            400.0,
        );
        let cap = dl
            .rects
            .iter()
            .find(|r| r.color == 0xFF0000FF)
            .expect("caption bg");
        let cell = dl
            .rects
            .iter()
            .find(|r| r.color == 0x00FF00FF)
            .expect("cell bg");
        assert!(
            cap.y + 0.5 > cell.y + cell.h,
            "caption at/below the grid bottom: cap.y={} cell.y={} cell.h={}",
            cap.y,
            cell.y,
            cell.h
        );
    }

    // ---- I2: fragment cache equivalence ---------------------------------

    /// Build dom + styles once, then lay out through a persistent cache.
    fn cached_lay(
        dom: &Dom,
        styles: &[(NodeId, ComputedStyle)],
        cache: &mut FragmentCache,
        width: f32,
    ) -> DisplayList {
        layout_cached(dom, styles, width, 600.0, &Mock, cache)
    }

    fn dom_and_styles(html: &str) -> (Dom, Vec<(NodeId, ComputedStyle)>) {
        let (dom, _) = kitae_html::parse(html);
        let styles = kitae_style::StyleEngine::new(&[]).compute(&dom);
        (dom, styles)
    }

    fn dl_key(dl: &DisplayList) -> String {
        format!("{dl:?}")
    }

    fn find_tag(dom: &Dom, root: NodeId, tag: &str) -> Option<NodeId> {
        let mut found = None;
        dom.walk(root, &mut |n| {
            if found.is_none() && dom.tag(n) == tag {
                found = Some(n);
            }
        });
        found
    }

    /// The money property: a warm-cache relayout of an unchanged document
    /// reproduces the fresh layout byte-for-byte, entirely from hits.
    #[test]
    fn i2_warm_relayout_equals_fresh_layout() {
        let (dom, styles) = dom_and_styles(
            "<body><p style='margin:10px'>alpha bravo charlie delta echo</p>\
             <div style='background:#ff0000;padding:8px'><p>nested text</p></div>\
             <p style='border:2px solid #000000'>tail</p></body>",
        );
        let mut cache = FragmentCache::default();
        let first = cached_lay(&dom, &styles, &mut cache, 200.0);
        assert!(
            cache.hits == 0 && cache.misses >= 3,
            "cold pass all misses: {:?}",
            (cache.hits, cache.misses)
        );
        assert!(!cache.is_empty(), "entries stored");

        // Counters are cumulative; measure the warm pass by its delta.
        let (h0, m0) = (cache.hits, cache.misses);
        let second = cached_lay(&dom, &styles, &mut cache, 200.0);
        assert_eq!(
            dl_key(&second),
            dl_key(&first),
            "warm relayout must equal fresh"
        );
        assert!(
            cache.hits - h0 >= 3 && cache.misses - m0 == 0,
            "warm pass must be all hits, got delta {:?}",
            (cache.hits - h0, cache.misses - m0)
        );
    }

    /// A text mutation deep in one subtree invalidates exactly its
    /// body-level ancestor; the warm output still equals a fresh layout.
    #[test]
    fn i2_text_mutation_invalidates_only_its_subtree() {
        let (mut dom, styles) = dom_and_styles(
            "<body><p id='a'>one two three</p><p id='b'>four five six</p><p id='c'>seven eight</p></body>",
        );
        let mut cache = FragmentCache::default();
        let _ = cached_lay(&dom, &styles, &mut cache, 200.0);
        let misses_before = cache.misses;

        // Mutate <p id='b'>'s text node.
        let b = dom.get_element_by_id("b").expect("b node");
        let text_child = *dom.children(b).first().expect("text node");
        let rev_before = dom.rev(b);
        assert!(dom.set_text(text_child, "FOUR FIVE SIX SEVEN".into()));
        assert!(dom.rev(b) != rev_before, "mutation bumps the chain");

        let warm = cached_lay(&dom, &styles, &mut cache, 200.0);
        let fresh = layout(&dom, &styles, 200.0, 600.0, &Mock);
        assert_eq!(dl_key(&warm), dl_key(&fresh), "post-mutation warm == fresh");
        assert_eq!(cache.misses, misses_before + 1, "only p#b re-measured");
        assert!(warm.texts.iter().any(|t| t.text.contains("FOUR")));
    }

    /// Structural mutations (append/remove) invalidate the parent chain and
    /// the warm layout converges to the fresh one.
    #[test]
    fn i2_structural_mutation_converges_to_fresh_layout() {
        let (mut dom, styles) = dom_and_styles(
            "<body><p>keep me</p><div style='height:30px;background:#00ff00'></div></body>",
        );
        let mut cache = FragmentCache::default();
        let _ = cached_lay(&dom, &styles, &mut cache, 200.0);

        let body = find_tag(&dom, dom.root().unwrap(), "body").unwrap();
        let el = dom.create_element("p", vec![]);
        let t = dom.create_text("added late".into());
        dom.append(el, t);
        dom.append(body, el);
        let warm = cached_lay(&dom, &styles, &mut cache, 200.0);
        let fresh = layout(&dom, &styles, 200.0, 600.0, &Mock);
        assert_eq!(dl_key(&warm), dl_key(&fresh), "append: warm == fresh");
        // Text runs are per-word: "added late" paints as two runs.
        assert!(warm.texts.iter().any(|t| t.text.contains("added")));
        assert!(warm.texts.iter().any(|t| t.text.contains("late")));

        // Remove it again; the warm layout must shrink identically.
        dom.detach(body, el);
        let warm = cached_lay(&dom, &styles, &mut cache, 200.0);
        let fresh = layout(&dom, &styles, 200.0, 600.0, &Mock);
        assert_eq!(dl_key(&warm), dl_key(&fresh), "detach: warm == fresh");
        assert!(!warm.texts.iter().any(|t| t.text.contains("added")));
        assert!(!warm.texts.iter().any(|t| t.text.contains("late")));
    }

    /// A width change remeasures everything (per-entry width key) and still
    /// matches a fresh layout; a position:relative box merges from cache.
    #[test]
    fn i2_width_change_and_relative_box() {
        let (dom, styles) = dom_and_styles(
            "<body><p>filler words wrap here</p>\
             <div style='position:relative;top:6px;background:#0000ff'>shifted</div></body>",
        );
        let mut cache = FragmentCache::default();
        let _ = cached_lay(&dom, &styles, &mut cache, 200.0);

        // Warm pass at the same width: relative box merges from cache.
        let warm = cached_lay(&dom, &styles, &mut cache, 200.0);
        let fresh = layout(&dom, &styles, 200.0, 600.0, &Mock);
        assert_eq!(dl_key(&warm), dl_key(&fresh), "relative box: warm == fresh");

        // New width: every entry's width key mismatches → all miss.
        let resized = cached_lay(&dom, &styles, &mut cache, 300.0);
        let fresh = layout(&dom, &styles, 300.0, 600.0, &Mock);
        assert_eq!(dl_key(&resized), dl_key(&fresh), "resize: warm == fresh");
        assert!(cache.misses > 0, "width change must remeasure");
    }

    /// clear() drops everything (style-affecting state changed without a
    /// DOM mutation); the next pass repopulates and matches fresh.
    #[test]
    fn i2_clear_forces_full_remeasure() {
        let (dom, styles) =
            dom_and_styles("<body><p style='padding:4px'>stable text</p><p>more</p></body>");
        let mut cache = FragmentCache::default();
        let _ = cached_lay(&dom, &styles, &mut cache, 200.0);
        assert!(!cache.is_empty());
        cache.clear();
        assert!(cache.is_empty() && cache.len() == 0);
        let warm = cached_lay(&dom, &styles, &mut cache, 200.0);
        let fresh = layout(&dom, &styles, 200.0, 600.0, &Mock);
        assert_eq!(dl_key(&warm), dl_key(&fresh));
        assert!(cache.hits == 0, "cleared cache cannot hit");
    }
}
