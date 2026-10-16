//! CSS parser: stylesheet text → `Stylesheet`. Hand-rolled subset of CSS
//! Selectors 3 + CSS 2.1 for a reader browser: type/class/id/attribute
//! selectors, descendant, child, adjacent-sibling, and general-sibling
//! combinators, statically matchable
//! pseudo-classes (`:first-child`, `:last-child`, `:root`, `:not()`),
//! declaration blocks, `!important`, comments, `@media` with width
//! conditions, and `var()` custom-property substitution. Also exposes value
//! parsers for colors and lengths.

/// Comparison operator of an attribute selector.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrOp {
    /// `[attr]`
    Exists,
    /// `[attr=v]`
    Equals,
    /// `[attr~=v]` — whitespace-separated word match
    Includes,
    /// `[attr|=v]` — `v` or `v-…`
    DashMatch,
    /// `[attr^=v]`
    Prefix,
    /// `[attr$=v]`
    Suffix,
    /// `[attr*=v]`
    Substring,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttrSelector {
    pub name: String,
    pub op: AttrOp,
    pub value: String,
}

/// Pseudo-classes: static ones match from the tree alone; dynamic ones
/// (`Hover`, `Active`, `Focus`, `Checked`, `Disabled`, `NthChild`) match
/// against the tree plus the engine's [`InteractionState`]-equivalent passed
/// at style time (see kitae_style). Truly unmatchable selectors
/// (pseudo-elements, unknown state) are dropped at parse time.
#[derive(Debug, Clone, PartialEq)]
pub enum Pseudo {
    FirstChild,
    LastChild,
    Root,
    Not(Box<Selector>),
    /// `:hover` — true for the engine's hover node (see `InteractionState`).
    Hover,
    /// `:active` — true for the engine's active (pressed) node.
    Active,
    /// `:focus` (and `focus-visible`, approximated) — the focused node.
    Focus,
    /// `:checked` — checked checkbox/radio or selected `<option>`.
    Checked,
    /// `:disabled` — elements with a `disabled` attribute.
    Disabled,
    /// `:invalid` (A10) — a form control whose value fails its constraints
    /// (`required`, type format, min/max/step, pattern). Computed live by
    /// the style engine; `:valid` parses to `:not(:invalid)`.
    Invalid,
    /// `:nth-child(An+B)` — 1-based element-sibling index matches `a*n+b`
    /// for some `n >= 0` (`odd` = 2n+1, `even` = 2n).
    NthChild {
        a: i32,
        b: i32,
    },
    /// `:nth-of-type(An+B)` — like NthChild but counting only element
    /// siblings with the same tag name.
    NthOfType {
        a: i32,
        b: i32,
    },
    FirstOfType,
    LastOfType,
    OnlyOfType,
    /// `::before` (and legacy `:before`, R3): the rule generates content
    /// before the element's own. Parsed as a trailing pseudo so the style
    /// engine can route the rule to the generated box instead of the
    /// element; `matches` treats it as a pass-through.
    Before,
    /// `::after` (and legacy `:after`, R3): generated after the element.
    After,
    /// `:is(list)` — matches when any full selector in the list matches;
    /// specificity is that of the most specific argument (kitae_style).
    Is(Vec<Selector>),
    /// `:where(list)` — identical matching to `:is`, but contributes zero
    /// specificity no matter what its arguments are.
    Where(Vec<Selector>),
    /// `:has(args)` (B3) — the subject matches when some element in its own
    /// subtree satisfies one of the relative-selector arguments (OR). Each
    /// argument remembers whether it was child-anchored (`:has(> a)`); the
    /// subtree walk and anchoring live in kitae_style.
    Has(Vec<HasArg>),
    /// `:lang(ranges)` (D3) — the element's language equals a range or is a
    /// subtag extension of one (case-insensitive; `*` = any assigned
    /// language). The ranges arrive lowercased; the language lookup (nearest
    /// `lang`/`xml:lang` up the ancestor chain) lives in kitae_style.
    Lang(Vec<String>),
}

/// One `:has()` argument: a relative selector. `child_anchored` marks the
/// `:has(> …)` form — the argument's outermost compound must match a direct
/// child of the subject rather than any descendant. `+`/`~` sibling anchors
/// are not supported (the parser drops the whole argument; documented).
#[derive(Debug, Clone, PartialEq)]
pub struct HasArg {
    pub child_anchored: bool,
    pub selector: Selector,
}

/// B4: one filter function (CSS Filter Effects 1).
///
/// - `Blur` is in CSS pixels.
/// - `DropShadow` carries the same shape as `box-shadow` (dx, dy, blur, color
///   0xRRGGBBAA). A `0` blur is a hard offset shadow.
/// - The single-argument scalar functions take a unitless multiplier
///   (`brightness(1.4)`, `grayscale(0.5)`, `invert(0)`, `opacity(0.6)`,
///   `saturate(2)`, `sepia(0.3)`). The `N=1` form is the identity.
/// - `HueRotate` is in degrees (0deg = identity, 360deg = full turn).
#[derive(Debug, Clone, PartialEq)]
pub enum FilterFn {
    Blur(f32),
    DropShadow {
        dx: f32,
        dy: f32,
        blur: f32,
        color: u32,
    },
    Brightness(f32),
    Contrast(f32),
    Grayscale(f32),
    HueRotate(f32),
    Invert(f32),
    Opacity(f32),
    Saturate(f32),
    Sepia(f32),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FilterList(pub Vec<FilterFn>);

/// B4: parse a `filter` / `backdrop-filter` value into a `FilterList`.
/// `none` (or empty/garbage) → an empty list. The caller is responsible for
/// treating the empty list as "no effect" at the property level.
pub fn parse_filter_list(v: &str) -> FilterList {
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return FilterList::default();
    }
    // Filter functions are space- OR comma-separated; the standard form
    // in practice is whitespace, but `filter: blur(2px), grayscale(0.5)`
    // is also valid. We split on commas first, then walk each comma-piece
    // for space-separated function calls.
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let bytes = v.as_bytes();
    let mut comma_spans: Vec<&str> = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' if depth > 0 => depth -= 1,
            b',' if depth == 0 => {
                comma_spans.push(v[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    comma_spans.push(v[start..].trim());

    for piece in comma_spans {
        // Now split the comma-piece on whitespace at depth 0, but
        // collapse runs of whitespace so `blur(2px)  grayscale(0.5)` and
        // `blur(2px),grayscale(0.5)` both work.
        let mut depth = 0usize;
        let mut sstart = 0usize;
        let pbytes = piece.as_bytes();
        let mut func_spans: Vec<&str> = Vec::new();
        for (i, &b) in pbytes.iter().enumerate() {
            match b {
                b'(' => depth += 1,
                b')' if depth > 0 => depth -= 1,
                b if (b as char).is_ascii_whitespace() && depth == 0 => {
                    if i > sstart {
                        func_spans.push(piece[sstart..i].trim());
                        sstart = i + 1;
                    } else {
                        sstart = i + 1;
                    }
                }
                _ => {}
            }
        }
        if sstart < piece.len() {
            func_spans.push(piece[sstart..].trim());
        }
        for s in func_spans {
            if s.is_empty() {
                continue;
            }
            match parse_filter_fn(s) {
                Some(f) => out.push(f),
                None => return FilterList::default(),
            }
        }
    }
    FilterList(out)
}

fn parse_filter_fn(s: &str) -> Option<FilterFn> {
    let s = s.trim();
    let open = s.find('(')?;
    if !s.ends_with(')') {
        return None;
    }
    let name = s[..open].trim().to_ascii_lowercase();
    let args = s[open + 1..s.len() - 1].trim();
    match name.as_str() {
        "blur" => {
            // `blur()` with no arg → 0 radius.
            if args.is_empty() {
                return Some(FilterFn::Blur(0.0));
            }
            let n = parse_length(args).and_then(|l| match l {
                Length::Px(x) => Some(x),
                Length::Em(x) => Some(x * 16.0),
                Length::Rem(x) => Some(x * 16.0),
                _ => None,
            })?;
            if n < 0.0 {
                return None;
            }
            Some(FilterFn::Blur(n))
        }
        "drop-shadow" => {
            // 2–4 args in any order: lengths (dx, dy, [blur]), color.
            // Spec lets color appear anywhere; we tokenize and route by kind.
            let tokens: Vec<&str> = args.split_whitespace().collect();
            if tokens.len() < 2 || tokens.len() > 4 {
                return None;
            }
            let mut dx = 0.0;
            let mut dy = 0.0;
            let mut blur = 0.0;
            let mut color: Option<u32> = None;
            let mut lens_left = 2; // dx, dy are mandatory
            for t in &tokens {
                if lens_left > 0 {
                    if let Some(n) = parse_length(t).and_then(|l| match l {
                        Length::Px(x) => Some(x),
                        Length::Em(x) => Some(x * 16.0),
                        Length::Rem(x) => Some(x * 16.0),
                        _ => None,
                    }) {
                        if dx == 0.0 {
                            dx = n;
                            lens_left -= 1;
                        } else if dy == 0.0 {
                            dy = n;
                            lens_left -= 1;
                        } else if blur == 0.0 {
                            blur = n;
                        }
                    } else if let Some(c) = parse_color(t) {
                        color = Some(c);
                    } else {
                        return None;
                    }
                } else if let Some(c) = parse_color(t) {
                    color = Some(c);
                } else if parse_length(t).is_some() && blur == 0.0 {
                    if let Some(n) = parse_length(t).and_then(|l| match l {
                        Length::Px(x) => Some(x),
                        Length::Em(x) => Some(x * 16.0),
                        Length::Rem(x) => Some(x * 16.0),
                        _ => None,
                    }) {
                        blur = n;
                    }
                } else {
                    return None;
                }
            }
            // Color defaults to currentColor (the element's own color) at
            // the property site; the resolver fills it in. We stash a 0
            // sentinel for "no color given" — `apply` patches the
            // element's text color in if it sees this value.
            Some(FilterFn::DropShadow {
                dx,
                dy,
                blur: blur.max(0.0),
                color: color.unwrap_or(0),
            })
        }
        "brightness" | "contrast" | "grayscale" | "invert" | "opacity" | "saturate" | "sepia" => {
            let n: f32 = args.trim().parse().ok()?;
            let f = match name.as_str() {
                "brightness" => FilterFn::Brightness,
                "contrast" => FilterFn::Contrast,
                "grayscale" => FilterFn::Grayscale,
                "invert" => FilterFn::Invert,
                "opacity" => FilterFn::Opacity,
                "saturate" => FilterFn::Saturate,
                "sepia" => FilterFn::Sepia,
                _ => unreachable!(),
            };
            Some(f(n))
        }
        "hue-rotate" => {
            // `0deg` = identity; numbers without a unit are treated as
            // degrees (CSS allows bare numbers in <angle> contexts).
            let t = args.trim();
            let deg = if let Some(rest) = t.strip_suffix("deg") {
                rest.trim().parse::<f32>().ok()?
            } else if let Some(rest) = t.strip_suffix("rad") {
                rest.trim().parse::<f32>().ok()? * 57.29578
            } else if let Some(rest) = t.strip_suffix("turn") {
                rest.trim().parse::<f32>().ok()? * 360.0
            } else {
                t.parse::<f32>().ok()?
            };
            Some(FilterFn::HueRotate(deg))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// B7: `clip-path` / mask basic shapes.
// ---------------------------------------------------------------------------

/// `<shape-radius>` in `circle()`/`ellipse()`: a length-percentage (em/rem/
/// viewport folded to px; percentages and `calc()` stay symbolic for the
/// layout resolver), or one of the spec's keyword radii. The resolver
/// folds keywords against the element's border box from the shape's own
/// center.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShapeRadius {
    Len(Len),
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
}

/// A CSS basic shape, stored symbolically: every scalar keeps its
/// percentage unresolved so layout can fold it against the element's own
/// border box. Positions are offsets from the box's top-left — `cx`
/// percentages resolve against the width, `cy` against the height, per
/// spec.
#[derive(Debug, Clone, PartialEq)]
pub enum ClipShape {
    Circle {
        r: ShapeRadius,
        cx: Len,
        cy: Len,
    },
    Ellipse {
        rx: ShapeRadius,
        ry: ShapeRadius,
        cx: Len,
        cy: Len,
    },
    /// Insets from the border-box edges (negative values allowed — the
    /// clip then spills past the box). `radii` holds the `round` clause
    /// as (horizontal, vertical) pairs in tl/tr/br/bl order — zeros
    /// without one.
    Inset {
        top: Len,
        right: Len,
        bottom: Len,
        left: Len,
        radii: [(Len, Len); 4],
    },
    Polygon {
        points: Vec<(Len, Len)>,
        evenodd: bool,
    },
}

/// B8 `mix-blend-mode`: how an element's painted items composite against
/// the backdrop. The wire carries `wire()` — a u8 whose value order the
/// host maps 1:1 to `CGBlendMode`. `Normal` (0) means "no entry on the
/// wire"; only non-zero modes are stamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixBlend {
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl MixBlend {
    /// CSS keyword → mode; unknown values yield `None` so the cascade
    /// keeps the inherited/initial value.
    pub fn from_keyword(v: &str) -> Option<MixBlend> {
        Some(match v.trim().to_ascii_lowercase().as_str() {
            "normal" => Self::Normal,
            "multiply" => Self::Multiply,
            "screen" => Self::Screen,
            "overlay" => Self::Overlay,
            "darken" => Self::Darken,
            "lighten" => Self::Lighten,
            "color-dodge" => Self::ColorDodge,
            "color-burn" => Self::ColorBurn,
            "hard-light" => Self::HardLight,
            "soft-light" => Self::SoftLight,
            "difference" => Self::Difference,
            "exclusion" => Self::Exclusion,
            "hue" => Self::Hue,
            "saturation" => Self::Saturation,
            "color" => Self::Color,
            "luminosity" => Self::Luminosity,
            _ => return None,
        })
    }

    /// u8 written to the v18 clip/blend tail (spec table in
    /// `kitae_core/include/kitae_core.h`).
    pub fn wire(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Multiply => 1,
            Self::Screen => 2,
            Self::Overlay => 3,
            Self::Darken => 4,
            Self::Lighten => 5,
            Self::ColorDodge => 6,
            Self::ColorBurn => 7,
            Self::HardLight => 8,
            Self::SoftLight => 9,
            Self::Difference => 10,
            Self::Exclusion => 11,
            Self::Hue => 12,
            Self::Saturation => 13,
            Self::Color => 14,
            Self::Luminosity => 15,
        }
    }
}

/// Parse a `clip-path` value. `none`, `path()`, `url()` and garbage all
/// yield `None` — the element simply isn't clipped. A trailing
/// reference-box keyword (`border-box`, `content-box`, …) is accepted and
/// ignored: the border box is the only basis the resolver knows
/// (documented). An omitted radius is treated as `closest-side`
/// (documented approximation).
pub fn parse_clip_shape(v: &str, em: f32, rem: f32, vw: f32, vh: f32) -> Option<ClipShape> {
    let v = v.trim().to_ascii_lowercase();
    if v.is_empty() || v == "none" {
        return None;
    }
    parse_basic_shape(strip_ref_box(&v), em, rem, vw, vh)
}

/// Parse a `mask` / `mask-image` value into an approximate clip shape:
/// the shape's INTERIOR is the visible region, matching the spec for
/// shape masks. Only basic shapes are honored — `url(...)` and gradient
/// layers yield `None` (a real mask engine needs image fetch, alpha
/// ramps and `mask-mode`/`mask-composite`; out of scope, documented).
/// For comma-separated layers the first shape wins.
pub fn parse_mask_shape(v: &str, em: f32, rem: f32, vw: f32, vh: f32) -> Option<ClipShape> {
    for layer in split_top(v, b',') {
        if let Some(s) = parse_basic_shape(strip_ref_box(layer), em, rem, vw, vh) {
            return Some(s);
        }
    }
    None
}

/// Drop a trailing geometry-box keyword (`<basic-shape> || <geometry-box>`).
fn strip_ref_box(v: &str) -> &str {
    const BOXES: &[&str] = &[
        "margin-box",
        "border-box",
        "padding-box",
        "content-box",
        "fill-box",
        "stroke-box",
        "view-box",
    ];
    let s = v.trim_end();
    match s.rsplit_once(char::is_whitespace) {
        Some((head, tail)) if BOXES.contains(&tail) => head.trim_end(),
        _ => s,
    }
}

/// One `<basic-shape>` function call, name-(args)- form. Anything else
/// (`url(...)`, `path(...)`, stray shorthand junk) returns `None`.
fn parse_basic_shape(s: &str, em: f32, rem: f32, vw: f32, vh: f32) -> Option<ClipShape> {
    let s = s.trim();
    let open = s.find('(')?;
    if !s.ends_with(')') {
        return None;
    }
    let name = s[..open].trim();
    let body = &s[open + 1..s.len() - 1];
    match name {
        "circle" => parse_circle(body, em, rem, vw, vh),
        "ellipse" => parse_ellipse(body, em, rem, vw, vh),
        "inset" => parse_inset(body, em, rem, vw, vh),
        "polygon" => parse_polygon(body, em, rem, vw, vh),
        _ => None,
    }
}

/// Split a shape body on the `at` keyword (at most one occurrence).
fn split_at_keyword<'a, 'b>(tokens: &'b [&'a str]) -> Option<(&'b [&'a str], &'b [&'a str])> {
    match tokens.iter().position(|t| *t == "at") {
        Some(i) => {
            if tokens[i + 1..].contains(&"at") {
                return None;
            }
            Some((&tokens[..i], &tokens[i + 1..]))
        }
        None => Some((tokens, &[][..])),
    }
}

fn parse_shape_radius(t: &str, em: f32, rem: f32, vw: f32, vh: f32) -> Option<ShapeRadius> {
    Some(match t {
        "closest-side" => ShapeRadius::ClosestSide,
        "farthest-side" => ShapeRadius::FarthestSide,
        "closest-corner" => ShapeRadius::ClosestCorner,
        "farthest-corner" => ShapeRadius::FarthestCorner,
        _ => ShapeRadius::Len(parse_len_px_v(t, em, rem, vw, vh)?),
    })
}

fn pct(n: f32) -> Len {
    Len::Sum { px: 0.0, pct: n }
}

/// `<position>` after `at`: the horizontal/vertical keyword slots accept
/// either order (`at top left` == `at left top`); `center` fills the
/// first free slot; a length fills `cx` then `cy`, and a LONE length sets
/// both axes (spec: one `<length-percentage>` applies to each offset).
/// Missing slots default to 50%.
fn parse_position(tokens: &[&str], em: f32, rem: f32, vw: f32, vh: f32) -> Option<(Len, Len)> {
    let mut cx: Option<Len> = None;
    let mut cy: Option<Len> = None;
    let mut lengths = 0usize;
    let mut keywords = 0usize;
    for t in tokens {
        match *t {
            "left" => {
                if cx.is_some() {
                    return None;
                }
                cx = Some(Len::Px(0.0));
                keywords += 1;
            }
            "right" => {
                if cx.is_some() {
                    return None;
                }
                cx = Some(pct(100.0));
                keywords += 1;
            }
            "top" => {
                if cy.is_some() {
                    return None;
                }
                cy = Some(Len::Px(0.0));
                keywords += 1;
            }
            "bottom" => {
                if cy.is_some() {
                    return None;
                }
                cy = Some(pct(100.0));
                keywords += 1;
            }
            "center" => {
                if cx.is_none() {
                    cx = Some(pct(50.0));
                } else if cy.is_none() {
                    cy = Some(pct(50.0));
                } else {
                    return None;
                }
                keywords += 1;
            }
            other => {
                let l = parse_len_px_v(other, em, rem, vw, vh)?;
                if cx.is_none() {
                    cx = Some(l);
                } else if cy.is_none() {
                    cy = Some(l);
                } else {
                    return None;
                }
                lengths += 1;
            }
        }
    }
    if lengths == 1 && keywords == 0 && cy.is_none() {
        cy = cx;
    }
    Some((cx.unwrap_or(pct(50.0)), cy.unwrap_or(pct(50.0))))
}

fn parse_circle(body: &str, em: f32, rem: f32, vw: f32, vh: f32) -> Option<ClipShape> {
    let toks = split_top(body, b' ');
    let (rad, pos) = split_at_keyword(&toks)?;
    if rad.len() > 1 {
        return None;
    }
    let r = match rad.first() {
        Some(t) => parse_shape_radius(t, em, rem, vw, vh)?,
        None => ShapeRadius::ClosestSide,
    };
    let (cx, cy) = parse_position(pos, em, rem, vw, vh)?;
    Some(ClipShape::Circle { r, cx, cy })
}

fn parse_ellipse(body: &str, em: f32, rem: f32, vw: f32, vh: f32) -> Option<ClipShape> {
    let toks = split_top(body, b' ');
    let (rad, pos) = split_at_keyword(&toks)?;
    // One radius applies to both axes (a lone keyword likewise).
    let (rx, ry) = match rad.len() {
        0 => (ShapeRadius::ClosestSide, ShapeRadius::ClosestSide),
        1 => {
            let r = parse_shape_radius(rad[0], em, rem, vw, vh)?;
            (r, r)
        }
        2 => (
            parse_shape_radius(rad[0], em, rem, vw, vh)?,
            parse_shape_radius(rad[1], em, rem, vw, vh)?,
        ),
        _ => return None,
    };
    let (cx, cy) = parse_position(pos, em, rem, vw, vh)?;
    Some(ClipShape::Ellipse { rx, ry, cx, cy })
}

/// CSS 1–4-value shorthand expansion to tl, tr, br, bl.
fn expand4(v: &[Len]) -> [Len; 4] {
    match v.len() {
        1 => [v[0], v[0], v[0], v[0]],
        2 => [v[0], v[1], v[0], v[1]],
        3 => [v[0], v[1], v[2], v[1]],
        _ => [v[0], v[1], v[2], v[3]],
    }
}

fn zero_len4() -> [(Len, Len); 4] {
    [(Len::Px(0.0), Len::Px(0.0)); 4]
}

/// `round` clause: 1–4 horizontal radii, optionally `/ 1–4 vertical`s
/// (verticals default to the horizontals).
fn parse_round_radii(
    toks: &[&str],
    em: f32,
    rem: f32,
    vw: f32,
    vh: f32,
) -> Option<[(Len, Len); 4]> {
    let slash = toks.iter().position(|t| *t == "/");
    let (hs, vs) = match slash {
        Some(i) => (&toks[..i], &toks[i + 1..]),
        None => (toks, &[][..]),
    };
    if hs.is_empty() || hs.len() > 4 || vs.len() > 4 {
        return None;
    }
    let h: Vec<Len> = hs
        .iter()
        .map(|t| parse_len_px_v(t, em, rem, vw, vh))
        .collect::<Option<_>>()?;
    let v: Vec<Len> = if vs.is_empty() {
        h.clone()
    } else {
        vs.iter()
            .map(|t| parse_len_px_v(t, em, rem, vw, vh))
            .collect::<Option<_>>()?
    };
    let (h4, v4) = (expand4(&h), expand4(&v));
    Some([
        (h4[0], v4[0]),
        (h4[1], v4[1]),
        (h4[2], v4[2]),
        (h4[3], v4[3]),
    ])
}

fn parse_inset(body: &str, em: f32, rem: f32, vw: f32, vh: f32) -> Option<ClipShape> {
    let toks = split_top(body, b' ');
    let (offs, radii) = match toks.iter().position(|t| *t == "round") {
        Some(i) => (&toks[..i], &toks[i + 1..]),
        None => (toks.as_slice(), &[][..]),
    };
    if offs.is_empty() || offs.len() > 4 || radii.len() > 9 {
        return None;
    }
    let o: Vec<Len> = offs
        .iter()
        .map(|t| parse_len_px_v(t, em, rem, vw, vh))
        .collect::<Option<_>>()?;
    let [top, right, bottom, left] = expand4(&o);
    let radii = if radii.is_empty() {
        zero_len4()
    } else {
        parse_round_radii(radii, em, rem, vw, vh)?
    };
    Some(ClipShape::Inset {
        top,
        right,
        bottom,
        left,
        radii,
    })
}

fn parse_polygon(body: &str, em: f32, rem: f32, vw: f32, vh: f32) -> Option<ClipShape> {
    let mut pieces = split_top(body, b',');
    let mut evenodd = false;
    if pieces
        .first()
        .is_some_and(|t| matches!(*t, "evenodd" | "nonzero"))
    {
        evenodd = pieces[0] == "evenodd";
        pieces.remove(0);
    }
    if pieces.len() < 3 || pieces.len() > 64 {
        return None;
    }
    let mut points = Vec::with_capacity(pieces.len());
    for p in &pieces {
        let xy = split_top(p, b' ');
        if xy.len() != 2 {
            return None;
        }
        points.push((
            parse_len_px_v(xy[0], em, rem, vw, vh)?,
            parse_len_px_v(xy[1], em, rem, vw, vh)?,
        ));
    }
    Some(ClipShape::Polygon { points, evenodd })
}

/// Split on `sep` (`b' '` = ASCII whitespace runs) at paren depth 0,
/// dropping empty pieces — so `calc()` arguments survive tokenization
/// intact.
fn split_top(v: &str, sep: u8) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = v.as_bytes();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' if depth > 0 => depth -= 1,
            _ => {}
        }
        if depth == 0 && (b == sep || (sep == b' ' && b.is_ascii_whitespace())) {
            if i > start {
                let piece = v[start..i].trim();
                if !piece.is_empty() {
                    out.push(piece);
                }
            }
            start = i + 1;
        }
    }
    if start < v.len() {
        let piece = v[start..].trim();
        if !piece.is_empty() {
            out.push(piece);
        }
    }
    out
}

/// One compound selector: `div.foo#bar[href]:first-child`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SimpleSelector {
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attrs: Vec<AttrSelector>,
    pub pseudos: Vec<Pseudo>,
}

/// Combinator between two adjacent parts of a selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combinator {
    /// `A B` — B inside A at any depth.
    Descendant,
    /// `A > B` — B a direct child of A.
    Child,
    /// `A + B` — B immediately follows A.
    AdjacentSibling,
    /// `A ~ B` — B follows A under the same parent.
    GeneralSibling,
}

/// A full selector: combinator-joined chain, outermost ancestor first,
/// target last.
#[derive(Debug, Clone, PartialEq)]
pub struct Selector {
    pub parts: Vec<SimpleSelector>,
    /// `combs[i]` joins `parts[i]` and `parts[i + 1]`.
    pub combs: Vec<Combinator>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    pub property: String,
    pub value: String,
    pub important: bool,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
    /// R7: enclosing `@layer` path ("" = unlayered). Nested paths join
    /// with dots; anonymous layers get a sheet-unique name.
    pub layer: String,
    /// B3: enclosing `@container` query (None = unconditional). Unlike
    /// `@media`, `@container` cannot be hoisted at parse time — its
    /// condition runs against a container element's laid-out width, which
    /// only exists after layout. kitae_style evaluates it at cascade time.
    pub container: Option<ContainerQuery>,
    /// B15: enclosing `@scope` gate (None = unconditional). Like
    /// `@container`, scoping is element-relative and runs at cascade time:
    /// the rule applies only to elements inside a `start` subtree, minus
    /// any `end`-matching subtree.
    pub scope: Option<ScopeQuery>,
}

/// B3: one `@container` rule's gate. `name` is the container name from the
/// head (`@container card (min-width: 400px)` → `card`; "" = unnamed, the
/// nearest eligible ancestor container). `cond` is the parenthesized
/// condition text ("" = always true, e.g. `@container card { … }`).
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerQuery {
    pub name: String,
    pub cond: String,
}

/// B15: one `@scope <start> [to <end>] { … }` gate. `start` is a parsed
/// single complex selector (the spec allows one; a selector list makes the
/// whole at-rule invalid → dropped). `end` is the optional `to` bound.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeQuery {
    pub start: Selector,
    pub end: Option<Selector>,
}

/// B15: one `@property --name { … }` registration. `syntax` is the quoted
/// grammar string verbatim (`"*"` = universal); `inherits` the `inherits`
/// descriptor (default false per spec); `initial` the `initial-value` text
/// when present. Value syntax validation is out of scope — the engine
/// records the registration and kitae_style applies the inheritance and
/// initial-value semantics only.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyRule {
    pub name: String,
    pub syntax: String,
    pub inherits: bool,
    pub initial: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
    /// `@font-face` blocks in source order (Phase N5).
    pub font_faces: Vec<FontFace>,
    /// `@keyframes` rules in source order (Phase N8).
    pub keyframes: Vec<Keyframes>,
    /// R7: layer names in first-appearance order (statements + blocks).
    pub layers: Vec<String>,
    /// B15: `@property` registrations in source order.
    pub properties: Vec<PropertyRule>,
}

/// One `@font-face` source: `url(...)` plus optional `format(...)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontSrc {
    pub url: String,
    pub format: Option<String>,
}
/// One `@font-face` block: family, the descriptors that tell it apart from
/// its siblings, plus candidate sources in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontFace {
    pub family: String,
    pub srcs: Vec<FontSrc>,
    /// B11: the `font-weight` descriptor (1–1000, `normal` = 400, `bold` =
    /// 700). An absent descriptor *is* `normal` per css-fonts-4, so the
    /// field is never optional — a page that ships a 400 and a 700 face of
    /// one family declares two faces, not one face with two weights.
    pub font_weight: u32,
    /// B11: the `font-style` descriptor; an absent descriptor is `normal`.
    pub font_style: FontStyle,
    /// B11: parsed `unicode-range` intervals (inclusive code points).
    /// Empty means the face covers every character.
    pub unicode_ranges: Vec<(u32, u32)>,
    /// B11: the `font-display` descriptor; `auto` when absent.
    pub font_display: FontDisplay,
}

/// The `@font-face` `font-style` descriptor (B11). The
/// `oblique <angle>?` form collapses to [`FontStyle::Oblique`] — the engine
/// has one oblique slot per family and the host picks the angle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    Oblique,
}

impl FontStyle {
    /// The CSS keyword for this value (`normal`/`italic`/`oblique`).
    pub fn keyword(self) -> &'static str {
        match self {
            FontStyle::Normal => "normal",
            FontStyle::Italic => "italic",
            FontStyle::Oblique => "oblique",
        }
    }
}

/// The `@font-face` `font-display` descriptor (B11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FontDisplay {
    #[default]
    Auto,
    Block,
    Swap,
    Fallback,
    Optional,
}

impl Default for FontFace {
    fn default() -> Self {
        FontFace {
            family: String::new(),
            srcs: Vec::new(),
            font_weight: 400,
            font_style: FontStyle::Normal,
            unicode_ranges: Vec::new(),
            font_display: FontDisplay::Auto,
        }
    }
}

impl FontFace {
    /// Identity of the face inside its family: the family plus the
    /// descriptors that pick it among the family's faces. Two blocks with
    /// the same key are the same face (the second block's `src` is a
    /// duplicate download), while two weights of one family are distinct
    /// keys — the registry and the install pass key off this.
    pub fn descriptor_key(&self) -> String {
        format!(
            "{}\u{1}{}\u{1}{}",
            self.family.trim(),
            self.font_weight,
            self.font_style.keyword()
        )
    }

    /// How well the face serves a run at `weight`/`style` within its
    /// family (css-fonts-4 family matching, simplified): `None` when the
    /// declared slant cannot serve the request at all, otherwise
    /// `(slant rank, weight distance)` — lower is better, and the caller
    /// keeps the earlier declaration on ties (source order is the cascade
    /// order for faces). Weight distance is absolute, so a family whose
    /// closest weight is a synthetic neighbor still resolves instead of
    /// falling through to the next family.
    pub fn match_rank(&self, weight: u32, style: FontStyle) -> Option<(u8, u32)> {
        let slant = match (style, self.font_style) {
            (want, have) if want == have => 0,
            // Oblique stands in for either slant (the host synthesizes the
            // requested angle) and an italic face stands in for an oblique
            // one; an upright face takes a slanted request as the last
            // resort. An italic face is never used for an upright request.
            (FontStyle::Normal | FontStyle::Italic, FontStyle::Oblique) => 1,
            (FontStyle::Oblique, FontStyle::Italic) => 1,
            (FontStyle::Italic | FontStyle::Oblique, FontStyle::Normal) => 2,
            _ => return None,
        };
        Some((slant, self.font_weight.abs_diff(weight)))
    }

    /// Whether `c` falls inside the face's `unicode-range`.
    pub fn covers(&self, c: char) -> bool {
        self.unicode_ranges.is_empty()
            || self.unicode_ranges.iter().any(|(lo, hi)| {
                let p = u32::from(c);
                p >= *lo && p <= *hi
            })
    }

    /// Whether any character of `text` overlaps the face's range (B11:
    /// faces a page never touches are not downloaded).
    pub fn intersects_text(&self, text: &str) -> bool {
        self.unicode_ranges.is_empty() || text.chars().any(|c| self.covers(c))
    }
}

/// One `@keyframes` block (Phase N8): sorted offset/declaration pairs.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyframeBlock {
    /// Normalized offset 0.0–1.0 (`from` = 0, `to` = 1).
    pub offset: f32,
    pub declarations: Vec<Declaration>,
}

/// A named `@keyframes` rule.
#[derive(Debug, Clone, PartialEq)]
pub struct Keyframes {
    pub name: String,
    pub frames: Vec<KeyframeBlock>,
}

/// Strip `/* ... *&#47;` comments (replaced by a space so selectors don't glue).
fn strip_comments(css: &str) -> String {
    let chars: Vec<char> = css.chars().collect();
    let mut out = String::with_capacity(css.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            out.push(' ');
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Parse a stylesheet without evaluating `@media` conditions: every media
/// block is skipped. Use [`parse_stylesheet_viewport`] to apply sheets whose
/// media queries matter.
pub fn parse_stylesheet(css: &str) -> Stylesheet {
    let src = strip_comments(css);
    let chars: Vec<char> = src.chars().collect();
    let parsed = parse_rule_list(
        &chars,
        0,
        chars.len(),
        None,
        &EnvironmentState::default(),
        "",
    );
    Stylesheet {
        rules: parsed.rules,
        font_faces: parsed.font_faces,
        keyframes: parsed.keyframes,
        layers: parsed.layers,
        properties: parsed.properties,
    }
}

/// One parsed rule list: rules, font faces, keyframes, layer names
/// (R7, first-appearance order) and `@property` registrations (B15).
#[derive(Debug, Default)]
struct ParsedList {
    rules: Vec<Rule>,
    font_faces: Vec<FontFace>,
    keyframes: Vec<Keyframes>,
    layers: Vec<String>,
    properties: Vec<PropertyRule>,
}

/// Parse a stylesheet, hoisting `@media` blocks whose condition matches the
/// given viewport width (e.g. the embedder's current layout width) and the
/// default environment (Q6).
pub fn parse_stylesheet_viewport(css: &str, viewport_width: f32) -> Stylesheet {
    parse_stylesheet_environment(css, viewport_width, &EnvironmentState::default())
}

/// [`parse_stylesheet_viewport`] with a concrete environment: the
/// `prefers-*` media features evaluate against it (Q6).
pub fn parse_stylesheet_environment(
    css: &str,
    viewport_width: f32,
    env: &EnvironmentState,
) -> Stylesheet {
    parse_stylesheet_environment_size(
        css,
        ViewportBox {
            w: viewport_width,
            h: 0.0,
        },
        env,
    )
}

/// [`parse_stylesheet_environment`] with a real viewport box, so
/// block-axis `@media` features (`orientation`, `min-height`, MQ4 ranges on
/// `height`) resolve the same way the inline-`style` path does.
pub fn parse_stylesheet_environment_size(
    css: &str,
    viewport: ViewportBox,
    env: &EnvironmentState,
) -> Stylesheet {
    let src = strip_comments(css);
    let chars: Vec<char> = src.chars().collect();
    let parsed = parse_rule_list(&chars, 0, chars.len(), Some(viewport), env, "");
    Stylesheet {
        rules: parsed.rules,
        font_faces: parsed.font_faces,
        keyframes: parsed.keyframes,
        layers: parsed.layers,
        properties: parsed.properties,
    }
}

/// The viewport box a stylesheet's `@media` conditions are evaluated against.
/// `h == 0.0` means the caller had no block-axis size, which the media
/// evaluator treats as "height unknown" (block-axis features stay false).
#[derive(Debug, Clone, Copy, Default)]
pub struct ViewportBox {
    pub w: f32,
    pub h: f32,
}

fn parse_rule_list(
    chars: &[char],
    start: usize,
    end: usize,
    viewport: Option<ViewportBox>,
    env: &EnvironmentState,
    layer: &str,
) -> ParsedList {
    let mut i = start;
    let mut out = ParsedList::default();
    let mut anon = 0usize;
    while i < end {
        while i < end && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= end {
            break;
        }
        if chars[i] == '@' {
            i = handle_at_rule(chars, i, end, viewport, env, &mut out, layer, &mut anon);
            continue;
        }
        // Selector text up to the top-level '{' (';' = junk, skip it).
        let Some((brace, ch)) = find_top_level(chars, i, end, &['{', ';']) else {
            break;
        };
        if ch == ';' {
            i = brace + 1;
            continue;
        }
        let selector_text: String = chars[i..brace].iter().collect();
        let close = matching_brace(chars, brace, end);
        let decl_text: String = chars[brace + 1..close].iter().collect();
        let selectors = parse_selectors(selector_text.trim());
        if !selectors.is_empty() {
            out.rules.push(Rule {
                selectors,
                declarations: parse_declarations(&decl_text),
                layer: layer.to_string(),
                container: None,
                scope: None,
            });
        }
        i = close + 1;
    }
    out
}

/// Register a layer name on first appearance (R7).
fn note_layer(layers: &mut Vec<String>, name: &str) {
    if !name.is_empty() && !layers.iter().any(|l| l == name) {
        layers.push(name.to_string());
    }
}

/// Consume one at-rule starting at `at` (the '@'), appending any rules it
/// contributes (only `@media` with a satisfied condition; `@font-face`
/// blocks go to `font_faces`, `@keyframes` to `keyframes`, `@layer`
/// statements/blocks register layers and scope their rules — R7; B15 adds
/// `@property` registrations and `@scope` gates), and returning the index
/// just past it.
fn handle_at_rule(
    chars: &[char],
    at: usize,
    end: usize,
    viewport: Option<ViewportBox>,
    env: &EnvironmentState,
    out: &mut ParsedList,
    layer: &str,
    anon: &mut usize,
) -> usize {
    let (rules, font_faces, keyframes) = (&mut out.rules, &mut out.font_faces, &mut out.keyframes);
    let mut i = at + 1;
    let name: String = chars[i..end]
        .iter()
        .take_while(|c| c.is_alphanumeric() || **c == '-')
        .collect::<String>()
        .to_ascii_lowercase();
    i += name.len();
    let Some((sep, ch)) = find_top_level(chars, i, end, &['{', ';']) else {
        return end;
    };
    match (name.as_str(), ch) {
        ("layer", ';') => {
            // Statement form: `@layer a, b;` fixes layer order.
            let names: String = chars[i..sep].iter().collect();
            for n in names.split(',') {
                note_layer(&mut out.layers, n.trim());
            }
            sep + 1
        }
        ("layer", '{') => {
            let head: String = chars[i..sep].iter().collect();
            let close = matching_brace(chars, sep, end);
            let head = head.trim();
            let inner_layer = if head.is_empty() {
                // Anonymous layer: sheet-unique generated name.
                *anon += 1;
                let name = format!("\u{0}anon{}", *anon);
                note_layer(&mut out.layers, &name);
                name
            } else {
                let name = head.replace(' ', "");
                note_layer(&mut out.layers, &name);
                if layer.is_empty() {
                    name
                } else {
                    // Nested layers join their path.
                    format!("{layer}.{name}")
                }
            };
            let inner = parse_rule_list(chars, sep + 1, close, viewport, env, &inner_layer);
            out.rules.extend(inner.rules);
            out.font_faces.extend(inner.font_faces);
            out.keyframes.extend(inner.keyframes);
            out.properties.extend(inner.properties);
            for l in inner.layers {
                // Nested registration already carries the full path.
                note_layer(&mut out.layers, &l);
            }
            close + 1
        }
        ("media", '{') => {
            let cond: String = chars[i..sep].iter().collect();
            let close = matching_brace(chars, sep, end);
            if viewport
                .map(|vp| eval_media_condition_size(&cond, vp.w, vp.h, env))
                .unwrap_or(false)
            {
                let inner = parse_rule_list(chars, sep + 1, close, viewport, env, layer);
                rules.extend(inner.rules);
                font_faces.extend(inner.font_faces);
                keyframes.extend(inner.keyframes);
                out.properties.extend(inner.properties);
                for l in inner.layers {
                    note_layer(&mut out.layers, &l);
                }
            }
            close + 1
        }
        ("font-face", '{') => {
            // Kept (Phase N5): family + sources feed the web-font loader.
            let close = matching_brace(chars, sep, end);
            let decl_text: String = chars[sep + 1..close].iter().collect();
            if let Some(face) = parse_font_face(&decl_text) {
                font_faces.push(face);
            }
            close + 1
        }
        ("supports", '{') => {
            // Kept when the condition holds (Phase N7).
            let cond: String = chars[i..sep].iter().collect();
            let close = matching_brace(chars, sep, end);
            if eval_supports_condition(&cond) {
                let inner = parse_rule_list(chars, sep + 1, close, viewport, env, layer);
                rules.extend(inner.rules);
                font_faces.extend(inner.font_faces);
                keyframes.extend(inner.keyframes);
                out.properties.extend(inner.properties);
                for l in inner.layers {
                    note_layer(&mut out.layers, &l);
                }
            }
            close + 1
        }
        ("keyframes", '{') => {
            // Kept (Phase N8): the embedder interpolates per tick.
            let label: String = chars[i..sep].iter().collect();
            let name = label.trim().trim_matches('"').to_string();
            let close = matching_brace(chars, sep, end);
            if !name.is_empty() {
                keyframes.push(parse_keyframes(&name, chars, sep + 1, close));
            }
            close + 1
        }
        ("container", '{') => {
            // B3: kept unconditionally (no parse-time hoisting — the query
            // runs against a laid-out container width, not the viewport).
            // The head is `[name]? <condition>`: a leading identifier names
            // the queried container, the rest is the condition text passed
            // through verbatim ("" = always true).
            let head_raw: String = chars[i..sep].iter().collect();
            let head = head_raw.trim();
            let cq = if head.starts_with('(') {
                ContainerQuery {
                    name: String::new(),
                    cond: head.to_string(),
                }
            } else if let Some(open) = head.find('(') {
                ContainerQuery {
                    name: head[..open].trim().to_string(),
                    cond: head[open..].trim().to_string(),
                }
            } else {
                // Name-only form (`@container card { … }`): any container
                // bearing that name, any size.
                ContainerQuery {
                    name: head.to_string(),
                    cond: String::new(),
                }
            };
            let close = matching_brace(chars, sep, end);
            let inner = parse_rule_list(chars, sep + 1, close, viewport, env, layer);
            for mut r in inner.rules {
                // A nested `@container` overrides the outer one (the inner
                // gate is the nearer, more specific one; documented).
                if r.container.is_none() {
                    r.container = Some(cq.clone());
                }
                rules.push(r);
            }
            font_faces.extend(inner.font_faces);
            keyframes.extend(inner.keyframes);
            out.properties.extend(inner.properties);
            for l in inner.layers {
                note_layer(&mut out.layers, &l);
            }
            close + 1
        }
        ("property", '{') => {
            // B15: `@property --name { syntax: "…"; inherits: …;
            // initial-value: … }`. Registrations feed kitae_style's
            // custom-property semantics (inheritance + initial value).
            let head: String = chars[i..sep].iter().collect();
            let close = matching_brace(chars, sep, end);
            let decl_text: String = chars[sep + 1..close].iter().collect();
            if let Some(pr) = parse_property_rule(&head, &decl_text) {
                out.properties.push(pr);
            }
            close + 1
        }
        ("scope", '{') => {
            // B15: `@scope <start> [to <end>] { rules }`. The gate cannot
            // hoist at parse time (it's element-relative) — it rides on each
            // inner rule like `@container`, and the inner cascade checks
            // ancestry. A missing/unparseable start invalidates the whole
            // at-rule (skipped); an unparseable end just drops the bound.
            let head: String = chars[i..sep].iter().collect();
            let close = matching_brace(chars, sep, end);
            let (start_text, end_text) = split_scope_head(head.trim());
            if let Some(start) = start_text.and_then(parse_selector) {
                let gate = ScopeQuery {
                    start,
                    end: end_text.and_then(parse_selector),
                };
                let inner = parse_rule_list(chars, sep + 1, close, viewport, env, layer);
                for mut r in inner.rules {
                    // Inner `@scope` wins (nearest gate, like @container).
                    if r.scope.is_none() {
                        r.scope = Some(gate.clone());
                    }
                    rules.push(r);
                }
                font_faces.extend(inner.font_faces);
                keyframes.extend(inner.keyframes);
                out.properties.extend(inner.properties);
                for l in inner.layers {
                    note_layer(&mut out.layers, &l);
                }
            }
            close + 1
        }
        // Block at-rules we don't interpret (@media without a viewport):
        // skip the whole block.
        (_, '{') => matching_brace(chars, sep, end) + 1,
        // Statement at-rules (@import, @charset): skip to the ';'.
        (_, ';') => sep + 1,
        _ => end,
    }
}

/// B15: parse one `@property --name` registration. The name must be a
/// custom-property name (`--` + non-empty); `inherits` defaults to false
/// per spec; `initial-value` is stored verbatim (no syntax validation —
/// documented). `syntax` must be present for the registration to count.
fn parse_property_rule(head: &str, decl_text: &str) -> Option<PropertyRule> {
    let name = head.trim().to_string();
    if !name.starts_with("--") || name.len() < 3 {
        return None;
    }
    let mut syntax = String::new();
    let mut inherits = false;
    let mut initial = None;
    for d in parse_declarations(decl_text) {
        match d.property.as_str() {
            "syntax" => {
                syntax = d
                    .value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
            }
            "inherits" => inherits = d.value.trim().eq_ignore_ascii_case("true"),
            "initial-value" => initial = Some(d.value.trim().to_string()),
            _ => {}
        }
    }
    if syntax.is_empty() {
        return None;
    }
    Some(PropertyRule {
        name,
        syntax,
        inherits,
        initial,
    })
}

/// B15: split a `@scope` head into (start, optional end) on the top-level
/// ` to ` keyword (outside any bracket/paren depth or quote), each side
/// optionally wrapped in parens (the spec allows bare or parenthesized).
fn split_scope_head(head: &str) -> (Option<&str>, Option<&str>) {
    let chars: Vec<char> = head.chars().collect();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            't' if depth == 0 => {
                // ` to ` as a whole word: space, t-o, space (case-insensitive).
                let is_word = i > 0
                    && chars[i - 1].is_whitespace()
                    && chars.get(i + 1) == Some(&'o')
                    && chars.get(i + 2).is_some_and(|&c2| c2.is_whitespace());
                if is_word {
                    let start = head[..i].trim();
                    let end = head[i + 3..].trim();
                    let start = strip_outer_parens(start);
                    let end = strip_outer_parens(end);
                    let start = (!start.is_empty()).then_some(start);
                    let end = (!end.is_empty()).then_some(end);
                    return (start, end);
                }
            }
            _ => {}
        }
        i += 1;
    }
    let start = strip_outer_parens(head.trim());
    ((!start.is_empty()).then_some(start), None)
}

/// Strip one balanced pair of wrapping parens, if present (B15 `@scope`
/// head sides; the spec allows parenthesized or bare selectors).
fn strip_outer_parens(s: &str) -> &str {
    strip_wrapping_parens(s).unwrap_or(s)
}

/// Parse one `@font-face` declaration block. `src` keeps every `url()` in
/// order with its `format()` hint (selection happens at load: first
/// supported format wins). `font-weight`/`font-style` identify the face
/// inside its family — two blocks of one family differing in either are two
/// faces, both kept.
pub fn parse_font_face(decls: &str) -> Option<FontFace> {
    let mut family = String::new();
    let mut srcs = Vec::new();
    let mut weight: Option<u32> = None;
    let mut style: Option<FontStyle> = None;
    let mut ranges: Option<Vec<(u32, u32)>> = None;
    let mut display = FontDisplay::Auto;
    for decl in parse_declarations(decls) {
        match decl.property.as_str() {
            "font-family" => {
                family = decl
                    .value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim()
                    .to_string();
            }
            "src" => {
                for item in split_src_list(&decl.value) {
                    if let Some(src) = parse_font_src(&item) {
                        srcs.push(src);
                    }
                }
            }
            // B11: a malformed descriptor is an invalid declaration and is
            // simply dropped, keeping the default (unrestricted / auto /
            // normal weight and slant).
            "font-weight" => {
                if let Some(w) = parse_font_weight(&decl.value) {
                    weight = Some(w);
                }
            }
            "font-style" => {
                if let Some(s) = parse_font_style(&decl.value) {
                    style = Some(s);
                }
            }
            "unicode-range" => {
                if let Some(r) = parse_unicode_ranges(&decl.value) {
                    ranges = Some(r);
                }
            }
            "font-display" => match decl.value.trim().to_ascii_lowercase().as_str() {
                "auto" => display = FontDisplay::Auto,
                "block" => display = FontDisplay::Block,
                "swap" => display = FontDisplay::Swap,
                "fallback" => display = FontDisplay::Fallback,
                "optional" => display = FontDisplay::Optional,
                _ => {}
            },
            _ => {}
        }
    }
    if family.is_empty() || srcs.is_empty() {
        return None;
    }
    Some(FontFace {
        family,
        srcs,
        font_weight: weight.unwrap_or(400),
        font_style: style.unwrap_or_default(),
        unicode_ranges: ranges.unwrap_or_default(),
        font_display: display,
    })
}

/// B11: parse a `font-weight` descriptor — `normal` (400), `bold` (700) or
/// a number in 1–1000. `bolder`/`lighter` have no parent weight to be
/// relative to in a `@font-face`, and the variable-font range form
/// (`font-weight: 100 900`) is not modeled yet: the first number stands in.
/// `None` = invalid declaration, so the caller keeps the default.
fn parse_font_weight(v: &str) -> Option<u32> {
    let first = v.split_whitespace().next()?.to_ascii_lowercase();
    match first.as_str() {
        "normal" => Some(400),
        "bold" => Some(700),
        other => {
            let n: u32 = other.parse().ok()?;
            (1..=1000).contains(&n).then_some(n)
        }
    }
}

/// B11: parse a `font-style` descriptor — `normal`, `italic`, or `oblique`
/// with its optional angle (collapsed into [`FontStyle::Oblique`]). `None` =
/// invalid declaration, so the caller keeps the default.
fn parse_font_style(v: &str) -> Option<FontStyle> {
    let first = v.to_ascii_lowercase();
    match first.split_whitespace().next()? {
        "normal" => Some(FontStyle::Normal),
        "italic" => Some(FontStyle::Italic),
        "oblique" => Some(FontStyle::Oblique),
        _ => None,
    }
}

/// B11: parse a comma-separated `unicode-range` list — `U+ABCD`,
/// `U+ABCD-EFGH`, or wildcard forms like `U+4??` (expanded to
/// `U+400-4FF`). Any malformed token invalidates the whole declaration.
fn parse_unicode_ranges(v: &str) -> Option<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    for token in v.split(',') {
        let t = token.trim().to_ascii_uppercase();
        let rest = t.strip_prefix("U+")?;
        if rest.is_empty()
            || !rest
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '?' || c == '-')
        {
            return None;
        }
        let (lo, hi) = if let Some((a, b)) = rest.split_once('-') {
            let x = u32::from_str_radix(a, 16).ok()?;
            let y = u32::from_str_radix(b, 16).ok()?;
            (x.min(y), x.max(y))
        } else if rest.contains('?') {
            let lo = u32::from_str_radix(&rest.replace('?', "0"), 16).ok()?;
            let hi = u32::from_str_radix(&rest.replace('?', "F"), 16).ok()?;
            (lo, hi)
        } else {
            let c = u32::from_str_radix(rest, 16).ok()?;
            (c, c)
        };
        if hi > 0x10_FFFF {
            return None;
        }
        out.push((lo, hi));
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Split a `src` list on top-level commas (parens- and quote-aware).
fn split_src_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    for c in s.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Parse one `src` item: `url(...) [format(...)]`. Anything else is skipped.
fn parse_font_src(item: &str) -> Option<FontSrc> {
    let lower = item.to_ascii_lowercase();
    let url_pos = lower.find("url(")?;
    let mut depth = 0usize;
    let mut close = None;
    for (i, c) in item[url_pos..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(url_pos + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let url = item[url_pos + 4..close]
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string();
    if url.is_empty() {
        return None;
    }
    let rest = item[close + 1..].trim().to_ascii_lowercase();
    let format = rest.find("format(").and_then(|p| {
        let inner = &rest[p + "format(".len()..];
        let end = inner.find(')')?;
        Some(
            inner[..end]
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string(),
        )
    });
    Some(FontSrc { url, format })
}

/// Parse one `@keyframes` body: `offset { decls }` groups where offsets are
/// `%`, `from` (= 0) or `to` (= 1); comma groups (`0%, 100%`) expand. The
/// frames come back sorted by offset (stable for ties).
fn parse_keyframes(name: &str, chars: &[char], start: usize, end: usize) -> Keyframes {
    let mut frames = Vec::new();
    let mut i = start;
    while i < end {
        while i < end && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= end {
            break;
        }
        let Some((brace, ch)) = find_top_level(chars, i, end, &['{', ';']) else {
            break;
        };
        if ch == ';' {
            i = brace + 1;
            continue;
        }
        let selector_text: String = chars[i..brace].iter().collect();
        let close = matching_brace(chars, brace, end).min(end);
        let decl_text: String = chars[brace + 1..close].iter().collect();
        let declarations = parse_declarations(&decl_text);
        for sel in selector_text.split(',') {
            let sel = sel.trim().to_ascii_lowercase();
            let offset = if sel == "from" {
                Some(0.0)
            } else if sel == "to" {
                Some(1.0)
            } else if let Some(pct) = sel.strip_suffix('%') {
                pct.trim()
                    .parse::<f32>()
                    .ok()
                    .map(|p| (p / 100.0).clamp(0.0, 1.0))
            } else {
                None
            };
            if let Some(offset) = offset {
                frames.push(KeyframeBlock {
                    offset,
                    declarations: declarations.clone(),
                });
            }
        }
        i = close + 1;
    }
    frames.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    Keyframes {
        name: name.to_string(),
        frames,
    }
}

/// First occurrence of any target char at paren nesting depth 0.
fn find_top_level(
    chars: &[char],
    from: usize,
    end: usize,
    targets: &[char],
) -> Option<(usize, char)> {
    let mut depth = 0i32;
    for i in from..end {
        match chars[i] {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if depth == 0 && targets.contains(&c) => return Some((i, c)),
            _ => {}
        }
    }
    None
}

/// Index of the '}' matching the '{' at `open` (or `end` if unterminated).
fn matching_brace(chars: &[char], open: usize, end: usize) -> usize {
    let mut depth = 0usize;
    for i in open..end {
        match chars[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    end
}

fn parse_selectors(text: &str) -> Vec<Selector> {
    // Paren-aware split: functional pseudo arguments like `:is(a, .b > c)`
    // carry their own commas.
    split_top_commas(text)
        .into_iter()
        .filter_map(parse_selector)
        .collect()
}

/// Parse one complex selector (`div.card > a[href]`, `#id`, `:is(a, b)`) —
/// the entry point for engine *queries* (`querySelector`-shaped), as
/// opposed to stylesheet parsing. A selector list is deliberately not
/// accepted here: callers that want one parse it themselves so a bad
/// alternative cannot silently become an empty match set.
pub fn parse_selector(s: &str) -> Option<Selector> {
    let toks = split_compounds(s);
    if toks.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(toks.len());
    let mut combs = Vec::with_capacity(toks.len().saturating_sub(1));
    for (idx, (comb, text)) in toks.into_iter().enumerate() {
        if idx > 0 {
            combs.push(comb);
        }
        parts.push(parse_compound(&text)?);
    }
    Some(Selector { parts, combs })
}

/// Split selector text into compound selectors plus the combinator preceding
/// each (the first compound's combinator is ignored). Respects brackets,
/// parens, and quotes so `[data-x=" > "]` survives intact.
fn split_compounds(s: &str) -> Vec<(Combinator, String)> {
    fn finish(
        out: &mut Vec<(Combinator, String)>,
        cur: &mut String,
        comb: Option<Combinator>,
        invalid: &mut bool,
    ) {
        let text = cur.trim().to_string();
        if text.is_empty() {
            *invalid = true;
            return;
        }
        if !out.is_empty() && comb.is_none() {
            *invalid = true;
        }
        out.push((comb.unwrap_or(Combinator::Descendant), text));
        cur.clear();
    }

    let mut out = Vec::new();
    let mut cur = String::new();
    let mut current_comb = None;
    let mut space_after_current = false;
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut invalid = false;

    for c in s.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '[' | '(' => {
                depth += 1;
                cur.push(c);
            }
            ']' | ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            '>' | '+' | '~' if depth == 0 => {
                if cur.trim().is_empty() {
                    invalid = true;
                } else {
                    finish(&mut out, &mut cur, current_comb.take(), &mut invalid);
                    current_comb = Some(match c {
                        '>' => Combinator::Child,
                        '+' => Combinator::AdjacentSibling,
                        _ => Combinator::GeneralSibling,
                    });
                }
                space_after_current = false;
            }
            c if depth == 0 && c.is_whitespace() => {
                if !cur.trim().is_empty() {
                    space_after_current = true;
                }
            }
            _ => {
                if !cur.is_empty() && space_after_current {
                    finish(&mut out, &mut cur, current_comb.take(), &mut invalid);
                    current_comb = Some(Combinator::Descendant);
                    space_after_current = false;
                } else if cur.is_empty() && !out.is_empty() && current_comb.is_none() {
                    invalid = true;
                }
                cur.push(c);
            }
        }
    }

    if !cur.trim().is_empty() {
        finish(&mut out, &mut cur, current_comb.take(), &mut invalid);
    } else if !out.is_empty() || current_comb.is_some() {
        invalid = true;
    }
    if invalid {
        Vec::new()
    } else {
        out
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

/// Parse one compound selector: `div.foo#bar[href="x"]:first-child`.
/// Returns `None` for anything unmatchable (pseudo-elements, stateful
/// pseudo-classes, junk), which drops the whole selector.
fn parse_compound(s: &str) -> Option<SimpleSelector> {
    let chars: Vec<char> = s.trim().chars().collect();
    let mut sel = SimpleSelector::default();
    let mut i = 0;

    // Optional type selector / universal.
    while i < chars.len() && is_ident_char(chars[i]) {
        i += 1;
    }
    let tag: String = chars[..i].iter().collect();
    if !tag.is_empty() && tag != "*" {
        sel.tag = Some(tag.to_ascii_lowercase());
    }

    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
                let start = i;
                while i < chars.len() && is_ident_char(chars[i]) {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                sel.classes.push(chars[start..i].iter().collect());
            }
            '#' => {
                i += 1;
                let start = i;
                while i < chars.len() && is_ident_char(chars[i]) {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                sel.id = Some(chars[start..i].iter().collect());
            }
            '[' => {
                let mut depth = 1usize;
                let mut j = i + 1;
                let inner_start = j;
                while j < chars.len() && depth > 0 {
                    match chars[j] {
                        '[' => depth += 1,
                        ']' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if j >= chars.len() {
                    return None;
                }
                let inner: String = chars[inner_start..j].iter().collect();
                sel.attrs.push(parse_attr(&inner)?);
                i = j + 1;
            }
            ':' => {
                i += 1;
                let double = chars.get(i) == Some(&':');
                if double {
                    // R3: pseudo-elements. `::before`/`::after` ride the
                    // selector as trailing markers; other pseudo-elements
                    // (`::first-line`, `::marker`, …) still drop the rule.
                    i += 1;
                    let start = i;
                    while i < chars.len() && is_ident_char(chars[i]) {
                        i += 1;
                    }
                    let name: String = chars[start..i]
                        .iter()
                        .collect::<String>()
                        .to_ascii_lowercase();
                    match name.as_str() {
                        "before" => sel.pseudos.push(Pseudo::Before),
                        "after" => sel.pseudos.push(Pseudo::After),
                        _ => return None,
                    }
                    continue;
                }
                let start = i;
                while i < chars.len() && is_ident_char(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase();
                let arg: Option<String> = if chars.get(i) == Some(&'(') {
                    // Balanced scan so `:not(a[href="x"])` keeps its argument.
                    let mut depth = 0usize;
                    let mut k = i;
                    let mut inner_start = 0usize;
                    loop {
                        match chars.get(k) {
                            Some('(') => {
                                if depth == 0 {
                                    inner_start = k + 1;
                                }
                                depth += 1;
                            }
                            Some(')') => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            None => return None,
                            _ => {}
                        }
                        k += 1;
                    }
                    let inner: String = chars[inner_start..k].iter().collect();
                    i = k + 1;
                    Some(inner)
                } else {
                    None
                };
                match name.as_str() {
                    "first-child" => sel.pseudos.push(Pseudo::FirstChild),
                    "last-child" => sel.pseudos.push(Pseudo::LastChild),
                    "root" => sel.pseudos.push(Pseudo::Root),
                    "not" => {
                        let inner = arg.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
                        let inner_sel = parse_selector(inner)?;
                        sel.pseudos.push(Pseudo::Not(Box::new(inner_sel)));
                    }
                    "hover" => sel.pseudos.push(Pseudo::Hover),
                    "active" => sel.pseudos.push(Pseudo::Active),
                    "focus" | "focus-visible" => sel.pseudos.push(Pseudo::Focus),
                    "checked" => sel.pseudos.push(Pseudo::Checked),
                    "disabled" => sel.pseudos.push(Pseudo::Disabled),
                    // A10: constraint-validation state — computed by the
                    // style engine from the control's semantics (see
                    // `kitae_style::control_invalid`), matching the browser
                    // contract that :invalid reflects live validity.
                    "invalid" => sel.pseudos.push(Pseudo::Invalid),
                    "valid" => sel.pseudos.push(Pseudo::Not(Box::new(Selector {
                        parts: vec![SimpleSelector {
                            pseudos: vec![Pseudo::Invalid],
                            ..Default::default()
                        }],
                        combs: Vec::new(),
                    }))),
                    "enabled" => sel.pseudos.push(Pseudo::Not(Box::new(Selector {
                        parts: vec![SimpleSelector {
                            attrs: vec![AttrSelector {
                                name: "disabled".into(),
                                op: AttrOp::Exists,
                                value: String::new(),
                            }],
                            ..Default::default()
                        }],
                        combs: Vec::new(),
                    }))),
                    "nth-child" => {
                        let arg = arg.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
                        let (a, b) = parse_nth(arg)?;
                        sel.pseudos.push(Pseudo::NthChild { a, b });
                    }
                    "nth-of-type" => {
                        let arg = arg.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
                        let (a, b) = parse_nth(arg)?;
                        sel.pseudos.push(Pseudo::NthOfType { a, b });
                    }
                    "first-of-type" => sel.pseudos.push(Pseudo::FirstOfType),
                    "last-of-type" => sel.pseudos.push(Pseudo::LastOfType),
                    "only-of-type" => sel.pseudos.push(Pseudo::OnlyOfType),
                    // Legacy single-colon pseudo-elements (R3).
                    "before" => sel.pseudos.push(Pseudo::Before),
                    "after" => sel.pseudos.push(Pseudo::After),
                    // `:is()`/`:where()` take a full selector list (arguments
                    // may carry combinators); an invalid argument drops the
                    // whole outer selector, like `:not`.
                    "is" | "where" => {
                        let arg = arg.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
                        let list: Vec<Selector> = split_top_commas(arg)
                            .into_iter()
                            .filter_map(parse_selector)
                            .collect();
                        if list.is_empty() {
                            return None;
                        }
                        if name == "is" {
                            sel.pseudos.push(Pseudo::Is(list));
                        } else {
                            sel.pseudos.push(Pseudo::Where(list));
                        }
                    }
                    "has" => {
                        // B3: relative selectors, comma-separated (OR).
                        // `> x` child-anchors the argument; `+ x`/`~ x`
                        // sibling anchors are unsupported — those arguments
                        // are dropped (an empty list drops the selector).
                        let arg = arg.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
                        let mut args: Vec<HasArg> = Vec::new();
                        for piece in split_top_commas(arg) {
                            let piece = piece.trim();
                            let (child_anchored, rest) =
                                if let Some(stripped) = piece.strip_prefix('>') {
                                    (true, stripped.trim())
                                } else {
                                    (false, piece)
                                };
                            if rest.starts_with('+') || rest.starts_with('~') {
                                continue;
                            }
                            if let Some(selector) = parse_selector(rest) {
                                args.push(HasArg {
                                    child_anchored,
                                    selector,
                                });
                            }
                        }
                        if args.is_empty() {
                            return None;
                        }
                        sel.pseudos.push(Pseudo::Has(args));
                    }
                    "lang" => {
                        // D3: comma-separated language ranges (lowercased;
                        // matching + the ancestor `lang` lookup live in
                        // kitae_style). An empty argument drops the selector.
                        let arg = arg.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
                        let mut ranges: Vec<String> = Vec::new();
                        for piece in split_top_commas(arg) {
                            let piece = piece.trim();
                            if !piece.is_empty() {
                                ranges.push(piece.to_ascii_lowercase());
                            }
                        }
                        if ranges.is_empty() {
                            return None;
                        }
                        sel.pseudos.push(Pseudo::Lang(ranges));
                    }
                    // Truly stateless-unmatchable leftovers (:visited for
                    // privacy, :target without fragment routing, …) drop the
                    // whole selector, as do pseudo-elements.
                    _ => return None,
                }
            }
            _ => return None,
        }
    }

    let empty = sel.tag.is_none()
        && sel.id.is_none()
        && sel.classes.is_empty()
        && sel.attrs.is_empty()
        && sel.pseudos.is_empty();
    if empty {
        None
    } else {
        Some(sel)
    }
}

/// Parse an `:nth-child()` microsyntax (`odd`, `even`, `3`, `2n+1`,
/// `-n+3`, `n`, `+2n-1`, …) into `(a, b)`. Per the An+B microgrammar,
/// whitespace surrounds the whole expression and separates the an part
/// from b, but a sign always touches what it signs (`+ 2n` is invalid)
/// and the coefficient touches `n` (`3 n` is invalid). Comments were
/// already replaced by spaces at tokenization level. Anything else is
/// invalid (`None` drops the selector).
/// Public for the WPT conformance harness (docs/WPT_SCORECARD.md).
pub fn parse_nth(s: &str) -> Option<(i32, i32)> {
    let t = strip_comments(s);
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if lower == "odd" {
        return Some((2, 1));
    }
    if lower == "even" {
        return Some((2, 0));
    }
    let b = lower.as_bytes();
    let mut i = 0;
    // Leading sign, glued to what follows (`+ 3`, `+ n`, `+ 2n` invalid).
    let neg = match b[0] {
        b'+' => {
            i += 1;
            false
        }
        b'-' => {
            i += 1;
            true
        }
        _ => false,
    };
    // Coefficient digits; they must touch the `n`.
    let dstart = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let digits = &lower[dstart..i];
    let signed = |digits: &str| -> Option<i32> {
        let v: i32 = digits.parse().ok()?;
        Some(if neg { -v } else { v })
    };
    if i == b.len() {
        // Plain integer (`0` is valid syntax matching nothing — the matcher
        // handles it, so keep it instead of dropping the selector).
        return if digits.is_empty() {
            None
        } else {
            Some((0, signed(digits)?))
        };
    }
    if b[i] != b'n' {
        return None;
    }
    i += 1;
    let a = if digits.is_empty() {
        if neg {
            -1
        } else {
            1
        }
    } else {
        signed(digits)?
    };
    // b part: whitespace is allowed around the connecting sign here.
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i == b.len() {
        return Some((a, 0));
    }
    let bneg = match b[i] {
        b'+' => {
            i += 1;
            false
        }
        b'-' => {
            i += 1;
            true
        }
        _ => return None,
    };
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let dstart = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if dstart == i || i != b.len() {
        return None;
    }
    let v: i32 = lower[dstart..i].parse().ok()?;
    Some((a, if bneg { -v } else { v }))
}

fn parse_attr(inner: &str) -> Option<AttrSelector> {
    let inner = inner.trim();
    let name: String = inner
        .chars()
        .take_while(|c| is_ident_char(*c))
        .collect::<String>()
        .to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let rest = inner[name.len()..].trim();
    let (op, value) = if rest.is_empty() {
        (AttrOp::Exists, String::new())
    } else if let Some(v) = rest.strip_prefix("~=") {
        (AttrOp::Includes, unquote(v))
    } else if let Some(v) = rest.strip_prefix("|=") {
        (AttrOp::DashMatch, unquote(v))
    } else if let Some(v) = rest.strip_prefix("^=") {
        (AttrOp::Prefix, unquote(v))
    } else if let Some(v) = rest.strip_prefix("$=") {
        (AttrOp::Suffix, unquote(v))
    } else if let Some(v) = rest.strip_prefix("*=") {
        (AttrOp::Substring, unquote(v))
    } else if let Some(v) = rest.strip_prefix('=') {
        (AttrOp::Equals, unquote(v))
    } else {
        return None;
    };
    Some(AttrSelector {
        name,
        op,
        value: value.trim().to_string(),
    })
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    let bytes = v.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// Evaluate an `@supports` condition (Phase N7): `not`/`and`/`or` over
/// `(property: value)` declarations, with parens. A declaration is
/// supported when the property is one the engine implements (the
/// `apply()` arms in kitae_style) and the value is non-empty; unknown
/// syntax (general-enclosed functions, bare words) is false per spec.
pub fn eval_supports_condition(cond: &str) -> bool {
    eval_supports_or(cond.trim())
}

fn eval_supports_or(s: &str) -> bool {
    split_bool_op(s, "or")
        .iter()
        .any(|part| eval_supports_and(part))
}

fn eval_supports_and(s: &str) -> bool {
    split_bool_op(s, "and")
        .iter()
        .all(|part| eval_supports_not(part))
}

fn eval_supports_not(s: &str) -> bool {
    let s = s.trim();
    if let Some(rest) = strip_bool_word(s, "not") {
        return !eval_supports_not(rest);
    }
    eval_supports_atom(s)
}

fn eval_supports_atom(s: &str) -> bool {
    let s = s.trim();
    // B15: `selector(<complex selector>)` — supported when the engine can
    // parse the selector (an optional `for <element>` clause is ignored;
    // documented).
    if let Some(inner) = strip_func_call(s, "selector") {
        let inner = inner.trim();
        let sel = strip_for_clause(inner).trim();
        return parse_selector(sel).is_some();
    }
    // Parenthesized group: declaration or nested condition.
    if let Some(inner) = strip_wrapping_parens(s) {
        if let Some((prop, value)) = split_declaration(inner.trim()) {
            return supports_declaration(prop, value);
        }
        return eval_supports_or(inner.trim());
    }
    false
}

/// `name(inner)` call form, case-insensitive (B15).
fn strip_func_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let lower = s.to_ascii_lowercase();
    let open = lower.find('(')?;
    if !lower[..open].trim_end().eq_ignore_ascii_case(name) || !s.ends_with(')') {
        return None;
    }
    // The '(' must be the last paren group's opening (single call form).
    if lower[..open].contains('(') {
        return None;
    }
    Some(&s[open + 1..s.len() - 1])
}

/// Drop a trailing top-level ` for …` clause from `selector()`'s argument
/// (B15; the argument selector itself may contain `for` inside function
/// parentheses — only depth-0 splits count).
fn strip_for_clause(s: &str) -> &str {
    let chars: Vec<char> = s.chars().collect();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            'f' | 'F'
                if depth == 0
                    && i + 3 <= chars.len()
                    && chars[i..(i + 3).min(chars.len())]
                        .iter()
                        .collect::<String>()
                        .eq_ignore_ascii_case("for")
                    && ((i == 0 && chars.len() > 3 && chars[3].is_whitespace())
                        || (i > 0 && chars[i - 1].is_whitespace()))
                    && chars.get(i + 3).is_some_and(|c| c.is_whitespace()) =>
            {
                return &s[..i];
            }
            _ => {}
        }
        i += 1;
    }
    s
}

/// Split on a whitespace-delimited boolean operator at paren depth 0
/// (quote-aware, so `or` inside `"border"` never splits).
fn split_bool_op(s: &str, op: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            c if depth == 0 && (c.is_whitespace()) => {
                // Collect the whole word, then decide.
                let mut word = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc.is_whitespace() || nc == '(' || nc == ')' {
                        break;
                    }
                    word.push(nc);
                    chars.next();
                }
                if word.eq_ignore_ascii_case(op) {
                    out.push(cur.trim().to_string());
                    cur.clear();
                } else {
                    cur.push(c);
                    cur.push_str(&word);
                }
            }
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out.retain(|p| !p.is_empty());
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Strip a leading boolean word (`not`) when it stands alone.
fn strip_bool_word<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(word)?;
    match rest.chars().next() {
        Some(c) if c.is_whitespace() || c == '(' => Some(rest.trim_start()),
        _ => None,
    }
}

/// Remove one balanced wrapping paren pair (`( … )` covering the whole
/// string); `None` when not fully wrapped.
fn strip_wrapping_parens(s: &str) -> Option<&str> {
    let s = s.trim();
    if !s.starts_with('(') || !s.ends_with(')') {
        return None;
    }
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i + 1 != s.len() {
                    return None; // closes early: not a wrapper
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    Some(&s[1..s.len() - 1])
}

/// Split `property: value` on the first depth-0 colon.
fn split_declaration(s: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => return Some((s[..i].trim(), s[i + 1..].trim())),
            _ => {}
        }
    }
    None
}

/// Properties the engine implements (mirrors the `apply()` arms + the `all`
/// shorthand consumed by the cascade). B15: refreshed to the full set.
fn supports_declaration(prop: &str, value: &str) -> bool {
    const SUPPORTED: &[&str] = &[
        "all",
        "color",
        "fill",
        "stroke",
        "direction",
        "display",
        "opacity",
        "vertical-align",
        "background",
        "background-color",
        "background-image",
        "background-position",
        "background-size",
        "background-repeat",
        "background-origin",
        "background-clip",
        "visibility",
        "font",
        "font-size",
        "font-weight",
        "font-style",
        "font-family",
        "text-align",
        "text-decoration",
        "text-decoration-line",
        "text-decoration-style",
        "text-decoration-color",
        "text-decoration-thickness",
        "text-overflow",
        "text-shadow",
        "text-transform",
        "letter-spacing",
        "line-height",
        "white-space",
        "word-break",
        "overflow-wrap",
        "word-wrap",
        "hyphens",
        "margin",
        "margin-top",
        "margin-right",
        "margin-bottom",
        "margin-left",
        "padding",
        "padding-top",
        "padding-right",
        "padding-bottom",
        "padding-left",
        "width",
        "height",
        "max-width",
        "min-width",
        "max-height",
        "min-height",
        "aspect-ratio",
        // C3: fragmentation properties (parsed + carried; the multicol
        // splitter consumes break-inside, print/H3 the rest).
        "break-before",
        "break-after",
        "break-inside",
        "page-break-before",
        "page-break-after",
        "page-break-inside",
        "orphans",
        "widows",
        // C5: table polish (the table layout consumes spacing/collapse
        // geometry and caption-side placement).
        "border-collapse",
        "border-spacing",
        "caption-side",
        "position",
        "top",
        "right",
        "bottom",
        "left",
        "float",
        "clear",
        "z-index",
        "overflow",
        "border",
        "border-top",
        "border-right",
        "border-bottom",
        "border-left",
        "border-width",
        "border-style",
        "border-color",
        "border-radius",
        "border-top-width",
        "border-right-width",
        "border-bottom-width",
        "border-left-width",
        "border-top-style",
        "border-right-style",
        "border-bottom-style",
        "border-left-style",
        "border-top-color",
        "border-right-color",
        "border-bottom-color",
        "border-left-color",
        "outline",
        "outline-width",
        "outline-style",
        "outline-color",
        "outline-offset",
        "box-shadow",
        "flex",
        "flex-direction",
        "flex-grow",
        "flex-shrink",
        "flex-basis",
        "flex-wrap",
        "align-items",
        "align-content",
        "justify-content",
        "gap",
        "column-gap",
        "row-gap",
        "grid-template-columns",
        "grid-template-rows",
        "grid-template-areas",
        "grid-column",
        "grid-row",
        "grid-area",
        "grid-column-start",
        "grid-column-end",
        "grid-row-start",
        "grid-row-end",
        "grid-gap",
        "grid-column-gap",
        "grid-row-gap",
        // C6: `grid-auto-flow` (row/column/dense) is parsed by the engine.
        "grid-auto-flow",
        "container-type",
        "container-name",
        "content",
        "counter-increment",
        "counter-reset",
        "list-style",
        "list-style-position",
        "list-style-type",
        "object-fit",
        "filter",
        "backdrop-filter",
        "transform",
        "transform-origin",
        "transition",
        "transition-property",
        "transition-duration",
        "transition-delay",
        "transition-timing-function",
        "animation",
        "animation-name",
        "animation-duration",
        "animation-delay",
        "animation-direction",
        "animation-iteration-count",
        "animation-timing-function",
        "overscroll-behavior",
        "overscroll-behavior-x",
        "overscroll-behavior-y",
        "scroll-snap-type",
        "scroll-snap-align",
        // B7: basic-shape clipping + the shape-approximated mask.
        "clip-path",
        "mask",
        "mask-image",
        "-webkit-mask-image",
        // B8: blending + isolation (were missing from the B15 refresh).
        "mix-blend-mode",
        "isolation",
        // E5: layer-promotion hint.
        "will-change",
        // B5: multi-column longhands (`column-gap` is already listed with
        // the flex/grid gaps).
        "columns",
        "column-count",
        "column-width",
        // B4: writing-mode + logical geometry (longhand and shorthand).
        "writing-mode",
        "inline-size",
        "block-size",
        "min-inline-size",
        "max-inline-size",
        "min-block-size",
        "max-block-size",
        "inset-inline",
        "inset-inline-start",
        "inset-inline-end",
        "inset-block",
        "inset-block-start",
        "inset-block-end",
        "margin-inline",
        "margin-inline-start",
        "margin-inline-end",
        "margin-block",
        "margin-block-start",
        "margin-block-end",
        "padding-inline",
        "padding-inline-start",
        "padding-inline-end",
        "padding-block",
        "padding-block-start",
        "padding-block-end",
        // D4: OpenType feature plumbing (`font-feature-settings` pairs and
        // the `font-variant` ligature/caps keywords are applied by the
        // cascade; numeric/east-asian/position axes stay unparsed).
        "font-feature-settings",
        "font-variant",
        "font-variant-caps",
        "font-variant-ligatures",
        // D6: glyph orientation inside vertical writing modes.
        "text-orientation",
    ];
    if value.trim().is_empty() {
        return false;
    }
    SUPPORTED.contains(&prop.trim().to_ascii_lowercase().as_str())
}

/// Resolved color scheme for `prefers-color-scheme` (Q6). The host resolves
/// its "system" setting to a concrete answer; the engine only ever sees
/// light or dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    #[default]
    Light,
    Dark,
}

/// The media environment a page is laid out in (H3 print). `Screen` is the
/// normal case; `Print` is the paper environment, where `@media print` rules
/// apply, `@media screen` ones do not, and the `prefers-*` features answer as
/// a print device rather than as the screen the user is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaType {
    #[default]
    Screen,
    Print,
}

impl MediaType {
    /// Paper is a light, motionless, normal-contrast device, so the
    /// `prefers-*` features ignore the screen's answers while print media is
    /// active (a dark page keeps its dark `@media screen` block, which does
    /// not apply on paper, and its print block styles for white paper).
    pub fn print_defaults(self) -> bool {
        matches!(self, MediaType::Print)
    }
}

/// Embedder environment for the `prefers-*` media features (Q6) and the
/// media type (H3 print).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EnvironmentState {
    pub color_scheme: ColorScheme,
    pub reduced_motion: bool,
    /// `(prefers-contrast: more)` — no host toggle yet, carried so pages can
    /// react once one exists.
    pub contrast_more: bool,
    /// The media environment (`@media print` vs `@media screen`).
    pub media_type: MediaType,
}

/// Evaluate a media-query condition (`"(max-width: 600px), screen and
/// (min-width: 400px)"`) against a viewport width. Comma terms are OR'd,
/// `and`-features AND'd; unknown features make their term false (per spec).
/// Environment features (`prefers-*`) evaluate against a default
/// environment (light, full motion, normal contrast) — use
/// [`eval_media_condition_env`] for a real one.
pub fn eval_media_condition(cond: &str, viewport_width: f32) -> bool {
    eval_media_condition_env(cond, viewport_width, &EnvironmentState::default())
}

/// [`eval_media_condition`] with the embedder environment (Q6): the media
/// type (`screen` / `print`) and the `prefers-color-scheme` /
/// `prefers-reduced-motion` / `prefers-contrast` features read from it.
pub fn eval_media_condition_env(cond: &str, viewport_width: f32, env: &EnvironmentState) -> bool {
    // Height-less callers keep the historical behavior: a block-axis
    // feature is simply unknown to them, so it evaluates false.
    eval_media_condition_size(cond, viewport_width, 0.0, env)
}

/// [`eval_media_condition`] against a real viewport box. Block-axis
/// features (`height`, `min-height`, `max-height`, `orientation`) and the
/// MQ4 range syntax (`(width >= 40em)`, `(400px < width <= 700px)`) need
/// the viewport height, which the width-only entry points above do not
/// carry.
pub fn eval_media_condition_size(
    cond: &str,
    viewport_width: f32,
    viewport_height: f32,
    env: &EnvironmentState,
) -> bool {
    cond.split(',')
        .any(|term| eval_media_term(term, viewport_width, viewport_height, env))
}

/// A media-feature length in px. `em`/`rem` fold at the initial font size
/// (media queries evaluate before any element font is known).
fn media_px(v: &str) -> Option<f32> {
    match parse_length(v) {
        Some(Length::Px(x)) => Some(x),
        Some(Length::Em(x)) | Some(Length::Rem(x)) => Some(x * 16.0),
        _ => None,
    }
}

/// One side of a range comparison (`>=`, `<=`, `>`, `<`, `=`). Longer
/// operators are listed first so `>=` never matches as `>`.
fn range_op(s: &str) -> Option<u8> {
    Some(match s {
        ">=" => 1,
        "<=" => 2,
        ">" => 3,
        "<" => 4,
        "=" => 5,
        _ => return None,
    })
}

fn range_holds(op: u8, actual: f32, bound: f32) -> bool {
    match op {
        1 => actual >= bound,
        2 => actual <= bound,
        3 => actual > bound,
        4 => actual < bound,
        _ => (actual - bound).abs() < 0.5,
    }
}

/// The same comparison read from the other side: `(600px <= width)` is
/// `width >= 600px`.
fn mirror_op(op: u8) -> u8 {
    match op {
        1 => 2,
        2 => 1,
        3 => 4,
        4 => 3,
        other => other,
    }
}

/// A parsed MQ4 range/prefix comparison: the feature name plus one or two
/// `(op, bound)` pairs, in source order.
struct MediaRange<'a> {
    name: &'a str,
    lo: Option<(u8, f32)>,
    hi: Option<(u8, f32)>,
}

/// Parse a media feature in range context. Three shapes, all with the
/// feature name on one side of the operator:
///
/// * `(width >= 40em)` / `(40em <= width)` — `name op value`
/// * `(400px < width)` — `value op name`
/// * `(400px <= width < 900px)` — `value op name op value`
///
/// A bare `>=`-style operator prefix (`(>= 600px)`) is the "prefix match"
/// form: it compares against every feature, so it is left to the legacy
/// `(name: value)` path and reads as unknown here.
fn parse_media_range(inner: &str) -> Option<MediaRange<'_>> {
    let toks: Vec<&str> = inner.split_whitespace().collect();
    let op_at = |i: usize| -> Option<u8> { range_op(toks[i]) };
    match toks.as_slice() {
        [a, _b, c] => {
            // `(name op value)` reads directly; `(value op name)` has the
            // comparison backwards, so its operator mirrors.
            let (op, name, value) = match (op_at(1), is_range_name(a)) {
                (Some(op), true) => (op, *a, c),
                (Some(op), false) => match is_range_name(c) {
                    true => (mirror_op(op), *c, a),
                    false => return None,
                },
                _ => return None,
            };
            Some(MediaRange {
                name,
                lo: Some((op, media_px(value)?)),
                hi: None,
            })
        }
        [a, b, name, d, e] if is_range_name(name) => {
            // `(400px <= width < 900px)`: the first bound sits left of the
            // name (mirrored), the second right of it (as written).
            let (lo, hi) = (mirror_op(op_at(1)?), op_at(3)?);
            let (l, h) = (media_px(a)?, media_px(e)?);
            Some(MediaRange {
                name,
                lo: Some((lo, l)),
                hi: Some((hi, h)),
            })
        }
        _ => None,
    }
}

/// Feature names that carry a length this engine can compare. A bound on
/// any other name is an unknown feature (false), per spec.
fn is_range_name(name: &str) -> bool {
    matches!(name, "width" | "height" | "device-width" | "device-height")
}

fn eval_media_term(term: &str, w: f32, h: f32, env: &EnvironmentState) -> bool {
    let lowered = term.trim().to_ascii_lowercase();
    let (negated, term) = match lowered.strip_prefix("not ") {
        Some(rest) => (true, rest.to_string()),
        None => (false, lowered),
    };
    // Paper answers the `prefers-*` questions its own way, whatever the
    // screen the page came from is set to (H3 print).
    let dark = !env.media_type.print_defaults() && env.color_scheme == ColorScheme::Dark;
    let reduced = env.media_type.print_defaults() || env.reduced_motion;
    let more_contrast = !env.media_type.print_defaults() && env.contrast_more;
    let mut result = true;
    for feat in term.split(" and ") {
        let feat = feat.trim();
        if feat.is_empty() {
            continue;
        }
        if !feat.starts_with('(') {
            // A media type: only the environment's own type (and `all`)
            // applies, so `@media print` is inert on screen and vice versa.
            if media_type_matches(feat, env.media_type) {
                continue;
            }
            result = false;
            continue;
        }
        let inner = feat.trim_start_matches('(').trim_end_matches(')');
        // MQ4 range context first: `(width >= 600px)` carries no colon, so
        // the legacy `(name: value)` split would read the whole thing as a
        // name. An unparseable range (bad operator, unknown feature) stays
        // unknown → false, like any other unrecognized feature.
        if let Some(range) = parse_media_range(inner) {
            let actual = match range.name {
                "width" | "device-width" => Some(w),
                "height" | "device-height" => {
                    // A height of 0 means the caller passed no viewport
                    // box (the width-only entry points) — treat the
                    // block-axis features as unknown rather than guessing.
                    (h > 0.0).then_some(h)
                }
                _ => None,
            };
            result &= actual.is_some_and(|actual| {
                range
                    .lo
                    .is_none_or(|(op, bound)| range_holds(op, actual, bound))
                    && range
                        .hi
                        .is_none_or(|(op, bound)| range_holds(op, actual, bound))
            });
            continue;
        }
        let (name, value) = match inner.split_once(':') {
            Some((n, v)) => (n.trim(), v.trim()),
            None => (inner, ""),
        };
        let px = media_px;
        let value = value.to_ascii_lowercase();
        result &= match name {
            "min-width" => px(&value).map(|x| w >= x).unwrap_or(false),
            "max-width" => px(&value).map(|x| w <= x).unwrap_or(false),
            // Equality queries are approximated as "at least this wide".
            "width" | "device-width" => px(&value).map(|x| w >= x).unwrap_or(false),
            "min-height" => h > 0.0 && px(&value).map(|x| h >= x).unwrap_or(false),
            "max-height" => h > 0.0 && px(&value).map(|x| h <= x).unwrap_or(false),
            "height" | "device-height" => h > 0.0 && px(&value).map(|x| h >= x).unwrap_or(false),
            // MQ4 `orientation`: portrait when the viewport is taller than
            // it is wide. Square viewports are landscape (the spec defers
            // to the block axis, which is how the UAs resolve it).
            "orientation" => {
                if h <= 0.0 {
                    false
                } else {
                    match value.as_str() {
                        "portrait" => h > w,
                        "landscape" => w >= h,
                        _ => false,
                    }
                }
            }
            // Environment features (Q6). A named feature with an
            // unrecognized value makes the term false, per spec.
            "prefers-color-scheme" => match value.as_str() {
                "light" => !dark,
                "dark" => dark,
                _ => false,
            },
            "prefers-reduced-motion" => match value.as_str() {
                "reduce" => reduced,
                "no-preference" => !reduced,
                _ => false,
            },
            "prefers-contrast" => match value.as_str() {
                "more" => more_contrast,
                "no-preference" => !more_contrast,
                _ => false,
            },
            _ => false,
        };
    }
    if negated {
        !result
    } else {
        result
    }
}

/// Does a bare media type (`screen`, `only print`, `all`, …) name the media
/// environment being rendered? `only` is a prefix keyword and never changes
/// the answer; an unknown type (speech, tty, …) never matches.
fn media_type_matches(feat: &str, media: MediaType) -> bool {
    let name = feat.trim();
    let name = name.strip_prefix("only ").unwrap_or(name);
    match name {
        "all" => true,
        "screen" => media == MediaType::Screen,
        "print" => media == MediaType::Print,
        _ => false,
    }
}

/// Substitute `var(--name, fallback)` references in a declaration value using
/// `lookup` for custom-property resolution. Returns `None` when a reference
/// has neither a value nor a fallback, or when references form a cycle
/// (substitution deeper than 8 levels).
pub fn resolve_vars(value: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    resolve_vars_depth(value, lookup, 0)
}

const VAR_OPEN: &[char] = &['v', 'a', 'r', '('];

fn resolve_vars_depth(
    value: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    depth: u32,
) -> Option<String> {
    if depth > 8 {
        return None; // cycle / runaway chain
    }
    if !value.contains("var(") {
        return Some(value.to_string());
    }
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == 'v' && chars[i..].starts_with(VAR_OPEN) {
            // Find the ')' closing this var( — ignoring nested parens so
            // `var(--x, rgb(0, 0, 0))` keeps its fallback whole.
            let mut nested = 0usize;
            let mut close = None;
            for k in (i + 4)..chars.len() {
                match chars[k] {
                    '(' => nested += 1,
                    ')' => {
                        if nested == 0 {
                            close = Some(k);
                            break;
                        }
                        nested -= 1;
                    }
                    _ => {}
                }
            }
            let close = close?;
            let inner: String = chars[i + 4..close].iter().collect();
            let (name, fallback) = match inner.split_once(',') {
                Some((n, f)) => (n.trim().to_string(), Some(f.trim().to_string())),
                None => (inner.trim().to_string(), None),
            };
            let resolved = match lookup(&name) {
                Some(v) => resolve_vars_depth(&v, lookup, depth + 1)?,
                None => {
                    let f = fallback?;
                    resolve_vars_depth(&f, lookup, depth + 1)?
                }
            };
            out.push_str(&resolved);
            i = close + 1;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Parse a declaration list (`color: red; margin: 0`) — used for inline
/// `style="..."` attributes as well as rule bodies. Custom properties
/// (`--name: value`) come through like any declaration.
pub fn parse_declarations(s: &str) -> Vec<Declaration> {
    s.split(';')
        .filter_map(|d| {
            let d = d.trim();
            if d.is_empty() {
                return None;
            }
            let (prop, val) = d.split_once(':')?;
            let property = prop.trim().to_ascii_lowercase();
            let mut value = val.trim().to_string();
            let mut important = false;
            if let Some(pos) = value.to_ascii_lowercase().find("!important") {
                important = true;
                value = value[..pos].trim().to_string();
            }
            if property.is_empty() {
                None
            } else {
                Some(Declaration {
                    property,
                    value,
                    important,
                })
            }
        })
        .collect()
}

/// Parse a color value into 0xRRGGBBAA. Supports `#rgb`, `#rgba`, `#rrggbb`,
/// `#rrggbbaa`, `rgb()/rgba()` (legacy commas or CSS Color 4
/// space-separated with `/ alpha`), `color(srgb …)` /
/// `color(display-p3 …)` (CSS Color 4, converted to sRGB), `transparent`,
/// and the CSS Color 3 named colors.
pub fn parse_color(v: &str) -> Option<u32> {
    let v = v.trim().to_ascii_lowercase();
    if v == "transparent" {
        return Some(0);
    }
    if let Some(hex) = v.strip_prefix('#') {
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let expand = |s: &str| -> Option<u32> { u32::from_str_radix(s, 16).ok() };
        return match hex.len() {
            3 => {
                let r = expand(&hex[0..1])? * 17;
                let g = expand(&hex[1..2])? * 17;
                let b = expand(&hex[2..3])? * 17;
                Some(((r << 24) | (g << 16) | (b << 8)) | 0xFF)
            }
            4 => {
                let r = expand(&hex[0..1])? * 17;
                let g = expand(&hex[1..2])? * 17;
                let b = expand(&hex[2..3])? * 17;
                let a = expand(&hex[3..4])? * 17;
                Some((r << 24) | (g << 16) | (b << 8) | a)
            }
            6 => expand(hex).map(|c| (c << 8) | 0xFF),
            8 => expand(hex),
            _ => None,
        };
    }
    if let Some(inner) = v.strip_prefix("rgba(").and_then(|r| r.strip_suffix(')')) {
        return parse_rgb_channels(inner);
    }
    if let Some(inner) = v.strip_prefix("rgb(").and_then(|r| r.strip_suffix(')')) {
        return parse_rgb_channels(inner);
    }
    // E7: CSS Color 4 `color(<space> c1 c2 c3 [/ a])`. `srgb` stores
    // as-is; `display-p3` converts to sRGB so the u32 pipeline stays
    // authoritative (out-of-gamut clamps; no gamut mapping). Other
    // spaces (a98-rgb, prophoto, rec2020, xyz-*) are unsupported and
    // fail the parse, which drops the declaration — cascade keeps the
    // previous color.
    if let Some(inner) = v.strip_prefix("color(").and_then(|r| r.strip_suffix(')')) {
        return parse_color_function(inner);
    }
    const NAMED: &[(&str, u32)] = &[
        ("black", 0x000000),
        ("silver", 0xC0C0C0),
        ("gray", 0x808080),
        ("grey", 0x808080),
        ("white", 0xFFFFFF),
        ("maroon", 0x800000),
        ("red", 0xFF0000),
        ("purple", 0x800080),
        ("fuchsia", 0xFF00FF),
        ("magenta", 0xFF00FF),
        ("green", 0x008000),
        ("lime", 0x00FF00),
        ("olive", 0x808000),
        ("yellow", 0xFFFF00),
        ("navy", 0x000080),
        ("blue", 0x0000FF),
        ("teal", 0x008080),
        ("aqua", 0x00FFFF),
        ("cyan", 0x00FFFF),
        ("orange", 0xFFA500),
        ("gold", 0xFFD700),
        ("pink", 0xFFC0CB),
        ("brown", 0xA52A2A),
        ("coral", 0xFF7F50),
        ("crimson", 0xDC143C),
        ("tomato", 0xFF6347),
        ("salmon", 0xFA8072),
        ("khaki", 0xF0E68C),
        ("indigo", 0x4B0082),
        ("violet", 0xEE82EE),
        ("plum", 0xDDA0DD),
        ("orchid", 0xDA70D6),
        ("tan", 0xD2B48C),
        ("beige", 0xF5F5DC),
        ("ivory", 0xFFFFF0),
        ("lavender", 0xE6E6FA),
        ("lightgray", 0xD3D3D3),
        ("lightgrey", 0xD3D3D3),
        ("darkgray", 0xA9A9A9),
        ("darkgrey", 0xA9A9A9),
        ("dimgray", 0x696969),
        ("dimgrey", 0x696969),
        ("whitesmoke", 0xF5F5F5),
        ("lightblue", 0xADD8E6),
        ("skyblue", 0x87CEEB),
        ("steelblue", 0x4682B4),
        ("royalblue", 0x4169E1),
        ("navajowhite", 0xFFDEAD),
        ("wheat", 0xF5DEB3),
        ("linen", 0xFAF0E6),
        ("lightyellow", 0xFFFFE0),
        ("lightgreen", 0x90EE90),
        ("palegreen", 0x98FB98),
        ("seagreen", 0x2E8B57),
        ("darkgreen", 0x006400),
        ("olivedrab", 0x6B8E23),
        ("sandybrown", 0xF4A460),
        ("peru", 0xCD853F),
        ("chocolate", 0xD2691E),
        ("sienna", 0xA0522D),
        ("rosybrown", 0xBC8F8F),
        ("burlywood", 0xDEB887),
        ("hotpink", 0xFF69B4),
        ("deeppink", 0xFF1493),
        ("mediumpurple", 0x9370DB),
        ("slateblue", 0x6A5ACD),
        ("slategray", 0x708090),
        ("slategrey", 0x708090),
        ("gainsboro", 0xDCDCDC),
        ("aliceblue", 0xF0F8FF),
        ("ghostwhite", 0xF8F8FF),
        ("mintcream", 0xF5FFFA),
        // CSS Color 3 extended (X11) keywords, values as specified.
        ("antiquewhite", 0xFAEBD7),
        ("aquamarine", 0x7FFFD4),
        ("azure", 0xF0FFFF),
        ("bisque", 0xFFE4C4),
        ("blanchedalmond", 0xFFEBCD),
        ("blueviolet", 0x8A2BE2),
        ("cadetblue", 0x5F9EA0),
        ("chartreuse", 0x7FFF00),
        ("cornflowerblue", 0x6495ED),
        ("cornsilk", 0xFFF8DC),
        ("darkblue", 0x00008B),
        ("darkcyan", 0x008B8B),
        ("darkgoldenrod", 0xB8860B),
        ("darkkhaki", 0xBDB76B),
        ("darkmagenta", 0x8B008B),
        ("darkolivegreen", 0x556B2F),
        ("darkorange", 0xFF8C00),
        ("darkorchid", 0x9932CC),
        ("darkred", 0x8B0000),
        ("darksalmon", 0xE9967A),
        ("darkseagreen", 0x8FBC8F),
        ("darkslateblue", 0x483D8B),
        ("darkslategray", 0x2F4F4F),
        ("darkslategrey", 0x2F4F4F),
        ("darkturquoise", 0x00CED1),
        ("darkviolet", 0x9400D3),
        ("deepskyblue", 0x00BFFF),
        ("dodgerblue", 0x1E90FF),
        ("firebrick", 0xB22222),
        ("floralwhite", 0xFFFAF0),
        ("forestgreen", 0x228B22),
        ("goldenrod", 0xDAA520),
        ("greenyellow", 0xADFF2F),
        ("honeydew", 0xF0FFF0),
        ("indianred", 0xCD5C5C),
        ("lavenderblush", 0xFFF0F5),
        ("lawngreen", 0x7CFC00),
        ("lemonchiffon", 0xFFFACD),
        ("lightcoral", 0xF08080),
        ("lightcyan", 0xE0FFFF),
        ("lightgoldenrodyellow", 0xFAFAD2),
        ("lightpink", 0xFFB6C1),
        ("lightsalmon", 0xFFA07A),
        ("lightseagreen", 0x20B2AA),
        ("lightskyblue", 0x87CEFA),
        ("lightslategray", 0x778899),
        ("lightslategrey", 0x778899),
        ("lightsteelblue", 0xB0C4DE),
        ("limegreen", 0x32CD32),
        ("mediumaquamarine", 0x66CDAA),
        ("mediumblue", 0x0000CD),
        ("mediumorchid", 0xBA55D3),
        ("mediumseagreen", 0x3CB371),
        ("mediumslateblue", 0x7B68EE),
        ("mediumspringgreen", 0x00FA9A),
        ("mediumturquoise", 0x48D1CC),
        ("mediumvioletred", 0xC71585),
        ("midnightblue", 0x191970),
        ("mistyrose", 0xFFE4E1),
        ("moccasin", 0xFFE4B5),
        ("oldlace", 0xFDF5E6),
        ("orangered", 0xFF4500),
        ("palegoldenrod", 0xEEE8AA),
        ("paleturquoise", 0xAFEEEE),
        ("palevioletred", 0xDB7093),
        ("papayawhip", 0xFFEFD5),
        ("peachpuff", 0xFFDAB9),
        ("powderblue", 0xB0E0E6),
        ("saddlebrown", 0x8B4513),
        ("seashell", 0xFFF5EE),
        ("snow", 0xFFFAFA),
        ("springgreen", 0x00FF7F),
        ("thistle", 0xD8BFD8),
        ("turquoise", 0x40E0D0),
        ("yellowgreen", 0x9ACD32),
    ];
    NAMED
        .iter()
        .find(|(name, _)| *name == v)
        .map(|(_, rgb)| (rgb << 8) | 0xFF)
}

fn parse_channel(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        let f: f32 = pct.trim().parse().ok()?;
        return Some((f / 100.0 * 255.0).round().clamp(0.0, 255.0) as u32);
    }
    let f: f32 = s.parse().ok()?;
    Some(f.round().clamp(0.0, 255.0) as u32)
}

/// rgba() alpha: 0.0–1.0 float or a percentage.
fn parse_alpha(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        let f: f32 = pct.trim().parse().ok()?;
        return Some((f / 100.0 * 255.0).round().clamp(0.0, 255.0) as u32);
    }
    let f: f32 = s.parse().ok()?;
    Some((f * 255.0).round().clamp(0.0, 255.0) as u32)
}

/// The body of `rgb()/rgba()` in both grammars: CSS Color 4 space-separated
/// `R G B [/ A]` and the legacy comma form `R, G, B[, A]`. The two
/// spellings never mix in valid CSS, so a body containing a comma splits on
/// commas (a 4th part is the legacy alpha slot) and otherwise splits on
/// whitespace with the alpha as a `/` component. `rgb()` with a slash alpha
/// is the modern spelling of `rgba()`, and `rgba()` with no alpha is the
/// modern spelling of `rgb()` — one parser covers both function names.
fn parse_rgb_channels(inner: &str) -> Option<u32> {
    let (main, slash_alpha) = match inner.split_once('/') {
        Some((m, a)) => (m, Some(a.trim())),
        None => (inner, None),
    };
    let parts: Vec<&str> = if main.contains(',') {
        main.split(',').map(|p| p.trim()).collect()
    } else {
        main.split_whitespace().collect()
    };
    let (r, g, b, alpha) = match (slash_alpha, parts.as_slice()) {
        (Some(a), [r, g, b]) => (*r, *g, *b, Some(a)),
        (Some(_), _) => return None,
        (None, [r, g, b]) => (*r, *g, *b, None),
        (None, [r, g, b, a]) => (*r, *g, *b, Some(*a)),
        (None, _) => return None,
    };
    let r = parse_channel(r)?;
    let g = parse_channel(g)?;
    let b = parse_channel(b)?;
    let a = match alpha {
        Some(s) => parse_alpha(s)?,
        None => 0xFF,
    };
    Some((r << 24) | (g << 16) | (b << 8) | a)
}

/// E7: one `color()` component — a number (0–1 nominal) or a percentage.
fn parse_color_component(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        let f: f32 = pct.trim().parse().ok()?;
        return Some(f / 100.0);
    }
    s.parse::<f32>().ok()
}

/// E7: the body of `color(<space> c1 c2 c3 [/ a])` — space name, then
/// space-separated components with an optional `/ alpha` suffix.
fn parse_color_function(inner: &str) -> Option<u32> {
    let inner = inner.trim();
    let (space, rest) = inner.split_once(char::is_whitespace)?;
    let comps_part = rest.split_once('/').map_or(rest, |(c, _)| c);
    let alpha_part = rest.split_once('/').map(|(_, a)| a.trim());
    let comps: Vec<&str> = comps_part.split_whitespace().collect();
    if comps.len() != 3 {
        return None;
    }
    let mut rgb = [0.0f32; 3];
    for (i, c) in comps.iter().enumerate() {
        rgb[i] = parse_color_component(c)?;
    }
    // Both supported spaces use the sRGB transfer curve, so decode to
    // linear, move primaries where needed, and re-encode. `srgb-linear`
    // skips both gamma steps.
    let linear = match space {
        "srgb" => rgb.map(srgb_decode),
        "srgb-linear" => rgb,
        "display-p3" => mul3(p3_to_srgb_matrix(), &rgb.map(srgb_decode)),
        _ => return None,
    };
    let out = linear.map(srgb_encode);
    let alpha = match alpha_part {
        Some(a) => parse_alpha(a)?,
        None => 0xFF,
    };
    let r = (out[0] * 255.0).round().clamp(0.0, 255.0) as u32;
    let g = (out[1] * 255.0).round().clamp(0.0, 255.0) as u32;
    let b = (out[2] * 255.0).round().clamp(0.0, 255.0) as u32;
    Some((r << 24) | (g << 16) | (b << 8) | alpha)
}

/// E7: sRGB electro-optical transfer (piecewise, CSS Color 4 §8.2).
/// Input clamps to [0, 1] — negative components (wide-gamut spill into
/// this space) clamp rather than NaN through `powf`.
fn srgb_decode(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// E7: inverse sRGB transfer.
fn srgb_encode(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// E7: linear sRGB → linear Display-P3 primaries matrix (CSS Color 4 §9).
const SRGB_TO_P3: [[f32; 3]; 3] = [
    [0.822_462_1, 0.177_538_0, 0.0],
    [0.033_194_1, 0.966_805_8, 0.0],
    [0.017_082_7, 0.072_397_4, 0.910_519_9],
];

fn p3_to_srgb_matrix() -> &'static [[f32; 3]; 3] {
    static INV: std::sync::OnceLock<[[f32; 3]; 3]> = std::sync::OnceLock::new();
    INV.get_or_init(|| invert3(&SRGB_TO_P3).expect("primaries matrix invertible"))
}

/// E7: adjugate-method 3×3 inverse (one-shot at startup for the P3
/// matrix; no linear-algebra dependency).
fn invert3(m: &[[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-12 {
        return None;
    }
    let i = 1.0 / det;
    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * i,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * i,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * i,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * i,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * i,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * i,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * i,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * i,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * i,
        ],
    ])
}

fn mul3(m: &[[f32; 3]; 3], v: &[f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// A CSS length value, unresolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Px(f32),
    Em(f32),
    Rem(f32),
    /// Relative to a basis width supplied by the caller.
    Percent(f32),
    /// R5: `n vw` — n% of the viewport width (folded at style time).
    Vw(f32),
    /// R5: `n vh` — n% of the viewport height.
    Vh(f32),
    /// R5: `n vmin` — n% of the smaller viewport dimension.
    Vmin(f32),
    /// R5: `n vmax` — n% of the larger viewport dimension.
    Vmax(f32),
    /// B6: `n ch` — n times the advance of "0" in the current font.
    /// Resolved at style time against the element's font size via
    /// `parse_len_px_ch` (the measurer provides the 0-advance).
    Ch(f32),
    /// B6: `n ex` — n times the x-height of the current font. Falls
    /// back to 0.5em when the measurer doesn't know the x-height.
    Ex(f32),
}

impl Default for Length {
    fn default() -> Self {
        Length::Px(0.0)
    }
}

impl Length {
    /// Resolve to pixels. `em` is the element's computed font size, `rem` the
    /// root font size, `basis` the width percentages resolve against.
    /// Viewport units resolve against a 1280×800 default here — callers that
    /// know the real viewport fold them earlier via `parse_len_px_v` (R5).
    /// B6: ch/ex need the current font's metrics, which `resolve` doesn't
    /// have — use [`Length::resolve_font`] (or the parser-time helpers)
    /// for those. Here we fall back to 0.5em for ex and the em size for
    /// ch when no metrics are available, which keeps Length usable in
    /// contexts that don't pass the measurer.
    pub fn resolve(&self, em: f32, rem: f32, basis: f32) -> f32 {
        match self {
            Length::Px(v) => *v,
            Length::Em(v) => *v * em,
            Length::Rem(v) => *v * rem,
            Length::Percent(v) => v / 100.0 * basis,
            Length::Vw(v) => v / 100.0 * 1280.0,
            Length::Vh(v) => v / 100.0 * 800.0,
            Length::Vmin(v) => v / 100.0 * 800.0,
            Length::Vmax(v) => v / 100.0 * 1280.0,
            Length::Ch(v) => v * em * 0.5,
            Length::Ex(v) => v * em * 0.5,
        }
    }

    /// B6: resolve a length when the current font's `ch`/`ex` widths
    /// (in pixels) are known. Falls back to 0.5em if `ex_px` is 0 and
    /// to the em size if `ch_px` is 0.
    pub fn resolve_font(&self, em: f32, rem: f32, basis: f32, ch_px: f32, ex_px: f32) -> f32 {
        match self {
            Length::Ch(v) => v * (if ch_px > 0.0 { ch_px } else { em * 0.5 }),
            Length::Ex(v) => v * (if ex_px > 0.0 { ex_px } else { em * 0.5 }),
            _ => self.resolve(em, rem, basis),
        }
    }
}

/// Parse a length (`12px`, `1.5em`, `2rem`, `10%`, `3pt`, `0`).
pub fn parse_length(v: &str) -> Option<Length> {
    let v = v.trim().to_ascii_lowercase();
    let (num, unit) = v.split_at(
        v.find(|c: char| c.is_ascii_alphabetic() || c == '%')
            .unwrap_or(v.len()),
    );
    let num = num.trim();
    // `0` is valid without a unit for any length.
    if num.is_empty() {
        return None;
    }
    match unit {
        "px" => num.parse::<f32>().ok().map(Length::Px),
        "rem" => num.parse::<f32>().ok().map(Length::Rem),
        "em" => num.parse::<f32>().ok().map(Length::Em),
        "pt" => num.parse::<f32>().ok().map(|n| Length::Px(n * 4.0 / 3.0)),
        "%" => num.parse::<f32>().ok().map(Length::Percent),
        // R5: viewport units — parsed here, folded at style time against
        // the viewport (parse_len_px_v).
        "vw" => num.parse::<f32>().ok().map(Length::Vw),
        "vh" => num.parse::<f32>().ok().map(Length::Vh),
        "vmin" => num.parse::<f32>().ok().map(Length::Vmin),
        "vmax" => num.parse::<f32>().ok().map(Length::Vmax),
        // B6: font-relative units resolved against the current font's
        // metrics. ch/ex are folded at style time via
        // `parse_len_px_ch` (the measurer provides the 0-advance and
        // x-height).
        "ch" => num.parse::<f32>().ok().map(Length::Ch),
        "ex" => num.parse::<f32>().ok().map(Length::Ex),
        "" if num.parse::<f32>() == Ok(0.0) => Some(Length::Px(0.0)),
        _ => None,
    }
}

/// Parse a bare number (line-height multipliers, opacities, …).
pub fn parse_number(v: &str) -> Option<f32> {
    v.trim().parse::<f32>().ok()
}

/// One term of a [`Len`] expression: a px amount (px/pt/em/rem folded at
/// style time) plus a percentage that stays symbolic until layout supplies
/// the containing-block width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LenTerm {
    pub px: f32,
    pub pct: f32,
}

impl LenTerm {
    fn resolve(&self, basis: f32) -> f32 {
        self.px + self.pct / 100.0 * basis
    }
}

/// A length that may be a `calc()`/`min()`/`max()`/`clamp()` expression.
/// Resolution is two-stage: em/rem fold to px at parse time, percentages
/// resolve at layout against the basis passed to [`Len::resolve`] (Q1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Len {
    Px(f32),
    /// A flattened calc() sum (a plain length folds here too).
    Sum {
        px: f32,
        pct: f32,
    },
    /// `min(a, b[, c])` over the first `n` terms.
    Min([LenTerm; 3], u8),
    /// `max(a, b[, c])` over the first `n` terms.
    Max([LenTerm; 3], u8),
    /// `clamp(min, val, max)`.
    Clamp([LenTerm; 3]),
}

impl Len {
    pub const ZERO: Len = Len::Px(0.0);

    pub const fn const_px(v: f32) -> Len {
        Len::Px(v)
    }

    /// Pixels given a layout basis (the containing-block width) for the
    /// percentage parts.
    pub fn resolve(&self, basis: f32) -> f32 {
        match self {
            Len::Px(v) => *v,
            Len::Sum { px, pct } => px + pct / 100.0 * basis,
            Len::Min(ts, n) => {
                let mut r = f32::INFINITY;
                for t in ts.iter().take(*n as usize) {
                    r = r.min(t.resolve(basis));
                }
                r
            }
            Len::Max(ts, n) => {
                let mut r = f32::NEG_INFINITY;
                for t in ts.iter().take(*n as usize) {
                    r = r.max(t.resolve(basis));
                }
                r
            }
            // CSS: clamp(MIN, VAL, MAX) = max(MIN, min(VAL, MAX)).
            Len::Clamp(ts) => ts[1]
                .resolve(basis)
                .min(ts[2].resolve(basis))
                .max(ts[0].resolve(basis)),
        }
    }

    /// True when any percentage part survives — the caller must supply a
    /// layout basis or treat the value as unset.
    pub fn has_pct(&self) -> bool {
        match self {
            Len::Px(_) => false,
            Len::Sum { pct, .. } => *pct != 0.0,
            Len::Min(ts, _) | Len::Max(ts, _) | Len::Clamp(ts) => ts.iter().any(|t| t.pct != 0.0),
        }
    }

    /// The value as plain pixels when nothing depends on a layout basis.
    pub fn px(&self) -> Option<f32> {
        if self.has_pct() {
            None
        } else {
            Some(self.resolve(0.0))
        }
    }

    /// Fold to a single term, or `None` for min/max/clamp results (they
    /// can't take part in further arithmetic without being evaluated).
    fn as_terms(&self) -> Option<LenTerm> {
        match self {
            Len::Px(v) => Some(LenTerm { px: *v, pct: 0.0 }),
            Len::Sum { px, pct } => Some(LenTerm { px: *px, pct: *pct }),
            Len::Min(..) | Len::Max(..) | Len::Clamp(..) => None,
        }
    }
}

fn term_to_len(t: LenTerm) -> Len {
    if t.pct == 0.0 {
        Len::Px(t.px)
    } else {
        Len::Sum {
            px: t.px,
            pct: t.pct,
        }
    }
}

/// A single length/percentage token, em/rem folded to px against `em`/`rem`,
/// viewport units against `vw`/`vh` (R5), font-relative units against
/// `ch_px` / `ex_px` (B6).
fn parse_len_term(
    v: &str,
    em: f32,
    rem: f32,
    vw: f32,
    vh: f32,
    ch_px: f32,
    ex_px: f32,
) -> Option<LenTerm> {
    match parse_length(v) {
        Some(Length::Px(x)) => Some(LenTerm { px: x, pct: 0.0 }),
        Some(Length::Em(x)) => Some(LenTerm {
            px: x * em,
            pct: 0.0,
        }),
        Some(Length::Rem(x)) => Some(LenTerm {
            px: x * rem,
            pct: 0.0,
        }),
        Some(Length::Percent(p)) => Some(LenTerm { px: 0.0, pct: p }),
        Some(Length::Vw(p)) if vw > 0.0 => Some(LenTerm {
            px: p / 100.0 * vw,
            pct: 0.0,
        }),
        Some(Length::Vh(p)) if vh > 0.0 => Some(LenTerm {
            px: p / 100.0 * vh,
            pct: 0.0,
        }),
        Some(Length::Vmin(p)) if vw > 0.0 && vh > 0.0 => Some(LenTerm {
            px: p / 100.0 * vw.min(vh),
            pct: 0.0,
        }),
        Some(Length::Vmax(p)) if vw > 0.0 && vh > 0.0 => Some(LenTerm {
            px: p / 100.0 * vw.max(vh),
            pct: 0.0,
        }),
        // B6: ch/ex fold against the current font's metrics. The
        // measurer (or a fallback) supplies the px equivalents; if it
        // doesn't know, ch/ex reject to None (callers fall back to em
        // via the engine).
        Some(Length::Ch(x)) if ch_px > 0.0 => Some(LenTerm {
            px: x * ch_px,
            pct: 0.0,
        }),
        Some(Length::Ex(x)) if ex_px > 0.0 => Some(LenTerm {
            px: x * ex_px,
            pct: 0.0,
        }),
        // Viewport / ch / ex units without a known basis reject
        // rather than fold to zero.
        _ => None,
    }
}

/// C1: a `width`/`height` value — a resolved length or an intrinsic
/// keyword. The keywords can't fold into [`Len`] (it is pure numeric);
/// layout measures the subtree and passes the intrinsic widths to
/// [`SizeVal::resolve`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeVal {
    Len(Len),
    MinContent,
    MaxContent,
    /// `fit-content` keyword (`None`) or `fit-content(<len>)` (`Some`).
    FitContent(Option<Len>),
}

impl SizeVal {
    /// Resolve against the available width `avail` and the subtree's
    /// measured intrinsic widths. CSS: `fit-content` = clamp(min-content,
    /// available, max-content); `fit-content(<len>)` replaces `available`
    /// with the argument.
    pub fn resolve(&self, avail: f32, min_c: f32, max_c: f32) -> f32 {
        match self {
            SizeVal::Len(l) => l.resolve(avail),
            SizeVal::MinContent => min_c,
            SizeVal::MaxContent => max_c,
            SizeVal::FitContent(arg) => {
                let cap = arg.map(|a| a.resolve(avail)).unwrap_or(avail);
                cap.min(max_c).max(min_c)
            }
        }
    }

    /// The value if it is a plain length — `None` for the intrinsic
    /// keywords. For read sites that have no way to measure a subtree
    /// (chrome widgets, replaced elements, the rotated-box scratch pass),
    /// where a keyword falls back to the same treatment as `auto`.
    pub fn len(&self) -> Option<Len> {
        match self {
            SizeVal::Len(l) => Some(*l),
            _ => None,
        }
    }
}

/// C1: parse a `width`/`height` value — intrinsic keywords first, then the
/// full length grammar of [`parse_len_px_ch`]. `None` = not a size value
/// (the caller treats it as auto/unset, matching today's behavior).
pub fn parse_size_val_ch(
    v: &str,
    em: f32,
    rem: f32,
    vw: f32,
    vh: f32,
    ch_px: f32,
    ex_px: f32,
) -> Option<SizeVal> {
    let lower = v.trim().to_ascii_lowercase();
    if lower == "min-content" {
        return Some(SizeVal::MinContent);
    }
    if lower == "max-content" {
        return Some(SizeVal::MaxContent);
    }
    if lower == "fit-content" {
        return Some(SizeVal::FitContent(None));
    }
    if let Some(body) = fn_arg(&lower, "fit-content") {
        let arg = parse_len_px_ch(body, em, rem, vw, vh, ch_px, ex_px);
        return Some(SizeVal::FitContent(arg));
    }
    parse_len_px_ch(v, em, rem, vw, vh, ch_px, ex_px).map(SizeVal::Len)
}

/// [`parse_size_val_ch`] without `ch`/`ex` support — the keyword grammar is
/// identical, and plain lengths fold through [`parse_len_px_v`] (mirroring
/// how the length-only readers pair `parse_len_px_ch` with `parse_len_px_v`).
pub fn parse_size_val_v(v: &str, em: f32, rem: f32, vw: f32, vh: f32) -> Option<SizeVal> {
    let lower = v.trim().to_ascii_lowercase();
    if lower == "min-content" {
        return Some(SizeVal::MinContent);
    }
    if lower == "max-content" {
        return Some(SizeVal::MaxContent);
    }
    if lower == "fit-content" {
        return Some(SizeVal::FitContent(None));
    }
    if let Some(body) = fn_arg(&lower, "fit-content") {
        let arg = parse_len_px_v(body, em, rem, vw, vh);
        return Some(SizeVal::FitContent(arg));
    }
    parse_len_px_v(v, em, rem, vw, vh).map(SizeVal::Len)
}

/// Parse a length that may be a `calc()`/`min()`/`max()`/`clamp()`
/// expression (`calc(100% - 24px)`, `min(200px, 100%)`, …). calc() supports
/// top-level `+`/`-` with whitespace (CSS rule) and flattens nested calc();
/// multiplication/division and nested min/max/clamp inside arithmetic
/// reject to `None`. `var()` is substituted before this runs, so
/// `calc(100% - var(--pad))` works.
pub fn parse_len_px(v: &str, em: f32, rem: f32) -> Option<Len> {
    parse_len_px_v(v, em, rem, 0.0, 0.0)
}

/// [`parse_len_px`] with a known viewport: `vw`/`vh`/`vmin`/`vmax` fold at
/// style time (R5). The ch/ex factors are 0 here — callers that care
/// about font-relative units use [`parse_len_px_ch`].
pub fn parse_len_px_v(v: &str, em: f32, rem: f32, vw: f32, vh: f32) -> Option<Len> {
    parse_len_px_ch(v, em, rem, vw, vh, 0.0, 0.0)
}

/// B6: [`parse_len_px_v`] with the current font's `ch`/`ex` widths in
/// pixels. ch is the advance of "0" in the element's font; ex is the
/// x-height (0.5em fallback when the measurer doesn't say otherwise).
/// The values flow from the style engine, which queries the measurer
/// once per text-bearing box.
pub fn parse_len_px_ch(
    v: &str,
    em: f32,
    rem: f32,
    vw: f32,
    vh: f32,
    ch_px: f32,
    ex_px: f32,
) -> Option<Len> {
    let v = v.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    if let Some(body) = fn_arg(&v, "calc") {
        return parse_calc_sum(body, em, rem, vw, vh, ch_px, ex_px);
    }
    if let Some(body) = fn_arg(&v, "min") {
        return parse_min_max(body, em, rem, false, vw, vh, ch_px, ex_px);
    }
    if let Some(body) = fn_arg(&v, "max") {
        return parse_min_max(body, em, rem, true, vw, vh, ch_px, ex_px);
    }
    if let Some(body) = fn_arg(&v, "clamp") {
        let args = split_top_commas(body);
        if args.len() != 3 {
            return None;
        }
        let mut ts = [LenTerm { px: 0.0, pct: 0.0 }; 3];
        for (t, a) in ts.iter_mut().zip(args.iter()) {
            *t = parse_len_operand(a, em, rem, vw, vh, ch_px, ex_px)?.as_terms()?;
        }
        return Some(Len::Clamp(ts));
    }
    parse_len_term(&v, em, rem, vw, vh, ch_px, ex_px).map(term_to_len)
}

/// If `v` is exactly `name(…)`, the argument text between the parens.
fn fn_arg<'a>(v: &'a str, name: &str) -> Option<&'a str> {
    let rest = v.strip_prefix(name)?;
    let rest = rest.strip_prefix('(')?;
    if !rest.ends_with(')') {
        return None;
    }
    Some(&rest[..rest.len() - 1])
}

/// One operand inside a calc()/min()/max()/clamp(): a plain length or any
/// nested function (min/max/clamp results reject via `as_terms` when the
/// caller needs arithmetic).
fn parse_len_operand(
    v: &str,
    em: f32,
    rem: f32,
    vw: f32,
    vh: f32,
    ch_px: f32,
    ex_px: f32,
) -> Option<Len> {
    let v = v.trim();
    if let Some(body) = fn_arg(v, "calc") {
        return parse_calc_sum(body, em, rem, vw, vh, ch_px, ex_px);
    }
    if let Some(body) = fn_arg(v, "min") {
        return parse_min_max(body, em, rem, false, vw, vh, ch_px, ex_px);
    }
    if let Some(body) = fn_arg(v, "max") {
        return parse_min_max(body, em, rem, true, vw, vh, ch_px, ex_px);
    }
    parse_len_term(v, em, rem, vw, vh, ch_px, ex_px).map(term_to_len)
}

fn parse_min_max(
    body: &str,
    em: f32,
    rem: f32,
    is_max: bool,
    vw: f32,
    vh: f32,
    ch_px: f32,
    ex_px: f32,
) -> Option<Len> {
    let args = split_top_commas(body);
    if args.len() < 2 || args.len() > 3 {
        return None;
    }
    let mut ts = [LenTerm { px: 0.0, pct: 0.0 }; 3];
    for (t, a) in ts.iter_mut().zip(args.iter()) {
        *t = parse_len_operand(a, em, rem, vw, vh, ch_px, ex_px)?.as_terms()?;
    }
    Some(if is_max {
        Len::Max(ts, args.len() as u8)
    } else {
        Len::Min(ts, args.len() as u8)
    })
}

/// Evaluate a calc() body: top-level addition/subtraction. A lone operand
/// may be any function; with two or more, every operand must fold to a
/// px/pct term (nested calc flattens, nested min/max/clamp reject).
fn parse_calc_sum(
    body: &str,
    em: f32,
    rem: f32,
    vw: f32,
    vh: f32,
    ch_px: f32,
    ex_px: f32,
) -> Option<Len> {
    let parts = split_calc_sum(body)?;
    if parts.len() == 1 {
        return parse_len_operand(parts[0].1, em, rem, vw, vh, ch_px, ex_px);
    }
    let mut px = 0.0f32;
    let mut pct = 0.0f32;
    for (sign, part) in parts {
        let t = parse_len_operand(part, em, rem, vw, vh, ch_px, ex_px)?.as_terms()?;
        px += sign * t.px;
        pct += sign * t.pct;
    }
    Some(term_to_len(LenTerm { px, pct }))
}

/// Split a calc() body into top-level `+`/`-` parts with their signs. CSS
/// requires whitespace around the binary operators, so a sign glued to its
/// number (`-4px`) is unary and stays inside its part.
fn split_calc_sum(s: &str) -> Option<Vec<(f32, &str)>> {
    let b = s.as_bytes();
    let mut parts: Vec<(f32, &str)> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut sign = 1.0f32;
    for i in 0..b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'+' | b'-'
                if depth == 0
                    && i > 0
                    && b[i - 1].is_ascii_whitespace()
                    && i + 1 < b.len()
                    && b[i + 1].is_ascii_whitespace() =>
            {
                let part = s[start..i].trim();
                if part.is_empty() {
                    return None;
                }
                parts.push((sign, part));
                sign = if b[i] == b'-' { -1.0 } else { 1.0 };
                start = i + 1;
            }
            _ => {}
        }
    }
    let part = s[start..].trim();
    if part.is_empty() {
        return None;
    }
    parts.push((sign, part));
    Some(parts)
}

/// Split on commas not nested inside parentheses.
fn split_top_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_len_plain_values() {
        assert_eq!(parse_len_px("12px", 16.0, 16.0), Some(Len::Px(12.0)));
        assert_eq!(parse_len_px("1.5em", 16.0, 16.0), Some(Len::Px(24.0)));
        assert_eq!(parse_len_px("2rem", 16.0, 16.0), Some(Len::Px(32.0)));
        assert_eq!(
            parse_len_px("10pt", 16.0, 16.0),
            Some(Len::Px(10.0 * 4.0 / 3.0))
        );
        assert_eq!(
            parse_len_px("10%", 16.0, 16.0),
            Some(Len::Sum { px: 0.0, pct: 10.0 })
        );
        assert_eq!(parse_len_px("0", 16.0, 16.0), Some(Len::Px(0.0)));
        assert_eq!(parse_len_px("auto", 16.0, 16.0), None);
        assert_eq!(parse_len_px("", 16.0, 16.0), None);
    }

    #[test]
    fn parse_len_calc_sums() {
        let l = parse_len_px("calc(100% - 24px)", 16.0, 16.0).unwrap();
        assert_eq!(
            l,
            Len::Sum {
                px: -24.0,
                pct: 100.0
            }
        );
        assert_eq!(l.resolve(200.0), 176.0);
        assert!(l.has_pct());
        assert_eq!(l.px(), None);
        // em folds at parse time.
        let l = parse_len_px("calc(10px + 2em)", 16.0, 16.0).unwrap();
        assert_eq!(l, Len::Px(42.0));
        // Unary minus glued to its number.
        let l = parse_len_px("calc(-8px + 100%)", 16.0, 16.0).unwrap();
        assert_eq!(
            l,
            Len::Sum {
                px: -8.0,
                pct: 100.0
            }
        );
        // Nested calc flattens.
        let l = parse_len_px("calc(calc(100% - 4px) + 8px)", 16.0, 16.0).unwrap();
        assert_eq!(
            l,
            Len::Sum {
                px: 4.0,
                pct: 100.0
            }
        );
        // No whitespace around the operator → invalid per CSS.
        assert_eq!(parse_len_px("calc(100%-24px)", 16.0, 16.0), None);
        // Multiplication is out of scope → reject.
        assert_eq!(parse_len_px("calc(10px * 2)", 16.0, 16.0), None);
    }

    #[test]
    fn parse_len_min_max_clamp() {
        let l = parse_len_px("min(50%, 300px)", 16.0, 16.0).unwrap();
        assert_eq!(l.resolve(400.0), 200.0);
        assert_eq!(l.resolve(700.0), 300.0);
        let l = parse_len_px("max(2em, 10px)", 16.0, 16.0).unwrap();
        assert!(matches!(l, Len::Max(_, 2)));
        assert_eq!(l.resolve(0.0), 32.0);
        let l = parse_len_px("clamp(24px, 50%, 120px)", 16.0, 16.0).unwrap();
        assert_eq!(l.resolve(100.0), 50.0);
        assert_eq!(l.resolve(400.0), 120.0);
        assert_eq!(l.resolve(20.0), 24.0);
        // Needs at least two args (min/max), exactly three (clamp).
        assert_eq!(parse_len_px("min(10px)", 16.0, 16.0), None);
        assert_eq!(parse_len_px("clamp(1px, 2px)", 16.0, 16.0), None);
        // Nested calc inside min() flattens.
        let l = parse_len_px("min(calc(100% - 10px), 50px)", 16.0, 16.0).unwrap();
        assert_eq!(l.resolve(200.0), 50.0);
        // Percent survives symbolically.
        let l = parse_len_px("min(50%, 300px)", 16.0, 16.0).unwrap();
        assert!(l.has_pct());
    }

    #[test]
    fn parses_rules_and_declarations() {
        let sheet = parse_stylesheet(
            "/* c */ body { color: #333; margin: 0 }\np.post, h1.title { font-weight: bold !important; }",
        );
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(sheet.rules[0].selectors.len(), 1);
        assert_eq!(sheet.rules[0].declarations.len(), 2);
        assert_eq!(sheet.rules[0].declarations[0].value, "#333");
        assert_eq!(sheet.rules[1].selectors.len(), 2);
        assert!(sheet.rules[1].declarations[0].important);
    }

    #[test]
    fn parses_selector_parts() {
        let sheet = parse_stylesheet("div.post #main a:first-child { color: red }");
        let sel = &sheet.rules[0].selectors[0];
        assert_eq!(sel.parts.len(), 3);
        assert_eq!(
            sel.combs,
            vec![Combinator::Descendant, Combinator::Descendant]
        );
        assert_eq!(sel.parts[0].tag.as_deref(), Some("div"));
        assert_eq!(sel.parts[0].classes, vec!["post"]);
        assert_eq!(sel.parts[1].id.as_deref(), Some("main"));
        assert_eq!(sel.parts[2].tag.as_deref(), Some("a"));
        assert_eq!(sel.parts[2].pseudos, vec![Pseudo::FirstChild]);
    }

    #[test]
    fn skips_at_rules_with_blocks() {
        let sheet = parse_stylesheet(
            "@media (max-width: 600px) { p { color: blue } }\n@import url(x.css);\np { color: red }",
        );
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].declarations[0].value, "red");
    }

    #[test]
    fn font_face_blocks_are_kept() {
        let sheet = parse_stylesheet(
            "@font-face { font-family: \"Test Face\"; \
             src: url(test.woff2) format(\"woff2\"), url(test.ttf) format('truetype'); }\
             p { color: red }",
        );
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.font_faces.len(), 1);
        let face = &sheet.font_faces[0];
        assert_eq!(face.family, "Test Face");
        assert_eq!(face.srcs.len(), 2);
        assert_eq!(face.srcs[0].url, "test.woff2");
        assert_eq!(face.srcs[0].format.as_deref(), Some("woff2"));
        assert_eq!(face.srcs[1].format.as_deref(), Some("truetype"));
        // No family or no sources → dropped.
        assert!(parse_stylesheet("@font-face { font-family: X; }")
            .font_faces
            .is_empty());
        assert!(parse_stylesheet("@font-face { src: url(x.ttf); }")
            .font_faces
            .is_empty());
        // Inside matching @media the face still registers.
        let sheet = parse_stylesheet_viewport(
            "@media (min-width: 100px) { @font-face { font-family: Y; src: url(y.ttf); } }",
            800.0,
        );
        assert_eq!(sheet.font_faces.len(), 1);
    }

    #[test]
    fn b11_unicode_range_and_display_parsing() {
        let sheet = parse_stylesheet(
            "@font-face { font-family: Ranged; src: url(r.ttf); \
             unicode-range: U+41, U+400-4FF, U+1F600-1F64F; font-display: swap; }",
        );
        let face = &sheet.font_faces[0];
        assert_eq!(
            face.unicode_ranges,
            vec![(0x41, 0x41), (0x400, 0x4FF), (0x1F600, 0x1F64F)]
        );
        assert_eq!(face.font_display, FontDisplay::Swap);
        assert!(face.covers('A'));
        assert!(!face.covers('a'));
        assert!(face.covers('\u{410}')); // Cyrillic А ∈ 400-4FF
                                         // Wildcard form expands; defaults are unrestricted / Auto.
        let face2 =
            parse_font_face("font-family: W; src: url(w.ttf); unicode-range: U+4??").unwrap();
        assert_eq!(face2.unicode_ranges, vec![(0x400, 0x4FF)]);
        assert_eq!(face2.font_display, FontDisplay::Auto);
        let plain = parse_font_face("font-family: P; src: url(p.ttf)").unwrap();
        assert!(plain.unicode_ranges.is_empty());
        assert!(plain.covers('☃'));
        assert!(plain.intersects_text("anything"));
        // Malformed descriptors are invalid declarations and are ignored.
        let bad = parse_font_face(
            "font-family: B; src: url(b.ttf); unicode-range: 41; font-display: sometimes",
        )
        .unwrap();
        assert!(bad.unicode_ranges.is_empty());
        assert_eq!(bad.font_display, FontDisplay::Auto);
        // Above the last code point → whole declaration dropped.
        assert_eq!(
            parse_font_face("font-family: N; src: url(n.ttf); unicode-range: U+FFFFFF")
                .unwrap()
                .unicode_ranges,
            vec![]
        );
        assert_eq!(
            parse_font_face("font-family: O; src: url(o.ttf); font-display: optional")
                .unwrap()
                .font_display,
            FontDisplay::Optional
        );
    }

    #[test]
    fn b11_font_weight_and_style_descriptors() {
        // Two weights of one family are two faces, kept in source order.
        let sheet = parse_stylesheet(
            "@font-face { font-family: Duplex; src: url(reg.ttf); font-weight: 400; }\
             @font-face { font-family: Duplex; src: url(bold.ttf); font-weight: 700; }",
        );
        assert_eq!(sheet.font_faces.len(), 2);
        assert_eq!(sheet.font_faces[0].font_weight, 400);
        assert_eq!(sheet.font_faces[1].font_weight, 700);
        assert_ne!(
            sheet.font_faces[0].descriptor_key(),
            sheet.font_faces[1].descriptor_key()
        );
        // Keywords, numbers and the slant keyword all parse.
        let face = &parse_stylesheet(
            "@font-face { font-family: K; src: url(k.ttf); font-weight: bold; font-style: oblique 12deg; }",
        )
        .font_faces[0];
        assert_eq!(face.font_weight, 700);
        assert_eq!(face.font_style, FontStyle::Oblique);
        assert_eq!(face.font_style.keyword(), "oblique");
        // Absent descriptors are `normal` 400 per css-fonts-4, and
        // `Default` agrees so `..Default::default()` faces are normal.
        let plain = parse_font_face("font-family: P; src: url(p.ttf)").unwrap();
        assert_eq!(plain.font_weight, 400);
        assert_eq!(plain.font_style, FontStyle::Normal);
        assert_eq!(FontFace::default().font_weight, 400);
        assert_eq!(FontFace::default().font_style, FontStyle::Normal);
        // Same descriptors, different src → the same face (one download).
        let dup = parse_font_face("font-family: P; src: url(other.ttf); font-weight: 400");
        assert_eq!(dup.unwrap().descriptor_key(), plain.descriptor_key());
        // Italic + a numeric weight outside 1–1000.
        let ital = parse_font_face(
            "font-family: I; src: url(i.ttf); font-weight: 1000; font-style: italic",
        )
        .unwrap();
        assert_eq!(ital.font_weight, 1000);
        assert_eq!(ital.font_style, FontStyle::Italic);
        // Malformed descriptors are invalid declarations and keep the
        // defaults (`bolder`/`lighter` have no meaning without a parent).
        let bad = parse_font_face(
            "font-family: B; src: url(b.ttf); font-weight: heavier; font-style: sideways",
        )
        .unwrap();
        assert_eq!(bad.font_weight, 400);
        assert_eq!(bad.font_style, FontStyle::Normal);
        let zero = parse_font_face("font-family: Z; src: url(z.ttf); font-weight: 0").unwrap();
        assert_eq!(zero.font_weight, 400);
        // Within-family matching: exact descriptors first, then the
        // closest weight, then a compatible slant.
        let bold = &sheet.font_faces[1];
        assert_eq!(bold.match_rank(700, FontStyle::Normal), Some((0, 0)));
        assert_eq!(
            sheet.font_faces[0].match_rank(700, FontStyle::Normal),
            Some((0, 300))
        );
        assert_eq!(bold.match_rank(400, FontStyle::Normal), Some((0, 300)));
        let ital_face = parse_font_face(
            "font-family: S; src: url(s.ttf); font-weight: 400; font-style: italic",
        )
        .unwrap();
        assert_eq!(ital_face.match_rank(400, FontStyle::Normal), None);
        assert_eq!(ital_face.match_rank(400, FontStyle::Italic), Some((0, 0)));
        assert_eq!(
            ital_face.match_rank(400, FontStyle::Oblique).map(|r| r.0),
            Some(1),
            "an oblique request takes the italic face"
        );
        assert_eq!(
            bold.match_rank(400, FontStyle::Oblique).map(|r| r.0),
            Some(2),
            "oblique request falls back to the upright face, last resort"
        );
    }

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color("#fff"), Some(0xFFFFFFFF));
        assert_eq!(parse_color("#333"), Some(0x333333FF));
        assert_eq!(parse_color("#ff000080"), Some(0xFF000080));
        assert_eq!(parse_color("rgb(0, 128, 255)"), Some(0x0080FFFF));
        assert_eq!(parse_color("rgba(255,0,0,0.5)"), Some(0xFF000080));
        assert_eq!(parse_color("transparent"), Some(0));
        assert_eq!(parse_color("tomato"), Some(0xFF6347FF));
        assert_eq!(parse_color("none"), None);
        assert_eq!(parse_color("NONE"), None);
        assert_eq!(parse_color("currentColor"), None);
        assert_eq!(parse_color("CURRENTCOLOR"), None);
        assert_eq!(parse_color("bogus"), None);
    }

    #[test]
    fn parses_color_function_e7() {
        // `srgb` uses the same transfer curve, so components round-trip
        // through decode/encode (tolerance ±1 from u32 rounding).
        let c = parse_color("color(srgb 1 0.5 0)").expect("srgb parses");
        assert!((c >> 24) & 0xFF ^ 0xFF <= 1);
        assert!((c >> 16) & 0xFF ^ 0x80 <= 1);
        assert_eq!(c & 0xFF, 0xFF);
        // display-p3 (0.3, 0.6, 0.3) → sRGB ≈ (36, 155, 66) (verified
        // against the published P3→sRGB matrix: decode → matrix → encode).
        let c = parse_color("color(display-p3 0.3 0.6 0.3)").expect("p3 parses");
        let (r, g, b) = ((c >> 24) & 0xFF, (c >> 16) & 0xFF, (c >> 8) & 0xFF);
        assert!((r as i32 - 36).abs() <= 2, "r={r}");
        assert!((g as i32 - 155).abs() <= 2, "g={g}");
        assert!((b as i32 - 66).abs() <= 2, "b={b}");
        // Percent components + `/ alpha`. Pure P3 red clamps to sRGB red.
        let c = parse_color("color(display-p3 100% 0% 0% / 50%)").expect("pct parses");
        assert_eq!((c >> 24) & 0xFF, 0xFF);
        assert_eq!(c & 0xFF, 0x80);
        // sRGB components round-trip the transfer curve unchanged.
        let c = parse_color("color(srgb 0.5 0.5 0.5)").expect("gray parses");
        let ch = (c >> 24) & 0xFF;
        assert_eq!(ch, 128);
        // Unsupported spaces and malformed bodies fail the parse.
        assert_eq!(parse_color("color(rec2020 0.5 0.5 0.5)"), None);
        assert_eq!(parse_color("color(srgb 1 0.5)"), None);
        assert_eq!(parse_color("color()"), None);
    }

    #[test]
    fn parses_lengths() {
        assert_eq!(parse_length("12px"), Some(Length::Px(12.0)));
        assert_eq!(parse_length("-3.5em"), Some(Length::Em(-3.5)));
        // `rem` must win over the `em` suffix
        assert_eq!(parse_length("2rem"), Some(Length::Rem(2.0)));
        assert_eq!(parse_length("50%"), Some(Length::Percent(50.0)));
        assert_eq!(parse_length("-50%"), Some(Length::Percent(-50.0)));
        assert_eq!(parse_length("0"), Some(Length::Px(0.0)));
        assert_eq!(parse_length("10pt"), Some(Length::Px(10.0 * 4.0 / 3.0)));
        assert_eq!(parse_length("auto"), None);
        assert_eq!(Length::Em(2.0).resolve(14.0, 16.0, 800.0), 28.0);
        assert_eq!(Length::Percent(25.0).resolve(16.0, 16.0, 800.0), 200.0);
    }

    #[test]
    fn parses_child_combinators() {
        let sheet = parse_stylesheet("div > p { color: red } ul li > a { color: blue }");
        let direct = &sheet.rules[0].selectors[0];
        assert_eq!(direct.parts.len(), 2);
        assert_eq!(direct.combs, vec![Combinator::Child]);
        // Whitespace around '>' is optional.
        let tight = parse_stylesheet("div>p{color:red}").rules[0].selectors[0].clone();
        assert_eq!(tight.combs, vec![Combinator::Child]);
        let mixed = &sheet.rules[1].selectors[0];
        assert_eq!(mixed.combs, vec![Combinator::Descendant, Combinator::Child]);
    }

    #[test]
    fn parses_sibling_combinators() {
        let sheet = parse_stylesheet("h1 + p, h2 ~ span, h1+p, h2~span { color: red }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(
            sheet.rules[0].selectors[0].combs,
            vec![Combinator::AdjacentSibling]
        );
        assert_eq!(
            sheet.rules[0].selectors[1].combs,
            vec![Combinator::GeneralSibling]
        );
        assert_eq!(
            sheet.rules[0].selectors[2].combs,
            vec![Combinator::AdjacentSibling]
        );
        assert_eq!(
            sheet.rules[0].selectors[3].combs,
            vec![Combinator::GeneralSibling]
        );
        assert!(parse_stylesheet("h1 + { color: red }").rules.is_empty());
        assert!(parse_stylesheet("+ p { color: red }").rules.is_empty());
        assert!(parse_stylesheet("h1 + + p { color: red }").rules.is_empty());
    }

    #[test]
    fn parses_complex_not_arguments() {
        let sheet = parse_stylesheet("a:not(div > .x ~ span) { color: red }");
        let pseudo = &sheet.rules[0].selectors[0].parts[0].pseudos[0];
        let Pseudo::Not(inner) = pseudo else {
            panic!("expected :not");
        };
        assert_eq!(inner.parts.len(), 3);
        assert_eq!(
            inner.combs,
            vec![Combinator::Child, Combinator::GeneralSibling]
        );
        assert_eq!(inner.parts[0].tag.as_deref(), Some("div"));
        assert_eq!(inner.parts[1].classes, vec!["x"]);
        assert_eq!(inner.parts[2].tag.as_deref(), Some("span"));
        assert!(parse_stylesheet("a:not(div >) { color: red }")
            .rules
            .is_empty());
    }

    #[test]
    fn parses_attribute_selectors() {
        let sheet = parse_stylesheet(
            "[hidden] { display: none } a[href^=\"https\"] { color: green } input[type=text] { }",
        );
        let bare = &sheet.rules[0].selectors[0];
        assert_eq!(
            bare.parts[0].attrs,
            vec![AttrSelector {
                name: "hidden".into(),
                op: AttrOp::Exists,
                value: String::new(),
            }]
        );
        let prefix = &sheet.rules[1].selectors[0];
        assert_eq!(prefix.parts[0].attrs[0].op, AttrOp::Prefix);
        assert_eq!(prefix.parts[0].attrs[0].value, "https");
        let exact = &sheet.rules[2].selectors[0];
        assert_eq!(exact.parts[0].attrs[0].op, AttrOp::Equals);
        assert_eq!(exact.parts[0].attrs[0].value, "text");
    }

    #[test]
    fn stateful_pseudo_classes_drop_their_selector() {
        // Dynamic pseudo-classes are kept (they match against interaction
        // state at style time); :not stays.
        let sheet =
            parse_stylesheet("a:hover { color: red } p:first-child:not(.lead) { clear: both }");
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(
            sheet.rules[0].selectors[0].parts[0].pseudos,
            vec![Pseudo::Hover]
        );
        let sel = &sheet.rules[1].selectors[0];
        assert_eq!(
            sel.parts[0].pseudos,
            vec![
                Pseudo::FirstChild,
                Pseudo::Not(Box::new(Selector {
                    parts: vec![SimpleSelector {
                        classes: vec!["lead".into()],
                        ..Default::default()
                    }],
                    combs: Vec::new(),
                }))
            ]
        );
        // R3: ::before/::after keep the rule with a trailing marker the
        // style engine routes to the generated box.
        let sheet = parse_stylesheet("p::before { content: \"x\" }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(
            sheet.rules[0].selectors[0].parts[0].pseudos,
            vec![Pseudo::Before]
        );
        let sheet = parse_stylesheet("p:after { content: \"x\" }");
        assert_eq!(
            sheet.rules[0].selectors[0].parts[0].pseudos,
            vec![Pseudo::After]
        );
        // Other pseudo-elements are still unmatchable.
        assert!(parse_stylesheet("p::first-line { color: red }")
            .rules
            .is_empty());
        // Truly unknown state still drops.
        assert!(parse_stylesheet("a:visited { color: red }")
            .rules
            .is_empty());
    }

    #[test]
    fn dynamic_pseudo_classes_and_nth_child_parse() {
        let sheet = parse_stylesheet(
            "a:active { } input:checked { } button:disabled { } div:focus { } \
             li:nth-child(2n+1) { } li:nth-child(odd) { } li:nth-child(even) { } \
             li:nth-child(3) { } li:nth-child(-n+2) { } li:nth-child(n) { }",
        );
        let pseudos: Vec<Pseudo> = sheet
            .rules
            .iter()
            .map(|r| r.selectors[0].parts[0].pseudos[0].clone())
            .collect();
        assert_eq!(
            pseudos,
            vec![
                Pseudo::Active,
                Pseudo::Checked,
                Pseudo::Disabled,
                Pseudo::Focus,
                Pseudo::NthChild { a: 2, b: 1 },
                Pseudo::NthChild { a: 2, b: 1 },
                Pseudo::NthChild { a: 2, b: 0 },
                Pseudo::NthChild { a: 0, b: 3 },
                Pseudo::NthChild { a: -1, b: 2 },
                Pseudo::NthChild { a: 1, b: 0 },
            ]
        );
        // Invalid An+B drops the selector.
        assert!(parse_stylesheet("li:nth-child(2x+1) { }").rules.is_empty());
        assert!(parse_stylesheet("li:nth-child() { }").rules.is_empty());
        assert!(parse_stylesheet("li:nth-child(nonsense) { }")
            .rules
            .is_empty());
    }

    #[test]
    fn of_type_and_functional_list_pseudos_parse() {
        let sheet = parse_stylesheet(
            "p:nth-of-type(2n) { } p:first-of-type { } p:last-of-type { } \
             p:only-of-type { } :is(a, .b > c) { } :where(#x, span) { }",
        );
        let pseudos: Vec<Pseudo> = sheet
            .rules
            .iter()
            .map(|r| r.selectors[0].parts[0].pseudos[0].clone())
            .collect();
        assert_eq!(pseudos.len(), 6);
        assert_eq!(pseudos[0], Pseudo::NthOfType { a: 2, b: 0 });
        assert_eq!(pseudos[1], Pseudo::FirstOfType);
        assert_eq!(pseudos[2], Pseudo::LastOfType);
        assert_eq!(pseudos[3], Pseudo::OnlyOfType);
        // :is arguments are full selectors, combinators included.
        let Pseudo::Is(list) = &pseudos[4] else {
            panic!("expected :is");
        };
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].parts.len(), 1);
        assert_eq!(list[1].parts.len(), 2, ".b > c keeps its Child combinator");
        let Pseudo::Where(wlist) = &pseudos[5] else {
            panic!("expected :where");
        };
        assert_eq!(wlist.len(), 2);

        // Nested functional lists survive the balanced-paren argument scan;
        // an empty or all-invalid argument list drops the whole selector.
        let nested = parse_stylesheet(":is(:where(p), h1) { }");
        assert_eq!(nested.rules.len(), 1);
        assert!(parse_stylesheet("li:is() { }").rules.is_empty());
        assert!(parse_stylesheet("li:is(bogus!) { }").rules.is_empty());
        // A :where with no arguments is invalid like any other functional
        // pseudo with an empty argument.
        assert!(parse_stylesheet("li:where() { }").rules.is_empty());
    }

    #[test]
    fn media_queries_evaluated_against_the_viewport() {
        let css = "@media (max-width: 600px) { p { color: blue } } @media (min-width: 900px) { h1 { color: green } }";
        let narrow = parse_stylesheet_viewport(css, 400.0);
        assert_eq!(narrow.rules.len(), 1);
        assert_eq!(
            narrow.rules[0].selectors[0].parts[0].tag.as_deref(),
            Some("p")
        );
        let wide = parse_stylesheet_viewport(css, 1000.0);
        assert_eq!(wide.rules.len(), 1);
        assert_eq!(
            wide.rules[0].selectors[0].parts[0].tag.as_deref(),
            Some("h1")
        );

        // Comma terms OR, `and` features AND, `not` inverts.
        let both = "@media screen and (min-width: 500px), print { p { color: red } }";
        assert_eq!(parse_stylesheet_viewport(both, 600.0).rules.len(), 1);
        assert!(parse_stylesheet_viewport(both, 300.0).rules.is_empty());
        assert!(
            parse_stylesheet_viewport("@media not screen { p { } }", 600.0)
                .rules
                .is_empty()
        );
        // Unknown features make the term false.
        assert!(
            parse_stylesheet_viewport("@media (color-gamut: p3) { p { } }", 600.0)
                .rules
                .is_empty()
        );
    }

    #[test]
    fn media_orientation_and_block_axis_features() {
        let env = EnvironmentState::default();
        // A landscape viewport (900×400).
        assert!(eval_media_condition_size(
            "(orientation: landscape)",
            900.0,
            400.0,
            &env
        ));
        assert!(!eval_media_condition_size(
            "(orientation: portrait)",
            900.0,
            400.0,
            &env
        ));
        // Portrait (400×900).
        assert!(eval_media_condition_size(
            "(orientation: portrait)",
            400.0,
            900.0,
            &env
        ));
        // Square resolves to landscape (the block axis is never greater).
        assert!(eval_media_condition_size(
            "(orientation: landscape)",
            500.0,
            500.0,
            &env
        ));
        // Case-insensitive; an unknown value makes the term false.
        assert!(eval_media_condition_size(
            "(ORIENTATION: PORTRAIT)",
            400.0,
            900.0,
            &env
        ));
        assert!(!eval_media_condition_size(
            "(orientation: sideways)",
            400.0,
            900.0,
            &env
        ));
        // Height features, and orientation AND-ed with a width range.
        assert!(eval_media_condition_size(
            "(min-height: 800px)",
            400.0,
            900.0,
            &env
        ));
        assert!(!eval_media_condition_size(
            "(max-height: 800px)",
            400.0,
            900.0,
            &env
        ));
        assert!(eval_media_condition_size(
            "screen and (orientation: portrait) and (min-width: 300px)",
            400.0,
            900.0,
            &env
        ));
        assert!(!eval_media_condition_size(
            "screen and (orientation: portrait) and (min-width: 500px)",
            400.0,
            900.0,
            &env
        ));
        // Comma-OR across queries, `not` inverting a range.
        assert!(eval_media_condition_size(
            "(orientation: landscape), (orientation: portrait)",
            400.0,
            900.0,
            &env
        ));
        assert!(eval_media_condition_size(
            "not (orientation: landscape)",
            400.0,
            900.0,
            &env
        ));
        // A width-only caller has no viewport box, so the block-axis
        // features stay unknown (false) — the historical behavior.
        assert!(!eval_media_condition_env(
            "(orientation: portrait)",
            400.0,
            &env
        ));
        assert!(!eval_media_condition_env("(min-height: 1px)", 400.0, &env));
        assert!(eval_media_condition_env("(min-width: 300px)", 400.0, &env));
    }

    #[test]
    fn media_range_context_syntax() {
        let env = EnvironmentState::default();
        let at = |cond: &str, w: f32, h: f32| eval_media_condition_size(cond, w, h, &env);
        // `name op value` and `value op name`.
        assert!(at("(width >= 600px)", 800.0, 600.0));
        assert!(!at("(width >= 900px)", 800.0, 600.0));
        assert!(at("(600px <= width)", 800.0, 600.0));
        assert!(at("(width > 700px)", 800.0, 600.0));
        assert!(!at("(width > 800px)", 800.0, 600.0));
        assert!(at("(width < 900px)", 800.0, 600.0));
        // `em`/`rem` bounds fold at the initial font size.
        assert!(at("(width >= 40em)", 700.0, 600.0));
        assert!(!at("(width >= 50em)", 700.0, 600.0));
        // The double-bound form.
        assert!(at("(400px <= width < 900px)", 800.0, 600.0));
        assert!(!at("(400px <= width < 700px)", 800.0, 600.0));
        assert!(at("(300px < width <= 800px)", 800.0, 600.0));
        assert!(at("(400px < width < 900px)", 800.0, 600.0));
        // Height ranges read the viewport height.
        assert!(at("(height >= 500px)", 800.0, 600.0));
        assert!(!at("(height < 500px)", 800.0, 600.0));
        // `=` is exact; ranges combine with `and`, `not` and other features.
        assert!(at("(width = 800px)", 800.0, 600.0));
        assert!(!at("(width = 801px)", 800.0, 600.0));
        assert!(at("(width >= 400px) and (width < 900px)", 800.0, 600.0));
        assert!(at("not (width < 400px)", 800.0, 600.0));
        // A range on an unknown feature is false, as is a malformed one.
        assert!(!at("(banana >= 3px)", 800.0, 600.0));
        assert!(!at("(aspect-ratio >= 16/9)", 800.0, 600.0));
        assert!(!at("(width ~ 600px)", 800.0, 600.0));
        // The width-only entry point still evaluates inline-axis ranges.
        assert!(eval_media_condition_env("(width >= 600px)", 800.0, &env));
        assert!(!eval_media_condition_env("(height >= 600px)", 800.0, &env));
    }

    #[test]
    fn media_prefers_features_evaluate_against_the_environment() {
        let dark_reduce = EnvironmentState {
            color_scheme: ColorScheme::Dark,
            reduced_motion: true,
            contrast_more: false,
            ..EnvironmentState::default()
        };
        assert!(eval_media_condition_env(
            "(prefers-color-scheme: dark)",
            800.0,
            &dark_reduce
        ));
        assert!(!eval_media_condition_env(
            "(prefers-color-scheme: light)",
            800.0,
            &dark_reduce
        ));
        assert!(eval_media_condition_env(
            "(prefers-reduced-motion: reduce)",
            800.0,
            &dark_reduce
        ));
        assert!(!eval_media_condition_env(
            "(prefers-reduced-motion: no-preference)",
            800.0,
            &dark_reduce
        ));
        assert!(eval_media_condition_env(
            "(prefers-contrast: no-preference)",
            800.0,
            &dark_reduce
        ));
        assert!(!eval_media_condition_env(
            "(prefers-contrast: more)",
            800.0,
            &dark_reduce
        ));
        // Mixed with types and viewport features, values are case-insensitive.
        assert!(eval_media_condition_env(
            "screen and (prefers-color-scheme: DARK)",
            800.0,
            &dark_reduce
        ));
        assert!(eval_media_condition_env(
            "(min-width: 400px) and (prefers-reduced-motion: reduce)",
            800.0,
            &dark_reduce
        ));
        // Unrecognized values make the term false; `not` inverts.
        assert!(!eval_media_condition_env(
            "(prefers-color-scheme: purple)",
            800.0,
            &dark_reduce
        ));
        assert!(eval_media_condition_env(
            "not (prefers-color-scheme: light)",
            800.0,
            &dark_reduce
        ));

        // The default environment: light, full motion, normal contrast.
        assert!(eval_media_condition("(prefers-color-scheme: light)", 800.0));
        assert!(!eval_media_condition("(prefers-color-scheme: dark)", 800.0));
        assert!(eval_media_condition(
            "(prefers-reduced-motion: no-preference)",
            800.0
        ));
    }

    #[test]
    fn media_type_selects_the_screen_or_print_environment() {
        let screen = EnvironmentState::default();
        let print = EnvironmentState {
            media_type: MediaType::Print,
            ..EnvironmentState::default()
        };
        // The type gates the block: `print` is inert on screen and vice
        // versa, `all` is inert nowhere, and `only` never changes the answer.
        assert!(!eval_media_condition_env("print", 800.0, &screen));
        assert!(!eval_media_condition_env("screen", 800.0, &print));
        assert!(eval_media_condition_env("all", 800.0, &print));
        assert!(eval_media_condition_env("all", 800.0, &screen));
        assert!(eval_media_condition_env("screen", 800.0, &screen));
        assert!(eval_media_condition_env("print", 800.0, &print));
        assert!(eval_media_condition_env("only print", 800.0, &print));
        assert!(!eval_media_condition_env("only screen", 800.0, &print));
        assert!(eval_media_condition_env("SCREEN", 800.0, &screen));
        // A type combined with features still needs both to hold.
        assert!(eval_media_condition_env(
            "print and (min-width: 400px)",
            800.0,
            &print
        ));
        assert!(!eval_media_condition_env(
            "print and (min-width: 900px)",
            800.0,
            &print
        ));
        assert!(!eval_media_condition_env(
            "screen and (min-width: 400px)",
            800.0,
            &print
        ));
        // Comma terms are OR'd, so a print query with a screen fallback still
        // matches on paper through the `all` term.
        assert!(eval_media_condition_env("print, all", 800.0, &print));
        // `not print` is the screen-only query, and a type we have no
        // environment for (speech) never matches.
        assert!(!eval_media_condition_env("not print", 800.0, &print));
        assert!(eval_media_condition_env("not print", 800.0, &screen));
        assert!(!eval_media_condition_env("speech", 800.0, &print));
        assert!(!eval_media_condition_env("speech", 800.0, &screen));
    }

    #[test]
    fn print_media_answers_prefers_as_paper() {
        // A dark screen with motion and high contrast on: on paper the
        // features report the print device instead, so a page's print block
        // never inherits the screen's dark/motion answers.
        let dark_motion = EnvironmentState {
            color_scheme: ColorScheme::Dark,
            reduced_motion: false,
            contrast_more: true,
            media_type: MediaType::Print,
        };
        assert!(eval_media_condition_env(
            "(prefers-color-scheme: light)",
            800.0,
            &dark_motion
        ));
        assert!(!eval_media_condition_env(
            "(prefers-color-scheme: dark)",
            800.0,
            &dark_motion
        ));
        assert!(eval_media_condition_env(
            "(prefers-reduced-motion: reduce)",
            800.0,
            &dark_motion
        ));
        assert!(!eval_media_condition_env(
            "(prefers-reduced-motion: no-preference)",
            800.0,
            &dark_motion
        ));
        assert!(eval_media_condition_env(
            "(prefers-contrast: no-preference)",
            800.0,
            &dark_motion
        ));
        assert!(!eval_media_condition_env(
            "(prefers-contrast: more)",
            800.0,
            &dark_motion
        ));
        // The screen environment is untouched by print defaults.
        let screen = EnvironmentState {
            color_scheme: ColorScheme::Dark,
            reduced_motion: false,
            contrast_more: true,
            media_type: MediaType::Screen,
        };
        assert!(eval_media_condition_env(
            "(prefers-color-scheme: dark)",
            800.0,
            &screen
        ));
        assert!(eval_media_condition_env(
            "(prefers-contrast: more)",
            800.0,
            &screen
        ));
    }

    #[test]
    fn media_blocks_hoist_by_media_type() {
        let css = "@media print { p { color: #010101 } } \
                   @media screen { p { color: #020202 } } \
                   @media not print { p { letter-spacing: 4px } }";
        let tags = |env: &EnvironmentState| -> Vec<Option<String>> {
            parse_stylesheet_environment(css, 800.0, env)
                .rules
                .iter()
                .map(|r| r.selectors[0].parts[0].tag.clone())
                .collect()
        };
        let screen = tags(&EnvironmentState::default());
        assert_eq!(screen.len(), 2, "screen + not-print both apply on screen");
        assert_eq!(screen[0].as_deref(), Some("p"));
        // The print block is hoisted last on paper; `not print` drops out.
        let printed = tags(&EnvironmentState {
            media_type: MediaType::Print,
            ..EnvironmentState::default()
        });
        assert_eq!(printed.len(), 1);
        assert_eq!(printed[0].as_deref(), Some("p"));
    }

    #[test]
    fn media_blocks_hoist_by_environment() {
        let css = "@media (prefers-color-scheme: dark) { p { color: #111111 } } \
                   @media (prefers-color-scheme: light) { b { color: #eeeeee } }";
        let dark = parse_stylesheet_environment(
            css,
            800.0,
            &EnvironmentState {
                color_scheme: ColorScheme::Dark,
                ..Default::default()
            },
        );
        assert_eq!(dark.rules.len(), 1);
        assert_eq!(
            dark.rules[0].selectors[0].parts[0].tag.as_deref(),
            Some("p")
        );

        let light = parse_stylesheet_viewport(css, 800.0);
        assert_eq!(light.rules.len(), 1);
        assert_eq!(
            light.rules[0].selectors[0].parts[0].tag.as_deref(),
            Some("b")
        );
    }

    #[test]
    fn vars_resolve_with_fallbacks_and_detect_cycles() {
        let vars = |name: &str| -> Option<String> {
            match name {
                "--accent" => Some("#ff0000".into()),
                "--pad" => Some("4px".into()),
                "--loop" => Some("var(--loop2)".into()),
                "--loop2" => Some("var(--loop)".into()),
                _ => None,
            }
        };
        assert_eq!(resolve_vars("var(--accent)", &vars), Some("#ff0000".into()));
        assert_eq!(
            resolve_vars("1px solid var(--missing, #00ff00)", &vars),
            Some("1px solid #00ff00".into())
        );
        // No value and no fallback → invalid.
        assert_eq!(resolve_vars("var(--missing)", &vars), None);
        // Nested var() inside the fallback survives.
        assert_eq!(
            resolve_vars("var(--nope, var(--pad))", &vars),
            Some("4px".into())
        );
        // A cycle is a substitution failure, not a hang.
        assert_eq!(resolve_vars("var(--loop)", &vars), None);
    }

    #[test]
    fn supports_conditions_evaluate() {
        assert!(eval_supports_condition("(display: grid)"));
        assert!(eval_supports_condition("(border-radius: 4px)"));
        assert!(eval_supports_condition("((color: red))"), "parens nest");
        assert!(!eval_supports_condition("(bogus-prop: 1)"));
        assert!(!eval_supports_condition("(color:)"), "empty value");
        assert!(
            !eval_supports_condition("flimflam"),
            "general-enclosed is false"
        );
        assert!(eval_supports_condition("(display: grid) and (color: red)"));
        assert!(!eval_supports_condition("(display: grid) and (bogus: 1)"));
        assert!(eval_supports_condition("(bogus: 1) or (color: red)"));
        assert!(eval_supports_condition("not (bogus: 1)"));
        assert!(!eval_supports_condition("not (color: red)"));
        assert!(eval_supports_condition("not (bogus: 1) and (color: red)"));
        assert!(
            eval_supports_condition("(DISPLAY: GRID)"),
            "case-insensitive"
        );
    }

    #[test]
    fn supports_blocks_hoist_when_true() {
        let css = "@supports (display: grid) { p { color: #123456 } } \
                   @supports (bogus-prop: 1) { p { font-weight: 700 } } \
                   @supports not (bogus: 1) { h1 { color: #abcdef } }";
        let sheet = parse_stylesheet(css);
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(sheet.rules[0].declarations[0].value, "#123456");
        assert_eq!(
            sheet.rules[1].selectors[0].parts[0].tag.as_deref(),
            Some("h1")
        );
    }

    #[test]
    fn keyframes_parse_and_sort() {
        let sheet = parse_stylesheet(
            "@keyframes fade { to { opacity: 1 } 0%, 50% { opacity: 0 } from { opacity: 0 } }",
        );
        assert!(sheet.rules.is_empty(), "keyframes add no rules");
        assert_eq!(sheet.keyframes.len(), 1);
        let kf = &sheet.keyframes[0];
        assert_eq!(kf.name, "fade");
        assert_eq!(kf.frames.len(), 4, "0%,50% expand plus to/from");
        let offsets: Vec<f32> = kf.frames.iter().map(|f| f.offset).collect();
        assert_eq!(offsets, vec![0.0, 0.0, 0.5, 1.0], "sorted, stable");
        // Unnamed and garbage selectors are dropped.
        assert!(parse_stylesheet("@keyframes { 0% { } }")
            .keyframes
            .is_empty());
        assert!(parse_stylesheet("@keyframes x { bogus { } }").keyframes[0]
            .frames
            .is_empty());
    }

    // --- B3: :has() parse shapes ---

    fn first_pseudo<'a>(sel: &'a Selector, name: &str) -> Option<&'a Pseudo> {
        sel.parts
            .iter()
            .flat_map(|p| p.pseudos.iter())
            .find(|p| pseudo_name(p) == name)
    }

    fn pseudo_name(p: &Pseudo) -> &'static str {
        match p {
            Pseudo::FirstChild => "first-child",
            Pseudo::LastChild => "last-child",
            Pseudo::Root => "root",
            Pseudo::Not(_) => "not",
            Pseudo::Hover => "hover",
            Pseudo::Active => "active",
            Pseudo::Focus => "focus",
            Pseudo::Checked => "checked",
            Pseudo::Disabled => "disabled",
            Pseudo::Invalid => "invalid",
            Pseudo::NthChild { .. } => "nth-child",
            Pseudo::NthOfType { .. } => "nth-of-type",
            Pseudo::FirstOfType => "first-of-type",
            Pseudo::LastOfType => "last-of-type",
            Pseudo::OnlyOfType => "only-of-type",
            Pseudo::Before => "before",
            Pseudo::After => "after",
            Pseudo::Is(_) => "is",
            Pseudo::Where(_) => "where",
            Pseudo::Has(_) => "has",
            Pseudo::Lang(_) => "lang",
        }
    }

    #[test]
    fn has_parses_descendant_and_child_anchored_args() {
        let sheet =
            parse_stylesheet("div:has(p) { color: red } div:has(> section) { color: blue }");
        // Each rule is a plain descendant rule, but carries one :has pseudo.
        assert_eq!(sheet.rules.len(), 2);
        let has0 = first_pseudo(&sheet.rules[0].selectors[0], "has").expect("first :has");
        let has1 = first_pseudo(&sheet.rules[1].selectors[0], "has").expect("second :has");
        match (has0, has1) {
            (Pseudo::Has(a0), Pseudo::Has(a1)) => {
                assert!(!a0[0].child_anchored, "descendant-anchored arg");
                assert_eq!(a0[0].selector.parts[0].tag.as_deref(), Some("p"));
                assert!(a1[0].child_anchored, "child-anchored arg");
                assert_eq!(a1[0].selector.parts[0].tag.as_deref(), Some("section"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn has_drops_sibling_anchored_args() {
        // `+` and `~` anchors are not implemented: the parser drops them
        // and keeps any remaining args. With only sibling args, the
        // whole pseudo is rejected and the rule falls back to a plain
        // descendant selector (no `:has`).
        let sheet = parse_stylesheet("div:has(+ p) { color: red } div:has(~ p) { color: blue }");
        for r in &sheet.rules {
            let any_has = r.selectors[0]
                .parts
                .iter()
                .any(|p| p.pseudos.iter().any(|q| pseudo_name(q) == "has"));
            assert!(
                !any_has,
                "no :has pseudos survive (got {:?})",
                r.selectors[0].parts
            );
        }
        // Mixed: a surviving descendant arg keeps the pseudo alive.
        let sheet = parse_stylesheet("div:has(+ p, section) { color: red }");
        let has = first_pseudo(&sheet.rules[0].selectors[0], "has").expect("mixed :has");
        match has {
            Pseudo::Has(args) => {
                assert_eq!(args.len(), 1);
                assert!(!args[0].child_anchored);
                assert_eq!(args[0].selector.parts[0].tag.as_deref(), Some("section"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn has_or_args_combine_relative_selectors() {
        let sheet = parse_stylesheet("div:has(span, > p) { color: red }");
        let has = first_pseudo(&sheet.rules[0].selectors[0], "has").unwrap();
        match has {
            Pseudo::Has(args) => {
                assert_eq!(args.len(), 2);
                let tags: Vec<(&str, bool)> = args
                    .iter()
                    .map(|a| {
                        (
                            a.selector.parts[0].tag.as_deref().unwrap_or(""),
                            a.child_anchored,
                        )
                    })
                    .collect();
                assert_eq!(tags, vec![("span", false), ("p", true)]);
            }
            _ => unreachable!(),
        }
    }

    // --- D3: :lang() parse shapes ---

    #[test]
    fn lang_pseudo_parses_ranges_lowercased() {
        let sheet =
            parse_stylesheet("p:lang(en) { color: red } div:lang(De, FR-hans) { color: blue }");
        assert_eq!(sheet.rules.len(), 2);
        let l0 = first_pseudo(&sheet.rules[0].selectors[0], "lang").expect(":lang");
        match l0 {
            Pseudo::Lang(r) => assert_eq!(r, &["en".to_string()]),
            _ => unreachable!(),
        }
        // Ranges are lowercased, commas split, whitespace trimmed.
        let l1 = first_pseudo(&sheet.rules[1].selectors[0], "lang").expect(":lang multi");
        match l1 {
            Pseudo::Lang(r) => assert_eq!(r, &["de".to_string(), "fr-hans".to_string()]),
            _ => unreachable!(),
        }
        // Empty argument drops the whole selector.
        assert!(parse_stylesheet("p:lang() { color: red }").rules.is_empty());
        assert!(parse_stylesheet("p:lang( , ) { color: red }")
            .rules
            .is_empty());
    }

    // --- B3: @container parse ---

    #[test]
    fn container_at_rule_parses_named_and_unnamed_forms() {
        let css = "@container card (min-width: 400px) { p { color: red } } \
                   @container (width > 300px) { div { color: blue } } \
                   @container wide { h1 { color: green } }";
        let sheet = parse_stylesheet(css);
        assert_eq!(sheet.rules.len(), 3, "one rule per inner block");
        // Each inner rule carries the same container context.
        for r in &sheet.rules {
            let cq = r
                .container
                .as_ref()
                .expect("inner rule keeps its container context");
            if r.selectors[0].parts[0].tag.as_deref() == Some("p") {
                assert_eq!(cq.name, "card");
                assert_eq!(cq.cond, "(min-width: 400px)");
            } else if r.selectors[0].parts[0].tag.as_deref() == Some("div") {
                assert_eq!(cq.name, "", "unnamed when head starts with `(`");
                assert_eq!(cq.cond, "(width > 300px)");
            } else {
                assert_eq!(cq.name, "wide");
                assert_eq!(cq.cond, "", "no condition is always-true");
            }
        }
    }

    #[test]
    fn nested_container_inner_overrides_outer() {
        let css = "@container (min-width: 100px) { \
                       @container (min-width: 200px) { p { color: red } } \
                       div { color: blue } \
                   }";
        let sheet = parse_stylesheet(css);
        assert_eq!(sheet.rules.len(), 2);
        let p_rule = sheet
            .rules
            .iter()
            .find(|r| r.selectors[0].parts[0].tag.as_deref() == Some("p"))
            .unwrap();
        let cq = p_rule.container.as_ref().unwrap();
        assert_eq!(cq.cond, "(min-width: 200px)", "inner wins");
        let div_rule = sheet
            .rules
            .iter()
            .find(|r| r.selectors[0].parts[0].tag.as_deref() == Some("div"))
            .unwrap();
        assert_eq!(
            div_rule.container.as_ref().unwrap().cond,
            "(min-width: 100px)"
        );
    }

    // --- B4: filter / backdrop-filter parse ---

    fn kinds(fs: &FilterList) -> Vec<&'static str> {
        fs.0.iter()
            .map(|f| match f {
                FilterFn::Blur(_) => "blur",
                FilterFn::DropShadow { .. } => "drop-shadow",
                FilterFn::Brightness(_) => "brightness",
                FilterFn::Contrast(_) => "contrast",
                FilterFn::Grayscale(_) => "grayscale",
                FilterFn::HueRotate(_) => "hue-rotate",
                FilterFn::Invert(_) => "invert",
                FilterFn::Opacity(_) => "opacity",
                FilterFn::Saturate(_) => "saturate",
                FilterFn::Sepia(_) => "sepia",
            })
            .collect()
    }

    #[test]
    fn filter_parses_none_and_blur() {
        assert!(parse_filter_list("none").0.is_empty());
        assert!(parse_filter_list("").0.is_empty());
        assert!(parse_filter_list("  NONE ").0.is_empty());
        let fs = parse_filter_list("blur(8px)");
        assert_eq!(kinds(&fs), vec!["blur"]);
        assert!(matches!(fs.0[0], FilterFn::Blur(8.0)));
    }

    #[test]
    fn filter_parses_drop_shadow_two_three_and_four_args() {
        let fs = parse_filter_list("drop-shadow(2px 4px)");
        assert!(matches!(
            fs.0[0],
            FilterFn::DropShadow {
                dx: 2.0,
                dy: 4.0,
                blur: 0.0,
                ..
            }
        ));
        let fs = parse_filter_list("drop-shadow(2px 4px 6px)");
        assert!(matches!(
            fs.0[0],
            FilterFn::DropShadow {
                dx: 2.0,
                dy: 4.0,
                blur: 6.0,
                ..
            }
        ));
        let fs = parse_filter_list("drop-shadow(2px 4px 6px #ff0000)");
        match fs.0[0] {
            FilterFn::DropShadow {
                dx,
                dy,
                blur,
                color,
            } => {
                assert_eq!((dx, dy, blur), (2.0, 4.0, 6.0));
                assert_eq!(color, 0xFF0000FF);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn filter_parses_scalar_chain() {
        let fs = parse_filter_list("brightness(1.2) grayscale(0.5) hue-rotate(45deg)");
        assert_eq!(kinds(&fs), vec!["brightness", "grayscale", "hue-rotate"]);
        assert!(matches!(fs.0[0], FilterFn::Brightness(1.2)));
        assert!(matches!(fs.0[1], FilterFn::Grayscale(0.5)));
        assert!(matches!(fs.0[2], FilterFn::HueRotate(45.0)));
    }

    #[test]
    fn filter_parses_hue_rotate_without_unit_as_degrees() {
        let fs = parse_filter_list("hue-rotate(180)");
        assert!(matches!(fs.0[0], FilterFn::HueRotate(180.0)));
    }

    #[test]
    fn filter_invalidates_whole_list_on_bad_token() {
        // Per spec, an invalid filter function nukes the whole declaration.
        let fs = parse_filter_list("blur(2px) nosuch(1)");
        assert!(fs.0.is_empty());
        let fs = parse_filter_list("drop-shadow(only-color)");
        assert!(fs.0.is_empty());
    }

    #[test]
    fn filter_commas_separate_functions_not_drop_shadow_args() {
        // drop-shadow args are space-separated; the comma here separates
        // drop-shadow from the next filter.
        let fs = parse_filter_list("drop-shadow(2px 4px #00000080), opacity(0.5)");
        assert_eq!(kinds(&fs), vec!["drop-shadow", "opacity"]);
    }

    // --- B6: ch/ex font-relative units ---

    #[test]
    fn ch_ex_parse_to_their_length_variants() {
        assert!(matches!(parse_length("3ch"), Some(Length::Ch(3.0))));
        assert!(matches!(parse_length("2ex"), Some(Length::Ex(2.0))));
    }

    #[test]
    fn parse_len_px_ch_folds_ch_and_ex_against_provided_widths() {
        // ch and ex fold against the supplied pixel widths. For a
        // bare ch/ex token the parser returns `Len::Px(px)`; a
        // calc/min/max expression flattens into `Sum`.
        let px = parse_len_px_ch("2ch", 16.0, 16.0, 0.0, 0.0, 8.0, 4.0).unwrap();
        match px {
            Len::Px(v) => assert!((v - 16.0).abs() < 0.01),
            _ => panic!("expected Len::Px"),
        }
        let px = parse_len_px_ch("3ex", 16.0, 16.0, 0.0, 0.0, 8.0, 4.0).unwrap();
        match px {
            Len::Px(v) => assert!((v - 12.0).abs() < 0.01),
            _ => panic!("expected Len::Px"),
        }
        // Without metrics (0×0), ch/ex reject rather than fold to 0.
        assert!(parse_len_px_ch("2ch", 16.0, 16.0, 0.0, 0.0, 0.0, 0.0).is_none());
    }

    #[test]
    fn parse_size_val_intrinsic_keywords() {
        // C1: the three keywords parse to their variants; the functional
        // fit-content carries its argument as a Len.
        assert_eq!(
            parse_size_val_ch("min-content", 16.0, 16.0, 0.0, 0.0, 0.0, 0.0),
            Some(SizeVal::MinContent)
        );
        assert_eq!(
            parse_size_val_ch("MAX-CONTENT", 16.0, 16.0, 0.0, 0.0, 0.0, 0.0),
            Some(SizeVal::MaxContent)
        );
        assert_eq!(
            parse_size_val_ch(" fit-content ", 16.0, 16.0, 0.0, 0.0, 0.0, 0.0),
            Some(SizeVal::FitContent(None))
        );
        assert_eq!(
            parse_size_val_ch("fit-content(200px)", 16.0, 16.0, 0.0, 0.0, 0.0, 0.0),
            Some(SizeVal::FitContent(Some(Len::Px(200.0))))
        );
        // Percentages inside fit-content() survive to resolve time.
        let pct = parse_size_val_ch("fit-content(50%)", 16.0, 16.0, 0.0, 0.0, 0.0, 0.0).unwrap();
        match pct {
            SizeVal::FitContent(Some(l)) => assert!((l.resolve(400.0) - 200.0).abs() < 0.01),
            _ => panic!("expected FitContent with a percent arg"),
        }
        // Plain lengths and calc() still parse through to SizeVal::Len.
        assert_eq!(
            parse_size_val_ch("120px", 16.0, 16.0, 0.0, 0.0, 0.0, 0.0),
            Some(SizeVal::Len(Len::Px(120.0)))
        );
        assert!(matches!(
            parse_size_val_ch("calc(100% - 24px)", 16.0, 16.0, 0.0, 0.0, 0.0, 0.0),
            Some(SizeVal::Len(_))
        ));
        // Garbage and `auto` reject (caller treats as unset).
        assert_eq!(
            parse_size_val_ch("auto", 16.0, 16.0, 0.0, 0.0, 0.0, 0.0),
            None
        );
        assert_eq!(
            parse_size_val_ch("banana", 16.0, 16.0, 0.0, 0.0, 0.0, 0.0),
            None
        );
    }

    #[test]
    fn size_val_resolve_clamps_like_css() {
        // fit-content = clamp(min-content, available, max-content).
        let fc = SizeVal::FitContent(None);
        assert_eq!(fc.resolve(500.0, 100.0, 300.0), 300.0); // capped by max-content
        assert_eq!(fc.resolve(50.0, 100.0, 300.0), 100.0); // floored by min-content
        assert_eq!(fc.resolve(200.0, 100.0, 300.0), 200.0); // available wins
                                                            // fit-content(<len>) replaces `available` with the argument.
        let farg = SizeVal::FitContent(Some(Len::Px(150.0)));
        assert_eq!(farg.resolve(500.0, 100.0, 300.0), 150.0);
        assert_eq!(farg.resolve(500.0, 200.0, 300.0), 200.0); // floored
                                                              // Keywords pass through their measured intrinsic.
        assert_eq!(SizeVal::MinContent.resolve(500.0, 100.0, 300.0), 100.0);
        assert_eq!(SizeVal::MaxContent.resolve(500.0, 100.0, 300.0), 300.0);
        // A plain Len still resolves percentages against avail.
        assert_eq!(
            SizeVal::Len(Len::Sum { px: 0.0, pct: 50.0 }).resolve(400.0, 0.0, 0.0),
            200.0
        );
    }

    #[test]
    fn b15_property_rule_parses_and_validates() {
        let sheet = parse_stylesheet(
            "@property --main { syntax: \"<color>\"; inherits: true; initial-value: red; }",
        );
        assert_eq!(sheet.properties.len(), 1);
        let p = &sheet.properties[0];
        assert_eq!(p.name, "--main");
        assert_eq!(p.syntax, "<color>");
        assert!(p.inherits);
        assert_eq!(p.initial.as_deref(), Some("red"));

        // `inherits` defaults to false; `initial-value` is optional.
        let s2 = parse_stylesheet("@property --b { syntax: \"<length>\"; }");
        assert_eq!(s2.properties.len(), 1);
        assert!(!s2.properties[0].inherits);
        assert_eq!(s2.properties[0].initial, None);

        // Invalid registrations drop: missing required `syntax`, bad name.
        let s3 =
            parse_stylesheet("@property --a { inherits: true; } @property bad { syntax: \"*\"; }");
        assert!(s3.properties.is_empty());
    }

    #[test]
    fn b15_scope_rule_parses_with_gate() {
        let sheet = parse_stylesheet("@scope (.card) to (.stop) { p { color: red; } }");
        assert_eq!(sheet.rules.len(), 1);
        let scope = sheet.rules[0].scope.as_ref().expect("scope gate");
        assert!(scope.end.is_some());

        let s2 = parse_stylesheet("@scope (h1) { p { color: red; } }");
        assert_eq!(s2.rules.len(), 1);
        assert!(s2.rules[0].scope.as_ref().unwrap().end.is_none());

        // Nested in @media: the gate rides on the hoisted rule alongside
        // the media condition (folded at parse time — needs a viewport).
        let s3 = parse_stylesheet_viewport(
            "@media (max-width: 2000px) { @scope (.a) to (.b) { p { color: red } } }",
            1280.0,
        );
        assert_eq!(s3.rules.len(), 1);
        assert!(s3.rules[0].scope.is_some());

        // No valid start selector: the whole at-rule drops.
        let s4 = parse_stylesheet("@scope { p { color: red; } }");
        assert!(s4.rules.is_empty());
    }

    #[test]
    fn b15_supports_selector_function() {
        // A parseable selector is "supported".
        let s = parse_stylesheet("@supports selector(p > a:hover) { .x { color: red; } }");
        assert_eq!(s.rules.len(), 1);
        // Unparseable selector: the block drops.
        let s = parse_stylesheet("@supports selector((( { .x { color: red; } }");
        assert_eq!(s.rules.len(), 0);
        // `for <element>` clause is ignored (still supported).
        let s = parse_stylesheet("@supports selector(#id for <a>) { .x { color: red; } }");
        assert_eq!(s.rules.len(), 1);
        // Composed with and/not.
        let s =
            parse_stylesheet("@supports selector(a b) and (display: grid) { .x { color: red } }");
        assert_eq!(s.rules.len(), 1);
        let s = parse_stylesheet("@supports not selector(a b) { .x { color: red; } }");
        assert_eq!(s.rules.len(), 0);
    }

    #[test]
    fn b15_supports_declaration_reflects_applied_props() {
        // B15: the SUPPORTED table now mirrors every `apply()` arm plus
        // the `all` shorthand.
        for css in [
            "@supports (overscroll-behavior: contain) { .a { color: red } }",
            "@supports (scroll-snap-type: y mandatory) { .a { color: red } }",
            "@supports (text-decoration-thickness: 2px) { .a { color: red } }",
            "@supports (container-type: inline-size) { .a { color: red } }",
            "@supports (all: initial) { .a { color: red } }",
            "@supports (outline-offset: 2px) { .a { color: red } }",
            "@supports (hyphens: auto) { .a { color: red } }",
            "@supports (grid-template-areas: 'a b') { .a { color: red } }",
            "@supports (animation-name: fade) { .a { color: red } }",
            "@supports (transition-duration: 2s) { .a { color: red } }",
            // C2: aspect-ratio is applied by the engine.
            "@supports (aspect-ratio: 16/9) { .a { color: red } }",
            // C3: fragmentation properties are parsed by the engine.
            "@supports (break-inside: avoid) { .a { color: red } }",
            // C5: table polish properties are applied by the engine.
            "@supports (border-collapse: collapse) { .a { color: red } }",
            "@supports (border-spacing: 2px 4px) { .a { color: red } }",
            "@supports (caption-side: bottom) { .a { color: red } }",
            // C6: grid auto-flow + `subgrid` template values are parsed.
            "@supports (grid-auto-flow: column dense) { .a { color: red } }",
            "@supports (grid-template-columns: subgrid) { .a { color: red } }",
            // B4: writing-mode + logical geometry are applied by the engine.
            "@supports (writing-mode: vertical-rl) { .a { color: red } }",
            "@supports (inline-size: 100px) { .a { color: red } }",
            "@supports (margin-block: 4px) { .a { color: red } }",
            // `visibility` + the `background` box properties are applied by
            // the engine.
            "@supports (visibility: hidden) { .a { color: red } }",
            "@supports (background-size: cover) { .a { color: red } }",
            "@supports (background-position: right 10px bottom 20px) { .a { color: red } }",
            "@supports (background-repeat: no-repeat) { .a { color: red } }",
            "@supports (background-origin: content-box) { .a { color: red } }",
            "@supports (background-clip: padding-box) { .a { color: red } }",
        ] {
            assert_eq!(parse_stylesheet(css).rules.len(), 1, "must support: {css}");
        }
        for css in ["@supports (banana: split) { .a { color: red } }"] {
            assert_eq!(
                parse_stylesheet(css).rules.len(),
                0,
                "must not support: {css}"
            );
        }
    }

    #[test]
    fn d4_supports_font_feature_props() {
        for css in [
            "@supports (font-feature-settings: 'liga') { .a { color: red } }",
            "@supports (font-feature-settings: \"smcp\" 1, 'liga' 0) { .a { color: red } }",
            "@supports (font-variant: small-caps) { .a { color: red } }",
            "@supports (font-variant-caps: all-small-caps) { .a { color: red } }",
            "@supports (font-variant-ligatures: no-common-ligatures) { .a { color: red } }",
        ] {
            assert_eq!(parse_stylesheet(css).rules.len(), 1, "must support: {css}");
        }
    }

    #[test]
    fn b7_shape_parse_circle() {
        let p = |v: &str| parse_clip_shape(v, 16.0, 16.0, 1280.0, 800.0);
        let pc = |n: f32| Len::Sum { px: 0.0, pct: n };
        let px = Len::Px;
        assert_eq!(
            p("circle(50%)"),
            Some(ClipShape::Circle {
                r: ShapeRadius::Len(pc(50.0)),
                cx: pc(50.0),
                cy: pc(50.0),
            })
        );
        // Omitted radius / position: closest-side centered.
        assert_eq!(
            p("circle()"),
            Some(ClipShape::Circle {
                r: ShapeRadius::ClosestSide,
                cx: pc(50.0),
                cy: pc(50.0),
            })
        );
        assert_eq!(
            p("circle(10px at 20% 30%)"),
            Some(ClipShape::Circle {
                r: ShapeRadius::Len(px(10.0)),
                cx: pc(20.0),
                cy: pc(30.0),
            })
        );
        // em folds against the element font (16px here).
        assert_eq!(
            p("circle(2em at left top)"),
            Some(ClipShape::Circle {
                r: ShapeRadius::Len(px(32.0)),
                cx: px(0.0),
                cy: px(0.0),
            })
        );
        assert_eq!(
            p("circle(farthest-corner at center bottom)"),
            Some(ClipShape::Circle {
                r: ShapeRadius::FarthestCorner,
                cx: pc(50.0),
                cy: pc(100.0),
            })
        );
        // Reference-box keyword stripped, lone length sets both offsets.
        assert_eq!(p("circle(50%) border-box"), p("circle(50%)"));
        assert_eq!(
            p("circle(50% at 10px)"),
            Some(ClipShape::Circle {
                r: ShapeRadius::Len(pc(50.0)),
                cx: px(10.0),
                cy: px(10.0),
            })
        );
        // `at` with no radius, and swapped keyword axes.
        assert_eq!(
            p("ellipse(closest-side at top left)"),
            Some(ClipShape::Ellipse {
                rx: ShapeRadius::ClosestSide,
                ry: ShapeRadius::ClosestSide,
                cx: px(0.0),
                cy: px(0.0),
            })
        );
    }

    #[test]
    fn b7_shape_parse_ellipse_inset() {
        let p = |v: &str| parse_clip_shape(v, 16.0, 16.0, 1280.0, 800.0);
        let pc = |n: f32| Len::Sum { px: 0.0, pct: n };
        let px = Len::Px;
        assert_eq!(
            p("ellipse(10px 20px)"),
            Some(ClipShape::Ellipse {
                rx: ShapeRadius::Len(px(10.0)),
                ry: ShapeRadius::Len(px(20.0)),
                cx: pc(50.0),
                cy: pc(50.0),
            })
        );
        // One radius applies to both axes.
        assert_eq!(
            p("ellipse(50%)"),
            Some(ClipShape::Ellipse {
                rx: ShapeRadius::Len(pc(50.0)),
                ry: ShapeRadius::Len(pc(50.0)),
                cx: pc(50.0),
                cy: pc(50.0),
            })
        );
        // Two-value inset shorthand: top/bottom then left/right.
        assert_eq!(
            p("inset(10px 20px)"),
            Some(ClipShape::Inset {
                top: px(10.0),
                right: px(20.0),
                bottom: px(10.0),
                left: px(20.0),
                radii: [(px(0.0), px(0.0)); 4],
            })
        );
        // Pill with a uniform round radius.
        let pill = p("inset(0 10px 0 10px round 999px)").unwrap();
        match pill {
            ClipShape::Inset { radii, .. } => {
                assert_eq!(radii, [(px(999.0), px(999.0)); 4]);
            }
            _ => panic!("inset expected"),
        }
        // Slash clause: vertical radii expand 1-2 like border-radius.
        match p("inset(10px round 5px / 10px 20px)").unwrap() {
            ClipShape::Inset { radii, .. } => {
                assert_eq!(
                    radii,
                    [
                        (px(5.0), px(10.0)),
                        (px(5.0), px(20.0)),
                        (px(5.0), px(10.0)),
                        (px(5.0), px(20.0)),
                    ]
                );
            }
            _ => unreachable!(),
        }
        // Single percentage offset fills all four edges.
        match p("inset(20%)").unwrap() {
            ClipShape::Inset {
                top,
                right,
                bottom,
                left,
                ..
            } => {
                assert_eq!(
                    (top, right, bottom, left),
                    (pc(20.0), pc(20.0), pc(20.0), pc(20.0))
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn b7_shape_parse_polygon() {
        let p = |v: &str| parse_clip_shape(v, 16.0, 16.0, 1280.0, 800.0);
        let pc = |n: f32| Len::Sum { px: 0.0, pct: n };
        let px = Len::Px;
        let tri = p("polygon(0 0, 100% 0, 0 100%)").unwrap();
        match tri {
            ClipShape::Polygon { points, evenodd } => {
                assert!(!evenodd);
                assert_eq!(
                    points,
                    vec![
                        (px(0.0), px(0.0)),
                        (pc(100.0), px(0.0)),
                        (px(0.0), pc(100.0))
                    ]
                );
            }
            _ => unreachable!(),
        }
        match p("polygon(evenodd, 10px 10px, 20px 20px, 30px 30px)").unwrap() {
            ClipShape::Polygon { points, evenodd } => {
                assert!(evenodd);
                assert_eq!(points.len(), 3);
            }
            _ => unreachable!(),
        }
        // calc() inside a point stays intact through depth-aware splitting.
        match p("polygon(calc(10px + 2%) 0, 100% 0, 0 100%)").unwrap() {
            ClipShape::Polygon { points, .. } => {
                assert_eq!(points[0], (Len::Sum { px: 10.0, pct: 2.0 }, px(0.0)));
            }
            _ => unreachable!(),
        }
        // Fewer than three points or a stray third coordinate: invalid.
        assert_eq!(p("polygon(0 0, 1px 1px)"), None);
        assert_eq!(p("polygon(0 0 0, 1px 1px, 2px 2px)"), None);
    }

    #[test]
    fn b7_rejects_and_mask_and_supports() {
        let p = |v: &str| parse_clip_shape(v, 16.0, 16.0, 1280.0, 800.0);
        for bad in [
            "none",
            "",
            "path('M 0 0 L 1 1')",
            "url(#clip)",
            "banana",
            "circle(50% at 10px 20px 30px)",
            "ellipse(1px 2px 3px)",
            "inset()",
        ] {
            assert_eq!(p(bad), None, "must reject: {bad}");
        }
        let m = |v: &str| parse_mask_shape(v, 16.0, 16.0, 1280.0, 800.0);
        assert!(matches!(m("circle(40%)"), Some(ClipShape::Circle { .. })));
        assert_eq!(m("url(x.png)"), None);
        assert_eq!(m("linear-gradient(black, transparent)"), None);
        assert_eq!(m("radial-gradient(circle, black, transparent)"), None);
        // Multi-layer: the first shape wins past the url layer.
        assert!(matches!(
            m("url(a.svg), ellipse(10px 20px)"),
            Some(ClipShape::Ellipse { .. })
        ));
        // @supports now answers for clip-path/mask too.
        assert_eq!(
            parse_stylesheet("@supports (clip-path: circle(50%)) { .a { color: red } }")
                .rules
                .len(),
            1
        );
        assert_eq!(
            parse_stylesheet("@supports (mask-image: circle(50%)) { .a { color: red } }")
                .rules
                .len(),
            1
        );
    }

    // ---- C1: intrinsic sizing keyword parsing ----
    #[test]
    fn c1_parse_size_val_keywords() {
        assert_eq!(
            parse_size_val_v("min-content", 16.0, 16.0, 0.0, 0.0),
            Some(SizeVal::MinContent)
        );
        assert_eq!(
            parse_size_val_v("MAX-CONTENT", 16.0, 16.0, 0.0, 0.0),
            Some(SizeVal::MaxContent)
        );
        assert_eq!(
            parse_size_val_v("fit-content", 16.0, 16.0, 0.0, 0.0),
            Some(SizeVal::FitContent(None))
        );
        let fc = parse_size_val_v("fit-content(200px)", 16.0, 16.0, 16.0, 16.0).unwrap();
        assert!(
            matches!(&fc, SizeVal::FitContent(Some(l)) if l.px() == Some(200.0)),
            "{:?}",
            fc
        );
        assert_eq!(parse_size_val_v("auto", 16.0, 16.0, 0.0, 0.0), None);
        assert!(matches!(
            parse_size_val_v("40%", 16.0, 16.0, 0.0, 0.0),
            Some(SizeVal::Len(_))
        ));
    }

    #[test]
    fn c1_size_val_resolve_clamps_like_css() {
        assert_eq!(SizeVal::MinContent.resolve(500.0, 100.0, 300.0), 100.0);
        assert_eq!(SizeVal::MaxContent.resolve(500.0, 100.0, 300.0), 300.0);
        assert_eq!(
            SizeVal::FitContent(None).resolve(500.0, 100.0, 300.0),
            300.0
        );
        assert_eq!(SizeVal::FitContent(None).resolve(50.0, 100.0, 300.0), 100.0);
        let arg = parse_len_px("200px", 16.0, 16.0).unwrap();
        assert_eq!(
            SizeVal::FitContent(Some(arg)).resolve(1000.0, 150.0, 300.0),
            200.0
        );
        assert_eq!(
            SizeVal::FitContent(Some(arg)).resolve(1000.0, 250.0, 300.0),
            250.0
        );
        assert_eq!(SizeVal::Len(arg).resolve(500.0, 100.0, 300.0), 200.0);
    }
}
