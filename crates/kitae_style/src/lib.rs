//! Style engine: matches CSS rules against the DOM, resolves the cascade
//! (specificity + `!important` + inline style + custom properties), applies
//! inheritance, and produces a `ComputedStyle` per element. Ships the Kitae
//! UA stylesheet — a typography-first reader theme, parsed by kitae_css.

use std::collections::HashMap;
use std::sync::Arc;

use kitae_css::{
    parse_clip_shape, parse_declarations, parse_filter_list, parse_len_px_ch, parse_len_px_v,
    parse_length, parse_mask_shape, parse_number, parse_size_val_ch, parse_size_val_v,
    parse_stylesheet_environment_size, resolve_vars, AttrOp, AttrSelector, Combinator, Declaration,
    FontFace, FontStyle, HasArg, PropertyRule, Pseudo, Rule, ScopeQuery, Selector, SimpleSelector,
};
use kitae_dom::{Dom, ElementData, NodeId};

/// Re-exported for layout's radius/offset math (single source in kitae_css).
pub use kitae_css::Len;
pub use kitae_css::Length;
// C1: re-exported for layout's intrinsic-sizing read sites.
/// Re-exported for layout's `<source media>` / `<img media>` evaluation:
/// the stylesheet media-query evaluator, height-aware (`orientation`,
/// `min-height`, the MQ4 range syntax). Single source in kitae_css.
pub use kitae_css::eval_media_condition_size;
/// Re-exported for layout's A7 control chrome (`<input type=color>`
/// swatches); single source in kitae_css.
pub use kitae_css::parse_color;
/// Re-exported for layout's `<col>` width attributes (single source in
/// kitae_css).
pub use kitae_css::parse_len_px;
/// Re-exported for layout's B8 per-item `blend`/isolation stamps.
pub use kitae_css::MixBlend;
pub use kitae_css::SizeVal;
/// Re-exported for layout's B7 per-item `clip: Option<ResolvedClip>` — the
/// symbolic basic-shape resolved against the element's border box.
pub use kitae_css::{ClipShape, ShapeRadius};
/// Environment media state (Q6); hosts construct this for the engine.
pub use kitae_css::{ColorScheme, EnvironmentState, MediaType};
/// Re-exported for layout's B4 per-item `filters: Option<FilterList>`.
pub use kitae_css::{FilterFn, FilterList};

/// How a box participates in layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    None,
    Block,
    Inline,
    InlineBlock,
    ListItem,
    Flex,
    Grid,
    Table,
    TableRow,
    TableCell,
    TableRowGroup,
    TableCaption,
    /// A4: `<col>`/`<colgroup>` — column metadata, not a box. Layout reads
    /// these during table column resolution; they generate no boxes.
    TableColumn,
    TableColumnGroup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Static,
    Relative,
    Absolute,
    Fixed,
    /// `position: sticky` (Q5): in flow like static, but the embedder shifts
    /// the painted box at scroll time (clamped inside the nearest scroll
    /// ancestor) via the layout's sticky side-list.
    Sticky,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatSide {
    None,
    Left,
    Right,
}

/// `clear` side (Phase N1): pushes the box below matching floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clear {
    None,
    Left,
    Right,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    Visible,
    Hidden,
    /// B2: `overflow: auto` — the box clips overflowing content and
    /// may show a scrollbar (engine signals the host; the host paints
    /// and tracks the wheel). The element becomes a scroll container
    /// when its content's measured height exceeds the box's height.
    Auto,
    /// B2: `overflow: scroll` — always a scroll container; the host
    /// always shows a scrollbar.
    Scroll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDirection {
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignItems {
    Stretch,
    FlexStart,
    Center,
    FlexEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JustifyContent {
    FlexStart,
    Center,
    FlexEnd,
    SpaceBetween,
}

/// `flex-wrap` (Q4): whether flex items break onto multiple lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexWrap {
    Now,
    Wrap,
    WrapReverse,
}

/// `align-content` (Q4): distribution of a flex container's leftover cross
/// space across its wrapped lines (matters only with a definite cross size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignContent {
    Stretch,
    FlexStart,
    Center,
    FlexEnd,
    SpaceBetween,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

/// `content` on ::before/::after (R3): quoted strings and `attr(name)`.
/// B14 added `counter()`/`counters()` and arbitrary concatenation
/// (`Parts`). `none`/`normal` and unsupported values (url(), quotes)
/// generate nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentSpec {
    Text(String),
    Attr(String),
    Parts(Vec<ContentPart>),
}

/// One concatenated unit of a `content` value (B14). Counter references
/// resolve against the cascade's counter scope stack at style time.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentPart {
    Text(String),
    Attr(String),
    Counter {
        name: String,
        style: Option<ListStyleType>,
    },
    Counters {
        name: String,
        sep: String,
        style: Option<ListStyleType>,
    },
}

/// B14 `list-style-type`: the marker of a `display: list-item` box, and
/// the style argument of `counter()`/`counters()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListStyleType {
    #[default]
    Disc,
    Circle,
    Square,
    None,
    Decimal,
    DecimalLeadingZero,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

/// B14 `list-style-position` (inherited). `Outside` hangs the marker in
/// the list's padding (the legacy look); `Inside` flows it as the first
/// inline box of the item's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListStylePosition {
    #[default]
    Outside,
    Inside,
}

/// B14: one `counter-reset`/`counter-increment` pair (value defaults to
/// 0 / 1 respectively when the declaration omits it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterOp {
    pub name: String,
    pub value: i32,
}

/// One generated box (R3): the resolved `content` text plus the pseudo's
/// own computed style (inherited from the generating element).
#[derive(Debug, Clone)]
pub struct PseudoStyle {
    pub text: String,
    pub style: ComputedStyle,
}

/// Base writing direction (Phase M3): inherited, drives the bidi paragraph
/// level and resolves `text-align: start/end` at style time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Ltr,
    Rtl,
}

/// B4 `writing-mode` (inherited): selects the subtree's inline axis. The
/// four non-default modes share one model in the v1 layout — the subtree
/// is laid out as a logical horizontal-tb box, then stamped with a rigid
/// rotation (clockwise for `-rl`/vertical, counter-clockwise for
/// `sideways-lr`). Glyph stance inside the rotated column is D6's
/// `text-orientation`; see docs/BROWSER_PARITY_PLAN.md (B4, D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WritingMode {
    #[default]
    HorizontalTb,
    VerticalRl,
    VerticalLr,
    SidewaysRl,
    SidewaysLr,
}

impl WritingMode {
    fn from_keyword(v: &str) -> Option<WritingMode> {
        Some(match v.trim().to_ascii_lowercase().as_str() {
            "horizontal-tb" => WritingMode::HorizontalTb,
            "vertical-rl" | "vertical" => WritingMode::VerticalRl,
            "vertical-lr" => WritingMode::VerticalLr,
            "sideways-rl" => WritingMode::SidewaysRl,
            "sideways-lr" => WritingMode::SidewaysLr,
            _ => return None,
        })
    }
}

/// D6 `text-orientation` (inherited; only meaningful inside a vertical
/// writing mode): how glyphs sit on the vertical axis. `Mixed` keeps CJK
/// upright while Latin rides the box rotation; `Upright` stands every
/// glyph up; `Sideways` is the plain rotated run the B4 stamp already
/// produces. Horizontal-tb text ignores it entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextOrientation {
    #[default]
    Mixed,
    Upright,
    Sideways,
}

impl TextOrientation {
    fn from_keyword(v: &str) -> Option<TextOrientation> {
        Some(match v.trim().to_ascii_lowercase().as_str() {
            "mixed" => TextOrientation::Mixed,
            "upright" => TextOrientation::Upright,
            "sideways" => TextOrientation::Sideways,
            _ => return None,
        })
    }
}

/// B4 v1 logical geometry: the `inline-*`/`block-*` longhand slots that
/// the `apply` arms fill without resolving an axis. [`fold_logical`] maps
/// them onto the physical fields once the element's final writing mode and
/// direction are known — the declarations may arrive in any order, so the
/// fold runs after the whole cascade. Like the physical properties it
/// aliases, it is never inherited.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LogicalGeometry {
    pub inline_size: Option<Len>,
    pub block_size: Option<Len>,
    pub min_inline_size: Option<Len>,
    pub max_inline_size: Option<Len>,
    pub min_block_size: Option<Len>,
    pub max_block_size: Option<Len>,
    pub inset_inline_start: Option<Len>,
    pub inset_inline_end: Option<Len>,
    pub inset_block_start: Option<Len>,
    pub inset_block_end: Option<Len>,
    pub margin_inline_start: Option<Len>,
    pub margin_inline_end: Option<Len>,
    pub margin_block_start: Option<Len>,
    pub margin_block_end: Option<Len>,
    pub padding_inline_start: Option<Len>,
    pub padding_inline_end: Option<Len>,
    pub padding_block_start: Option<Len>,
    pub padding_block_end: Option<Len>,
}

impl LogicalGeometry {
    fn is_empty(&self) -> bool {
        *self == LogicalGeometry::default()
    }
}

/// Fold the element's logical geometry slots onto the physical fields
/// (B4 v1). Mapping: the inline axis is x for `horizontal-tb` and y for
/// every vertical/sideways mode; the block axis is the other one. Start/
/// end follow `direction` along the inline axis (rtl swaps left/right in
/// horizontal-tb; the vertical modes treat inline-start as top as a
/// documented simplification). When both a logical slot and its physical
/// twin are set, the logical one wins — CSS states logical properties
/// override physical within the same declaration origin.
pub fn fold_logical(style: &mut ComputedStyle) {
    if style.logical.is_empty() {
        return;
    }
    let vert = style.writing_mode != WritingMode::HorizontalTb;
    let rtl = !vert && style.direction == Direction::Rtl;
    let l = &style.logical;
    if vert {
        if let Some(x) = l.inline_size {
            style.height = Some(SizeVal::Len(x));
        }
        if let Some(x) = l.block_size {
            style.width = Some(SizeVal::Len(x));
        }
        if let Some(x) = l.min_inline_size {
            style.min_height = Some(x);
        }
        if let Some(x) = l.max_inline_size {
            style.max_height = Some(x);
        }
        if let Some(x) = l.min_block_size {
            style.min_width = Some(x);
        }
        if let Some(x) = l.max_block_size {
            style.max_width = Some(x);
        }
    } else {
        if let Some(x) = l.inline_size {
            style.width = Some(SizeVal::Len(x));
        }
        if let Some(x) = l.block_size {
            style.height = Some(SizeVal::Len(x));
        }
        if let Some(x) = l.min_inline_size {
            style.min_width = Some(x);
        }
        if let Some(x) = l.max_inline_size {
            style.max_width = Some(x);
        }
        if let Some(x) = l.min_block_size {
            style.min_height = Some(x);
        }
        if let Some(x) = l.max_block_size {
            style.max_height = Some(x);
        }
    }
    // Offsets. Inline axis: vertical modes run top→bottom; horizontal
    // runs left→right, swapped under rtl. Block axis: vertical modes
    // stack right→left in the v1 rotation model, so block-start is the
    // right edge; horizontal keeps top.
    if let Some(x) = l.inset_inline_start {
        if vert {
            style.top = Some(x);
        } else if rtl {
            style.right = Some(x);
        } else {
            style.left = Some(x);
        }
    }
    if let Some(x) = l.inset_inline_end {
        if vert {
            style.bottom = Some(x);
        } else if rtl {
            style.left = Some(x);
        } else {
            style.right = Some(x);
        }
    }
    if let Some(x) = l.inset_block_start {
        if vert {
            style.right = Some(x);
        } else {
            style.top = Some(x);
        }
    }
    if let Some(x) = l.inset_block_end {
        if vert {
            style.left = Some(x);
        } else {
            style.bottom = Some(x);
        }
    }
    // Margins and paddings write named sides on `Edges` (plain `Len`s).
    let (mut ml, mut mr, mut mt, mut mb) = (
        style.margin.left,
        style.margin.right,
        style.margin.top,
        style.margin.bottom,
    );
    fold_edges(
        &mut ml,
        &mut mr,
        &mut mt,
        &mut mb,
        vert,
        rtl,
        (
            l.margin_inline_start,
            l.margin_inline_end,
            l.margin_block_start,
            l.margin_block_end,
        ),
    );
    style.margin = Edges {
        top: mt,
        right: mr,
        bottom: mb,
        left: ml,
    };
    let (mut pl, mut pr, mut pt, mut pb) = (
        style.padding.left,
        style.padding.right,
        style.padding.top,
        style.padding.bottom,
    );
    fold_edges(
        &mut pl,
        &mut pr,
        &mut pt,
        &mut pb,
        vert,
        rtl,
        (
            l.padding_inline_start,
            l.padding_inline_end,
            l.padding_block_start,
            l.padding_block_end,
        ),
    );
    style.padding = Edges {
        top: pt,
        right: pr,
        bottom: pb,
        left: pl,
    };
}

/// Assign one logical four-slot group (inline-start, inline-end,
/// block-start, block-end) onto the physical sides, respecting the
/// `vert`/`rtl` axis flags. An unset (`None`) slot leaves its twin alone.
fn fold_edges(
    left: &mut Len,
    right: &mut Len,
    top: &mut Len,
    bottom: &mut Len,
    vert: bool,
    rtl: bool,
    slots: (Option<Len>, Option<Len>, Option<Len>, Option<Len>),
) {
    let (is, ie, bs, be) = slots;
    match (vert, rtl) {
        (true, _) => {
            if let Some(x) = is {
                *top = x;
            }
            if let Some(x) = ie {
                *bottom = x;
            }
        }
        (false, false) => {
            if let Some(x) = is {
                *left = x;
            }
            if let Some(x) = ie {
                *right = x;
            }
        }
        (false, true) => {
            if let Some(x) = is {
                *right = x;
            }
            if let Some(x) = ie {
                *left = x;
            }
        }
    }
    if vert {
        if let Some(x) = bs {
            *right = x;
        }
        if let Some(x) = be {
            *left = x;
        }
    } else {
        if let Some(x) = bs {
            *top = x;
        }
        if let Some(x) = be {
            *bottom = x;
        }
    }
}

/// `text-transform` (Q3): case folding applied at layout before measuring,
/// so wrap widths and the display list both carry the transformed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTransform {
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

/// Inline vertical alignment (Phase M4): baseline shifts on inline runs.
/// Block-level keywords (`top`/`bottom`/`middle`/…) approximate as baseline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VerticalAlign {
    Baseline,
    Sub,
    Super,
    Length(Length),
    /// Percentage of the element's computed line height.
    Percent(f32),
}

/// One grid track sizing function.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridTrack {
    /// Fixed length (`100px`, `2em`, `20%` — resolved at layout).
    Fixed(Length),
    /// Fractional share of the remaining space (`1fr`, `2fr`).
    Flex(f32),
    /// Sized to its content (`auto`).
    Auto,
    /// C1b: sized to the smallest fit of its items (`min-content`).
    MinContent,
    /// C1b: sized to the largest fit of its items (`max-content`).
    MaxContent,
    /// C1b: `fit-content` / `fit-content(<len>)` — max-content clamped by
    /// the cap, floored by min-content.
    FitContent(Option<Length>),
    /// C6: `minmax(<min>, <max>)` — `min` floors, `max` sizes.
    MinMax(MinSizing, MaxTrack),
}

/// C6: the min side of `minmax()`. Kept flat (CSS forbids `minmax` inside
/// `minmax`) so `GridTrack` stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MinSizing {
    Len(Length),
    MinContent,
    MaxContent,
    Auto,
}

/// C6: the max side of `minmax()` — any track sizing function except
/// `minmax` itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MaxTrack {
    Fixed(Length),
    Flex(f32),
    Auto,
    MinContent,
    MaxContent,
    FitContent(Option<Length>),
}

/// C3: `break-before` / `break-after` (+ `page-break-*` aliases). The
/// engine has no page or column *splitting* yet (multicol blocks are
/// already atomic), so these are parsed and carried for print/H3 work.
/// Not inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BreakAction {
    #[default]
    Auto,
    Avoid,
    AvoidPage,
    AvoidColumn,
    Page,
    Column,
    Left,
    Right,
}

/// C3: `break-inside` (+ `page-break-inside`). Not inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BreakInside {
    #[default]
    Auto,
    Avoid,
    AvoidPage,
}

/// C3 keyword map; `always` (the legacy `page-break-*` spelling) is
/// accepted as `page` too.
fn break_action_kw(v: &str) -> Option<BreakAction> {
    Some(match v.trim().to_ascii_lowercase().as_str() {
        "auto" => BreakAction::Auto,
        "avoid" => BreakAction::Avoid,
        "avoid-page" => BreakAction::AvoidPage,
        "avoid-column" => BreakAction::AvoidColumn,
        "page" | "always" => BreakAction::Page,
        "column" => BreakAction::Column,
        "left" => BreakAction::Left,
        "right" => BreakAction::Right,
        _ => return None,
    })
}

fn break_inside_kw(v: &str) -> Option<BreakInside> {
    Some(match v.trim().to_ascii_lowercase().as_str() {
        "auto" => BreakInside::Auto,
        "avoid" => BreakInside::Avoid,
        "avoid-page" => BreakInside::AvoidPage,
        _ => return None,
    })
}

/// C5: `caption-side`, read on the `<caption>` box itself. `top` is the
/// only value that does anything else today (`left`/`block-start` map to
/// it, per the horizontal-tb shortcut). Not inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptionSide {
    #[default]
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFit {
    Fill,
    Contain,
    Cover,
    None,
    ScaleDown,
}

impl Default for ObjectFit {
    fn default() -> Self {
        ObjectFit::Fill
    }
}

/// C2: a parsed `aspect-ratio` — the width-to-height ratio a box is sized
/// against when one of its axes is `auto`, plus whether the `auto` keyword
/// was written alongside it.
///
/// `auto || <ratio>` (CSS Sizing 4 §4.1) means "this ratio, but a box that
/// has an intrinsic one may keep it": the keyword is the author declining to
/// override the natural ratio, and the ratio stays as the fallback for boxes
/// that have none (`<video>`, an empty flex item, a plain block). A bare
/// `<ratio>` always wins, which is what makes it able to reshape a replaced
/// element whose natural ratio says otherwise.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AspectRatio {
    /// `w / h` (e.g. `16 / 9` → 1.777…); always positive — the parser drops
    /// a zero or negative component rather than carrying an unusable ratio.
    pub ratio: f32,
    /// The `auto` keyword was present (`auto` alone clears the property
    /// entirely, so this is only ever set alongside a ratio).
    pub auto_keyword: bool,
}

/// C2: split an `aspect-ratio` value into `(auto keyword present, rest)`.
/// `auto` on its own is the whole value (it clears the property); otherwise
/// it is a prefix, ASCII case-insensitive like every CSS keyword, followed by
/// whitespace. A word that merely starts with `auto` is not the keyword, so
/// `autox` falls through to the ratio parse and is rejected there.
fn split_aspect_auto(v: &str) -> (bool, &str) {
    let t = v.trim();
    if t.eq_ignore_ascii_case("auto") {
        return (true, "");
    }
    match t.get(..4) {
        Some(head) if head.eq_ignore_ascii_case("auto") => (true, t[4..].trim_start()),
        _ => (false, t),
    }
}

/// Per-corner box radii as authored lengths (`%` resolves against the box
/// width at layout; documented approximation for the elliptical axis).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BorderRadius {
    pub top_left: Length,
    pub top_right: Length,
    pub bottom_right: Length,
    pub bottom_left: Length,
}

impl BorderRadius {
    pub const ZERO: BorderRadius = BorderRadius {
        top_left: Length::Px(0.0),
        top_right: Length::Px(0.0),
        bottom_right: Length::Px(0.0),
        bottom_left: Length::Px(0.0),
    };

    pub fn uniform(l: Length) -> BorderRadius {
        BorderRadius {
            top_left: l,
            top_right: l,
            bottom_right: l,
            bottom_left: l,
        }
    }

    /// Resolve all corners to px against `basis` (the box width).
    pub fn resolve(&self, em: f32, rem: f32, basis: f32) -> [f32; 4] {
        [
            self.top_left.resolve(em, rem, basis).max(0.0),
            self.top_right.resolve(em, rem, basis).max(0.0),
            self.bottom_right.resolve(em, rem, basis).max(0.0),
            self.bottom_left.resolve(em, rem, basis).max(0.0),
        ]
    }
}

/// One gradient color stop: `offset` 0.0–1.0 along the gradient line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorStop {
    pub offset: f32,
    pub color: u32, // 0xRRGGBBAA
}

/// Background paint model (Phase L4 data model, N4 parsing): the resolved
/// `background` u32 stays the CPU-approximation fast path (solid color, or
/// the gradient's first stop when paint is a gradient, transparent for
/// images); accurate backends read `background_paint`.
#[derive(Debug, Clone, PartialEq)]
pub enum BackgroundPaint {
    Color(u32),
    LinearGradient {
        /// Clockwise degrees from "to top" (CSS `0deg` = pointing up).
        angle_deg: f32,
        stops: Vec<ColorStop>,
    },
    RadialGradient {
        /// Center as fractions of the box, radius in px when `Some`
        /// (farthest-corner at paint time when `None`).
        cx: f32,
        cy: f32,
        radius: Option<f32>,
        stops: Vec<ColorStop>,
    },
    /// B5 `conic-gradient([from angle]? [at x y]?, stops)`: stops sweep
    /// clockwise around the center starting from the given angle.
    ConicGradient {
        /// Clockwise degrees from "to top" (CSS `0deg` = pointing up).
        angle_deg: f32,
        cx: f32,
        cy: f32,
        stops: Vec<ColorStop>,
    },
    /// Unresolved `url()` (resolved against the page URL at paint).
    Image(String),
}

impl Default for BackgroundPaint {
    fn default() -> Self {
        BackgroundPaint::Color(0)
    }
}

/// `background-position` on one axis: a keyword edge/center, a percentage
/// of the free space, or a length offset from the keyword's edge.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BgPos {
    /// `left` / `top` — 0% of the free space.
    #[default]
    Start,
    /// `center` — 50% of the free space.
    Center,
    /// `right` / `bottom` — 100% of the free space.
    End,
    /// `%` of (positioning area − image) along the axis.
    Percent(f32),
    /// A length from the `left`/`top` edge.
    Offset(Length),
    /// A length measured inward from the `right`/`bottom` edge — the
    /// `background-position: right 10px` form.
    Inset(Length),
}

/// `background-position`: the horizontal then vertical offset of the image
/// inside its positioning area.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BackgroundPosition {
    pub x: BgPos,
    pub y: BgPos,
}

/// `background-size`: the painted image size, per axis.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BgSize {
    /// `auto` — the image's intrinsic size (or its ratio against the other
    /// axis when that one is definite).
    #[default]
    Auto,
    /// A length (resolved at layout against the element's font size, like
    /// every other length this engine carries symbolically).
    Length(Length),
    /// A percentage of the positioning area on that axis.
    Percent(f32),
}

/// `background-size`: `<w> <h>`, one value (h = `auto`), or the
/// `cover` / `contain` keywords.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BackgroundSize {
    #[default]
    Auto,
    /// `cover` / `contain`: the largest/smallest size that covers/contains
    /// the positioning area at the image's intrinsic ratio.
    Keyword(BgSizeKeyword),
    Axes(BgSize, BgSize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgSizeKeyword {
    Cover,
    Contain,
}

/// `background-repeat`: tiling along each axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackgroundRepeat {
    pub x: BgRepeat,
    pub y: BgRepeat,
}

/// `background-origin` / `background-clip`: which CSS box a background layer
/// is positioned in (origin) or painted inside (clip). Both default to the
/// border box and are not inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BgBox {
    #[default]
    BorderBox,
    PaddingBox,
    ContentBox,
}

impl BgBox {
    /// The `border-box` / `padding-box` / `content-box` keywords.
    pub fn from_keyword(kw: &str) -> Option<BgBox> {
        match kw {
            "border-box" => Some(BgBox::BorderBox),
            "padding-box" => Some(BgBox::PaddingBox),
            "content-box" => Some(BgBox::ContentBox),
            _ => None,
        }
    }
}

/// One layer of a `background` layer list (CSS Backgrounds 3 §3). The
/// shorthand takes a comma-separated list of these, and each layer carries
/// its own image, color, placement and origin/clip box.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BackgroundLayer {
    /// The layer's image: `none`, a gradient, or an unresolved `url()`.
    pub paint: BackgroundPaint,
    /// The solid color painted under the layer (its `<color>`, transparent
    /// when absent) — also the CPU fast path, and the gradient's first stop
    /// when [`Self::paint`] is a gradient. Mirrors what
    /// [`ComputedStyle::background`] carries for the whole box.
    pub color: u32,
    /// Where the image sits in the positioning area (`background-origin`
    /// picks the area).
    pub position: BackgroundPosition,
    /// The painted image size.
    pub size: BackgroundSize,
    /// Tiling per axis.
    pub repeat: BackgroundRepeat,
    /// The box the image is positioned in.
    pub origin: BgBox,
    /// The box the layer is painted inside.
    pub clip: BgBox,
}

impl BackgroundLayer {
    /// True when the layer's paint is a gradient (a solid color and an image
    /// both leave the rect to the layer's `color` backdrop).
    pub fn is_gradient(&self) -> bool {
        layer_is_gradient(&self.paint)
    }

    /// The unresolved `url()` of an image layer.
    pub fn image_src(&self) -> Option<&str> {
        match &self.paint {
            BackgroundPaint::Image(src) => Some(src.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BgRepeat {
    #[default]
    Repeat,
    NoRepeat,
    /// `space` / `round`: tiles are laid down with gaps (never clipped) or
    /// resized (never gapped) so a whole number fills the axis.
    Space,
    Round,
}

/// One `box-shadow` entry (outset unless `inset`; blur is a Gaussian radius
/// in px, spread expands the base rect).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: u32, // 0xRRGGBBAA
    pub inset: bool,
}

/// One `text-shadow` (R8): offset/blur in px plus the shadow color. Blur is
/// parsed but painted crisp in v1 (duplicate runs — no per-run blur field
/// on the wire).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub color: u32, // 0xRRGGBBAA
}

/// Timing function for declarative animations (Phase N8).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Timing {
    Linear,
    Ease,
    EaseIn,
    EaseOut,
    EaseInOut,
    Bezier(f32, f32, f32, f32),
    /// `steps(n)`: `n` discrete jumps (end position).
    Steps(u32),
}

impl Timing {
    /// Map a linear progress 0–1 through the curve.
    pub fn apply(&self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match *self {
            Timing::Linear => t,
            Timing::Ease => cubic_bezier(0.25, 0.1, 0.25, 1.0, t),
            Timing::EaseIn => cubic_bezier(0.42, 0.0, 1.0, 1.0, t),
            Timing::EaseOut => cubic_bezier(0.0, 0.0, 0.58, 1.0, t),
            Timing::EaseInOut => cubic_bezier(0.42, 0.0, 0.58, 1.0, t),
            Timing::Bezier(x1, y1, x2, y2) => cubic_bezier(x1, y1, x2, y2, t),
            Timing::Steps(n) => {
                if n == 0 {
                    return 1.0;
                }
                ((t * n as f32).floor() / n as f32).min(1.0)
            }
        }
    }
}

/// Solve y for a cubic Bézier easing curve at progress x (Newton iterations
/// with a subdivision fallback — the standard CSS timing solver).
fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let (ax, bx, cx) = (3.0 * x1 - 3.0 * x2 + 1.0, 3.0 * x2 - 6.0 * x1, 3.0 * x1);
    let (ay, by, cy) = (3.0 * y1 - 3.0 * y2 + 1.0, 3.0 * y2 - 6.0 * y1, 3.0 * y1);
    let sample_x = |t: f32| ((ax * t + bx) * t + cx) * t;
    let sample_dx = |t: f32| (3.0 * ax * t + 2.0 * bx) * t + cx;
    // Newton.
    let mut t = x;
    for _ in 0..8 {
        let err = sample_x(t) - x;
        if err.abs() < 1e-5 {
            return ((ay * t + by) * t + cy) * t;
        }
        let d = sample_dx(t);
        if d.abs() < 1e-5 {
            break;
        }
        t = (t - err / d).clamp(0.0, 1.0);
    }
    // Subdivision fallback.
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    t = x;
    while (sample_x(t) - x).abs() > 1e-5 && hi - lo > 1e-6 {
        if sample_x(t) < x {
            lo = t;
        } else {
            hi = t;
        }
        t = (lo + hi) * 0.5;
    }
    ((ay * t + by) * t + cy) * t
}

/// Animation playback direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimDirection {
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

/// Iteration count.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IterationCount {
    Infinite,
    Count(f32),
}

/// Fill mode (parsed and kept; sampling has no fill model — overrides apply
/// only during the active interval, documented).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillMode {
    None,
    Forwards,
    Backwards,
    Both,
}

/// One declarative animation (Phase N8): the engine parses, the embedder
/// interpolates per tick via [`sample_animation`] and republishes.
#[derive(Debug, Clone, PartialEq)]
pub struct Animation {
    pub name: String,
    pub duration_secs: f32,
    pub delay_secs: f32,
    pub timing: Timing,
    pub iteration: IterationCount,
    pub direction: AnimDirection,
    pub fill: FillMode,
    /// `false` freezes the clock (play-state: paused handled embedder-side).
    pub running: bool,
}

impl Default for Animation {
    fn default() -> Self {
        Animation {
            name: String::new(),
            duration_secs: 0.0,
            delay_secs: 0.0,
            timing: Timing::Ease,
            iteration: IterationCount::Count(1.0),
            direction: AnimDirection::Normal,
            fill: FillMode::None,
            running: true,
        }
    }
}

/// One entry of a `transition-*` list (B1): which property eases, how long,
/// and with what curve. `property` is the raw declared name — `all` (empty
/// string is also `all`) matches any changed property; per-longhand values
/// keep their real names. Durations default to 0s (`none` → instant).
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub property: String,
    pub duration_secs: f32,
    pub delay_secs: f32,
    pub timing: Timing,
}

impl Default for Transition {
    fn default() -> Self {
        Transition {
            property: "all".to_string(),
            duration_secs: 0.0,
            delay_secs: 0.0,
            timing: Timing::Ease,
        }
    }
}

/// Match a declared `transition-property` entry against a changed property.
/// `all` (or empty) matches anything; names compare verbatim (case-insensitive).
pub fn transition_matches(prop: &str, changed: &str) -> bool {
    if prop.is_empty() || prop.eq_ignore_ascii_case("all") {
        return true;
    }
    prop.eq_ignore_ascii_case(changed)
}

#[derive(Debug, Clone, Copy)]
pub struct Edges {
    pub top: Len,
    pub right: Len,
    pub bottom: Len,
    pub left: Len,
}

impl Edges {
    pub const ZERO: Edges = Edges {
        top: Len::ZERO,
        right: Len::ZERO,
        bottom: Len::ZERO,
        left: Len::ZERO,
    };

    /// Resolve against a basis (the containing-block width — CSS resolves
    /// percentage margins/paddings against width on every side).
    pub fn resolve(&self, basis: f32) -> EdgesPx {
        EdgesPx {
            top: self.top.resolve(basis),
            right: self.right.resolve(basis),
            bottom: self.bottom.resolve(basis),
            left: self.left.resolve(basis),
        }
    }
}

/// Fully resolved [`Edges`] in pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EdgesPx {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

/// `border-style` (R1). `solid` paints exactly; `dashed`/`dotted` paint as
/// segment runs (the display list is rect-based); the 3D styles fall back
/// to solid; `none`/`hidden` paint nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderStyle {
    #[default]
    None,
    Hidden,
    Solid,
    Dashed,
    Dotted,
    /// 3D/decorative styles, painted solid in v1.
    Double,
    Groove,
    Ridge,
    Inset,
    Outset,
}

impl BorderStyle {
    pub fn paints(&self) -> bool {
        !matches!(self, BorderStyle::None | BorderStyle::Hidden)
    }
}

/// B12 `text-decoration-style`: how the decoration lines are drawn.
/// Dashed/dotted paint as segment runs (the display list is rect-based,
/// like R1 borders); wavy paints as a zigzag of rotated segments;
/// double as two parallel lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecorationStyle {
    #[default]
    Solid,
    Double,
    Dotted,
    Dashed,
    Wavy,
}

/// B13 `overflow-wrap`: whether the engine may break inside a word when it
/// would otherwise overflow its line. `BreakWord` only splits words that
/// lead a fresh line (the word still tries to fit whole first);
/// `Anywhere` additionally lets the split affect min-content sizing —
/// layout treats them alike (no min-content pass yet, documented).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverflowWrap {
    #[default]
    Normal,
    BreakWord,
    Anywhere,
}

/// B13 `word-break`: `BreakAll` breaks anywhere between any two
/// characters (Latin text wraps mid-word); `KeepAll` forbids breaks
/// inside CJK — our UAX #14 segmentation already keeps CJK words whole
/// per character, so `KeepAll` is accepted and behaves like `Normal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WordBreak {
    #[default]
    Normal,
    BreakAll,
    KeepAll,
}

/// B13 `text-overflow`: how inline content that overflows its line box is
/// signalled. `Ellipsis` truncates the overflowing tail with `…` — applied
/// on nowrap single-line text (the dominant real-world use); wrapping
/// multi-line ellipsis is deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextOverflow {
    #[default]
    Clip,
    Ellipsis,
}

/// B13 `hyphens`: `Manual` (the CSS initial value) breaks only at
/// U+00AD soft hyphens — the soft hyphen is invisible mid-line and becomes
/// a visible `-` when the break lands at line end. `Auto` would need a
/// dictionary; it behaves as `Manual` (documented). `None` renders the
/// soft hyphen as a normal visible character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hyphens {
    #[default]
    Manual,
    None,
    Auto,
}

/// `visibility`: whether a box paints. Inherited, and a descendant may
/// set `visible` to paint again inside a hidden subtree. `hidden` keeps
/// the box's layout; `collapse` is `hidden` everywhere except on table
/// rows/row-groups (and flex items), where it also removes the box from
/// layout — the row-removal half is implemented, the flex half is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
    Collapse,
}

/// One side's border (R1): width/style/color. `color: None` means
/// `currentColor` — resolved at layout against the element's color.
/// Initial width is `medium` (3px) per CSS; the side is invisible while
/// the style is `none`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderSide {
    pub width: Len,
    pub style: BorderStyle,
    pub color: Option<u32>,
}

impl Default for BorderSide {
    fn default() -> Self {
        BorderSide {
            width: Len::const_px(3.0),
            style: BorderStyle::None,
            color: None,
        }
    }
}

/// One border edge after resolution (layout-facing): px width, paint color
/// (currentColor already substituted) and style.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderSidePx {
    pub width: f32,
    pub style: BorderStyle,
    pub color: u32,
}

/// All four border sides (R1). Not inherited.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Border {
    pub top: BorderSide,
    pub right: BorderSide,
    pub bottom: BorderSide,
    pub left: BorderSide,
}

impl Default for Border {
    fn default() -> Self {
        Border {
            top: BorderSide::default(),
            right: BorderSide::default(),
            bottom: BorderSide::default(),
            left: BorderSide::default(),
        }
    }
}

impl Border {
    /// Resolve every side: percentage/em widths against the containing-block
    /// `basis`, `None` colors against the element's `color`. A side whose
    /// style paints nothing resolves to width 0 — the initial `medium` width
    /// must not eat layout space while the border is invisible.
    pub fn resolve(&self, basis: f32, color: u32) -> [BorderSidePx; 4] {
        // Order: top, right, bottom, left.
        [
            (&self.top, basis),
            (&self.right, basis),
            (&self.bottom, basis),
            (&self.left, basis),
        ]
        .map(|(s, b)| BorderSidePx {
            width: if s.style.paints() {
                s.width.resolve(b)
            } else {
                0.0
            },
            style: s.style,
            color: s.color.unwrap_or(color),
        })
    }

    pub fn any_visible(&self) -> bool {
        [self.top, self.right, self.bottom, self.left]
            .iter()
            .any(|s| s.style.paints())
    }
}

/// One translate/origin length after style resolution (Q2): px folded
/// (em/rem resolved against the element font size), percentage kept
/// symbolic — it resolves at layout against the element's own border box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransLen {
    pub px: f32,
    pub pct: f32,
}

impl TransLen {
    pub const ZERO: TransLen = TransLen { px: 0.0, pct: 0.0 };
    /// Pixels given the border-box dimension the percentage resolves against.
    pub fn resolve(self, box_px: f32) -> f32 {
        self.px + self.pct / 100.0 * box_px
    }
}

/// One function of a CSS `transform` list (Q2). Angles are degrees. The
/// `Matrix` payload is row-major [m0..m5]:
/// x' = m0·x + m1·y + m2, y' = m3·x + m4·y + m5 — CSS
/// `matrix(a,b,c,d,e,f)` stores as [a, c, e, b, d, f].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransformFn {
    Translate {
        x: TransLen,
        y: TransLen,
    },
    TranslateX(TransLen),
    TranslateY(TransLen),
    Scale(f32, f32),
    ScaleX(f32),
    ScaleY(f32),
    /// Clockwise degrees (screen space, y down).
    Rotate(f32),
    /// Horizontal then vertical angle, both degrees.
    Skew(f32, f32),
    SkewX(f32),
    SkewY(f32),
    Matrix([f32; 6]),
}

/// A parsed `transform` value (`none` → no property at all).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TransformList(pub Vec<TransformFn>);

/// `transform-origin`: px folded, percentages symbolic against the border
/// box. Initial value 50% 50%.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransformOrigin {
    pub x: TransLen,
    pub y: TransLen,
}

impl Default for TransformOrigin {
    fn default() -> Self {
        TransformOrigin {
            x: TransLen { px: 0.0, pct: 50.0 },
            y: TransLen { px: 0.0, pct: 50.0 },
        }
    }
}

/// Everything layout and paint need about a node.
#[derive(Debug, Clone)]
pub struct ComputedStyle {
    pub display: Display,
    pub color: u32,
    pub background: u32,
    pub fill: Option<u32>,
    pub stroke: Option<u32>,
    pub font_size: f32,
    pub font_weight: u32,
    pub italic: bool,
    pub underline: bool,
    /// `text-decoration: overline` (R2): painted above the ascent.
    pub overline: bool,
    /// `text-decoration: line-through` (R2): painted at the midline.
    pub line_through: bool,
    /// B12 `text-decoration-color`: `None` = `currentColor` (the element's
    /// text color, resolved at paint).
    pub decoration_color: Option<u32>,
    /// B12 `text-decoration-thickness`: `None` = `auto` (the painter picks a
    /// font-relative width). Percentages resolve to 0 → auto at paint.
    pub decoration_thickness: Option<Len>,
    /// B12 `text-decoration-style`: how the lines are drawn.
    pub decoration_style: DecorationStyle,
    /// `visibility` (inherited): whether this box paints. Layout drops the
    /// painted items of every node that computes to hidden/collapse, so a
    /// descendant that sets `visible` paints again.
    pub visibility: Visibility,
    /// B13 `overflow-wrap` / legacy `word-wrap` (inherited).
    pub overflow_wrap: OverflowWrap,
    /// B13 `word-break` (inherited).
    pub word_break: WordBreak,
    /// B13 `text-overflow` (inherited).
    pub text_overflow: TextOverflow,
    /// B13 `hyphens` (inherited).
    pub hyphens: Hyphens,
    pub monospace: bool,
    /// First non-generic family from `font-family` (Q11): the measurer
    /// resolves it against registered `@font-face` faces, then falls back.
    /// Empty = no explicit family (host picks its default). Generic keywords
    /// (`serif`, `monospace`, …) stay empty here — the monospace flag and the
    /// host defaults already cover them.
    pub font_family: String,
    /// D3: the element's language (nearest `lang`/`xml:lang`, lowercased,
    /// inherited like CSS would): drives `:lang()` matching, hyphenation
    /// dictionary selection (D5) and language-aware font fallback.
    pub lang: Option<String>,
    /// D4 `font-variant` / `-caps` / `-ligatures` keywords translated to
    /// OpenType tag requests (applied BEFORE `font-feature-settings`,
    /// which wins on a tag conflict). Inherited.
    pub variant_features: Vec<(String, u32)>,
    /// D4 `font-feature-settings` — low-level OpenType `(tag, value)`
    /// pairs applied last. Inherited.
    pub font_features: Vec<(String, u32)>,
    /// D4: small-caps request (`small-caps`, `all-small-caps`, `petite-caps`,
    /// `unicase`). Hosts may synthesize scaled capitals when the chosen
    /// font lacks the `smcp`/`pcap` feature. Inherited.
    pub small_caps: bool,
    pub text_align: TextAlign,
    pub line_height: f32, // multiplier
    pub margin: Edges,
    pub padding: Edges,
    /// Borders (R1): per-side width/style/color, painted at the box edges.
    /// Not inherited.
    pub border: Border,
    /// `outline` (R1): painted around the border edge; never affects layout.
    pub outline: BorderSide,
    /// `outline-offset` (R1): px outward from the border edge.
    pub outline_offset: Len,
    /// `content` (R3): raw spec; only meaningful on ::before/::after.
    pub content: Option<ContentSpec>,
    /// R3: `::before` generated box — text resolved at style time.
    pub before: Option<Box<PseudoStyle>>,
    /// R3: `::after` generated box.
    pub after: Option<Box<PseudoStyle>>,
    /// B14 `list-style-type` (inherited): marker of a `display: list-item`
    /// box and default style for `counter()`.
    pub list_style_type: ListStyleType,
    /// B14 `list-style-position` (inherited).
    pub list_style_position: ListStylePosition,
    /// B14 `counter-reset` (not inherited): scopes pushed for children.
    pub counter_reset: Vec<CounterOp>,
    /// B14 `counter-increment` (not inherited).
    pub counter_increment: Vec<CounterOp>,
    /// B14: the element's resolved list marker text, stamped during the
    /// cascade once counters are known (numbered types get `"N."`, symbol
    /// types the glyph). `None` = no marker. Layout consumes this.
    pub marker_text: Option<String>,
    /// B14 internal flag: a `list-style-type`/`list-style` declaration
    /// applied to this element (or inherited from one), so the HTML `type`
    /// attribute and the ol numbering heuristic must not override it.
    pub list_type_author: bool,
    /// Symbolic lengths (Q1): percentages resolve at layout against the
    /// containing-block width. C1: `width`/`height` carry the intrinsic
    /// keywords (`min-content`, `max-content`, `fit-content`); the
    /// min/max-* clamps stay plain lengths.
    pub width: Option<SizeVal>,
    pub height: Option<SizeVal>,
    /// C2: `aspect-ratio` — the ratio used when `height` is `auto` (and, for
    /// a replaced-ish box, when `width` is). `None` = unset, or `auto` with
    /// no ratio. Applies to the content box; padding + borders add back.
    pub aspect_ratio: Option<AspectRatio>,
    pub max_width: Option<Len>,
    pub min_width: Option<Len>,
    pub min_height: Option<Len>,
    pub max_height: Option<Len>,
    /// C3 fragmentation: `break-before`/`break-after`/`break-inside`
    /// (+ `page-break-*` aliases). Parsed and carried; consumed once the
    /// engine gains page/column splitter machinery (H3). Not inherited.
    pub break_before: BreakAction,
    pub break_after: BreakAction,
    pub break_inside: BreakInside,
    /// C3 `orphans`/`widows` (inherited, default 2): minimum lines kept at
    /// the bottom of a fragment / top of the next. Multicol keeps blocks
    /// atomic today, so these apply when text fragmentation lands.
    pub orphans: u32,
    pub widows: u32,
    /// C5: `border-collapse` — `true` = collapsed grid (spacing forced to
    /// zero, shared borders are approximated by abutting cell borders;
    /// full conflict resolution is deferred). Inherited, like CSS.
    pub border_collapse: bool,
    /// C5: `border-spacing: <h> [<v>]` in px; lengths only (a percentage
    /// or negative makes the declaration invalid). Only meaningful under
    /// `border-collapse: separate`. Inherited.
    pub border_spacing: (f32, f32),
    /// C5: `caption-side`, consulted on the `<caption>` box by the table
    /// layout. Not inherited.
    pub caption_side: CaptionSide,
    pub white_space_pre: bool,
    /// `white-space: pre-wrap` — pre newlines preserved AND long lines wrap
    /// (used by the QTSQ text/JSON views so nothing clips).
    pub white_space_wrap: bool,
    /// B13: `white-space: nowrap` — suppresses wrapping in the normal
    /// (non-pre) flow; the flag `white_space_wrap` alone can't express it
    /// because the default `normal` value also leaves it false.
    pub white_space_nowrap: bool,
    /// `a[href]` target when this node is a link.
    pub link_href: Option<String>,
    pub position: Position,
    pub top: Option<Len>,
    pub right: Option<Len>,
    pub bottom: Option<Len>,
    pub left: Option<Len>,
    pub float: FloatSide,
    pub clear: Clear,
    pub z_index: Option<i32>,
    pub overflow: Overflow,
    pub opacity: f32,
    pub flex_direction: FlexDirection,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    /// C1c: carries the intrinsic keywords (`min-content`, `max-content`,
    /// `fit-content`) alongside lengths.
    pub flex_basis: Option<SizeVal>,
    pub align_items: AlignItems,
    pub justify_content: JustifyContent,
    /// `flex-wrap` (Q4): single-line flex when `Now`.
    pub flex_wrap: FlexWrap,
    /// `align-content` (Q4): leftover cross-space distribution across lines.
    pub align_content: AlignContent,
    pub gap: Len,
    pub grid_template_columns: Vec<GridTrack>,
    pub grid_template_rows: Vec<GridTrack>,
    pub grid_column_span: u32,
    pub grid_row_span: u32,
    /// B5 `grid-template-areas`: raw cell names per row (`.` = empty).
    pub grid_template_areas: Vec<Vec<String>>,
    /// B5 item placement: `grid-area: <name>` plus the four longhands.
    /// Values are lowercase: `""` = auto, an area/line name, an integer
    /// line number, or `span N` (start longhands with `span N` fold into
    /// the existing `grid_*_span` instead).
    pub grid_area: String,
    pub grid_column_start: String,
    pub grid_column_end: String,
    pub grid_row_start: String,
    pub grid_row_end: String,
    /// B5 named lines from `[name]` tokens in the templates: (line index,
    /// name). Sparse; line i sits before track i.
    pub grid_line_names_col: Vec<(usize, String)>,
    pub grid_line_names_row: Vec<(usize, String)>,
    /// C6 `grid-auto-flow: column` — parsed and carried; placement stays
    /// row-major (implicit tracks along the column axis are not built yet).
    pub grid_flow_column: bool,
    /// C6 `grid-auto-flow: dense` — auto-placement backfills earlier holes.
    /// The default (sparse) advances a monotonic cursor instead.
    pub grid_flow_dense: bool,
    /// C6 `grid-template-columns: subgrid` — the child grid aligns its tracks
    /// to the nearest grid ancestor's column template.
    pub grid_subgrid_columns: bool,
    /// C6 `grid-template-rows: subgrid` — parsed; row placement is deferred.
    pub grid_subgrid_rows: bool,
    pub object_fit: ObjectFit,
    pub border_radius: BorderRadius,
    pub background_paint: BackgroundPaint,
    /// `background-position` — where an image sits inside its positioning
    /// area (the border box here). Unused for colors/gradients.
    pub background_position: BackgroundPosition,
    /// `background-size` — the painted image size. Unused for
    /// colors/gradients (they always fill the box).
    pub background_size: BackgroundSize,
    /// `background-repeat` — tiling per axis. Unused for
    /// colors/gradients.
    pub background_repeat: BackgroundRepeat,
    /// The `background` layer list, in CSS order: index 0 paints on TOP of
    /// index 1, and the last entry paints at the bottom (CSS Backgrounds 3
    /// §3.1).
    ///
    /// An EMPTY list is the initial value — the one transparent `border-box`
    /// layer the scalar fields above already describe — so a style no
    /// background declaration ever touched allocates nothing. Every writer in
    /// this crate materializes the list, so a list is non-empty exactly when
    /// some `background` declaration ran.
    ///
    /// The scalar fields above (`background`, `background_paint`,
    /// `background_position`, `background_size`, `background_repeat`) mirror
    /// the **topmost** layer — the one the CPU fast path and the legacy
    /// single-layer APIs read. Layout paints the whole list.
    pub background_layers: Vec<BackgroundLayer>,
    pub box_shadows: Vec<BoxShadow>,
    /// `text-shadow` (R8): inherited; painted as duplicate runs.
    pub text_shadows: Vec<TextShadow>,
    pub direction: Direction,
    pub vertical_align: VerticalAlign,
    /// `text-transform` (Q3): layout applies it to text before measuring.
    pub text_transform: TextTransform,
    /// `letter-spacing` in px (Q3): extra advance after each glyph except the
    /// last. 0.0 = normal.
    pub letter_spacing: f32,
    /// `transform` (Q2): parsed function list; percentage translates
    /// resolve at layout against the element's own border box. Not inherited.
    pub transform: Option<TransformList>,
    /// `transform-origin` (Q2): percentages resolve against the border box;
    /// initial 50% 50%. Not inherited.
    pub transform_origin: TransformOrigin,
    /// B4 `filter`: visual effect applied to the element's rendered output
    /// (blur, drop-shadow, color-matrix, hue-rotate, etc.). `None` is the
    /// initial value (`filter: none`). Not inherited.
    pub filter: Option<FilterList>,
    /// B4 `backdrop-filter`: same function set, but the input is what's
    /// already painted behind the element. The host needs a real
    /// backdrop layer for this (CABackdropLayer / private APIs on macOS,
    /// `backdrop-filter` on the WebKit side); we ship the parsed value
    /// through the wire and let the host decide. Not inherited.
    pub backdrop_filter: Option<FilterList>,
    /// B7 `clip-path`: a basic shape clipping the element's painted box.
    /// Resolved against the border box at layout. `clip-path` wins over
    /// `mask` when both are set (single clip on the wire). Not inherited.
    pub clip_path: Option<ClipShape>,
    /// B7 `mask` / `mask-image`: shape layers are approximated as a clip
    /// (shape interior = visible); gradient/url layers yield `None`. Not
    /// inherited.
    pub mask: Option<ClipShape>,
    /// B8 `mix-blend-mode`: how this element's painted items composite
    /// against the backdrop. Layout stamps the mode onto every item the
    /// element (and, until its own value overrides, its subtree) paints;
    /// the host maps `wire()` to `CGBlendMode`. Not inherited.
    pub mix_blend_mode: MixBlend,
    /// B8 `isolation: isolate`: paint the subtree into an isolated group
    /// so descendant blends never reach the page backdrop. Layout stamps
    /// a group id on every item in the subtree. Not inherited.
    pub isolate: bool,
    /// E5 `will-change`: hint that the subtree is composited on its own
    /// layer. Layout stamps a distinct group id (same mechanism as
    /// `isolate`) so painters can cache the subtree's raster. Not inherited.
    pub will_change: bool,
    /// B5 multi-column: `column-count` (`None` = `auto`), `column-width`
    /// (`None` = `auto`) and `column-gap` (`None` = `normal`, which the
    /// layout folds to 1em). A count alone fixes the columns; a width
    /// alone estimates the count from the available inline size; both
    /// together mean "up to `count`, at least `width` wide". Not inherited.
    pub column_count: Option<u32>,
    pub column_width: Option<Len>,
    pub column_gap: Option<Len>,
    /// B4 `writing-mode` (inherited): the axis model for this subtree.
    /// Non-horizontal modes make layout rotate a logical horizontal pass
    /// (see kitae_layout `layout_rotated_box`).
    pub writing_mode: WritingMode,
    /// D6 `text-orientation` (inherited): glyph stance inside vertical
    /// writing modes — CJK upright vs per-char upright vs sideways.
    /// Horizontal-tb text ignores it.
    pub text_orientation: TextOrientation,
    /// B4 v1: logical geometry slots (not inherited), folded into the
    /// physical fields by [`fold_logical`] after the cascade.
    pub logical: LogicalGeometry,
    /// Declarative animations (Phase N8, comma lists welcome).
    pub animations: Vec<Animation>,
    /// `transition-*` (B1): what to interpolate when computed values change.
    /// Not inherited.
    pub transitions: Vec<Transition>,
    /// B3 `container-type` (not inherited): makes the element a query
    /// container for its descendants. `InlineSize` publishes its inline
    /// (laid-out) width; `Size` publishes width and height.
    pub container_type: ContainerType,
    /// B3 `container-name` (not inherited): names this container for
    /// `@container name (…)` lookups.
    pub container_names: Vec<String>,
    /// Computed custom properties (`--name` → value): inherited defaults
    /// overlaid with this element's own declarations, in cascade order.
    pub vars: Vec<(String, String)>,
    /// B15 `overscroll-behavior` (per axis, not inherited): behavior at a
    /// scroll container's boundary. Parsed and stored for the host; no
    /// host-side chaining yet (documented, like `backdrop-filter`).
    pub overscroll_x: OverscrollBehavior,
    pub overscroll_y: OverscrollBehavior,
    /// B15 `scroll-snap-type` (not inherited): snap axis; `None` disables
    /// snapping. `snap_proximity` carries the strictness keyword.
    pub snap_axis: SnapAxis,
    /// B15: `scroll-snap-type: … proximity` (false = `mandatory`, the
    /// spec default whenever an axis is set).
    pub snap_proximity: bool,
    /// B15 `scroll-snap-align` (not inherited): alignment on the
    /// container's snap points, `(x, y)` per axis.
    pub snap_align: (SnapAlign, SnapAlign),
}

/// B15 `overscroll-behavior` values. `pan-x`/`pan-y`/`pinch-zoom` (legacy
/// pointer-events-style keywords) resolve to `Auto` (documented).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverscrollBehavior {
    #[default]
    Auto,
    Contain,
    None,
}

/// B15 `scroll-snap-type` axis. `x`/`inline` → `X`, `y`/`block` → `Y`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapAxis {
    #[default]
    None,
    X,
    Y,
    Both,
}

/// B15 `scroll-snap-align` per-axis keyword. Spec initial value is `none`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapAlign {
    #[default]
    None,
    Start,
    Center,
    End,
}

/// B3 `container-type`: which element sizes a descendant's `@container`
/// rule may query. `None` = the element is not a query container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContainerType {
    #[default]
    None,
    /// Inline-size containment: queries see the laid-out width only.
    InlineSize,
    /// Size containment: queries see width and height (same published
    /// width here; height recorded for future use, documented).
    Size,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        ComputedStyle {
            display: Display::Inline,
            color: 0x1A1D23FF,
            background: 0,
            fill: Some(0x000000FF),
            stroke: None,
            font_size: 16.0,
            font_weight: 400,
            italic: false,
            underline: false,
            overline: false,
            line_through: false,
            decoration_color: None,
            decoration_thickness: None,
            decoration_style: DecorationStyle::Solid,
            visibility: Visibility::Visible,
            overflow_wrap: OverflowWrap::Normal,
            word_break: WordBreak::Normal,
            text_overflow: TextOverflow::Clip,
            hyphens: Hyphens::Manual,
            monospace: false,
            font_family: String::new(),
            lang: None,
            variant_features: Vec::new(),
            font_features: Vec::new(),
            small_caps: false,
            text_align: TextAlign::Left,
            line_height: 1.6,
            margin: Edges::ZERO,
            padding: Edges::ZERO,
            border: Border::default(),
            outline: BorderSide::default(),
            outline_offset: Len::ZERO,
            content: None,
            before: None,
            after: None,
            list_style_type: ListStyleType::default(),
            list_style_position: ListStylePosition::default(),
            counter_reset: Vec::new(),
            counter_increment: Vec::new(),
            marker_text: None,
            list_type_author: false,
            width: None,
            height: None,
            aspect_ratio: None,
            max_width: None,
            min_width: None,
            min_height: None,
            max_height: None,
            break_before: BreakAction::Auto,
            break_after: BreakAction::Auto,
            break_inside: BreakInside::Auto,
            orphans: 2,
            widows: 2,
            border_collapse: false,
            border_spacing: (0.0, 0.0),
            caption_side: CaptionSide::Top,
            grid_flow_column: false,
            grid_flow_dense: false,
            grid_subgrid_columns: false,
            grid_subgrid_rows: false,
            white_space_pre: false,
            white_space_wrap: false,
            white_space_nowrap: false,
            link_href: None,
            position: Position::Static,
            top: None,
            right: None,
            bottom: None,
            left: None,
            float: FloatSide::None,
            clear: Clear::None,
            z_index: None,
            overflow: Overflow::Visible,
            container_type: ContainerType::None,
            container_names: Vec::new(),
            opacity: 1.0,
            flex_direction: FlexDirection::Row,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: None,
            align_items: AlignItems::Stretch,
            justify_content: JustifyContent::FlexStart,
            flex_wrap: FlexWrap::Now,
            align_content: AlignContent::Stretch,
            gap: Len::ZERO,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_column_span: 1,
            grid_row_span: 1,
            grid_template_areas: Vec::new(),
            grid_area: String::new(),
            grid_column_start: String::new(),
            grid_column_end: String::new(),
            grid_row_start: String::new(),
            grid_row_end: String::new(),
            grid_line_names_col: Vec::new(),
            grid_line_names_row: Vec::new(),
            object_fit: ObjectFit::Fill,
            border_radius: BorderRadius::ZERO,
            background_paint: BackgroundPaint::default(),
            background_position: BackgroundPosition::default(),
            background_size: BackgroundSize::default(),
            background_repeat: BackgroundRepeat::default(),
            // Empty is the initial single layer (see the field doc).
            background_layers: Vec::new(),
            box_shadows: Vec::new(),
            text_shadows: Vec::new(),
            direction: Direction::Ltr,
            vertical_align: VerticalAlign::Baseline,
            text_transform: TextTransform::None,
            letter_spacing: 0.0,
            transform: None,
            transform_origin: TransformOrigin::default(),
            filter: None,
            backdrop_filter: None,
            clip_path: None,
            mask: None,
            mix_blend_mode: MixBlend::Normal,
            isolate: false,
            will_change: false,
            column_count: None,
            column_width: None,
            column_gap: None,
            writing_mode: WritingMode::HorizontalTb,
            text_orientation: TextOrientation::Mixed,
            logical: LogicalGeometry::default(),
            animations: Vec::new(),
            transitions: Vec::new(),
            vars: Vec::new(),
            overscroll_x: OverscrollBehavior::Auto,
            overscroll_y: OverscrollBehavior::Auto,
            snap_axis: SnapAxis::None,
            snap_proximity: false,
            snap_align: (SnapAlign::None, SnapAlign::None),
        }
    }
}

/// Inheritable properties flow down; everything else resets to its initial
/// value. This mirrors CSS: `color` inherits, `margin` does not. Custom
/// properties inherit too, so they ride along in `vars`.
fn child_base(parent: &ComputedStyle) -> ComputedStyle {
    ComputedStyle {
        display: Display::Inline,
        color: parent.color,
        background: 0,
        fill: parent.fill,
        stroke: parent.stroke,
        font_size: parent.font_size,
        font_weight: parent.font_weight,
        italic: parent.italic,
        underline: parent.underline,
        overline: parent.overline,
        line_through: parent.line_through,
        decoration_color: parent.decoration_color,
        decoration_thickness: parent.decoration_thickness,
        decoration_style: parent.decoration_style,
        visibility: parent.visibility,
        overflow_wrap: parent.overflow_wrap,
        word_break: parent.word_break,
        text_overflow: parent.text_overflow,
        hyphens: parent.hyphens,
        monospace: parent.monospace,
        font_family: parent.font_family.clone(),
        lang: parent.lang.clone(),
        variant_features: parent.variant_features.clone(),
        font_features: parent.font_features.clone(),
        small_caps: parent.small_caps,
        text_align: parent.text_align,
        line_height: parent.line_height,
        margin: Edges::ZERO,
        padding: Edges::ZERO,
        border: Border::default(),
        outline: BorderSide::default(),
        outline_offset: Len::ZERO,
        content: None,
        before: None,
        after: None,
        list_style_type: parent.list_style_type,
        list_style_position: parent.list_style_position,
        counter_reset: Vec::new(),
        counter_increment: Vec::new(),
        marker_text: None,
        list_type_author: parent.list_type_author,
        width: None,
        height: None,
        aspect_ratio: None,
        max_width: None,
        min_width: None,
        min_height: None,
        max_height: None,
        break_before: BreakAction::Auto,
        break_after: BreakAction::Auto,
        break_inside: BreakInside::Auto,
        orphans: parent.orphans,
        widows: parent.widows,
        border_collapse: parent.border_collapse,
        border_spacing: parent.border_spacing,
        caption_side: CaptionSide::Top,
        // C6 grid-flow properties are not inherited.
        grid_flow_column: false,
        grid_flow_dense: false,
        grid_subgrid_columns: false,
        grid_subgrid_rows: false,
        white_space_pre: parent.white_space_pre,
        white_space_wrap: parent.white_space_wrap,
        white_space_nowrap: parent.white_space_nowrap,
        link_href: None,
        position: Position::Static,
        top: None,
        right: None,
        bottom: None,
        left: None,
        float: FloatSide::None,
        clear: Clear::None,
        z_index: None,
        overflow: Overflow::Visible,
        container_type: ContainerType::None,
        container_names: Vec::new(),
        opacity: 1.0,
        flex_direction: FlexDirection::Row,
        flex_grow: 0.0,
        flex_shrink: 1.0,
        flex_basis: None,
        align_items: AlignItems::Stretch,
        justify_content: JustifyContent::FlexStart,
        flex_wrap: FlexWrap::Now,
        align_content: AlignContent::Stretch,
        gap: Len::ZERO,
        grid_template_columns: Vec::new(),
        grid_template_rows: Vec::new(),
        grid_column_span: 1,
        grid_row_span: 1,
        grid_template_areas: Vec::new(),
        grid_area: String::new(),
        grid_column_start: String::new(),
        grid_column_end: String::new(),
        grid_row_start: String::new(),
        grid_row_end: String::new(),
        grid_line_names_col: Vec::new(),
        grid_line_names_row: Vec::new(),
        object_fit: ObjectFit::Fill,
        border_radius: BorderRadius::ZERO,
        background_paint: BackgroundPaint::default(),
        background_position: BackgroundPosition::default(),
        background_size: BackgroundSize::default(),
        background_repeat: BackgroundRepeat::default(),
        // Empty is the initial single layer (see the field doc).
        background_layers: Vec::new(),
        box_shadows: Vec::new(),
        text_shadows: parent.text_shadows.clone(),
        direction: parent.direction,
        vertical_align: VerticalAlign::Baseline,
        text_transform: parent.text_transform,
        letter_spacing: parent.letter_spacing,
        transform: None,
        transform_origin: TransformOrigin::default(),
        filter: None,
        backdrop_filter: None,
        clip_path: None,
        mask: None,
        mix_blend_mode: MixBlend::Normal,
        isolate: false,
        will_change: false,
        column_count: None,
        column_width: None,
        column_gap: None,
        writing_mode: parent.writing_mode,
        text_orientation: parent.text_orientation,
        logical: LogicalGeometry::default(),
        animations: Vec::new(),
        transitions: Vec::new(),
        vars: parent.vars.clone(),
        overscroll_x: OverscrollBehavior::Auto,
        overscroll_y: OverscrollBehavior::Auto,
        snap_axis: SnapAxis::None,
        snap_proximity: false,
        snap_align: (SnapAlign::None, SnapAlign::None),
    }
}

/// Interaction state for dynamic pseudo-classes (Phase N6): the nodes the
/// embedder reports via mouse/keyboard/focus events. Everything defaults to
/// resting state (`compute` uses this).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InteractionState {
    pub hover: Option<NodeId>,
    pub active: Option<NodeId>,
    pub focused: Option<NodeId>,
}

/// Specificity: (ids, classes, tags) compared lexicographically. Attribute
/// selectors and pseudo-classes count in the class bucket; `:not()` adds its
/// argument's specificity, `:is()` that of its most specific argument, and
/// `:where()` contributes nothing at all.
fn specificity(sel: &Selector) -> (u32, u32, u32) {
    let mut ids = 0;
    let mut classes = 0;
    let mut tags = 0;
    fn add_selector(sel: &Selector, ids: &mut u32, classes: &mut u32, tags: &mut u32) {
        for part in &sel.parts {
            add_simple(part, ids, classes, tags);
        }
    }
    fn add_simple(s: &SimpleSelector, ids: &mut u32, classes: &mut u32, tags: &mut u32) {
        if s.id.is_some() {
            *ids += 1;
        }
        let own = s.pseudos.len()
            - s.pseudos
                .iter()
                .filter(|p| {
                    matches!(
                        p,
                        Pseudo::Not(_) | Pseudo::Is(_) | Pseudo::Where(_) | Pseudo::Has(_)
                    )
                })
                .count();
        *classes += (s.classes.len() + s.attrs.len() + own) as u32;
        if s.tag.is_some() {
            *tags += 1;
        }
        for p in &s.pseudos {
            match p {
                Pseudo::Not(inner) => add_selector(inner, ids, classes, tags),
                Pseudo::Is(list) => {
                    if let Some(best) = list.iter().map(specificity).max() {
                        *ids += best.0;
                        *classes += best.1;
                        *tags += best.2;
                    }
                }
                Pseudo::Has(args) => {
                    if let Some(best) = args.iter().map(|a| specificity(&a.selector)).max() {
                        *ids += best.0;
                        *classes += best.1;
                        *tags += best.2;
                    }
                }
                Pseudo::Where(_) => {}
                _ => {}
            }
        }
    }
    add_selector(sel, &mut ids, &mut classes, &mut tags);
    (ids, classes, tags)
}

/// Does this selector match `node`, given its ancestor chain (root first,
/// parent last)? Parts are matched right-to-left; child, descendant, and
/// sibling combinators use the corresponding tree relationship.
fn matches(
    selector: &Selector,
    dom: &Dom,
    node: NodeId,
    ancestors: &[NodeId],
    state: &InteractionState,
) -> bool {
    let Some(last) = selector.parts.len().checked_sub(1) else {
        return false;
    };
    matches_part(selector, last, dom, node, ancestors, ancestors.len(), state)
}

/// Match `parts[pi]` against `node`, with `ancestors[..ai]` available to the
/// outer parts still to be matched.
fn matches_part(
    selector: &Selector,
    pi: usize,
    dom: &Dom,
    node: NodeId,
    ancestors: &[NodeId],
    ai: usize,
    state: &InteractionState,
) -> bool {
    if !simple_matches(&selector.parts[pi], dom, node, ancestors, state) {
        return false;
    }
    if pi == 0 {
        return true;
    }
    match selector.combs[pi - 1] {
        Combinator::Child => {
            ai > 0
                && matches_part(
                    selector,
                    pi - 1,
                    dom,
                    ancestors[ai - 1],
                    ancestors,
                    ai - 1,
                    state,
                )
        }
        Combinator::Descendant => (0..ai)
            .rev()
            .any(|k| matches_part(selector, pi - 1, dom, ancestors[k], ancestors, k, state)),
        Combinator::AdjacentSibling => {
            ai > 0
                && previous_element_siblings(dom, node, true)
                    .into_iter()
                    .any(|sibling| {
                        matches_part(selector, pi - 1, dom, sibling, ancestors, ai, state)
                    })
        }
        Combinator::GeneralSibling => {
            ai > 0
                && previous_element_siblings(dom, node, false)
                    .into_iter()
                    .any(|sibling| {
                        matches_part(selector, pi - 1, dom, sibling, ancestors, ai, state)
                    })
        }
    }
}

fn previous_element_siblings(dom: &Dom, node: NodeId, adjacent: bool) -> Vec<NodeId> {
    let Some(parent) = dom.node(node).parent else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for &child in dom.children(parent) {
        if child == node {
            if adjacent {
                return out.into_iter().rev().take(1).collect();
            }
            return out;
        }
        if dom.element(child).is_some() {
            out.push(child);
        }
    }
    Vec::new()
}

fn simple_matches(
    s: &SimpleSelector,
    dom: &Dom,
    node: NodeId,
    ancestors: &[NodeId],
    state: &InteractionState,
) -> bool {
    let Some(el) = dom.element(node) else {
        return false;
    };
    if let Some(tag) = &s.tag {
        if &el.tag != tag {
            return false;
        }
    }
    if let Some(id) = &s.id {
        if el.id().as_deref() != Some(id.as_str()) {
            return false;
        }
    }
    for want in &s.classes {
        if !el.classes().any(|c| c == want.as_str()) {
            return false;
        }
    }
    for a in &s.attrs {
        if !attr_matches(a, dom, node) {
            return false;
        }
    }
    let parent = ancestors.last().copied();
    for p in &s.pseudos {
        let ok = match p {
            Pseudo::FirstChild => parent.is_some_and(|pa| edge_element_child(dom, pa, node, true)),
            Pseudo::LastChild => parent.is_some_and(|pa| edge_element_child(dom, pa, node, false)),
            Pseudo::Root => dom.root() == Some(node),
            Pseudo::Not(inner) => !matches(inner, dom, node, ancestors, state),
            Pseudo::Hover => state.hover == Some(node),
            Pseudo::Active => state.active == Some(node),
            Pseudo::Focus => state.focused == Some(node),
            Pseudo::Checked => {
                let tag = dom.tag(node);
                match tag {
                    "input" => {
                        let t = dom
                            .element(node)
                            .and_then(|el| el.attr("type"))
                            .unwrap_or("text");
                        matches!(t.to_ascii_lowercase().as_str(), "checkbox" | "radio")
                            && dom
                                .element(node)
                                .is_some_and(|el| el.attr("checked").is_some())
                    }
                    "option" => dom
                        .element(node)
                        .is_some_and(|el| el.attr("selected").is_some()),
                    _ => false,
                }
            }
            Pseudo::Disabled => dom
                .element(node)
                .is_some_and(|el| el.attr("disabled").is_some()),
            // A10: live constraint validation. Empty-but-optional controls
            // are valid (the standard validity model: only interacted-with
            // or required controls expose invalidity; we approximate to
            // "value missing OR type mismatch").
            Pseudo::Invalid => control_invalid(dom, node),
            Pseudo::NthChild { a, b } => parent.is_some_and(|pa| {
                nth_child_index(dom, pa, node).is_some_and(|idx| nth_index_matches(idx, *a, *b))
            }),
            Pseudo::NthOfType { a, b } => parent.is_some_and(|pa| {
                of_type_position(dom, pa, node)
                    .is_some_and(|(idx, _)| nth_index_matches(idx, *a, *b))
            }),
            Pseudo::FirstOfType => parent
                .is_some_and(|pa| of_type_position(dom, pa, node).is_some_and(|(idx, _)| idx == 1)),
            Pseudo::LastOfType => parent.is_some_and(|pa| {
                of_type_position(dom, pa, node).is_some_and(|(idx, total)| idx == total)
            }),
            Pseudo::OnlyOfType => parent.is_some_and(|pa| {
                of_type_position(dom, pa, node).is_some_and(|(_, total)| total == 1)
            }),
            // `:is`/`:where` match identically — the arguments are full
            // selectors (combinators included) against the same node.
            Pseudo::Is(list) | Pseudo::Where(list) => list
                .iter()
                .any(|arg| matches(arg, dom, node, ancestors, state)),
            // B3 `:has(...)`: some element in `node`'s own subtree satisfies
            // one of the relative-selector arguments.
            Pseudo::Has(args) => has_matches(dom, node, ancestors, args, state),
            // D3 `:lang(...)`: the element's language (nearest `lang` /
            // `xml:lang` up the ancestor chain) matches one of the ranges.
            Pseudo::Lang(ranges) => {
                let lang = element_language(dom, node, ancestors);
                lang.is_some_and(|l| ranges.iter().any(|r| lang_matches_range(&l, r)))
            }
            // R3: pseudo-element markers never gate element matching — the
            // cascade routes such rules to the generated box instead.
            Pseudo::Before | Pseudo::After => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// D3: the element's language — HTML's inheritance rule: the nearest
/// `lang` attribute (or `xml:lang`) on the element itself or any ancestor
/// wins, lowercased; empty values are treated as absent.
fn element_language(dom: &Dom, node: NodeId, ancestors: &[NodeId]) -> Option<String> {
    std::iter::once(node)
        .chain(ancestors.iter().rev().copied())
        .find_map(|id| {
            dom.element(id).and_then(|el| {
                el.attr("lang")
                    .or_else(|| el.attr("xml:lang"))
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
            })
        })
}

/// D3: one `:lang()` range against an element language (both lowercased).
/// `*` matches any assigned language; a range matches the exact tag or any
/// subtag extension of it (`en` matches `en` and `en-US`; `zh-hans` matches
/// `zh-hans-cn` but not plain `zh`).
fn lang_matches_range(lang: &str, range: &str) -> bool {
    if range == "*" {
        return true;
    }
    lang == range
        || lang
            .strip_prefix(range)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// B3 `:has()`: OR over the arguments — any element inside `node`'s subtree
/// that satisfies an argument's relative selector makes the pseudo match.
/// Spec semantics kept simple and correct for the supported forms:
/// candidates are searched only within the subject's subtree, and the
/// subject itself is never a candidate. Intermediate compounds may match
/// nodes between the scope and the candidate (e.g. `section:has(h2 span)`
/// matches `<section><h2><div><span>` — the span's parent is a div and the
/// `h2` sits between scope and candidate). Nested `:has()` terminates
/// because each scope's subtree shrinks strictly.
fn has_matches(
    dom: &Dom,
    scope: NodeId,
    ancestors: &[NodeId],
    args: &[HasArg],
    state: &InteractionState,
) -> bool {
    args.iter()
        .any(|arg| has_arg_matches(dom, scope, ancestors, arg, state))
}

fn has_arg_matches(
    dom: &Dom,
    scope: NodeId,
    ancestors: &[NodeId],
    arg: &HasArg,
    state: &InteractionState,
) -> bool {
    // `path` = ancestor chain (root first) + [scope] + nodes between scope
    // and the candidate (top-down) + [candidate]. Evaluating the argument's
    // parts right-to-left over this path reuses `matches_part`, whose
    // Child/Descendant logic already knows how to step through both real
    // ancestors and the synthetic middle segment. `scope_idx` is captured
    // once so anchor checks stay stable at any depth — recomputing it from
    // `path.len()` per visit would let every candidate pass as its own
    // anchor.
    let mut path: Vec<NodeId> = ancestors.to_vec();
    let scope_idx = path.len();
    path.push(scope);
    // The scope itself is not a candidate: relative selectors are
    // :scope-anchored, so every match must lie strictly inside the subtree.
    for &c in dom.children(scope) {
        if dom.element(c).is_some() && has_dfs(dom, c, &mut path, scope_idx, arg, state) {
            return true;
        }
    }
    false
}

/// Depth-first walk over `scope`'s subtree. The candidate is appended to
/// `path` before evaluation so `j` (its index) and `current` always agree.
fn has_dfs(
    dom: &Dom,
    current: NodeId,
    path: &mut Vec<NodeId>,
    scope_idx: usize,
    arg: &HasArg,
    state: &InteractionState,
) -> bool {
    path.push(current);
    let j = path.len() - 1;
    if has_part(
        &arg.selector,
        arg.selector.parts.len() - 1,
        dom,
        current,
        path,
        j,
        scope_idx,
        arg.child_anchored,
        state,
    ) {
        path.pop();
        return true;
    }
    for &c in dom.children(current) {
        if dom.element(c).is_some() && has_dfs(dom, c, path, scope_idx, arg, state) {
            path.pop();
            return true;
        }
    }
    path.pop();
    false
}

/// Evaluate the argument selector's parts right-to-left against `path`,
/// where `path[j]` is the node that compound `pi` must match. Mirrors
/// [`matches_part`] with the anchoring rules of `:has()` bolted on: at the
/// anchor step (`pi == 0`), a child-anchored argument must have matched a
/// direct child of the scope; a descendant-anchored one any strict
/// descendant. (Outer parts may still look above the scope — that check
/// only constrains where the anchor sits.)
#[allow(clippy::too_many_arguments)]
fn has_part(
    selector: &Selector,
    pi: usize,
    dom: &Dom,
    node: NodeId,
    path: &[NodeId],
    j: usize,
    scope_idx: usize,
    child_anchored: bool,
    state: &InteractionState,
) -> bool {
    if !simple_matches(&selector.parts[pi], dom, node, &path[..j], state) {
        return false;
    }
    if pi == 0 {
        // Anchor check: the outermost compound of the argument must have
        // been found within the :has() scope's subtree.
        return if child_anchored {
            j == scope_idx + 1
        } else {
            j > scope_idx
        };
    }
    match selector.combs[pi - 1] {
        Combinator::Child => {
            j > 0
                && has_part(
                    selector,
                    pi - 1,
                    dom,
                    path[j - 1],
                    path,
                    j - 1,
                    scope_idx,
                    child_anchored,
                    state,
                )
        }
        Combinator::Descendant => (0..j).rev().any(|k| {
            has_part(
                selector,
                pi - 1,
                dom,
                path[k],
                path,
                k,
                scope_idx,
                child_anchored,
                state,
            )
        }),
        Combinator::AdjacentSibling | Combinator::GeneralSibling => false,
    }
}

/// 1-based element-sibling index of `node` under `parent` (`None` when `node`
/// is not an element child — text nodes never match structural selectors).
fn nth_child_index(dom: &Dom, parent: NodeId, node: NodeId) -> Option<usize> {
    let mut idx = 0usize;
    for &c in dom.children(parent) {
        if dom.element(c).is_some() {
            idx += 1;
            if c == node {
                return Some(idx);
            }
        }
    }
    None
}

/// An+B membership for a 1-based index: `a*n+b` hits for some `n >= 0`.
fn nth_index_matches(idx: usize, a: i32, b: i32) -> bool {
    let idx = idx as i32;
    if a == 0 {
        idx == b
    } else if a > 0 {
        idx >= b && (idx - b) % a == 0
    } else {
        idx <= b && (b - idx) % (-a) == 0
    }
}

/// `(1-based position, total count)` of `node` among `parent`'s element
/// children sharing its tag name — the basis of the `:nth-of-type` family.
/// `None` when `node` is not an element child of `parent`.
fn of_type_position(dom: &Dom, parent: NodeId, node: NodeId) -> Option<(usize, usize)> {
    let tag = dom.tag(node);
    let mut idx = 0usize;
    let mut total = 0usize;
    for &c in dom.children(parent) {
        if dom.element(c).is_some() && dom.tag(c) == tag {
            total += 1;
            if c == node {
                idx = total;
            }
        }
    }
    if idx == 0 {
        None
    } else {
        Some((idx, total))
    }
}

/// A10: does this form control currently fail its constraints? The engine's
/// validity model — `required` missing, a `type` format mismatch
/// (email/url/number/date/time families), or a number outside
/// `min`/`max`. Only `input`/`textarea`/`select` participate; a control
/// inside a `<form>` is not required (page-level checks are the form's, not
/// the control's). Empty-but-optional = valid.
/// B3: nearest container for an `@container` gate — unnamed queries take
/// the innermost ancestor container; named ones the innermost whose
/// `container-name` list contains the query name.
fn container_width_for(name: &str, cq: &[(Vec<String>, f32)]) -> Option<f32> {
    if name.is_empty() {
        return cq.last().map(|(_, w)| *w);
    }
    cq.iter()
        .rev()
        .find(|(names, _)| names.iter().any(|n| n == name))
        .map(|(_, w)| *w)
}

/// B15: `@scope` membership for one element. Walking from the element
/// upward: the first ancestor-or-self matching `start` puts it in scope
/// (the root itself is in its own scope even if it also matches `end`);
/// a nearer `end` match prunes — the element and its subtree are out.
/// The walk uses the same self→root chain the rule gate needs because
/// boundaries always cut from below the root.
fn in_scope(
    dom: &Dom,
    node: NodeId,
    ancestors: &[NodeId],
    state: &InteractionState,
    gate: &ScopeQuery,
) -> bool {
    // `ancestors` is root-first; candidate chain is self, then nearest
    // ancestor up. For candidate index `c` (0 = self), its own ancestor
    // slice is `ancestors[..len - c]`.
    for c in 0..=ancestors.len() {
        let (id, anc) = if c == 0 {
            (node, ancestors)
        } else {
            (
                ancestors[ancestors.len() - c],
                &ancestors[..ancestors.len() - c],
            )
        };
        if matches(&gate.start, dom, id, anc, state) {
            return true;
        }
        if let Some(end) = &gate.end {
            if matches(end, dom, id, anc, state) {
                return false;
            }
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CssWideKeyword {
    Inherit,
    Initial,
    Unset,
}

fn css_wide_keyword(value: &str) -> Option<CssWideKeyword> {
    match value.trim().to_ascii_lowercase().as_str() {
        "inherit" => Some(CssWideKeyword::Inherit),
        "initial" => Some(CssWideKeyword::Initial),
        "unset" => Some(CssWideKeyword::Unset),
        _ => None,
    }
}

fn inherited_property(property: &str) -> bool {
    matches!(
        property,
        "color"
            | "fill"
            | "stroke"
            | "border-collapse"
            | "border-spacing"
            | "direction"
            | "font"
            | "font-family"
            | "font-feature-settings"
            | "font-size"
            | "font-style"
            | "font-variant"
            | "font-variant-caps"
            | "font-variant-ligatures"
            | "font-weight"
            | "hyphens"
            | "letter-spacing"
            | "line-height"
            | "list-style"
            | "list-style-position"
            | "list-style-type"
            | "orphans"
            | "overflow-wrap"
            | "text-align"
            | "text-decoration"
            | "text-decoration-color"
            | "text-decoration-line"
            | "text-decoration-style"
            | "text-decoration-thickness"
            | "text-orientation"
            | "text-overflow"
            | "text-shadow"
            | "text-transform"
            | "visibility"
            | "white-space"
            | "widows"
            | "word-break"
            | "word-wrap"
            | "writing-mode"
    )
}

fn copy_computed_property(target: &mut ComputedStyle, source: &ComputedStyle, property: &str) {
    macro_rules! copy {
        ($field:ident) => {
            target.$field = source.$field.clone()
        };
    }
    match property {
        "color" => copy!(color),
        "fill" => copy!(fill),
        "stroke" => copy!(stroke),
        "direction" => copy!(direction),
        "display" => copy!(display),
        "opacity" => copy!(opacity),
        "vertical-align" => copy!(vertical_align),
        "background" => {
            copy!(background);
            copy!(background_paint);
            copy!(background_layers);
        }
        "background-color" => copy!(background),
        "background-image" => {
            copy!(background_paint);
            copy!(background_layers);
        }
        "background-position" => copy!(background_position),
        "background-size" => copy!(background_size),
        "background-repeat" => copy!(background_repeat),
        // Per-layer box keywords: the value is a list, so it travels with
        // the layer list itself. Inheriting copies each layer's box
        // pairwise; a list length mismatch keeps the target's own (a
        // longhand never changes the number of layers).
        "background-origin" => copy_layer_boxes(target, source, LayerBox::Origin),
        "background-clip" => copy_layer_boxes(target, source, LayerBox::Clip),
        "visibility" => copy!(visibility),
        "font" => {
            copy!(font_size);
            copy!(font_weight);
            copy!(italic);
            copy!(line_height);
            copy!(font_family);
            copy!(monospace);
        }
        "font-size" => copy!(font_size),
        "font-weight" => copy!(font_weight),
        "font-style" => copy!(italic),
        "font-family" => {
            copy!(font_family);
            copy!(monospace);
        }
        "font-variant" => {
            copy!(variant_features);
            copy!(small_caps);
        }
        "font-variant-caps" => {
            let tags = axis_tags("caps");
            target
                .variant_features
                .retain(|(tag, _)| !tags.contains(&tag.as_str()));
            target.variant_features.extend(
                source
                    .variant_features
                    .iter()
                    .filter(|(tag, _)| tags.contains(&tag.as_str()))
                    .cloned(),
            );
            target.small_caps = source.small_caps;
        }
        "font-variant-ligatures" => {
            let tags = axis_tags("ligatures");
            target
                .variant_features
                .retain(|(tag, _)| !tags.contains(&tag.as_str()));
            target.variant_features.extend(
                source
                    .variant_features
                    .iter()
                    .filter(|(tag, _)| tags.contains(&tag.as_str()))
                    .cloned(),
            );
        }
        "font-feature-settings" => copy!(font_features),
        "text-align" => copy!(text_align),
        "text-decoration" => {
            copy!(underline);
            copy!(overline);
            copy!(line_through);
            copy!(decoration_color);
            copy!(decoration_thickness);
            copy!(decoration_style);
        }
        "text-decoration-line" => {
            copy!(underline);
            copy!(overline);
            copy!(line_through);
        }
        "text-decoration-style" => copy!(decoration_style),
        "text-decoration-color" => copy!(decoration_color),
        "text-decoration-thickness" => copy!(decoration_thickness),
        "text-overflow" => copy!(text_overflow),
        "text-shadow" => copy!(text_shadows),
        "text-transform" => copy!(text_transform),
        "letter-spacing" => copy!(letter_spacing),
        "line-height" => copy!(line_height),
        "white-space" => {
            copy!(white_space_pre);
            copy!(white_space_wrap);
            copy!(white_space_nowrap);
        }
        "word-break" => copy!(word_break),
        "overflow-wrap" | "word-wrap" => copy!(overflow_wrap),
        "hyphens" => copy!(hyphens),
        "margin" => copy!(margin),
        "margin-top" => target.margin.top = source.margin.top,
        "margin-right" => target.margin.right = source.margin.right,
        "margin-bottom" => target.margin.bottom = source.margin.bottom,
        "margin-left" => target.margin.left = source.margin.left,
        "padding" => copy!(padding),
        "padding-top" => target.padding.top = source.padding.top,
        "padding-right" => target.padding.right = source.padding.right,
        "padding-bottom" => target.padding.bottom = source.padding.bottom,
        "padding-left" => target.padding.left = source.padding.left,
        "width" => copy!(width),
        "height" => copy!(height),
        "max-width" => copy!(max_width),
        "min-width" => copy!(min_width),
        "max-height" => copy!(max_height),
        "min-height" => copy!(min_height),
        "aspect-ratio" => copy!(aspect_ratio),
        "break-before" | "page-break-before" => copy!(break_before),
        "break-after" | "page-break-after" => copy!(break_after),
        "break-inside" | "page-break-inside" => copy!(break_inside),
        "orphans" => copy!(orphans),
        "widows" => copy!(widows),
        "border-collapse" => copy!(border_collapse),
        "border-spacing" => copy!(border_spacing),
        "caption-side" => copy!(caption_side),
        "position" => copy!(position),
        "top" => copy!(top),
        "right" => copy!(right),
        "bottom" => copy!(bottom),
        "left" => copy!(left),
        "float" => copy!(float),
        "clear" => copy!(clear),
        "z-index" => copy!(z_index),
        "overflow" => copy!(overflow),
        "border" => copy!(border),
        "border-top" => target.border.top = source.border.top,
        "border-right" => target.border.right = source.border.right,
        "border-bottom" => target.border.bottom = source.border.bottom,
        "border-left" => target.border.left = source.border.left,
        "border-width" => {
            target.border.top.width = source.border.top.width;
            target.border.right.width = source.border.right.width;
            target.border.bottom.width = source.border.bottom.width;
            target.border.left.width = source.border.left.width;
        }
        "border-style" => {
            target.border.top.style = source.border.top.style;
            target.border.right.style = source.border.right.style;
            target.border.bottom.style = source.border.bottom.style;
            target.border.left.style = source.border.left.style;
        }
        "border-color" => {
            target.border.top.color = source.border.top.color;
            target.border.right.color = source.border.right.color;
            target.border.bottom.color = source.border.bottom.color;
            target.border.left.color = source.border.left.color;
        }
        "border-radius" => copy!(border_radius),
        "border-top-width" => target.border.top.width = source.border.top.width,
        "border-right-width" => target.border.right.width = source.border.right.width,
        "border-bottom-width" => target.border.bottom.width = source.border.bottom.width,
        "border-left-width" => target.border.left.width = source.border.left.width,
        "border-top-style" => target.border.top.style = source.border.top.style,
        "border-right-style" => target.border.right.style = source.border.right.style,
        "border-bottom-style" => target.border.bottom.style = source.border.bottom.style,
        "border-left-style" => target.border.left.style = source.border.left.style,
        "border-top-color" => target.border.top.color = source.border.top.color,
        "border-right-color" => target.border.right.color = source.border.right.color,
        "border-bottom-color" => target.border.bottom.color = source.border.bottom.color,
        "border-left-color" => target.border.left.color = source.border.left.color,
        "outline" => copy!(outline),
        "outline-width" => target.outline.width = source.outline.width,
        "outline-style" => target.outline.style = source.outline.style,
        "outline-color" => target.outline.color = source.outline.color,
        "outline-offset" => copy!(outline_offset),
        "box-shadow" => copy!(box_shadows),
        "flex" => {
            copy!(flex_grow);
            copy!(flex_shrink);
            copy!(flex_basis);
        }
        "flex-direction" => copy!(flex_direction),
        "flex-grow" => copy!(flex_grow),
        "flex-shrink" => copy!(flex_shrink),
        "flex-basis" => copy!(flex_basis),
        "align-items" => copy!(align_items),
        "align-content" => copy!(align_content),
        "justify-content" => copy!(justify_content),
        "flex-wrap" => copy!(flex_wrap),
        "gap" | "grid-gap" | "grid-column-gap" => {
            copy!(gap);
            copy!(column_gap);
        }
        "row-gap" => copy!(gap),
        "column-gap" => {
            copy!(gap);
            copy!(column_gap);
        }
        "grid-template-columns" => {
            copy!(grid_template_columns);
            copy!(grid_line_names_col);
            copy!(grid_subgrid_columns);
        }
        "grid-template-rows" => {
            copy!(grid_template_rows);
            copy!(grid_line_names_row);
            copy!(grid_subgrid_rows);
        }
        "grid-auto-flow" => {
            copy!(grid_flow_column);
            copy!(grid_flow_dense);
        }
        "grid-template-areas" => copy!(grid_template_areas),
        "grid-area" => {
            copy!(grid_area);
            copy!(grid_column_start);
            copy!(grid_column_end);
            copy!(grid_row_start);
            copy!(grid_row_end);
            copy!(grid_column_span);
            copy!(grid_row_span);
        }
        "grid-column" => {
            copy!(grid_column_start);
            copy!(grid_column_end);
            copy!(grid_column_span);
        }
        "grid-row" => {
            copy!(grid_row_start);
            copy!(grid_row_end);
            copy!(grid_row_span);
        }
        "grid-column-start" => {
            copy!(grid_column_start);
            copy!(grid_column_span);
        }
        "grid-column-end" => {
            copy!(grid_column_end);
            copy!(grid_column_span);
        }
        "grid-row-start" => {
            copy!(grid_row_start);
            copy!(grid_row_span);
        }
        "grid-row-end" => {
            copy!(grid_row_end);
            copy!(grid_row_span);
        }
        "container-type" => copy!(container_type),
        "container-name" => copy!(container_names),
        "content" => copy!(content),
        "counter-reset" => copy!(counter_reset),
        "counter-increment" => copy!(counter_increment),
        "list-style" => {
            copy!(list_style_type);
            copy!(list_style_position);
            copy!(list_type_author);
        }
        "list-style-type" => {
            copy!(list_style_type);
            copy!(list_type_author);
        }
        "list-style-position" => copy!(list_style_position),
        "object-fit" => copy!(object_fit),
        "filter" => copy!(filter),
        "backdrop-filter" => copy!(backdrop_filter),
        "transform" => copy!(transform),
        "transform-origin" => copy!(transform_origin),
        "transition" => copy!(transitions),
        "transition-property"
        | "transition-duration"
        | "transition-delay"
        | "transition-timing-function" => copy!(transitions),
        "animation" => copy!(animations),
        "animation-name"
        | "animation-duration"
        | "animation-delay"
        | "animation-timing-function"
        | "animation-iteration-count"
        | "animation-direction" => {
            copy!(animations)
        }
        "overscroll-behavior" => {
            copy!(overscroll_x);
            copy!(overscroll_y);
        }
        "overscroll-behavior-x" => copy!(overscroll_x),
        "overscroll-behavior-y" => copy!(overscroll_y),
        "scroll-snap-type" => {
            copy!(snap_axis);
            copy!(snap_proximity);
        }
        "scroll-snap-align" => copy!(snap_align),
        "clip-path" => copy!(clip_path),
        "mask" | "mask-image" | "-webkit-mask" | "-webkit-mask-image" => copy!(mask),
        "mix-blend-mode" => copy!(mix_blend_mode),
        "isolation" => copy!(isolate),
        "will-change" => copy!(will_change),
        "columns" => {
            copy!(column_count);
            copy!(column_width);
        }
        "column-count" => copy!(column_count),
        "column-width" => copy!(column_width),
        "writing-mode" => copy!(writing_mode),
        "text-orientation" => copy!(text_orientation),
        "inline-size" => target.logical.inline_size = source.logical.inline_size,
        "block-size" => target.logical.block_size = source.logical.block_size,
        "min-inline-size" => target.logical.min_inline_size = source.logical.min_inline_size,
        "max-inline-size" => target.logical.max_inline_size = source.logical.max_inline_size,
        "min-block-size" => target.logical.min_block_size = source.logical.min_block_size,
        "max-block-size" => target.logical.max_block_size = source.logical.max_block_size,
        "inset-inline-start" => {
            target.logical.inset_inline_start = source.logical.inset_inline_start
        }
        "inset-inline-end" => target.logical.inset_inline_end = source.logical.inset_inline_end,
        "inset-block-start" => target.logical.inset_block_start = source.logical.inset_block_start,
        "inset-block-end" => target.logical.inset_block_end = source.logical.inset_block_end,
        "margin-inline-start" => {
            target.logical.margin_inline_start = source.logical.margin_inline_start
        }
        "margin-inline-end" => target.logical.margin_inline_end = source.logical.margin_inline_end,
        "margin-block-start" => {
            target.logical.margin_block_start = source.logical.margin_block_start
        }
        "margin-block-end" => target.logical.margin_block_end = source.logical.margin_block_end,
        "padding-inline-start" => {
            target.logical.padding_inline_start = source.logical.padding_inline_start
        }
        "padding-inline-end" => {
            target.logical.padding_inline_end = source.logical.padding_inline_end
        }
        "padding-block-start" => {
            target.logical.padding_block_start = source.logical.padding_block_start
        }
        "padding-block-end" => target.logical.padding_block_end = source.logical.padding_block_end,
        "inset-inline" => {
            target.logical.inset_inline_start = source.logical.inset_inline_start;
            target.logical.inset_inline_end = source.logical.inset_inline_end;
        }
        "inset-block" => {
            target.logical.inset_block_start = source.logical.inset_block_start;
            target.logical.inset_block_end = source.logical.inset_block_end;
        }
        "margin-inline" => {
            target.logical.margin_inline_start = source.logical.margin_inline_start;
            target.logical.margin_inline_end = source.logical.margin_inline_end;
        }
        "margin-block" => {
            target.logical.margin_block_start = source.logical.margin_block_start;
            target.logical.margin_block_end = source.logical.margin_block_end;
        }
        "padding-inline" => {
            target.logical.padding_inline_start = source.logical.padding_inline_start;
            target.logical.padding_inline_end = source.logical.padding_inline_end;
        }
        "padding-block" => {
            target.logical.padding_block_start = source.logical.padding_block_start;
            target.logical.padding_block_end = source.logical.padding_block_end;
        }
        _ => {}
    }
}

/// Copy one per-layer box keyword (`background-origin` / `background-clip`)
/// from `source` to `target`, pairwise by layer index. Those two properties
/// are lists, so they cannot travel on their own scalar field the way the
/// other background longhands do. A length mismatch leaves the target's own
/// values alone (a box keyword never changes the number of layers).
fn copy_layer_boxes(target: &mut ComputedStyle, source: &ComputedStyle, which: LayerBox) {
    for (t, s) in target
        .background_layers
        .iter_mut()
        .zip(source.background_layers.iter())
    {
        match which {
            LayerBox::Origin => t.origin = s.origin,
            LayerBox::Clip => t.clip = s.clip,
        }
    }
}

/// Which per-layer box keyword [`copy_layer_boxes`] moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerBox {
    Origin,
    Clip,
}

fn apply_css_wide_property(
    style: &mut ComputedStyle,
    parent: Option<&ComputedStyle>,
    property: &str,
    keyword: CssWideKeyword,
) {
    match keyword {
        CssWideKeyword::Inherit => {
            let source = parent.cloned().unwrap_or_default();
            copy_computed_property(style, &source, property);
        }
        CssWideKeyword::Initial => {
            let source = ComputedStyle::default();
            copy_computed_property(style, &source, property);
        }
        CssWideKeyword::Unset if inherited_property(property) => {
            let source = parent.cloned().unwrap_or_default();
            copy_computed_property(style, &source, property);
        }
        CssWideKeyword::Unset => {
            let source = ComputedStyle::default();
            copy_computed_property(style, &source, property);
        }
    }
}

fn apply_all_css_wide(style: &mut ComputedStyle, parent: Option<&ComputedStyle>, keyword: &str) {
    let direction = style.direction;
    let vars = style.vars.clone();
    let link_href = style.link_href.clone();
    match keyword.trim().to_ascii_lowercase().as_str() {
        "initial" => *style = ComputedStyle::default(),
        "inherit" => *style = parent.cloned().unwrap_or_default(),
        "unset" => *style = parent.map(child_base).unwrap_or_default(),
        _ => return,
    }
    style.direction = direction;
    style.vars = vars;
    style.link_href = link_href;
    style.marker_text = None;
    style.before = None;
    style.after = None;
}

/// B15: process the `all` shorthand in a resolved (cascade-ordered)
/// declaration list. The FIRST `all` consumes the reset: its keyword is
/// returned so the caller picks the base style; the shorthand itself is
/// removed, and every earlier declaration whose property isn't re-declared
/// after it is dropped. `direction` is exempt (`all` never touches it per
/// spec); custom properties live in the vars fold and are untouched here.
/// Later `all` declarations are dropped (first one decides, documented).
fn apply_all_shorthand(decls: &mut Vec<(Declaration, CascadeKey)>) -> Option<String> {
    let pos = decls.iter().position(|(d, _)| d.property == "all")?;
    let kw = decls[pos].0.value.trim().to_ascii_lowercase();
    let keep: Vec<String> = decls[pos + 1..]
        .iter()
        .map(|(d, _)| d.property.clone())
        .collect();
    let input = std::mem::take(decls);
    for (i, (d, key)) in input.into_iter().enumerate() {
        if d.property == "all" {
            continue;
        }
        if i < pos && d.property != "direction" && !keep.contains(&d.property) {
            continue; // reset by the shorthand
        }
        decls.push((d, key));
    }
    Some(kw)
}

/// B15: drop declarations whose value is the CSS-wide keyword
/// `revert`/`revert-layer`. At author level the spec outcome is "the
/// next-lower-priority value wins" — dropping the declaration cascades
/// exactly that. There is no per-origin revert in the engine (UA rules
/// are author rules here; documented approximation).
fn drop_revert_decls(decls: &mut Vec<(Declaration, CascadeKey)>) {
    decls.retain(|(d, _)| {
        let v = d.value.trim().to_ascii_lowercase();
        v != "revert" && v != "revert-layer"
    });
}

/// B15: one `overscroll-behavior` token. The legacy pointer keywords
/// (`pan-x`, `pan-y`, `pinch-zoom`) and anything unrecognized resolve to
/// `Auto` (documented).
fn parse_overscroll_token(s: &str) -> OverscrollBehavior {
    match s.trim().to_ascii_lowercase().as_str() {
        "contain" => OverscrollBehavior::Contain,
        "none" => OverscrollBehavior::None,
        _ => OverscrollBehavior::Auto,
    }
}

/// B15: `scroll-snap-type` — axis keyword plus optional strictness.
/// `block`→Y and `inline`→X; `mandatory` is the default strictness,
/// `proximity` opts in. A strictness keyword without an axis (and any
/// `none`) disables snapping; with two axes the last one wins
/// (documented leniency).
fn parse_snap_type(v: &str) -> (SnapAxis, bool) {
    let mut axis = SnapAxis::None;
    let mut proximity = false;
    let mut named = false;
    for tok in v.split_whitespace() {
        match tok.to_ascii_lowercase().as_str() {
            "none" => return (SnapAxis::None, false),
            "x" | "inline" => {
                axis = SnapAxis::X;
                named = true;
            }
            "y" | "block" => {
                axis = SnapAxis::Y;
                named = true;
            }
            "both" => {
                axis = SnapAxis::Both;
                named = true;
            }
            "proximity" => proximity = true,
            "mandatory" => proximity = false,
            _ => {}
        }
    }
    if !named {
        return (SnapAxis::None, false);
    }
    (axis, proximity)
}

/// B15: `scroll-snap-align` — one token applies to both axes; two give
/// the inline (x) then block (y) alignment. Unknown keywords resolve to
/// `none` (the spec initial).
fn parse_snap_align(v: &str) -> (SnapAlign, SnapAlign) {
    let parse = |t: &str| match t.trim().to_ascii_lowercase().as_str() {
        "start" => SnapAlign::Start,
        "center" => SnapAlign::Center,
        "end" => SnapAlign::End,
        _ => SnapAlign::None,
    };
    let toks: Vec<&str> = v.split_whitespace().collect();
    match toks.as_slice() {
        [one] => {
            let a = parse(one);
            (a, a)
        }
        [a, b, ..] => (parse(a), parse(b)),
        _ => (SnapAlign::None, SnapAlign::None),
    }
}

/// B3: evaluate a `@container` condition against the queried container's
/// content inline size. Comma-separated terms OR; `and` chains within a
/// term AND; a leading `not` negates the term. Recognized features:
/// `min-width`, `max-width`, `width` with absolute lengths. calc() and
/// range syntax (`(400px < width <= 800px)`) are unsupported, and an
/// unrecognized feature makes its term false — an unmatchable query never
/// applies, per the spec's unknown-terms rule.
fn eval_container_condition(cond: &str, width: f32) -> bool {
    cond.split(',').any(|term| {
        let (negated, term) = match term.trim().strip_prefix("not ") {
            Some(rest) => (true, rest.trim()),
            None => (false, term.trim()),
        };
        let mut ok = true;
        for part in term.split(" and ") {
            let inner = part
                .trim()
                .strip_prefix('(')
                .and_then(|p| p.strip_suffix(')'))
                .unwrap_or(part.trim());
            let Some((feature, value)) = inner.split_once(':') else {
                ok = false;
                break;
            };
            let Some(px) = absolute_length_px(value.trim()) else {
                ok = false;
                break;
            };
            ok &= match feature.trim().to_ascii_lowercase().as_str() {
                "min-width" => width >= px,
                "max-width" => width <= px,
                "width" => (width - px).abs() < 0.01,
                _ => false,
            };
        }
        if negated {
            !ok
        } else {
            ok
        }
    })
}

/// B3: absolute length (`px`/`em`/`rem`) to px; relative viewport and
/// percentage lengths are not valid in container conditions.
fn absolute_length_px(value: &str) -> Option<f32> {
    match parse_length(value) {
        Some(Length::Px(v)) => Some(v),
        Some(Length::Em(v)) => Some(v * 16.0),
        Some(Length::Rem(v)) => Some(v * 16.0),
        _ => None,
    }
}

/// B3: cheap gate before the char-level cq-unit scan.
fn value_has_cq_unit(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    ["cqw", "cqi", "cqh", "cqb", "cqmin", "cqmax"]
        .iter()
        .any(|u| lower.contains(u))
}

/// B9: does this declaration value contain a `ch`/`ex` length token — a
/// digit run immediately followed by the unit (case-insensitive, and not
/// glued to more letters, so `2chars` doesn't trip it)? Purely a gate to
/// skip the metrics probe; `parse_len_px_ch` is the precise parser.
fn value_has_ch_ex(value: &str) -> bool {
    let b = value.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() || b[i] == b'.' {
            let start = i;
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
                j += 1;
            }
            if j > start && j + 1 < b.len() {
                let unit = (b[j].to_ascii_lowercase(), b[j + 1].to_ascii_lowercase());
                let free = j + 2 >= b.len() || !b[j + 2].is_ascii_alphabetic();
                if free && (unit == (b'c', b'h') || unit == (b'e', b'x')) {
                    return true;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// B3: rewrite `cqw`/`cqi`/`cqh`/`cqb`/`cqmin`/`cqmax` lengths to px in a
/// declaration value. `container_w` is the nearest ancestor query
/// container's content inline width; without one the spec's small-viewport
/// fallback applies — inline-axis units fold against the viewport width,
/// block-axis ones against the viewport height. Only number+unit tokens
/// are rewritten, so colors, URLs and idents pass through untouched.
/// Values inside custom-property definitions are substituted at use time
/// (the using element's cq context decides).
fn subst_cq_units(
    value: &str,
    container_w: Option<f32>,
    viewport_w: f32,
    viewport_h: f32,
) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() || chars[i] == '.' {
            let num_start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let unit_start = i;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            let num: String = chars[num_start..unit_start].iter().collect();
            let unit: String = chars[unit_start..i]
                .iter()
                .collect::<String>()
                .to_ascii_lowercase();
            let w = container_w.unwrap_or(viewport_w);
            let factor = match unit.as_str() {
                "cqw" | "cqi" => Some(w / 100.0),
                "cqh" | "cqb" => Some(viewport_h / 100.0),
                "cqmin" => Some(w.min(viewport_h) / 100.0),
                "cqmax" => Some(w.max(viewport_h) / 100.0),
                _ => None,
            };
            if let (Some(factor), Ok(n)) = (factor, num.parse::<f32>()) {
                let s = format!("{:.3}", n * factor);
                out.push_str(s.trim_end_matches('0').trim_end_matches('.'));
                out.push_str("px");
            } else {
                out.push_str(&num);
                out.extend(chars[unit_start..i].iter());
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn parse_style_color(value: &str, current_color: u32) -> Option<u32> {
    if value.trim().eq_ignore_ascii_case("currentcolor") {
        Some(current_color)
    } else {
        parse_color(value)
    }
}

/// B4: drop-shadow's color defaults to `currentColor` (the element's
/// text color). The parser leaves the value alone when the author
/// specified a color, and writes a `None`-marker (`0x00000000`) when
/// the author omitted it; we patch that to the element's `style.color`
/// here, after the cascade is settled.
fn fill_drop_shadow_color(fl: FilterList, current_color: u32) -> FilterList {
    FilterList(
        fl.0.into_iter()
            .map(|f| match f {
                FilterFn::DropShadow {
                    dx,
                    dy,
                    blur,
                    color: 0,
                } => FilterFn::DropShadow {
                    dx,
                    dy,
                    blur,
                    color: current_color,
                },
                other => other,
            })
            .collect(),
    )
}

pub fn control_invalid(dom: &Dom, node: NodeId) -> bool {
    let Some(el) = dom.element(node) else {
        return false;
    };
    let disabled = el.attr("disabled").is_some();
    if disabled {
        return false;
    }
    let required = el.attr("required").is_some();
    match dom.tag(node) {
        "textarea" => {
            let mut txt = String::new();
            for &c in dom.children(node) {
                let n = dom.node(c);
                if matches!(n.kind, kitae_dom::NodeKind::Text) {
                    txt.push_str(&n.text);
                }
            }
            required && txt.trim().is_empty()
        }
        "select" => {
            if !required {
                return false;
            }
            // Required select: a selected option, else the first option,
            // must carry a non-empty value (placeholder options use value="").
            let mut any_selected = false;
            let mut first_value: Option<bool> = None;
            for &c in dom.children(node) {
                if dom.tag(c) != "option" {
                    continue;
                }
                let v = dom
                    .element(c)
                    .and_then(|o| o.attr("value"))
                    .map(|v| !v.is_empty())
                    .unwrap_or(true);
                if first_value.is_none() {
                    first_value = Some(v);
                }
                if dom.element(c).is_some_and(|o| o.attr("selected").is_some()) {
                    any_selected = v;
                }
            }
            !(any_selected || first_value.unwrap_or(false))
        }
        "input" => {
            let t = el.attr("type").unwrap_or("text").to_ascii_lowercase();
            if matches!(
                t.as_str(),
                "submit" | "button" | "reset" | "image" | "hidden" | "file"
            ) {
                return false;
            }
            let v = el.attr("value").unwrap_or("");
            if required && v.trim().is_empty() {
                return true;
            }
            if v.trim().is_empty() {
                return false; // optional + empty = valid
            }
            let type_bad = match t.as_str() {
                "email" => {
                    // One address, no multiple-attr support (v1): local@domain
                    // with a dot in the domain and no spaces.
                    let e = v.trim();
                    !(e.contains('@')
                        && !e.contains(char::is_whitespace)
                        && e.split('@').count() == 2
                        && e.split('@').nth(1).is_some_and(|d| {
                            d.contains('.') && !d.starts_with('.') && !d.ends_with('.')
                        }))
                }
                "url" => {
                    let u = v.trim();
                    let scheme_ok = u.starts_with("http://")
                        || u.starts_with("https://")
                        || u.starts_with("ftp://");
                    !(scheme_ok && u.len() > scheme_len(u))
                }
                "number" | "range" => {
                    let n = v.trim().parse::<f32>().ok();
                    match n {
                        None => true,
                        Some(n) => {
                            let min = el.attr("min").and_then(|m| m.trim().parse::<f32>().ok());
                            let max = el.attr("max").and_then(|m| m.trim().parse::<f32>().ok());
                            min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m)
                        }
                    }
                }
                "date" => valid_date(v.trim()).is_none(),
                "time" => valid_time(v.trim()).is_none(),
                "datetime-local" => match v
                    .trim()
                    .split_once('T')
                    .or_else(|| v.trim().split_once(' '))
                {
                    Some((d, t)) => {
                        valid_date(d.trim()).is_none() || valid_time(t.trim()).is_none()
                    }
                    None => true,
                },
                "color" => {
                    let c = v.trim();
                    !(c.len() == 7
                        && c.starts_with('#')
                        && c[1..].chars().all(|h| h.is_ascii_hexdigit()))
                }
                _ => false,
            };
            if type_bad {
                return true;
            }
            // A10 finish: the `pattern` attribute — full-string regex-subset
            // match (see `pattern_matches`). Applies on top of the type
            // checks; an unparseable pattern is ignored, not a failure.
            if let Some(p) = el.attr("pattern") {
                if !pattern_matches(p, v) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Length of the matched scheme prefix (for the url check above).
fn scheme_len(url: &str) -> usize {
    for s in ["https://", "http://", "ftp://"] {
        if url.starts_with(s) {
            return s.len();
        }
    }
    0
}

/// `YYYY-MM-DD` (A10) — Some((y, m, d)) when the shape parses, month 1–12,
/// day 1–31 (no per-month day precision in v1).
fn valid_date(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.split('-');
    let y: u32 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    (y >= 1 && (1..=12).contains(&m) && (1..=31).contains(&d)).then_some((y, m, d))
}

/// `HH:MM[:SS]` (A10) — Some(()) when valid.
fn valid_time(s: &str) -> Option<()> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let h: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    if parts.len() == 3 {
        parts[2].parse::<u32>().ok()?;
    }
    (h < 24 && m < 60).then_some(())
}

// -- A10 finish: `pattern` matching ------------------------------------------

/// A10 finish: the `pattern` attribute — a full-string match against the
/// regex subset [`PatParser`] understands (literals, `.`, character classes
/// with ranges/negation/shorthands, groups, alternation, the `* + ? {m,n}`
/// quantifiers, `\d \w \s` classes). The spec anchors the pattern at both
/// ends (`^(?:…)$` with the `v` flag), so this is an anchored match — the
/// whole value must match. An unparseable (or absurdly long) pattern is
/// ignored — returns true, per the HTML rules for invalid patterns.
pub fn pattern_matches(pattern: &str, value: &str) -> bool {
    if pattern.chars().count() > 512 {
        return true; // absurd pattern: ignore rather than burn the stack
    }
    let mut p = PatParser {
        chars: pattern.chars().peekable(),
    };
    let Ok(alts) = p.parse_alt(0) else {
        return true;
    };
    if p.peek().is_some() {
        return true; // trailing junk (unbalanced `)` / stray quantifier)
    }
    let s: Vec<char> = value.chars().collect();
    pat_node(&PatNode::Group(alts), &s, 0, &mut |end| end == s.len())
}

/// One parsed `pattern` node (the regex subset the validator supports).
#[derive(Debug, Clone)]
enum PatNode {
    Char(char),
    Any,
    Class {
        neg: bool,
        items: Vec<ClassItem>,
    },
    /// Alternation of sequences.
    Group(Vec<Vec<PatNode>>),
    Repeat {
        node: Box<PatNode>,
        min: u32,
        max: Option<u32>,
    },
    Start,
    End,
}

#[derive(Debug, Clone)]
enum ClassItem {
    Ch(char),
    Range(char, char),
    /// `\d` / `\D` — the bool negates the matched set (also inside classes).
    Digit(bool),
    Word(bool),
    Space(bool),
}

struct PatParser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl<'a> PatParser<'a> {
    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.chars.next();
            true
        } else {
            false
        }
    }

    /// `alt ('|' alt)*`
    fn parse_alt(&mut self, depth: u32) -> Result<Vec<Vec<PatNode>>, ()> {
        if depth > 16 {
            return Err(());
        }
        let mut alts = vec![self.parse_seq(depth)?];
        while self.eat('|') {
            alts.push(self.parse_seq(depth)?);
        }
        Ok(alts)
    }

    fn parse_seq(&mut self, depth: u32) -> Result<Vec<PatNode>, ()> {
        let mut out = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            out.push(self.parse_repeat(depth)?);
        }
        Ok(out)
    }

    /// atom + optional quantifier. Lazy suffixes parse but behave like
    /// greedy — validation only asks whether a match exists.
    fn parse_repeat(&mut self, depth: u32) -> Result<PatNode, ()> {
        let atom = self.parse_atom(depth)?;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.chars.next();
                (0, None)
            }
            Some('+') => {
                self.chars.next();
                (1, None)
            }
            Some('?') => {
                self.chars.next();
                (0, Some(1))
            }
            Some('{') => {
                // A well-formed {m}/{m,}/{m,n} is a quantifier; a bare '{'
                // is a literal. Clone-restore to try, and fall back.
                let save = self.chars.clone();
                self.chars.next();
                match self.parse_braces() {
                    Ok(mm) => mm,
                    Err(()) => {
                        self.chars = save;
                        return Ok(atom);
                    }
                }
            }
            _ => return Ok(atom),
        };
        self.eat('?'); // lazy marker
        Ok(PatNode::Repeat {
            node: Box::new(atom),
            min,
            max,
        })
    }

    /// After the `{` is consumed: `m}`, `m,}` or `m,n}`.
    fn parse_braces(&mut self) -> Result<(u32, Option<u32>), ()> {
        let min = self.parse_number()?;
        if self.eat('}') {
            return Ok((min, Some(min)));
        }
        if !self.eat(',') {
            return Err(());
        }
        if self.eat('}') {
            return Ok((min, None));
        }
        let max = self.parse_number()?;
        if !self.eat('}') || max < min {
            return Err(());
        }
        Ok((min, Some(max)))
    }

    fn parse_number(&mut self) -> Result<u32, ()> {
        let mut n: u32 = 0;
        let mut any = false;
        while let Some(c) = self.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            self.chars.next();
            n = n.saturating_mul(10).saturating_add(c as u32 - '0' as u32);
            any = true;
        }
        any.then_some(n).ok_or(())
    }

    fn parse_atom(&mut self, depth: u32) -> Result<PatNode, ()> {
        let c = self.chars.next().ok_or(())?;
        match c {
            '(' => {
                // `(?:` and `(` are the same here (no capture bookkeeping);
                // named groups and inline flags are out of the subset.
                if self.peek() == Some('?') {
                    self.chars.next();
                    if !self.eat(':') {
                        return Err(());
                    }
                }
                let alts = self.parse_alt(depth + 1)?;
                if !self.eat(')') {
                    return Err(());
                }
                Ok(PatNode::Group(alts))
            }
            '[' => self.parse_class(),
            '.' => Ok(PatNode::Any),
            '^' => Ok(PatNode::Start),
            '$' => Ok(PatNode::End),
            '\\' => self.parse_escape(),
            '*' | '+' | '?' | ')' | '|' => Err(()), // quantifier with no atom / stray
            _ => Ok(PatNode::Char(c)),
        }
    }

    fn parse_escape(&mut self) -> Result<PatNode, ()> {
        let c = self.chars.next().ok_or(())?;
        Ok(match c {
            'd' => PatNode::Class {
                neg: false,
                items: vec![ClassItem::Digit(false)],
            },
            'D' => PatNode::Class {
                neg: false,
                items: vec![ClassItem::Digit(true)],
            },
            'w' => PatNode::Class {
                neg: false,
                items: vec![ClassItem::Word(false)],
            },
            'W' => PatNode::Class {
                neg: false,
                items: vec![ClassItem::Word(true)],
            },
            's' => PatNode::Class {
                neg: false,
                items: vec![ClassItem::Space(false)],
            },
            'S' => PatNode::Class {
                neg: false,
                items: vec![ClassItem::Space(true)],
            },
            'n' => PatNode::Char('\n'),
            't' => PatNode::Char('\t'),
            'r' => PatNode::Char('\r'),
            other => PatNode::Char(other), // \. \\ \[ … escaped punctuation is a literal
        })
    }

    /// After the `[` is consumed. `]` as the first item is a literal (POSIX
    /// flavor); an empty class matches nothing.
    fn parse_class(&mut self) -> Result<PatNode, ()> {
        let neg = self.eat('^');
        let mut items = Vec::new();
        loop {
            let c = self.chars.next().ok_or(())?;
            match c {
                ']' => break,
                '\\' => match self.chars.next().ok_or(())? {
                    'd' => items.push(ClassItem::Digit(false)),
                    'D' => items.push(ClassItem::Digit(true)),
                    'w' => items.push(ClassItem::Word(false)),
                    'W' => items.push(ClassItem::Word(true)),
                    's' => items.push(ClassItem::Space(false)),
                    'S' => items.push(ClassItem::Space(true)),
                    'n' => items.push(ClassItem::Ch('\n')),
                    't' => items.push(ClassItem::Ch('\t')),
                    'r' => items.push(ClassItem::Ch('\r')),
                    esc => items.push(ClassItem::Ch(esc)),
                },
                _ => {
                    // Range `a-z` when the `-` is not last / not before an
                    // escape; otherwise both characters are literals.
                    if self.peek() == Some('-') {
                        let save = self.chars.clone();
                        self.chars.next(); // '-'
                        match self.peek() {
                            Some(e) if e != '\\' && e != ']' => {
                                self.chars.next();
                                items.push(ClassItem::Range(c, e));
                            }
                            _ => {
                                self.chars = save;
                                items.push(ClassItem::Ch(c));
                                items.push(ClassItem::Ch('-'));
                            }
                        }
                    } else {
                        items.push(ClassItem::Ch(c));
                    }
                }
            }
        }
        Ok(PatNode::Class { neg, items })
    }
}

fn class_has(items: &[ClassItem], c: char) -> bool {
    items.iter().any(|i| match i {
        ClassItem::Ch(x) => *x == c,
        ClassItem::Range(a, b) => *a <= c && c <= *b,
        ClassItem::Digit(neg) => c.is_ascii_digit() != *neg,
        ClassItem::Word(neg) => (c.is_alphanumeric() || c == '_') != *neg,
        ClassItem::Space(neg) => c.is_whitespace() != *neg,
    })
}

/// Backtracking matcher over the parsed subset. `k` is the continuation —
/// the rest of the sequence, ultimately `end == s.len()` for the anchored
/// full match.
fn pat_node(node: &PatNode, s: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    match node {
        PatNode::Char(c) => s.get(pos) == Some(c) && k(pos + 1),
        PatNode::Any => pos < s.len() && s[pos] != '\n' && k(pos + 1),
        PatNode::Class { neg, items } => {
            pos < s.len() && class_has(items, s[pos]) != *neg && k(pos + 1)
        }
        PatNode::Start => pos == 0 && k(pos),
        PatNode::End => pos == s.len() && k(pos),
        PatNode::Group(alts) => alts.iter().any(|seq| pat_seq(seq, s, pos, k)),
        PatNode::Repeat { node, min, max } => pat_repeat(node, *min, *max, 0, s, pos, k),
    }
}

fn pat_seq(nodes: &[PatNode], s: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    match nodes.split_first() {
        None => k(pos),
        Some((first, rest)) => pat_node(first, s, pos, &mut |p| pat_seq(rest, s, p, k)),
    }
}

fn pat_repeat(
    node: &PatNode,
    min: u32,
    max: Option<u32>,
    done: u32,
    s: &[char],
    pos: usize,
    k: &mut dyn FnMut(usize) -> bool,
) -> bool {
    // Every counted repetition consumes ≥1 char (zero-width matches don't
    // recurse), so a `min` longer than the rest of the value can't match —
    // this bound also caps the recursion depth.
    if (min.saturating_sub(done) as usize) > s.len() - pos {
        return false;
    }
    let can_more = max.map_or(true, |m| done < m);
    if can_more
        && pat_node(node, s, pos, &mut |p| {
            if p == pos {
                return false; // zero-width: don't loop forever
            }
            pat_repeat(node, min, max, done + 1, s, p, k)
        })
    {
        return true;
    }
    done >= min && k(pos)
}

/// A10 finish: a human-readable reason for a failed control — the text the
/// engine's validation bubble paints (Chrome-flavored, English v1). Empty
/// string when the control is valid. Reasons follow the spec's priority
/// (value-missing before type/format before pattern).
pub fn control_invalid_message(dom: &Dom, node: NodeId) -> String {
    if !control_invalid(dom, node) {
        return String::new();
    }
    let Some(el) = dom.element(node) else {
        return String::new();
    };
    match dom.tag(node) {
        "select" => "Please select an item in the list.".to_string(),
        "textarea" => "Please fill out this field.".to_string(),
        "input" => {
            let t = el.attr("type").unwrap_or("text").to_ascii_lowercase();
            let v = el.attr("value").unwrap_or("").trim().to_string();
            if el.attr("required").is_some() && v.is_empty() {
                return "Please fill out this field.".to_string();
            }
            let min = el.attr("min").and_then(|m| m.trim().parse::<f32>().ok());
            let max = el.attr("max").and_then(|m| m.trim().parse::<f32>().ok());
            match t.as_str() {
                "email" => "Please enter an email address.".to_string(),
                "url" => "Please enter a URL.".to_string(),
                "number" | "range" => {
                    let n = v.parse::<f32>().ok();
                    match n {
                        None => "Please enter a number.".to_string(),
                        Some(n) if min.is_some_and(|m| n < m) => {
                            format!(
                                "Value must be greater than or equal to {}.",
                                fmt_num(min.unwrap())
                            )
                        }
                        Some(n) if max.is_some_and(|m| n > m) => {
                            format!(
                                "Value must be less than or equal to {}.",
                                fmt_num(max.unwrap())
                            )
                        }
                        _ => "Please enter a number.".to_string(),
                    }
                }
                "date" => "Please enter a valid date.".to_string(),
                "time" => "Please enter a valid time.".to_string(),
                "datetime-local" => "Please enter a valid date and time.".to_string(),
                "color" => "Please use a color in #rrggbb form.".to_string(),
                _ => {
                    if el.attr("pattern").is_some() {
                        "Please match the format requested.".to_string()
                    } else {
                        "Please fill out this field.".to_string()
                    }
                }
            }
        }
        _ => "Please fill out this field.".to_string(),
    }
}

fn attr_matches(a: &AttrSelector, dom: &Dom, node: NodeId) -> bool {
    let Some(raw) = dom.element(node).and_then(|el| el.attr(&a.name)) else {
        return false;
    };
    match a.op {
        AttrOp::Exists => true,
        AttrOp::Equals => raw == a.value,
        AttrOp::Includes => raw.split_ascii_whitespace().any(|w| w == a.value),
        AttrOp::DashMatch => raw == a.value || raw.starts_with(&format!("{}-", a.value)),
        AttrOp::Prefix => !a.value.is_empty() && raw.starts_with(&a.value),
        AttrOp::Suffix => !a.value.is_empty() && raw.ends_with(&a.value),
        AttrOp::Substring => !a.value.is_empty() && raw.contains(&a.value),
    }
}

/// Is `node` the first (or last) element child of `parent`?
fn edge_element_child(dom: &Dom, parent: NodeId, node: NodeId, first: bool) -> bool {
    let mut edge: Option<NodeId> = None;
    for &c in dom.children(parent) {
        if dom.element(c).is_some() {
            edge = Some(c);
            if first {
                break;
            }
        }
    }
    edge == Some(node)
}

/// The UA stylesheet: a calm, typography-first reader theme (the brief's
/// "reading mode as the default philosophy").
pub const UA_STYLESHEET: &str = r#"
html { font-size: 16px; }
body {
    margin: 0px; padding: 16px;
    color: #1a1d23; background: #ffffff;
}
html, body, p, div, section, article, aside, header, footer, main, nav,
address, blockquote, figure, figcaption, dl, dt, dd, form, fieldset, legend,
h1, h2, h3, h4, h5, h6, hr, pre, ul, ol, dir, menu,
center, details, summary, hgroup, dialog { display: block; }
table { display: table; }
caption { display: table-caption; }
thead, tbody, tfoot { display: table-row-group; }
tr { display: table-row; }
td, th { display: table-cell; }
colgroup { display: table-column-group; }
col { display: table-column; }
p, ul, ol, blockquote, pre, figure { margin: 0px 0px 14px 0px; }
table { margin: 0px 0px 14px 0px; }
h1, h2, h3, h4, h5, h6 {
    font-weight: 700;
    margin: 26px 0px 10px 0px;
    line-height: 1.25;
}
h1 { font-size: 1.9rem; }
h2 { font-size: 1.55rem; }
h3 { font-size: 1.3rem; }
h4 { font-size: 1.15rem; }
h5 { font-size: 1rem; }
h6 { font-size: 0.92rem; }
ul, ol { padding: 0px 0px 0px 26px; }
li { display: list-item; margin: 0px 0px 4px 0px; }
blockquote {
    margin: 14px 0px 14px 0px;
    padding: 2px 0px 2px 14px;
    color: #555b66;
}
pre, code, tt, kbd, samp { font-family: monospace; }
pre {
    padding: 10px;
    background: #f4f5f7;
    color: #1a1d23;
    white-space: pre-wrap;
    border-radius: 6px;
}
/* QTSQ text documents: a centered reader column so prose lines stay at a
   comfortable measure (boxes narrower than the viewport auto-center). */
.qtsq-text { max-width: 780px; }
.qtsq-text table {
    width: 100%;
    border-collapse: collapse;
    border-radius: 6px;
}
.qtsq-text th, .qtsq-text td {
    border: 1px solid #d5d9df;
    padding: 6px 8px;
    vertical-align: top;
}
.qtsq-text th { background: #f4f5f7; }
.qtsq-text blockquote {
    border-left: 3px solid #c7ccd3;
    color: #555b66;
}
.qtsq-text code {
    background: #f4f5f7;
    padding: 2px 4px;
    border-radius: 4px;
}
.qtsq-text pre code {
    background: transparent;
    padding: 0;
}
.qtsq-text img { max-width: 100%; }
.qtsq-text hr { border: 0; border-top: 1px solid #d5d9df; }
/* QTSQ signature banners (slice ④): a verdict strip leading signed viewer
   pages, tinted by verdict. */
.qtsq-sig-valid, .qtsq-sig-invalid, .qtsq-sig-unverified {
    margin: 0px 0px 14px 0px;
    padding: 8px 12px;
    border-radius: 6px;
}
.qtsq-sig-valid { background: #e6f4ea; color: #1e6b34; }
.qtsq-sig-invalid { background: #fce8e6; color: #a3372c; }
.qtsq-sig-unverified { background: #f1f3f4; color: #555b66; }
/* Saved-page archives (slice ②): the provenance note leading a reopened
   snapshot, ahead of the original document's own markup. */
.qtsq-archive-base { font-size: 12px; color: #777777; margin: 0px 0px 4px 0px; }
code, tt, kbd, samp { color: #a3372c; }
a { color: #0b57d0; text-decoration: underline; }
strong, b { font-weight: 700; }
em, i, cite, var, dfn, address { font-style: italic; }
u, ins { text-decoration: underline; }
hr { margin: 22px 0px; }
small { font-size: 0.82em; }
big { font-size: 1.2em; }
sub, sup { font-size: 0.75em; }
center { text-align: center; }
th { font-weight: 700; }
td, th { padding: 3px 8px; }
dt { font-weight: 700; }
dd { margin: 0px 0px 8px 0px; padding: 0px 0px 0px 26px; }
figure { margin: 16px 0px; }
figcaption { font-size: 0.85em; color: #6b7280; text-align: center; }
caption { font-size: 0.9em; color: #6b7280; }
del, s, strike { color: #9aa0a6; }
noscript { display: block; }
[dir="rtl"] { direction: rtl; }
[dir="ltr"] { direction: ltr; }
head, title, meta, link, base, script, style, template { display: none; }
audio, object, embed { display: none; }
/* Inline <svg> renders through resvg as a replaced element (R6). A
 * <canvas> is a replaced element too: page JS draws into its backing
 * store and the engine paints the registered raster (JS-6). */
svg { display: inline-block; }
/* A12: MathML — the 2D constructs (mfrac/msqrt/scripts/over-under) paint
 * through kitae_layout's `math_walk`; this sheet supplies the typography:
 * all identifiers italic (single-char is the convention; the multi-letter
 * upright exception is a nicety, skipped), mo gets a thin space each side
 * so a+b reads separated, mn stays upright against an italic page rule. */
math { display: inline-block; }
math mi, math mn, math mo, math mtext, math mrow > * { display: inline; }
mi { font-style: italic; }
mo { margin: 0px 3px 0px 3px; }
mn { font-style: normal; }
iframe { display: block; }
/* A8/A9: value widgets render as replaced boxes; output flows inline
 * (the default) and datalist stays hidden — a popup source is meaningless
 * without script. */
progress, meter { display: inline-block; }
output { display: inline; }
datalist { display: none; }
/* A5: details/summary — the disclosure marker is generated content whose
 * glyph flips with the [open] attribute (ASCII for guaranteed glyph
 * coverage across host fonts). */
summary { display: block; font-weight: bold; }
summary::before { content: "+ "; color: #5f6368; }
details[open] > summary::before { content: "- "; color: #5f6368; }
/* A6: only [open] dialogs render — a closed dialog must be invisible
 * whatever its authored styles say (mirrors how we treat
 * details-closed content at layout time, not just display). */
dialog { display: none; }
dialog[open] {
    display: block;
    position: fixed;
    left: 50%;
    top: 10%;
    transform: translateX(-50%);
    background: #ffffff;
    border: 1px solid #dadce0;
    border-radius: 8px;
    padding: 24px;
    box-shadow: 0px 24px 48px 16px rgba(0, 0, 0, 0.25);
}
video { display: inline-block; }
button, input, select, textarea { display: inline-block; }
form { display: block; margin: 0px 0px 14px 0px; }
input, textarea, select { background: #ffffff; }
button, input[type=submit], input[type=button] { background: #f4f5f7; }
/* Control chrome defaults: real padding and border so unstyled controls
 * have sensible chrome (and the control's intrinsic width includes it),
 * plus nowrap so a button's label never wraps when the control is placed
 * as a flex/grid/block box — there its text children DO compose. A
 * transparent author background now wins over these (author > UA), which
 * is how ghost buttons stay hollow. */
button, input, select, textarea {
    padding: 2px 6px;
    border: 1px solid #9aa0a6;
}
button, input[type=submit], input[type=button] { padding: 3px 10px; }
button, input, select { white-space: nowrap; }
/* A10: a failing control gets the standard error border (layout's control
 * chrome reads the computed border color; this rule only sets it). */
input:invalid, textarea:invalid, select:invalid {
    border: 1px solid #d93025;
}
"#;

/// Monospace font family keywords seen in author CSS.
const MONO_FAMILIES: &[&str] = &[
    "monospace",
    "mono",
    "courier",
    "consolas",
    "menlo",
    "monaco",
    "dejavu sans mono",
    "liberation mono",
    "source code",
    "fira code",
    "jetbrains",
    "cascadia",
];

/// Shared by the `font-family` longhand and the `font` shorthand: set the
/// monospace flag and keep the first non-generic family for
/// measurement/painting. Generic keywords stay empty — the host's default
/// logic (Menlo for monospace, its UI font otherwise) already handles them.
fn set_font_family(style: &mut ComputedStyle, v: &str) {
    let lower = v.to_ascii_lowercase();
    style.monospace = MONO_FAMILIES.iter().any(|f| lower.contains(f));
    let first = v.split(',').next().unwrap_or("").trim();
    let first = if first.len() >= 2
        && (first.starts_with('"') || first.starts_with('\''))
        && (first.ends_with('"') || first.ends_with('\''))
    {
        // Quoted family name: strip the quotes and process CSS escapes.
        css_unquote(first)
    } else {
        first.to_string()
    };
    if !first.is_empty()
        && !matches!(
            first.to_ascii_lowercase().as_str(),
            "serif"
                | "sans-serif"
                | "monospace"
                | "cursive"
                | "fantasy"
                | "system-ui"
                | "ui-monospace"
        )
    {
        style.font_family = first.to_string();
    }
}

/// Sort key for cascade winners: inline style beats normal author rules,
/// `!important` beats everything, then specificity, then source order.
// R7: cascade key = (importance, layer rank, specificity, source order).
// Unlayered normal rules rank last (they win over layered); under
// `!important` the order inverts (layered beats unlayered).
type CascadeKey = (u8, u32, (u32, u32, u32), usize);

/// B3: one query container's measured geometry, discovered by the probe
/// layout and consumed by the second style pass. Container queries measure
/// the content box, so the width is the content inline size.
#[derive(Debug, Clone, Default)]
pub struct ContainerInfo {
    /// `container-name` values on the container element.
    pub names: Vec<String>,
    /// Content-box inline (horizontal) size in px.
    pub width: f32,
    /// Content-box height in px. Recorded for `container-type: size`, but
    /// height-based conditions are not evaluated yet (documented).
    pub height: f32,
}

/// Per-tree-walk state for [`StyleEngine::compute_subtree`]: the
/// interaction state plus the B3 container-query context (the stack of
/// ancestor query containers, innermost last).
struct Walk<'a> {
    state: &'a InteractionState,
    /// Probe-layout container boxes keyed by node id. Empty on the plain
    /// path — lookups miss and the cq stack stays empty.
    containers: &'a HashMap<u32, ContainerInfo>,
    /// Ancestor query containers as (names, content width), innermost last.
    cq: Vec<(Vec<String>, f32)>,
    /// B14 counter scopes, document order, innermost last. Each element
    /// pushes its own scope after the cascade (holding `counter-reset`s,
    /// which is exactly "visible to descendants and younger siblings");
    /// `counter-increment`s edit existing scopes in place.
    counters: Vec<HashMap<String, i32>>,
    /// I1: per interned ancestor key, how many chain elements carry it —
    /// maintained around the ancestors push/pop, read by the rule index.
    anc_counts: Vec<u32>,
}

/// I1: precomputed rule lookup — which rules can possibly match an element.
/// Each selector is filed under the keys of its rightmost (target) compound
/// (tag, id, classes; `:is()`/`:where()` arguments contribute theirs);
/// selectors with no extractable keys land in `universal`. Rules whose
/// outer compounds demand ancestor keys are additionally filtered by the
/// walk's per-key ancestor counts. `candidates` returns a superset of the
/// matching rules — the cascade's own `matches` stays the exact decision.
#[derive(Default)]
struct RuleIndex {
    by_tag: HashMap<String, Vec<u32>>,
    by_class: HashMap<String, Vec<u32>>,
    by_id: HashMap<String, Vec<u32>>,
    universal: Vec<u32>,
    /// Per rule id: one alternative conjunction per selector — the keys its
    /// outer compounds demand of the ancestor chain. A selector with no
    /// outer compounds contributes the empty conjunction (vacuously true);
    /// the rule is a candidate when ANY alternative is satisfied.
    anc_alts: Vec<Vec<Vec<AncKey>>>,
    /// Interner for ancestor keys. Tags, classes and ids share one space: a
    /// name collision between kinds only ever merges counts upward, adding
    /// candidates — never removing one — so the filter stays sound.
    anc_keys: HashMap<String, u32>,
    total_anc_keys: usize,
}

/// Interned ancestor key: an index into `Walk::anc_counts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AncKey(u32);

/// Keys a compound selector demands of the element it matches: tag, id and
/// classes, plus — for `:is()`/`:where()` — the keys of each argument's own
/// target compound (arguments match the same element). `:not()` contributes
/// nothing (a negative requirement would wrongly exclude elements) and
/// `:has()` nothing (its arguments match descendants, not the element).
fn compound_keys(s: &SimpleSelector) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut keys = (
        s.tag.iter().cloned().collect::<Vec<_>>(),
        s.classes.clone(),
        s.id.iter().cloned().collect::<Vec<_>>(),
    );
    for p in &s.pseudos {
        if let Pseudo::Is(list) | Pseudo::Where(list) = p {
            for arg in list {
                if let Some(last) = arg.parts.last() {
                    let (t, c, i) = compound_keys(last);
                    keys.0.extend(t);
                    keys.1.extend(c);
                    keys.2.extend(i);
                }
            }
        }
    }
    keys
}

fn required_compound_keys(s: &SimpleSelector) -> (Vec<String>, Vec<String>, Vec<String>) {
    (
        s.tag.iter().cloned().collect(),
        s.classes.clone(),
        s.id.iter().cloned().collect(),
    )
}

impl RuleIndex {
    fn build(rules: &[Rule]) -> RuleIndex {
        let mut idx = RuleIndex::default();
        for (ri, rule) in rules.iter().enumerate() {
            let ri = ri as u32;
            let mut alts: Vec<Vec<AncKey>> = Vec::new();
            for sel in &rule.selectors {
                let Some((last, outer)) = sel.parts.split_last() else {
                    continue;
                };
                let (tags, classes, ids) = compound_keys(last);
                if tags.is_empty() && classes.is_empty() && ids.is_empty() {
                    idx.universal.push(ri);
                } else {
                    for t in &tags {
                        idx.by_tag.entry(t.clone()).or_default().push(ri);
                    }
                    for c in &classes {
                        idx.by_class.entry(c.clone()).or_default().push(ri);
                    }
                    for i in &ids {
                        idx.by_id.entry(i.clone()).or_default().push(ri);
                    }
                }
                // Outer compounds must match some real ancestor — require
                // their keys from the walk's ancestor counts.
                let mut req: Vec<AncKey> = Vec::new();
                let sibling_boundary = sel.combs.iter().rposition(|comb| {
                    matches!(
                        comb,
                        Combinator::AdjacentSibling | Combinator::GeneralSibling
                    )
                });
                for (i, part) in outer.iter().enumerate() {
                    if sibling_boundary.is_some_and(|boundary| i <= boundary) {
                        continue;
                    }
                    let (t, c, i) = required_compound_keys(part);
                    for name in t.iter().chain(c.iter()).chain(i.iter()) {
                        let next = idx.anc_keys.len() as u32;
                        let k = *idx.anc_keys.entry(name.clone()).or_insert(next);
                        req.push(AncKey(k));
                    }
                }
                alts.push(req);
            }
            idx.anc_alts.push(alts);
        }
        idx.total_anc_keys = idx.anc_keys.len();
        idx
    }

    /// Rules that can possibly match `el`: filed by its tag/id/classes plus
    /// the universal set, filtered to rules whose ancestor demand is met,
    /// ascending and deduplicated — i.e. source order, so cascade priority
    /// (which breaks ties by rule order) is unchanged.
    fn candidates(&self, el: &ElementData, anc: &[u32]) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        if let Some(v) = self.by_tag.get(&el.tag) {
            out.extend_from_slice(v);
        }
        for c in el.classes() {
            if let Some(v) = self.by_class.get(c) {
                out.extend_from_slice(v);
            }
        }
        if let Some(id) = el.id() {
            if let Some(v) = self.by_id.get(id) {
                out.extend_from_slice(v);
            }
        }
        out.extend_from_slice(&self.universal);
        out.retain(|&r| {
            self.anc_alts[r as usize].is_empty()
                || self.anc_alts[r as usize].iter().any(|set| {
                    set.iter()
                        .all(|k| anc.get(k.0 as usize).is_some_and(|&c| c > 0))
                })
        });
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Add (`delta` 1) or remove (−1) `el`'s ancestor keys from the walk's
    /// counts, around the ancestors push/pop in `compute_subtree`. Names no
    /// rule demands (absent from the interner) are skipped.
    fn tally_anc(&self, el: &ElementData, counts: &mut [u32], delta: i32) {
        let mut bump = |name: &str| {
            if let Some(&k) = self.anc_keys.get(name) {
                counts[k as usize] = (counts[k as usize] as i32 + delta) as u32;
            }
        };
        bump(&el.tag);
        if let Some(id) = el.id() {
            bump(id);
        }
        for c in el.classes() {
            bump(c);
        }
    }
}

/// B9: font-metric probes the embedder supplies so `ch`/`ex` lengths fold
/// at style time against the element's real font. `ch_px` is the advance
/// of `0`, `ex_px` the x-height, both for the given font context at
/// `size` px. kitae_style cannot name kitae_layout's `TextMeasurer`
/// (dependency direction), so the engine talks to this trait; kitae_core
/// adapts the measurer. Without one, ch/ex fall back to 0.5em.
pub trait FontMetrics {
    fn ch_px(&self, family: &str, size: f32, weight: u32, italic: bool, monospace: bool) -> f32;
    fn ex_px(&self, family: &str, size: f32, weight: u32, italic: bool, monospace: bool) -> f32;
}

pub struct StyleEngine {
    /// Rules from the UA sheet + author sheets, in source order.
    rules: Vec<Rule>,
    /// `@font-face` blocks from the same sheets, in source order (N5).
    font_faces: Vec<FontFace>,
    /// R7: `@layer` names in first-appearance order (UA + author sheets).
    layers: Vec<String>,
    /// B15: `@property` registrations from the same sheets, in source
    /// order — later registrations override earlier ones.
    properties: Vec<PropertyRule>,
    /// Viewport for R5 length folding (`vw`/`vh`/`vmin`/`vmax`). Height may
    /// be a headless default (600) where no viewport exists.
    viewport_w: f32,
    viewport_h: f32,
    /// B3: whether any parsed rule carries an `@container` gate or cq
    /// units — the embedder uses this to decide whether the probe layout +
    /// second style pass is worth running. Inline `style=""` attributes
    /// are not scanned (documented: containers in inline styles only work
    /// when some sheet rule also opts in).
    uses_containers: bool,
    /// I1: rule lookup index — built with the sheets in
    /// [`StyleEngine::with_environment`], consulted by the cascade instead
    /// of scanning every rule for every element.
    index: RuleIndex,
    /// B9: font-metric source for `ch`/`ex` folding at style time. `None`
    /// (the default, and every headless/test path) keeps the 0.5em
    /// fallback; embedders attach the real measurer via [`with_metrics`].
    metrics: Option<Arc<dyn FontMetrics>>,
}

impl StyleEngine {
    /// UA sheet plus any author CSS (`<style>` blocks, linked sheets).
    pub fn new(author_css: &[&str]) -> StyleEngine {
        StyleEngine::with_viewport(author_css, 1280.0)
    }

    /// Author CSS parsed with `@media` conditions evaluated against the
    /// given viewport width.
    pub fn with_viewport(author_css: &[&str], viewport_width: f32) -> StyleEngine {
        // Headless default height: no real viewport exists here (documented).
        StyleEngine::with_environment(
            author_css,
            viewport_width,
            600.0,
            &EnvironmentState::default(),
        )
    }

    /// [`with_viewport`] with a concrete environment (Q6): the `prefers-*`
    /// media features in UA + author sheets evaluate against it.
    /// `viewport_height` folds `vh`/`vmin`/`vmax` (R5); headless callers
    /// without a real viewport pass 600.
    pub fn with_environment(
        author_css: &[&str],
        viewport_width: f32,
        viewport_height: f32,
        env: &EnvironmentState,
    ) -> StyleEngine {
        let viewport = kitae_css::ViewportBox {
            w: viewport_width,
            h: viewport_height,
        };
        let ua = parse_stylesheet_environment_size(UA_STYLESHEET, viewport, env);
        let mut rules = ua.rules;
        let mut font_faces = ua.font_faces;
        let mut layers = ua.layers;
        let mut properties = ua.properties;
        for css in author_css {
            let sheet = parse_stylesheet_environment_size(css, viewport, env);
            rules.extend(sheet.rules);
            font_faces.extend(sheet.font_faces);
            properties.extend(sheet.properties);
            for l in sheet.layers {
                if !layers.contains(&l) {
                    layers.push(l);
                }
            }
        }
        let uses_containers = rules.iter().any(|r| {
            r.container.is_some() || r.declarations.iter().any(|d| value_has_cq_unit(&d.value))
        });
        let index = RuleIndex::build(&rules);
        StyleEngine {
            rules,
            font_faces,
            layers,
            properties,
            viewport_w: viewport_width,
            viewport_h: viewport_height,
            uses_containers,
            index,
            metrics: None,
        }
    }

    /// B9: attach a font-metrics source so `ch`/`ex` lengths resolve
    /// against the element's actual font instead of the 0.5em fallback.
    pub fn with_metrics(mut self, m: Arc<dyn FontMetrics>) -> StyleEngine {
        self.metrics = Some(m);
        self
    }

    /// B3: does this engine need the probe-layout + second-style-pass
    /// treatment (`@container` rules or cq units present in any sheet)?
    pub fn uses_containers(&self) -> bool {
        self.uses_containers
    }

    /// I1: does any rule key off `:hover`/`:active`/`:focus` (including
    /// inside `:not()`/`:is()`/`:where()`/`:has()` arguments)? When no rule
    /// does, the embedder can skip the full relayout on hover/active
    /// changes — nothing in the computed styles could change.
    pub fn uses_interaction_state(&self) -> bool {
        fn sel_sensitive(sel: &Selector) -> bool {
            sel.parts.iter().any(part_sensitive)
        }
        fn part_sensitive(s: &SimpleSelector) -> bool {
            s.pseudos.iter().any(|p| match p {
                Pseudo::Hover | Pseudo::Active | Pseudo::Focus => true,
                Pseudo::Not(inner) => sel_sensitive(inner),
                Pseudo::Is(list) | Pseudo::Where(list) => list.iter().any(sel_sensitive),
                Pseudo::Has(args) => args.iter().any(|a| sel_sensitive(&a.selector)),
                _ => false,
            })
        }
        self.rules
            .iter()
            .any(|r| r.selectors.iter().any(sel_sensitive))
    }

    /// R7: cascade rank for a rule's layer. Normal declarations: unlayered
    /// wins over layered (rank `MAX`); `!important` inverts (layered beats
    /// unlayered, later layers beat earlier — both ascending-sort friendly).
    fn layer_rank(&self, layer: &str, important: bool) -> u32 {
        if layer.is_empty() {
            return if important { 0 } else { u32::MAX };
        }
        self.layers
            .iter()
            .position(|l| l == layer)
            .map(|i| i as u32)
            .unwrap_or(0)
            .min(u32::MAX - 1)
    }

    /// `@font-face` blocks from the UA + author sheets, in source order.
    pub fn font_faces(&self) -> &[FontFace] {
        &self.font_faces
    }

    /// B11: the declared face that serves `family` for a run at `weight`
    /// and `italic` — exact `font-weight`/`font-style` descriptors first,
    /// then the closest weight and a compatible slant, earlier declarations
    /// winning ties (source order is the cascade order for faces). A family
    /// declared at only one weight still answers, so a bold-only family
    /// serves upright runs instead of dropping to the next family.
    /// `None` when no sheet declares the family.
    pub fn font_face_for(&self, family: &str, weight: u32, italic: bool) -> Option<&FontFace> {
        let want = if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        self.font_faces
            .iter()
            .filter(|f| f.family.eq_ignore_ascii_case(family))
            .filter_map(|f| f.match_rank(weight, want).map(|rank| (rank, f)))
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, face)| face)
    }

    /// Compute styles for every element. Text nodes inherit their parent's
    /// style at layout time.
    pub fn compute(&self, dom: &Dom) -> Vec<(NodeId, ComputedStyle)> {
        self.compute_with_state(dom, &InteractionState::default())
    }

    /// [`compute`] with dynamic pseudo-class state (hover/active/focus).
    /// Hover restyles ride the incremental path in the engine (dirty-chain
    /// recompute); this call itself is a full recompute (correct, not yet
    /// minimal — documented).
    pub fn compute_with_state(
        &self,
        dom: &Dom,
        state: &InteractionState,
    ) -> Vec<(NodeId, ComputedStyle)> {
        let mut out = Vec::new();
        if let Some(root) = dom.root() {
            let mut walk = Walk {
                state,
                containers: &HashMap::new(),
                cq: Vec::new(),
                counters: vec![HashMap::new()],
                anc_counts: vec![0; self.index.total_anc_keys],
            };
            self.compute_subtree(dom, root, None, &mut Vec::new(), &mut out, &mut walk);
        }
        out
    }

    /// B3: [`compute_with_state`] with the probe layout's container boxes.
    /// Run as the second style pass when the sheets carry `@container`
    /// rules or cq units: the map (node id → measured container geometry,
    /// built from `DisplayList::containers`) lets `@container` gates and
    /// `cqw`-family lengths resolve against real laid-out widths.
    pub fn compute_with_containers(
        &self,
        dom: &Dom,
        state: &InteractionState,
        containers: &HashMap<u32, ContainerInfo>,
    ) -> Vec<(NodeId, ComputedStyle)> {
        let mut out = Vec::new();
        if let Some(root) = dom.root() {
            let mut walk = Walk {
                state,
                containers,
                cq: Vec::new(),
                counters: vec![HashMap::new()],
                anc_counts: vec![0; self.index.total_anc_keys],
            };
            self.compute_subtree(dom, root, None, &mut Vec::new(), &mut out, &mut walk);
        }
        out
    }

    fn compute_subtree(
        &self,
        dom: &Dom,
        node: NodeId,
        parent: Option<&ComputedStyle>,
        ancestors: &mut Vec<NodeId>,
        out: &mut Vec<(NodeId, ComputedStyle)>,
        walk: &mut Walk<'_>,
    ) {
        let mut style = self.cascade(dom, node, ancestors, parent, walk);

        // B14: own counter scope — pushed after the cascade (like B3
        // containers, an element never sees its own scope), holding the
        // implicit `list-item` seed for lists and the author's
        // `counter-reset`s. It covers the subtree and pops with it;
        // document-order counting across siblings comes from resets on
        // their common parent (the classic pattern) — a documented
        // simplification of the full "scope reaches younger siblings"
        // rule.
        walk.counters.push(HashMap::new());
        {
            let tag = dom.tag(node);
            let scope = walk.counters.last_mut().expect("scope just pushed");
            if matches!(tag, "ol" | "ul" | "menu") {
                let base = if tag == "ol" {
                    ol_start_attr(dom, node).map(|s| s - 1).unwrap_or(0)
                } else {
                    0
                };
                scope.insert("list-item".to_string(), base);
            }
            for op in &style.counter_reset {
                scope.insert(op.name.clone(), op.value);
            }
        }

        // `href` rides along for layout's link hit-testing.
        if dom.tag(node) == "a" {
            style.link_href = dom
                .element(node)
                .and_then(|el| el.attr("href"))
                .map(|s| s.to_string());
        }

        // D3: the element's language (nearest `lang`/`xml:lang` on itself
        // or an ancestor, lowercased). Child_base inherits it, so a
        // `<html lang>` blankets the whole document.
        style.lang = element_language(dom, node, &ancestors);

        // B3: a container element scopes its descendants' queries — pushed
        // after its own cascade (an element never queries itself).
        let pushed_container = match walk.containers.get(&node.0) {
            Some(info) => {
                walk.cq.push((info.names.clone(), info.width));
                true
            }
            None => false,
        };
        // I1: maintain the ancestor-key counts the rule index filters
        // against, exactly while `node` sits on the ancestor chain.
        if let Some(el) = dom.element(node) {
            self.index.tally_anc(el, &mut walk.anc_counts, 1);
        }
        ancestors.push(node);
        for &c in dom.children(node) {
            if dom.element(c).is_some() {
                self.compute_subtree(dom, c, Some(&style), ancestors, out, walk);
            }
        }
        ancestors.pop();
        if let Some(el) = dom.element(node) {
            self.index.tally_anc(el, &mut walk.anc_counts, -1);
        }
        if pushed_container {
            walk.cq.pop();
        }
        walk.counters.pop();
        out.push((node, style));
    }

    /// Cascade all matching declarations onto `node`, starting from the
    /// parent's inheritable values (or the initial values at the root).
    fn cascade(
        &self,
        dom: &Dom,
        node: NodeId,
        ancestors: &[NodeId],
        parent: Option<&ComputedStyle>,
        walk: &mut Walk<'_>,
    ) -> ComputedStyle {
        // Owned declarations: inline styles are parsed locally, so borrowing
        // rule declarations and local ones in the same list gets messy.
        // R3: selectors ending in ::before/::after cascade to the generated
        // box instead of the element — three separate declaration lists.
        let mut matched: Vec<(Declaration, CascadeKey)> = Vec::new();
        let mut pseudo_decls: [Vec<(Declaration, CascadeKey)>; 2] = [Vec::new(), Vec::new()];
        // I1: candidate rules from the index — a superset of the rules any
        // of whose selectors can match, in ascending rule id (source order),
        // so the loop below behaves exactly like scanning every rule.
        let cand = match dom.element(node) {
            Some(el) => self.index.candidates(el, &walk.anc_counts),
            None => Vec::new(),
        };
        for ri in cand {
            let order = ri as usize;
            let rule = &self.rules[order as usize];
            // B3: `@container` gate — the rule only applies when an ancestor
            // query container exists and matches the condition. Unnamed
            // queries take the nearest container; named ones the nearest
            // carrying that `container-name`.
            if let Some(gate) = &rule.container {
                let Some(w) = container_width_for(&gate.name, &walk.cq) else {
                    continue;
                };
                if !gate.cond.is_empty() && !eval_container_condition(&gate.cond, w) {
                    continue;
                }
            }
            // B15: `@scope` gate — the rule applies only inside the scope
            // root's subtree, stopping at (and excluding) any `to` bound.
            if let Some(gate) = &rule.scope {
                if !in_scope(dom, node, ancestors, walk.state, gate) {
                    continue;
                }
            }
            // A rule applies once, with its best-matching selector's specificity.
            let mut spec: Option<(u32, u32, u32)> = None;
            let mut pspec: [Option<(u32, u32, u32)>; 2] = [None, None];
            for sel in &rule.selectors {
                let slot = match sel.parts.last().and_then(|p| {
                    p.pseudos
                        .iter()
                        .find(|ps| matches!(ps, Pseudo::Before | Pseudo::After))
                }) {
                    Some(Pseudo::Before) => &mut pspec[0],
                    Some(Pseudo::After) => &mut pspec[1],
                    _ => &mut spec,
                };
                if matches(sel, dom, node, ancestors, walk.state) {
                    let sp = specificity(sel);
                    *slot = Some(match *slot {
                        Some(prev) if prev >= sp => prev,
                        _ => sp,
                    });
                }
            }
            if let Some(sp) = spec {
                for decl in &rule.declarations {
                    // R7: layer rank (unlayered wins normally, inverts under
                    // !important).
                    let rank = self.layer_rank(&rule.layer, decl.important);
                    matched.push((decl.clone(), (0, rank, sp, order)));
                }
            }
            for (k, sp) in pspec.iter().enumerate() {
                if let Some(sp) = sp {
                    for decl in &rule.declarations {
                        let rank = self.layer_rank(&rule.layer, decl.important);
                        pseudo_decls[k].push((decl.clone(), (0, rank, *sp, order)));
                    }
                }
            }
        }
        // Inline style: beats normal author rules, loses to !important.
        let inline_start = self.rules.len();
        if let Some(inline) = dom.element(node).and_then(|el| el.attr("style")) {
            for (i, decl) in parse_declarations(inline).into_iter().enumerate() {
                matched.push((
                    decl,
                    (
                        1,
                        u32::MAX,
                        (u32::MAX, u32::MAX, u32::MAX),
                        inline_start + i,
                    ),
                ));
            }
        }
        // Author !important beats inline style.
        for (decl, key) in matched.iter_mut() {
            if decl.important {
                key.0 = 2;
            }
        }
        matched.sort_by_key(|(_, key)| *key);

        // Custom properties fold over the inherited set in cascade order, so
        // a higher-priority `--name` overrides, and later declarations on the
        // same element can reference earlier ones.
        let parent_vars = parent.map(|p| p.vars.clone()).unwrap_or_default();
        let mut vars: Vec<(String, String)> = parent_vars.clone();
        // B15: `@property` registration semantics. A non-inherited custom
        // property must not ride down from the parent — drop registered
        // non-inheriting names from the inherited set (this element's own
        // declarations re-add them in the fold below). A registered property
        // with an `initial-value` and no value in scope starts from that
        // initial — injected before the fold, so `--x: var(--y)` sees y's
        // registered initial and `var(--registered, fallback)` never takes
        // the fallback (registered properties are guaranteed-value per
        // spec; `syntax` is not validated — documented).
        if !self.properties.is_empty() {
            vars.retain(|(name, _)| {
                self.properties
                    .iter()
                    .rfind(|p| &p.name == name)
                    .map(|p| p.inherits)
                    .unwrap_or(true)
            });
            for p in &self.properties {
                if let Some(init) = &p.initial {
                    if !vars.iter().any(|(n, _)| n == &p.name) {
                        vars.push((p.name.clone(), init.clone()));
                    }
                }
            }
        }
        for (decl, _) in matched.iter() {
            if decl.property.starts_with("--") {
                // Store the full `--name` — var() references include it.
                let value = resolve_vars(&decl.value, &|n: &str| {
                    find_var(&vars, n).map(str::to_string)
                })
                .and_then(|value| {
                    css_wide_keyword(&value)
                        .map(|keyword| {
                            custom_property_wide_value(
                                &self.properties,
                                &decl.property,
                                keyword,
                                &parent_vars,
                            )
                        })
                        .unwrap_or(Some(value))
                });
                if let Some(value) = value {
                    upsert_var(&mut vars, decl.property.clone(), value);
                } else {
                    vars.retain(|(name, _)| name != &decl.property);
                }
            }
        }
        // Own a snapshot so `vars` itself can still move into the style
        // (the pseudo-element cascade below reuses `lookup`).
        let lookup = {
            let vars = vars.clone();
            move |n: &str| find_var(&vars, n).map(str::to_string)
        };

        // Substitute var() everywhere else; a declaration whose variables are
        // missing (and carry no fallback) is invalid and dropped.
        let mut resolved: Vec<(Declaration, CascadeKey)> = matched
            .into_iter()
            .filter(|(decl, _)| !decl.property.starts_with("--"))
            .filter_map(|(mut decl, key)| {
                decl.value = resolve_vars(&decl.value, &lookup)?;
                // B3: fold cq units against the nearest ancestor container.
                if value_has_cq_unit(&decl.value) {
                    let cw = walk.cq.last().map(|(_, w)| *w);
                    decl.value = subst_cq_units(&decl.value, cw, self.viewport_w, self.viewport_h);
                }
                Some((decl, key))
            })
            .collect();
        // B15: the `all` shorthand — processed first, since `all: revert`
        // is a shorthand keyword rather than a dropped declaration.
        let all_kw = apply_all_shorthand(&mut resolved);
        // B15: CSS-wide keywords. `revert`/`revert-layer` as a value drop
        // the declaration, so the next-lower-priority one wins — the
        // correct outcome at author level (the engine has no separate UA
        // origin to revert to; documented approximation).
        drop_revert_decls(&mut resolved);

        let mut style = parent.map(child_base).unwrap_or_default();
        if let Some(keyword) = all_kw.as_deref() {
            apply_all_css_wide(&mut style, parent, keyword);
        }
        style.vars = vars;
        let parent_em = style.font_size;
        for (decl, _) in resolved.iter().filter(|(d, _)| d.property == "color") {
            self.apply(decl, &mut style, parent_em, parent);
        }
        for (decl, _) in resolved.iter().filter(|(d, _)| d.property == "font-size") {
            self.apply(decl, &mut style, parent_em, parent);
        }
        let em = style.font_size;
        for (decl, _) in resolved
            .iter()
            .filter(|(d, _)| d.property != "font-size" && d.property != "color")
        {
            self.apply(decl, &mut style, em, parent);
        }
        // B4: logical geometry resolves against the element's FINAL
        // writing-mode + direction, so it folds after every declaration
        // (order of `writing-mode:` vs `inline-size:` no longer matters).
        fold_logical(&mut style);

        // B14: counters and list markers, resolved before pseudo content so
        // a `::before { content: counter(…) }` sees this element's own
        // increments. `walk.counters.last()` is the parent's scope here —
        // the element's own scope is pushed back in `compute_subtree`.
        let implicit_li = style.display == Display::ListItem && style.counter_increment.is_empty();
        if implicit_li {
            counter_bump(&mut walk.counters, "list-item", 1);
        }
        for op in &style.counter_increment {
            if implicit_li && op.name == "list-item" {
                continue; // covered by the implicit step above
            }
            counter_bump(&mut walk.counters, &op.name, op.value);
        }
        if style.display == Display::ListItem && style.list_style_type != ListStyleType::None {
            let mut t = style.list_style_type;
            if !style.list_type_author {
                // No author/UA `list-style-type` in play: emulate the
                // browser UA rule `ol { list-style-type: decimal }` and the
                // HTML `type` attribute by consulting the nearest list
                // ancestor. (A real UA rule would flip the author flag and
                // block the attribute — hence the marker-time heuristic.)
                let mut p = dom.node(node).parent;
                while let Some(id) = p {
                    let n = dom.node(id);
                    let tag = n.tag();
                    if matches!(tag, "ol" | "ul" | "menu") {
                        let av = n
                            .element
                            .as_ref()
                            .and_then(|e| e.attr("type"))
                            .and_then(list_style_type_attr);
                        match av {
                            Some(a) => t = a,
                            None if tag == "ol" => t = ListStyleType::Decimal,
                            None => {}
                        }
                        break;
                    }
                    p = n.parent;
                }
            }
            style.marker_text = Some(match t {
                // Symbol markers are constant; counting markers read the
                // post-increment implicit `list-item` counter.
                ListStyleType::Disc | ListStyleType::Circle | ListStyleType::Square => {
                    format_counter(t, 0)
                }
                _ => format!(
                    "{}.",
                    format_counter(t, counter_value(&walk.counters, "list-item"))
                ),
            });
        }

        // R3: generated content. The pseudo's cascade inherits from the
        // element's own computed style; `content` decides whether anything
        // generates at all. Index 0 = ::before, 1 = ::after.
        for k in [0usize, 1usize] {
            if pseudo_decls[k].is_empty() {
                continue;
            }
            let mut resolved_p: Vec<(Declaration, CascadeKey)> = pseudo_decls[k]
                .iter()
                .filter_map(|(decl, key)| {
                    let mut d = decl.clone();
                    d.value = resolve_vars(&d.value, &lookup)?;
                    // B3: pseudo boxes inherit the element's cq context.
                    if value_has_cq_unit(&d.value) {
                        let cw = walk.cq.last().map(|(_, w)| *w);
                        d.value = subst_cq_units(&d.value, cw, self.viewport_w, self.viewport_h);
                    }
                    Some((d, *key))
                })
                .collect();
            // B15: same CSS-wide keyword handling as element styles.
            let all_p = apply_all_shorthand(&mut resolved_p);
            drop_revert_decls(&mut resolved_p);
            let mut ps = child_base(&style);
            if let Some(keyword) = all_p.as_deref() {
                apply_all_css_wide(&mut ps, Some(&style), keyword);
            }
            let parent_em = ps.font_size;
            for (decl, _) in resolved_p.iter().filter(|(d, _)| d.property == "color") {
                self.apply(decl, &mut ps, parent_em, Some(&style));
            }
            for (decl, _) in resolved_p.iter().filter(|(d, _)| d.property == "font-size") {
                self.apply(decl, &mut ps, parent_em, Some(&style));
            }
            let em = ps.font_size;
            for (decl, _) in resolved_p
                .iter()
                .filter(|(d, _)| d.property != "font-size" && d.property != "color")
            {
                self.apply(decl, &mut ps, em, Some(&style));
            }
            // B4: pseudo-element declarations can carry logical geometry
            // too; fold against the (inherited) writing mode + direction.
            fold_logical(&mut ps);
            let text = match ps.content.take() {
                Some(ContentSpec::Text(t)) => Some(t),
                Some(ContentSpec::Attr(name)) => dom
                    .element(node)
                    .and_then(|el| el.attr(&name))
                    .map(str::to_string),
                Some(ContentSpec::Parts(parts)) => Some(
                    parts
                        .iter()
                        .map(|p| resolve_content_part(p, dom, node, &walk.counters))
                        .collect::<String>(),
                ),
                None => None,
            }
            .unwrap_or_default();
            if !text.is_empty() {
                let boxed = Box::new(PseudoStyle { text, style: ps });
                if k == 0 {
                    style.before = Some(boxed);
                } else {
                    style.after = Some(boxed);
                }
            }
        }
        style
    }

    /// Apply one declaration. `em` is the font size relative lengths resolve
    /// against (parent's for font-size, own for everything else).
    fn apply(
        &self,
        decl: &Declaration,
        style: &mut ComputedStyle,
        em: f32,
        parent: Option<&ComputedStyle>,
    ) {
        let v = decl.value.trim();
        if let Some(keyword) = css_wide_keyword(v) {
            apply_css_wide_property(style, parent, &decl.property, keyword);
            return;
        }
        let rem = 16.0_f32;
        // Q1: lengths may be calc()/min()/max()/clamp() expressions. em/rem
        // fold here; percentages stay symbolic until layout resolves them
        // against the containing-block width.
        // R5: viewport units fold at style time against the engine's viewport.
        let (vw, vh) = (self.viewport_w, self.viewport_h);
        // B9: when the value mentions ch/ex and a metrics source is
        // attached, fold those units against the element's real font.
        // Snapshot the font context now — later declarations in the same
        // block must not retroactively change this one's units.
        let ch_ex = if value_has_ch_ex(v) {
            match &self.metrics {
                Some(m) => {
                    let fam = style.font_family.clone();
                    let (fw, fi, fm) = (style.font_weight, style.italic, style.monospace);
                    Some((m.ch_px(&fam, em, fw, fi, fm), m.ex_px(&fam, em, fw, fi, fm)))
                }
                // No metrics source (headless, tests): the CSS fallback
                // ratio — both units fold to half the font size.
                None => Some((em * 0.5, em * 0.5)),
            }
        } else {
            None
        };
        let len = |s: &str| match ch_ex {
            Some((ch, ex)) => parse_len_px_ch(s, em, rem, vw, vh, ch, ex),
            None => parse_len_px_v(s, em, rem, vw, vh),
        };
        // Same, but a percentage-dependent value is unset: only meaningful
        // where no containing-block width exists (height-ish properties).
        let len_px = |s: &str| len(s).filter(|l| !l.has_pct());
        // C1: `width`/`height` grammar — the intrinsic keywords plus the
        // full length parse. Percentages are kept in `sz`; the `%`-free
        // variant `sz_px` (keywords allowed, `%` dropped) is for height-ish
        // slots whose containing-block size is indefinite.
        let sz = |s: &str| match ch_ex {
            Some((ch, ex)) => parse_size_val_ch(s, em, rem, vw, vh, ch, ex),
            None => parse_size_val_v(s, em, rem, vw, vh),
        };
        let sz_px = |s: &str| {
            sz(s).filter(|v| match v {
                SizeVal::Len(l) => !l.has_pct(),
                _ => true,
            })
        };

        // Four-edge shorthand: 1–4 whitespace-separated values. Paren-aware
        // so `calc(100% - 4px)` isn't split inside the function.
        let edges4 = |val: &str| -> Option<Edges> {
            let parts = split_top_whitespace(val);
            let f = |s: &str| len(s);
            match parts.len() {
                1 => {
                    let x = f(parts[0])?;
                    Some(Edges {
                        top: x,
                        right: x,
                        bottom: x,
                        left: x,
                    })
                }
                2 => {
                    let tb = f(parts[0])?;
                    let lr = f(parts[1])?;
                    Some(Edges {
                        top: tb,
                        right: lr,
                        bottom: tb,
                        left: lr,
                    })
                }
                3 => {
                    let t = f(parts[0])?;
                    let lr = f(parts[1])?;
                    let b = f(parts[2])?;
                    Some(Edges {
                        top: t,
                        right: lr,
                        bottom: b,
                        left: lr,
                    })
                }
                4 => {
                    let t = f(parts[0])?;
                    let r = f(parts[1])?;
                    let b = f(parts[2])?;
                    let l = f(parts[3])?;
                    Some(Edges {
                        top: t,
                        right: r,
                        bottom: b,
                        left: l,
                    })
                }
                _ => None,
            }
        };

        match decl.property.as_str() {
            "display" => {
                if let Some(d) = match v {
                    "none" => Some(Display::None),
                    "block" => Some(Display::Block),
                    "inline" => Some(Display::Inline),
                    "inline-block" => Some(Display::InlineBlock),
                    "list-item" => Some(Display::ListItem),
                    "flex" => Some(Display::Flex),
                    "grid" => Some(Display::Grid),
                    "table" => Some(Display::Table),
                    "table-row" => Some(Display::TableRow),
                    "table-cell" => Some(Display::TableCell),
                    "table-row-group" | "table-header-group" | "table-footer-group" => {
                        Some(Display::TableRowGroup)
                    }
                    "table-caption" => Some(Display::TableCaption),
                    // Columns are metadata, not boxes.
                    "table-column" => Some(Display::TableColumn),
                    "table-column-group" => Some(Display::TableColumnGroup),
                    _ => None,
                } {
                    style.display = d;
                }
            }
            "color" => {
                if v.eq_ignore_ascii_case("currentcolor") {
                    style.color = parent
                        .map(|p| p.color)
                        .unwrap_or(ComputedStyle::default().color);
                } else if let Some(c) = parse_color(v) {
                    style.color = c;
                }
            }
            "fill" | "stroke" => {
                let color = if v.eq_ignore_ascii_case("none") {
                    None
                } else {
                    parse_style_color(v, style.color)
                };
                if color.is_some() || v.eq_ignore_ascii_case("none") {
                    if decl.property == "fill" {
                        style.fill = color;
                    } else {
                        style.stroke = color;
                    }
                }
            }
            "background" => {
                if let Some(layers) = parse_background_layers(v, style) {
                    style.background_layers = layers;
                    sync_background_scalars(style);
                }
            }
            "background-color" => {
                if let Some(c) = parse_style_color(v, style.color) {
                    apply_background_color(style, c);
                }
            }
            "background-image" => {
                if let Some(paints) = parse_background_image_layers(v, style.color) {
                    resize_background_layers(style, paints.len());
                    for (layer, paint) in style.background_layers.iter_mut().zip(paints) {
                        layer.paint = match paint {
                            Some(p) => p,
                            // `none` drops the layer's image and keeps its
                            // color, exactly as the longhand always has.
                            None => BackgroundPaint::Color(layer.color),
                        };
                        // A gradient has no separate color: the fast path is
                        // its first stop.
                        if layer_is_gradient(&layer.paint) {
                            layer.color = gradient_first_stop(&layer.paint).unwrap_or(layer.color);
                        }
                    }
                    sync_background_scalars(style);
                }
            }
            "background-position" => {
                if let Some(values) = background_layer_list(v, parse_background_position) {
                    ensure_background_layers(style);
                    apply_layer_list(style, values, |l, p| l.position = p);
                    style.background_position = style.background_layers[0].position;
                }
            }
            "background-size" => {
                if let Some(values) = background_layer_list(v, parse_background_size) {
                    ensure_background_layers(style);
                    apply_layer_list(style, values, |l, s| l.size = s);
                    style.background_size = style.background_layers[0].size;
                }
            }
            "background-repeat" => {
                if let Some(values) = background_layer_list(v, parse_background_repeat) {
                    ensure_background_layers(style);
                    apply_layer_list(style, values, |l, r| l.repeat = r);
                    style.background_repeat = style.background_layers[0].repeat;
                }
            }
            "background-origin" | "background-clip" => {
                if let Some(values) = background_layer_list(v, parse_bg_box) {
                    ensure_background_layers(style);
                    let origin = decl.property == "background-origin";
                    apply_layer_list(style, values, |l, b| {
                        if origin {
                            l.origin = b
                        } else {
                            l.clip = b
                        }
                    });
                }
            }
            "visibility" => {
                style.visibility = match v {
                    "hidden" => Visibility::Hidden,
                    "collapse" => Visibility::Collapse,
                    "visible" => Visibility::Visible,
                    // `inherit`/`initial` are handled before this point; an
                    // unknown keyword leaves the cascaded value alone.
                    _ => return,
                };
            }
            "border-radius" => {
                // Circular corners only (the `/` elliptical half is ignored);
                // 1–4 space-separated lengths, CSS order TL TR BR BL.
                let before_slash = v.split('/').next().unwrap_or("").trim();
                let parts: Vec<Length> = before_slash
                    .split_whitespace()
                    .filter_map(|t| parse_length(t))
                    .collect();
                let r = match parts.as_slice() {
                    [a] => BorderRadius::uniform(*a),
                    [v1, h1] => BorderRadius {
                        top_left: *v1,
                        top_right: *h1,
                        bottom_right: *v1,
                        bottom_left: *h1,
                    },
                    [t, h, b] => BorderRadius {
                        top_left: *t,
                        top_right: *h,
                        bottom_right: *b,
                        bottom_left: *h,
                    },
                    [tl, tr, br, bl, ..] => BorderRadius {
                        top_left: *tl,
                        top_right: *tr,
                        bottom_right: *br,
                        bottom_left: *bl,
                    },
                    [] => BorderRadius::ZERO,
                };
                style.border_radius = r;
            }
            "box-shadow" => {
                style.box_shadows = parse_box_shadows(v, em);
            }
            // R8: inherited text shadows, painted as duplicate runs behind
            // the text (crisp — blur parsed but not painted in v1).
            "text-shadow" => {
                style.text_shadows = parse_text_shadows(v, em);
            }
            "font-size" => {
                // In this pass `em` is the parent font size, so `120%` and
                // `1.2em` both resolve against the parent (Q1 fix).
                if let Some(px) = len(v).map(|l| l.resolve(em)) {
                    style.font_size = px.max(6.0);
                }
            }
            "font-weight" => {
                style.font_weight = match v {
                    "normal" => 400,
                    "bold" => 700,
                    "bolder" => style.font_weight.max(400) + 200,
                    "lighter" => style.font_weight.min(700).saturating_sub(200),
                    other => other
                        .parse::<u32>()
                        .map(|n| n.clamp(100, 900))
                        .unwrap_or(style.font_weight),
                };
            }
            "font-style" => {
                style.italic = v == "italic" || v == "oblique";
            }
            "font-family" => {
                set_font_family(style, v);
            }
            // CSS `font` shorthand: [ <'font-style'> || <'font-variant'> ||
            // <'font-weight'> ]? <'font-size'> [ / <'line-height'> ]?
            // <'font-family'>. The site workhorse `font: inherit` (and
            // `unset`/`initial`/the system-ui font keywords) is a no-op —
            // font properties inherit by default, which is exactly what
            // those spellings ask for.
            "font" => {
                let lower = v.to_ascii_lowercase();
                if !matches!(
                    lower.as_str(),
                    "inherit"
                        | "unset"
                        | "initial"
                        | "caption"
                        | "icon"
                        | "menu"
                        | "message-box"
                        | "small-caption"
                        | "status-bar"
                ) {
                    let mut italic = false;
                    let mut weight: Option<f32> = None;
                    let mut size_px: Option<f32> = None;
                    let mut line_h: Option<f32> = None;
                    let mut family: Option<String> = None;
                    let mut toks = v.split_whitespace().peekable();
                    while let Some(tok) = toks.peek() {
                        let t = tok.to_ascii_lowercase();
                        if size_px.is_none() {
                            match t.as_str() {
                                "italic" | "oblique" => {
                                    italic = true;
                                    toks.next();
                                    continue;
                                }
                                // `normal` fills whichever style/variant/
                                // weight slot it appears in — all initial.
                                "normal" | "small-caps" | "bolder" | "lighter" => {
                                    toks.next();
                                    continue;
                                }
                                // The shorthand grammar only allows
                                // hundred-step weights here.
                                w if w
                                    .parse::<u32>()
                                    .map(|n| (100..=900).contains(&n) && n % 100 == 0)
                                    .unwrap_or(false) =>
                                {
                                    weight = Some(w.parse::<u32>().unwrap() as f32);
                                    toks.next();
                                    continue;
                                }
                                // Font-size keywords (the length parse below
                                // handles `18px` and friends).
                                "xx-small" => {
                                    size_px = Some(9.0);
                                    toks.next();
                                    continue;
                                }
                                "x-small" => {
                                    size_px = Some(10.0);
                                    toks.next();
                                    continue;
                                }
                                "small" => {
                                    size_px = Some(13.0);
                                    toks.next();
                                    continue;
                                }
                                "medium" => {
                                    size_px = Some(16.0);
                                    toks.next();
                                    continue;
                                }
                                "large" => {
                                    size_px = Some(18.0);
                                    toks.next();
                                    continue;
                                }
                                "x-large" => {
                                    size_px = Some(24.0);
                                    toks.next();
                                    continue;
                                }
                                "xx-large" => {
                                    size_px = Some(32.0);
                                    toks.next();
                                    continue;
                                }
                                "larger" => {
                                    size_px = Some(em * 1.2);
                                    toks.next();
                                    continue;
                                }
                                "smaller" => {
                                    size_px = Some((em / 1.2).max(6.0));
                                    toks.next();
                                    continue;
                                }
                                _ => {}
                            }
                            // The size slot may carry the line-height:
                            // `18px/1.5`.
                            let (size_tok, lh_tok) = match t.split_once('/') {
                                Some((a, b)) => (a, Some(b)),
                                None => (t.as_str(), None),
                            };
                            if let Some(px) =
                                len(size_tok).map(|l| l.resolve(em)).filter(|px| *px > 0.0)
                            {
                                size_px = Some(px);
                                if let Some(b) = lh_tok {
                                    if let Some(n) = parse_number(b).filter(|n| *n > 0.0) {
                                        line_h = Some(n);
                                    } else if let Some(lh) =
                                        len(b).map(|l| l.resolve(em)).filter(|px| *px > 0.0)
                                    {
                                        line_h = Some((lh / px).max(0.8));
                                    }
                                }
                                toks.next();
                                continue;
                            }
                            // The grammar puts the size ahead of the family —
                            // everything left is the family list.
                        }
                        family = Some(toks.by_ref().collect::<Vec<_>>().join(" "));
                        break;
                    }
                    if let Some(px) = size_px {
                        style.font_size = px.max(6.0);
                    }
                    if let Some(wgt) = weight {
                        style.font_weight = wgt.round() as u32;
                    }
                    if italic {
                        style.italic = true;
                    }
                    if let Some(n) = line_h {
                        style.line_height = n;
                    }
                    if let Some(fam) = family {
                        set_font_family(style, &fam);
                    }
                }
            }
            // D4: OpenType feature plumbing. `font-variant*` fills the
            // high-level vector (per-axis; a later longhand overrides the
            // shorthand's whole axis), `font-feature-settings` the
            // low-level one; consumers apply variant first, then settings,
            // so the two mechanisms never fight. The numeric /
            // east-asian / position axes are not applied yet.
            "font-variant" | "font-variant-caps" | "font-variant-ligatures" => {
                let axis = decl.property.as_str();
                let axis = if axis == "font-variant" {
                    "shorthand"
                } else {
                    axis.trim_start_matches("font-variant-")
                };
                let (features, small_caps) = parse_font_variant(v, axis);
                if axis == "shorthand" {
                    style.variant_features = features;
                } else {
                    // Longhand: replace only this axis's tags, keeping the
                    // other axes' keyword requests intact.
                    let tags = axis_tags(axis);
                    style
                        .variant_features
                        .retain(|(t, _)| !tags.contains(&t.as_str()));
                    style.variant_features.extend(features);
                }
                if let Some(sc) = small_caps {
                    style.small_caps = sc;
                }
            }
            "font-feature-settings" => {
                style.font_features = parse_font_features(v);
            }
            // R3: `content` — meaningful only on ::before/::after (cascade
            // resolves it against the element and drops empty results).
            "content" => {
                style.content = parse_content_spec(v);
            }
            // B14: list markers and counters. The type/position trio is
            // inherited; the counter ops are per-element instructions the
            // cascade executes in document order.
            "list-style-type" => {
                if let Some(t) = list_style_type_kw(v) {
                    style.list_style_type = t;
                    style.list_type_author = true;
                }
            }
            "list-style-position" => match v.trim().to_ascii_lowercase().as_str() {
                "inside" => style.list_style_position = ListStylePosition::Inside,
                "outside" => style.list_style_position = ListStylePosition::Outside,
                _ => {}
            },
            "list-style" => {
                // Shorthand: resets both, then consumes any-order tokens;
                // image (url()) tokens are accepted and ignored.
                style.list_style_type = ListStyleType::Disc;
                style.list_style_position = ListStylePosition::Outside;
                style.list_type_author = true;
                for tok in split_top_whitespace(v) {
                    let tok = tok.trim();
                    if tok.eq_ignore_ascii_case("inside") {
                        style.list_style_position = ListStylePosition::Inside;
                    } else if tok.eq_ignore_ascii_case("outside") {
                        style.list_style_position = ListStylePosition::Outside;
                    } else if tok.starts_with("url(") {
                        // Marker image: painted markers are a later slice.
                    } else if let Some(t) = list_style_type_kw(tok) {
                        style.list_style_type = t;
                    }
                }
            }
            "counter-reset" => {
                style.counter_reset = parse_counter_ops(v, 0);
            }
            "counter-increment" => {
                style.counter_increment = parse_counter_ops(v, 1);
            }
            "text-decoration"
            | "text-decoration-line"
            | "text-decoration-style"
            | "text-decoration-color"
            | "text-decoration-thickness" => {
                // R2: `none` clears every line; the three lines set
                // independently (comma/space separated per CSS).
                // B12: the shorthand also carries style / color / thickness
                // and resets the longhands it does not mention.
                let prop = decl.property.as_str();
                if prop == "text-decoration-style" {
                    if let Some(s) = decoration_style_kw(v) {
                        style.decoration_style = s;
                    }
                } else if prop == "text-decoration-color" {
                    style.decoration_color = if v.eq_ignore_ascii_case("currentcolor") {
                        None
                    } else {
                        parse_color(v)
                    };
                } else if prop == "text-decoration-thickness" {
                    style.decoration_thickness = match v {
                        "auto" | "from-font" => None,
                        _ => len(v),
                    };
                } else {
                    if prop == "text-decoration" {
                        style.decoration_style = DecorationStyle::Solid;
                        style.decoration_color = None;
                        style.decoration_thickness = None;
                    }
                    if v.trim() == "none" {
                        style.underline = false;
                        style.overline = false;
                        style.line_through = false;
                    } else {
                        style.underline = v.contains("underline");
                        style.overline = v.contains("overline");
                        style.line_through = v.contains("line-through");
                        if prop == "text-decoration" {
                            // Extra shorthand tokens: style keyword, length,
                            // color — in any order.
                            for tok in split_top_whitespace(v) {
                                if let Some(s) = decoration_style_kw(tok) {
                                    style.decoration_style = s;
                                } else if tok.eq_ignore_ascii_case("auto")
                                    || tok.eq_ignore_ascii_case("from-font")
                                {
                                    // thickness keyword: stays None (auto).
                                } else if let Some(l) = len(tok) {
                                    style.decoration_thickness = Some(l);
                                } else if let Some(c) = parse_color(tok) {
                                    style.decoration_color = Some(c);
                                }
                            }
                        }
                    }
                }
            }
            // B13: line-break control. All four are inherited; unknown
            // keywords leave the current value (CSS ignores invalid).
            "overflow-wrap" | "word-wrap" => {
                // Legacy `word-wrap: break` maps to `break-word`.
                style.overflow_wrap = match v.to_ascii_lowercase().as_str() {
                    "normal" => OverflowWrap::Normal,
                    "break-word" | "break" => OverflowWrap::BreakWord,
                    "anywhere" => OverflowWrap::Anywhere,
                    _ => style.overflow_wrap,
                };
            }
            "word-break" => {
                style.word_break = match v.to_ascii_lowercase().as_str() {
                    "normal" => WordBreak::Normal,
                    "break-all" => WordBreak::BreakAll,
                    "keep-all" => WordBreak::KeepAll,
                    _ => style.word_break,
                };
            }
            "text-overflow" => {
                // A string argument (`text-overflow: "→"`) is treated as the
                // generic ellipsis; only the two keywords are distinguished.
                if v.to_ascii_lowercase().starts_with("ellipsis") || v.trim().starts_with('"') {
                    style.text_overflow = TextOverflow::Ellipsis;
                } else if v.eq_ignore_ascii_case("clip") {
                    style.text_overflow = TextOverflow::Clip;
                }
            }
            "hyphens" => {
                style.hyphens = match v.to_ascii_lowercase().as_str() {
                    "none" => Hyphens::None,
                    "manual" => Hyphens::Manual,
                    "auto" => Hyphens::Auto,
                    _ => style.hyphens,
                };
            }
            "text-align" => {
                // `start`/`end` resolve against the (possibly inherited)
                // direction already on the style: later `direction`
                // declarations in the same rule still see the old value —
                // matches the cascade's declaration-by-declaration walk.
                let rtl = style.direction == Direction::Rtl;
                style.text_align = match v {
                    "center" => TextAlign::Center,
                    "right" => TextAlign::Right,
                    "end" => {
                        if rtl {
                            TextAlign::Left
                        } else {
                            TextAlign::Right
                        }
                    }
                    "start" => {
                        if rtl {
                            TextAlign::Right
                        } else {
                            TextAlign::Left
                        }
                    }
                    _ => TextAlign::Left,
                };
            }
            "direction" => {
                style.direction = match v {
                    "rtl" => Direction::Rtl,
                    _ => Direction::Ltr,
                };
            }
            "vertical-align" => {
                style.vertical_align = if v == "sub" {
                    VerticalAlign::Sub
                } else if v == "super" {
                    VerticalAlign::Super
                } else if v == "baseline" {
                    VerticalAlign::Baseline
                } else if let Some(pct) = v
                    .strip_suffix('%')
                    .and_then(|n| n.trim().parse::<f32>().ok())
                {
                    VerticalAlign::Percent(pct / 100.0)
                } else if let Some(l) = parse_length(v) {
                    VerticalAlign::Length(l)
                } else {
                    // top/bottom/middle/text-top/…: baseline approximation.
                    VerticalAlign::Baseline
                };
            }
            "text-transform" => {
                style.text_transform = match v {
                    "uppercase" => TextTransform::Uppercase,
                    "lowercase" => TextTransform::Lowercase,
                    "capitalize" => TextTransform::Capitalize,
                    _ => TextTransform::None,
                };
            }
            "letter-spacing" => {
                // `normal` is the initial 0; lengths resolve px/em/rem (and
                // % is spacing with no basis here → dropped).
                style.letter_spacing = if v == "normal" {
                    0.0
                } else {
                    len(v).and_then(|l| l.px()).unwrap_or(0.0)
                };
            }
            "transform" => {
                // `none` (or an unparseable value) resets to the initial.
                style.transform = if v.eq_ignore_ascii_case("none") {
                    None
                } else {
                    parse_transform(v, em).filter(|t| !t.0.is_empty())
                };
            }
            "transform-origin" => {
                style.transform_origin = parse_transform_origin(v, em).unwrap_or_default();
            }
            "filter" => {
                // `none` (or empty/garbage) clears the effect; the parser
                // also returns an empty list when it sees a bad function,
                // which the CSS spec says nukes the whole declaration.
                let fl = parse_filter_list(v);
                style.filter = if fl.0.is_empty() {
                    None
                } else {
                    Some(fill_drop_shadow_color(fl, style.color))
                };
            }
            "backdrop-filter" => {
                let fl = parse_filter_list(v);
                style.backdrop_filter = if fl.0.is_empty() {
                    None
                } else {
                    Some(fill_drop_shadow_color(fl, style.color))
                };
            }
            // B7: symbolic basic-shape clip; keyword units fold now, % stays
            // symbolic for layout's border-box resolution. `none` clears.
            "clip-path" => {
                style.clip_path = parse_clip_shape(v, em, rem, vw, vh);
            }
            "mask" | "mask-image" | "-webkit-mask" | "-webkit-mask-image" => {
                style.mask = parse_mask_shape(v, em, rem, vw, vh);
            }
            // B8: blend mode + isolation group. Unknown keywords keep the
            // current (cascade) value; `normal`/`auto` reset explicitly.
            "mix-blend-mode" => {
                if let Some(m) = MixBlend::from_keyword(v) {
                    style.mix_blend_mode = m;
                }
            }
            "isolation" => {
                style.isolate = v.trim().eq_ignore_ascii_case("isolate");
            }
            // E5: `will-change: <custom-ident>` — any non-`auto` value
            // promotes the subtree to its own composited layer. (`auto`
            // and `initial` reset; `inherit` is handled by the cascade.)
            "will-change" => {
                style.will_change = !v.trim().eq_ignore_ascii_case("auto");
            }
            // B5: multi-column containers. `auto`/`normal` restore
            // single-column flow; percentages resolve against the column
            // container's inline size at layout, so they survive folding.
            "column-count" => {
                if v == "auto" {
                    style.column_count = None;
                } else if let Some(n) = parse_number(v) {
                    if n >= 1.0 {
                        style.column_count = Some(n as u32);
                    }
                }
            }
            "column-width" => {
                style.column_width = if v == "auto" { None } else { len(v) };
            }
            "columns" => {
                // `<length> || <integer> | auto | normal` in either order;
                // the shorthand resets the longhand it does not mention.
                let mut count = None;
                let mut width = None;
                let mut applied = false;
                for tok in split_top_whitespace(v) {
                    match tok {
                        "auto" | "normal" => applied = true,
                        t => {
                            if let Some(n) = parse_number(t) {
                                if n >= 1.0 {
                                    count = Some(n as u32);
                                    applied = true;
                                }
                            } else if let Some(l) = len(t) {
                                width = Some(l);
                                applied = true;
                            }
                        }
                    }
                }
                if applied {
                    style.column_count = count;
                    style.column_width = width;
                }
            }
            "column-gap" => {
                if let Some(x) = len(v) {
                    style.gap = x;
                    style.column_gap = Some(x);
                } else if v == "normal" {
                    style.column_gap = None;
                }
            }
            // B4 v1: `writing-mode` (inherited keyword; an unknown value
            // keeps the inherited mode). Logical geometry lands in slots
            // and folds onto the physical fields after the cascade — see
            // `fold_logical`.
            "writing-mode" => {
                if let Some(wm) = WritingMode::from_keyword(v) {
                    style.writing_mode = wm;
                }
            }
            // D6: glyph stance on the vertical axis (inherited keyword; an
            // unknown value keeps the inherited orientation).
            "text-orientation" => {
                if let Some(o) = TextOrientation::from_keyword(v) {
                    style.text_orientation = o;
                }
            }
            "inline-size" => style.logical.inline_size = len(v),
            "block-size" => style.logical.block_size = len(v),
            "min-inline-size" => style.logical.min_inline_size = len(v),
            "max-inline-size" => style.logical.max_inline_size = len(v),
            "min-block-size" => style.logical.min_block_size = len(v),
            "max-block-size" => style.logical.max_block_size = len(v),
            "inset-inline-start" => style.logical.inset_inline_start = len(v),
            "inset-inline-end" => style.logical.inset_inline_end = len(v),
            "inset-block-start" => style.logical.inset_block_start = len(v),
            "inset-block-end" => style.logical.inset_block_end = len(v),
            "margin-inline-start" => style.logical.margin_inline_start = len(v),
            "margin-inline-end" => style.logical.margin_inline_end = len(v),
            "margin-block-start" => style.logical.margin_block_start = len(v),
            "margin-block-end" => style.logical.margin_block_end = len(v),
            "padding-inline-start" => style.logical.padding_inline_start = len(v),
            "padding-inline-end" => style.logical.padding_inline_end = len(v),
            "padding-block-start" => style.logical.padding_block_start = len(v),
            "padding-block-end" => style.logical.padding_block_end = len(v),
            "inset-inline" | "inset-block" | "margin-inline" | "margin-block"
            | "padding-inline" | "padding-block" => {
                // Two-value shorthand: first = start, second = end; a
                // single value sets both sides (CSS convention).
                let toks = split_top_whitespace(v);
                let first = toks.first().copied().and_then(len);
                let second = if toks.len() > 1 {
                    toks.get(1).copied().and_then(len)
                } else {
                    first
                };
                match decl.property.as_str() {
                    "inset-inline" => {
                        style.logical.inset_inline_start = first;
                        style.logical.inset_inline_end = second;
                    }
                    "inset-block" => {
                        style.logical.inset_block_start = first;
                        style.logical.inset_block_end = second;
                    }
                    "margin-inline" => {
                        style.logical.margin_inline_start = first;
                        style.logical.margin_inline_end = second;
                    }
                    "margin-block" => {
                        style.logical.margin_block_start = first;
                        style.logical.margin_block_end = second;
                    }
                    "padding-inline" => {
                        style.logical.padding_inline_start = first;
                        style.logical.padding_inline_end = second;
                    }
                    _ => {
                        style.logical.padding_block_start = first;
                        style.logical.padding_block_end = second;
                    }
                }
            }
            "line-height" => {
                if let Some(n) = parse_number(v) {
                    if n > 0.0 {
                        style.line_height = n;
                    }
                } else if let Some(px) = len(v).map(|l| l.resolve(em)) {
                    // A % line-height is relative to the font size (CSS).
                    style.line_height = (px / style.font_size).max(0.8);
                }
            }
            "margin" => {
                if let Some(e) = edges4(v) {
                    style.margin = e;
                }
            }
            "margin-top" => {
                if let Some(x) = len(v) {
                    style.margin.top = x
                }
            }
            "margin-right" => {
                if let Some(x) = len(v) {
                    style.margin.right = x
                }
            }
            "margin-bottom" => {
                if let Some(x) = len(v) {
                    style.margin.bottom = x
                }
            }
            "margin-left" => {
                if let Some(x) = len(v) {
                    style.margin.left = x
                }
            }
            "padding" => {
                if let Some(e) = edges4(v) {
                    style.padding = e;
                }
            }
            "padding-top" => {
                if let Some(x) = len(v) {
                    style.padding.top = x
                }
            }
            "padding-right" => {
                if let Some(x) = len(v) {
                    style.padding.right = x
                }
            }
            "padding-bottom" => {
                if let Some(x) = len(v) {
                    style.padding.bottom = x
                }
            }
            "padding-left" => {
                if let Some(x) = len(v) {
                    style.padding.left = x
                }
            }
            // R1: borders. `border` and per-side shorthands reset their
            // sides to the initial (medium/none/currentColor) then apply the
            // given `<width> || <style> || <color>` components in any order.
            "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
                let sides: [&str; 4] = if decl.property == "border" {
                    ["top", "right", "bottom", "left"]
                } else {
                    let one = decl.property.trim_start_matches("border-");
                    [one, "", "", ""]
                };
                for name in sides {
                    if !name.is_empty() {
                        *border_side_mut(&mut style.border, name) = BorderSide::default();
                    }
                }
                for part in split_top_whitespace(v) {
                    if let Some(w) = border_width_len(part, len) {
                        for name in sides {
                            if !name.is_empty() {
                                border_side_mut(&mut style.border, name).width = w;
                            }
                        }
                    } else if let Some(st) = border_style_kw(part) {
                        for name in sides {
                            if !name.is_empty() {
                                border_side_mut(&mut style.border, name).style = st;
                            }
                        }
                    } else if part.eq_ignore_ascii_case("currentcolor") {
                        // Leave None (currentColor).
                    } else if let Some(c) = parse_color(part) {
                        for name in sides {
                            if !name.is_empty() {
                                border_side_mut(&mut style.border, name).color = Some(c);
                            }
                        }
                    }
                }
            }
            "border-width" => {
                if let Some([t, r, b, l]) = border_edges(v, |s| border_width_len(s, len)) {
                    style.border.top.width = t;
                    style.border.right.width = r;
                    style.border.bottom.width = b;
                    style.border.left.width = l;
                }
            }
            "border-style" => {
                if let Some([t, r, b, l]) = border_edges(v, border_style_kw) {
                    style.border.top.style = t;
                    style.border.right.style = r;
                    style.border.bottom.style = b;
                    style.border.left.style = l;
                }
            }
            "border-color" => {
                if let Some([t, r, b, l]) = border_edges(v, |s| {
                    if s.eq_ignore_ascii_case("currentcolor") {
                        Some(None)
                    } else {
                        parse_color(s).map(Some)
                    }
                }) {
                    style.border.top.color = t;
                    style.border.right.color = r;
                    style.border.bottom.color = b;
                    style.border.left.color = l;
                }
            }
            "border-top-width"
            | "border-right-width"
            | "border-bottom-width"
            | "border-left-width"
            | "border-top-style"
            | "border-right-style"
            | "border-bottom-style"
            | "border-left-style"
            | "border-top-color"
            | "border-right-color"
            | "border-bottom-color"
            | "border-left-color" => {
                // "border-<side>-<which>"
                let rest = decl.property.trim_start_matches("border-");
                if let Some((side_name, which)) = rest.split_once('-') {
                    let side = border_side_mut(&mut style.border, side_name);
                    match which {
                        "width" => {
                            if let Some(w) = border_width_len(v, len) {
                                side.width = w;
                            }
                        }
                        "style" => {
                            if let Some(st) = border_style_kw(v) {
                                side.style = st;
                            }
                        }
                        _ => {
                            side.color = if v.eq_ignore_ascii_case("currentcolor") {
                                None
                            } else {
                                parse_color(v).or(side.color)
                            };
                        }
                    }
                }
            }
            // R1: outline — painted around the border edge, never affects
            // layout. `outline: auto` paints a medium solid currentColor ring.
            "outline" | "outline-width" | "outline-style" | "outline-color" | "outline-offset" => {
                if decl.property == "outline" {
                    style.outline = BorderSide::default();
                    if v.trim() == "auto" {
                        style.outline.style = BorderStyle::Solid;
                    } else {
                        for part in split_top_whitespace(v) {
                            if let Some(w) = border_width_len(part, len) {
                                style.outline.width = w;
                            } else if let Some(st) = border_style_kw(part) {
                                style.outline.style = st;
                            } else if let Some(c) = parse_color(part) {
                                style.outline.color = Some(c);
                            }
                        }
                    }
                } else {
                    match decl.property.trim_start_matches("outline-") {
                        "width" => {
                            if let Some(w) = border_width_len(v, len) {
                                style.outline.width = w;
                            }
                        }
                        "style" => {
                            if v.trim() == "auto" {
                                style.outline.style = BorderStyle::Solid;
                            } else if let Some(st) = border_style_kw(v) {
                                style.outline.style = st;
                            }
                        }
                        "color" => style.outline.color = parse_color(v).or(style.outline.color),
                        _ => {
                            if let Some(o) = len(v) {
                                style.outline_offset = o;
                            }
                        }
                    }
                }
            }
            "width" => {
                if !v.contains("auto") {
                    // C1: intrinsic keywords ride the same value channel.
                    style.width = sz(v);
                }
            }
            "height" => {
                if !v.contains("auto") {
                    // A % height is indefinite (containing-block height is
                    // unknown) → treated as unset.
                    style.height = sz_px(v);
                }
            }
            "aspect-ratio" => {
                // C2: `auto` clears; accepts `w / h` or a bare positive
                // number, each optionally prefixed by `auto` (the "keep the
                // intrinsic ratio if there is one" keyword). Invalid input
                // keeps the previous value.
                let (auto_keyword, t) = split_aspect_auto(v);
                if t.is_empty() {
                    style.aspect_ratio = None;
                } else if let Some((a, b)) = t.split_once('/') {
                    if let (Ok(a), Ok(b)) = (a.trim().parse::<f32>(), b.trim().parse::<f32>()) {
                        if a > 0.0 && b > 0.0 {
                            style.aspect_ratio = Some(AspectRatio {
                                ratio: a / b,
                                auto_keyword,
                            });
                        }
                    }
                } else if let Ok(r) = t.parse::<f32>() {
                    if r > 0.0 {
                        style.aspect_ratio = Some(AspectRatio {
                            ratio: r,
                            auto_keyword,
                        });
                    }
                }
            }
            "break-before" | "page-break-before" => {
                // C3: aliases share the unified `break-*` model.
                if let Some(b) = break_action_kw(v) {
                    style.break_before = b;
                }
            }
            "break-after" | "page-break-after" => {
                if let Some(b) = break_action_kw(v) {
                    style.break_after = b;
                }
            }
            "break-inside" | "page-break-inside" => {
                if let Some(b) = break_inside_kw(v) {
                    style.break_inside = b;
                }
            }
            "orphans" => {
                if let Some(n) = v.trim().parse::<u32>().ok().filter(|n| *n >= 1) {
                    style.orphans = n;
                }
            }
            "widows" => {
                if let Some(n) = v.trim().parse::<u32>().ok().filter(|n| *n >= 1) {
                    style.widows = n;
                }
            }
            "border-collapse" => {
                // C5: the other keyword (`separate`) is the default; an
                // unknown value leaves the previous one.
                match v.trim().to_ascii_lowercase().as_str() {
                    "collapse" => style.border_collapse = true,
                    "separate" => style.border_collapse = false,
                    _ => {}
                }
            }
            "border-spacing" => {
                // C5: one or two non-negative lengths (% invalid per spec);
                // `0` is the default, so invalid input is simply dropped.
                let mut parts = v.split_whitespace();
                if let Some(h) = parts
                    .next()
                    .and_then(|s| len(s))
                    .and_then(|l| l.px())
                    .filter(|p| *p >= 0.0)
                {
                    let w = match parts.next() {
                        Some(second) => len(second).and_then(|l| l.px()).filter(|p| *p >= 0.0),
                        None => Some(h),
                    };
                    if let Some(w) = w {
                        style.border_spacing = (h, w);
                    }
                }
            }
            "caption-side" => {
                // C5: `left`/`right`/`block-start` collapse to `top` for
                // the horizontal-tb-only engine.
                match v.trim().to_ascii_lowercase().as_str() {
                    "top" | "left" | "block-start" => style.caption_side = CaptionSide::Top,
                    "bottom" | "right" | "block-end" => style.caption_side = CaptionSide::Bottom,
                    _ => {}
                }
            }
            "max-width" => {
                if !v.contains("auto") && !v.contains("none") {
                    style.max_width = len(v);
                }
            }
            "min-width" => {
                if !v.contains("auto") {
                    style.min_width = len(v);
                }
            }
            "min-height" => {
                if !v.contains("auto") {
                    style.min_height = len_px(v);
                }
            }
            "max-height" => {
                if !v.contains("auto") && !v.contains("none") {
                    style.max_height = len_px(v);
                }
            }
            "white-space" => {
                style.white_space_pre = v.starts_with("pre");
                style.white_space_wrap = v.contains("wrap");
                style.white_space_nowrap = v.eq_ignore_ascii_case("nowrap");
            }
            "position" => {
                style.position = match v {
                    "relative" => Position::Relative,
                    "absolute" => Position::Absolute,
                    "fixed" => Position::Fixed,
                    "sticky" => Position::Sticky,
                    _ => Position::Static,
                };
            }
            "top" => style.top = if v == "auto" { None } else { len(v) },
            "right" => style.right = if v == "auto" { None } else { len(v) },
            "bottom" => style.bottom = if v == "auto" { None } else { len(v) },
            "left" => style.left = if v == "auto" { None } else { len(v) },
            "float" => {
                style.float = match v {
                    "left" => FloatSide::Left,
                    "right" => FloatSide::Right,
                    _ => FloatSide::None,
                };
            }
            "clear" => {
                style.clear = match v.split_whitespace().next().unwrap_or("") {
                    "left" => Clear::Left,
                    "right" => Clear::Right,
                    "both" => Clear::Both,
                    _ => Clear::None,
                };
            }
            "z-index" => {
                style.z_index = if v == "auto" {
                    None
                } else {
                    v.parse::<i32>().ok()
                };
            }
            "overflow" => {
                style.overflow = match v.split_whitespace().next().unwrap_or("") {
                    "hidden" | "clip" => Overflow::Hidden,
                    "auto" => Overflow::Auto,
                    "scroll" => Overflow::Scroll,
                    _ => Overflow::Visible,
                };
            }
            // B3 container queries. The `container` shorthand
            // (`container: card / inline-size`) is not parsed — use the
            // longhands.
            "container-type" => {
                style.container_type = match v.trim().to_ascii_lowercase().as_str() {
                    "inline-size" => ContainerType::InlineSize,
                    "size" => ContainerType::Size,
                    _ => ContainerType::None,
                };
            }
            "container-name" => {
                style.container_names = v
                    .split_whitespace()
                    .filter(|n| !n.eq_ignore_ascii_case("none"))
                    .map(|n| n.trim_matches('"').trim_matches('\'').to_string())
                    .collect();
            }
            // B15: scroll behavior. Parsed and stored; the host consumes
            // them for chaining/snapping (no wire fields yet — documented,
            // like `backdrop-filter`).
            "overscroll-behavior" => {
                let toks: Vec<&str> = v.split_whitespace().collect();
                let (x, y) = match toks.as_slice() {
                    [] => (OverscrollBehavior::Auto, OverscrollBehavior::Auto),
                    [one] => match one.to_ascii_lowercase().as_str() {
                        // Legacy single-axis forms (`x` = contain
                        // horizontally, `y` = vertically).
                        "x" => (OverscrollBehavior::Contain, OverscrollBehavior::Auto),
                        "y" => (OverscrollBehavior::Auto, OverscrollBehavior::Contain),
                        _ => {
                            let b = parse_overscroll_token(one);
                            (b, b)
                        }
                    },
                    [a, b, ..] => (parse_overscroll_token(a), parse_overscroll_token(b)),
                };
                style.overscroll_x = x;
                style.overscroll_y = y;
            }
            "overscroll-behavior-x" => style.overscroll_x = parse_overscroll_token(v),
            "overscroll-behavior-y" => style.overscroll_y = parse_overscroll_token(v),
            "scroll-snap-type" => {
                let (axis, proximity) = parse_snap_type(v);
                style.snap_axis = axis;
                style.snap_proximity = proximity;
            }
            "scroll-snap-align" => style.snap_align = parse_snap_align(v),
            "opacity" => {
                if let Some(o) = parse_number(v) {
                    style.opacity = o.clamp(0.0, 1.0);
                }
            }
            "flex-direction" => {
                style.flex_direction = match v {
                    "row-reverse" => FlexDirection::RowReverse,
                    "column" => FlexDirection::Column,
                    "column-reverse" => FlexDirection::ColumnReverse,
                    _ => FlexDirection::Row,
                };
            }
            "flex" => {
                // `none` → 0 0 auto, `auto` → 1 1 auto, one number → n 1 0%.
                // Paren-aware split so `flex: 1 calc(20px + 2%)` parses.
                let parts = split_top_whitespace(v);
                match parts.as_slice() {
                    ["none"] => {
                        style.flex_grow = 0.0;
                        style.flex_shrink = 0.0;
                        style.flex_basis = None;
                    }
                    ["auto"] => {
                        style.flex_grow = 1.0;
                        style.flex_shrink = 1.0;
                        style.flex_basis = None;
                    }
                    [one] => {
                        if let Some(n) = parse_number(one) {
                            style.flex_grow = n.max(0.0);
                            style.flex_basis = Some(SizeVal::Len(Len::ZERO));
                        }
                    }
                    [a, b] => match (parse_number(a), parse_number(b)) {
                        (Some(g), Some(s)) => {
                            style.flex_grow = g.max(0.0);
                            style.flex_shrink = s.max(0.0);
                            style.flex_basis = Some(SizeVal::Len(Len::ZERO));
                        }
                        (Some(g), _) => {
                            style.flex_grow = g.max(0.0);
                            style.flex_basis = sz(b);
                        }
                        _ => {}
                    },
                    [g, s, basis] => {
                        if let (Some(g), Some(s)) = (parse_number(g), parse_number(s)) {
                            style.flex_grow = g.max(0.0);
                            style.flex_shrink = s.max(0.0);
                            style.flex_basis = if *basis == "auto" { None } else { sz(basis) };
                        }
                    }
                    _ => {}
                }
            }
            "flex-grow" => {
                if let Some(n) = parse_number(v) {
                    style.flex_grow = n.max(0.0);
                }
            }
            "flex-shrink" => {
                if let Some(n) = parse_number(v) {
                    style.flex_shrink = n.max(0.0);
                }
            }
            "flex-basis" => {
                style.flex_basis = if v == "auto" { None } else { sz(v) };
            }
            "align-items" => {
                style.align_items = match v {
                    "flex-start" | "start" => AlignItems::FlexStart,
                    "center" => AlignItems::Center,
                    "flex-end" | "end" => AlignItems::FlexEnd,
                    // baseline approximated as stretch
                    _ => AlignItems::Stretch,
                };
            }
            "justify-content" => {
                style.justify_content = match v {
                    "center" => JustifyContent::Center,
                    "flex-end" | "end" => JustifyContent::FlexEnd,
                    "space-between" | "space-around" | "space-evenly" => {
                        JustifyContent::SpaceBetween
                    }
                    _ => JustifyContent::FlexStart,
                };
            }
            "flex-wrap" => {
                style.flex_wrap = match v {
                    "wrap" => FlexWrap::Wrap,
                    "wrap-reverse" => FlexWrap::WrapReverse,
                    _ => FlexWrap::Now,
                };
            }
            "align-content" => {
                style.align_content = match v {
                    "flex-start" | "start" => AlignContent::FlexStart,
                    "center" => AlignContent::Center,
                    "flex-end" | "end" => AlignContent::FlexEnd,
                    "space-between" => AlignContent::SpaceBetween,
                    // space-around/evenly approximated as stretch
                    _ => AlignContent::Stretch,
                };
            }
            "gap" | "row-gap" | "grid-gap" | "grid-column-gap" | "grid-row-gap" => {
                if let Some(x) = len(v) {
                    style.gap = x;
                    // B5: `gap`/`grid-column-gap` also feed the multicol
                    // column gap; `row-gap` alone does not.
                    if matches!(
                        decl.property.as_str(),
                        "gap" | "grid-gap" | "grid-column-gap"
                    ) {
                        style.column_gap = Some(x);
                    }
                } else if v == "normal" {
                    style.column_gap = None;
                }
            }
            "grid-template-columns" => {
                // C6: a lone `subgrid` is not a track list — it flags the
                // child grid to consume its grid parent's column template.
                if v.trim().eq_ignore_ascii_case("subgrid") {
                    style.grid_template_columns = Vec::new();
                    style.grid_line_names_col = Vec::new();
                    style.grid_subgrid_columns = true;
                } else {
                    let (tracks, names) = parse_grid_tracks_named(v);
                    style.grid_template_columns = tracks;
                    style.grid_line_names_col = names;
                    style.grid_subgrid_columns = false;
                }
            }
            "grid-template-rows" => {
                if v.trim().eq_ignore_ascii_case("subgrid") {
                    style.grid_template_rows = Vec::new();
                    style.grid_line_names_row = Vec::new();
                    style.grid_subgrid_rows = true;
                } else {
                    let (tracks, names) = parse_grid_tracks_named(v);
                    style.grid_template_rows = tracks;
                    style.grid_line_names_row = names;
                    style.grid_subgrid_rows = false;
                }
            }
            "grid-auto-flow" => {
                // C6: `row | column | dense` in either order. `column` is
                // stored for placement work (the engine is row-major today);
                // `dense` is honored by the auto-placement pass. Any unknown
                // token invalidates the whole declaration.
                let mut col = false;
                let mut dense = false;
                let mut ok = true;
                for tok in v.split_whitespace() {
                    match tok.to_ascii_lowercase().as_str() {
                        "row" => {}
                        "column" => col = true,
                        "dense" => dense = true,
                        _ => ok = false,
                    }
                }
                if ok {
                    style.grid_flow_column = col;
                    style.grid_flow_dense = dense;
                }
            }
            "grid-template-areas" => {
                style.grid_template_areas = parse_grid_template_areas(v);
            }
            "grid-area" => {
                // The named form (`grid-area: main`) is what layout resolves;
                // the four-line slash shorthand falls back to its longhands.
                let name = v.trim().to_ascii_lowercase();
                if !name.is_empty() && name != "auto" && !name.contains('/') {
                    style.grid_area = name;
                } else if let Some([rs, cs, re, ce]) = split_grid_shorthand(v) {
                    apply_grid_line(&mut style.grid_row_start, &mut style.grid_row_span, &rs);
                    apply_grid_line(
                        &mut style.grid_column_start,
                        &mut style.grid_column_span,
                        &cs,
                    );
                    apply_grid_line(&mut style.grid_row_end, &mut style.grid_row_span, &re);
                    apply_grid_line(&mut style.grid_column_end, &mut style.grid_column_span, &ce);
                    if let Some(s) = grid_span_between(&style.grid_row_start, &style.grid_row_end) {
                        style.grid_row_span = s;
                    }
                    if let Some(s) =
                        grid_span_between(&style.grid_column_start, &style.grid_column_end)
                    {
                        style.grid_column_span = s;
                    }
                }
            }
            "grid-column" => {
                if let Some([start, end]) = split_grid_shorthand(v) {
                    apply_grid_line(
                        &mut style.grid_column_start,
                        &mut style.grid_column_span,
                        &start,
                    );
                    apply_grid_line(
                        &mut style.grid_column_end,
                        &mut style.grid_column_span,
                        &end,
                    );
                    if let Some(s) =
                        grid_span_between(&style.grid_column_start, &style.grid_column_end)
                    {
                        style.grid_column_span = s;
                    }
                } else {
                    style.grid_column_span = parse_grid_span(v);
                }
            }
            "grid-row" => {
                if let Some([start, end]) = split_grid_shorthand(v) {
                    apply_grid_line(&mut style.grid_row_start, &mut style.grid_row_span, &start);
                    apply_grid_line(&mut style.grid_row_end, &mut style.grid_row_span, &end);
                    if let Some(s) = grid_span_between(&style.grid_row_start, &style.grid_row_end) {
                        style.grid_row_span = s;
                    }
                } else {
                    style.grid_row_span = parse_grid_span(v);
                }
            }
            "grid-column-start" | "grid-row-start" => {
                let is_col = decl.property == "grid-column-start";
                let (slot, span) = if is_col {
                    (&mut style.grid_column_start, &mut style.grid_column_span)
                } else {
                    (&mut style.grid_row_start, &mut style.grid_row_span)
                };
                apply_grid_line(slot, span, v);
            }
            "grid-column-end" | "grid-row-end" => {
                let is_col = decl.property == "grid-column-end";
                let (slot, span) = if is_col {
                    (&mut style.grid_column_end, &mut style.grid_column_span)
                } else {
                    (&mut style.grid_row_end, &mut style.grid_row_span)
                };
                apply_grid_line(slot, span, v);
            }
            "object-fit" => {
                style.object_fit = match v {
                    "contain" => ObjectFit::Contain,
                    "cover" => ObjectFit::Cover,
                    "none" => ObjectFit::None,
                    "scale-down" => ObjectFit::ScaleDown,
                    _ => ObjectFit::Fill,
                };
            }
            "animation" => {
                style.animations = parse_animation_list(v);
            }
            "animation-name" => {
                for (anim, name) in style.animations.iter_mut().zip(split_fn_args(v)) {
                    anim.name = name.trim().trim_matches('"').to_string();
                }
                if style.animations.is_empty()
                    && !v.trim().is_empty()
                    && !v.trim().eq_ignore_ascii_case("none")
                {
                    let mut anim = Animation::default();
                    anim.name = v.trim().trim_matches('"').to_string();
                    style.animations.push(anim);
                }
            }
            "animation-duration" | "animation-delay" => {
                let times: Vec<f32> = split_fn_args(v)
                    .iter()
                    .filter_map(|t| parse_time(t))
                    .collect();
                for (i, anim) in style.animations.iter_mut().enumerate() {
                    if let Some(t) = times.get(i).or(times.first()) {
                        if decl.property == "animation-duration" {
                            anim.duration_secs = t.max(0.0);
                        } else {
                            anim.delay_secs = *t;
                        }
                    }
                }
            }
            "animation-timing-function" => {
                let timings: Vec<Timing> = split_fn_args(v)
                    .iter()
                    .filter_map(|t| parse_timing(t))
                    .collect();
                for (i, anim) in style.animations.iter_mut().enumerate() {
                    if let Some(t) = timings.get(i).or(timings.first()) {
                        anim.timing = *t;
                    }
                }
            }
            "animation-iteration-count" => {
                for anim in style.animations.iter_mut() {
                    anim.iteration = parse_iteration(v);
                }
            }
            "animation-direction" => {
                for anim in style.animations.iter_mut() {
                    anim.direction = parse_anim_direction(v);
                }
            }
            "transition" => {
                style.transitions = parse_transition_list(v);
            }
            "transition-property" => {
                let names: Vec<String> = split_fn_args(v)
                    .iter()
                    .map(|t| t.trim().trim_matches('"').to_string())
                    .collect();
                pad_transitions(&mut style.transitions, names.len());
                for (i, tr) in style.transitions.iter_mut().enumerate() {
                    if let Some(n) = names.get(i).or(names.first()) {
                        tr.property = n.clone();
                    }
                }
            }
            "transition-duration" | "transition-delay" => {
                let times: Vec<f32> = split_fn_args(v)
                    .iter()
                    .filter_map(|t| parse_time(t))
                    .collect();
                pad_transitions(&mut style.transitions, times.len());
                for (i, tr) in style.transitions.iter_mut().enumerate() {
                    if let Some(t) = times.get(i).or(times.first()) {
                        if decl.property == "transition-duration" {
                            tr.duration_secs = t.max(0.0);
                        } else {
                            tr.delay_secs = *t;
                        }
                    }
                }
            }
            "transition-timing-function" => {
                let timings: Vec<Timing> = split_fn_args(v)
                    .iter()
                    .filter_map(|t| parse_timing(t))
                    .collect();
                pad_transitions(&mut style.transitions, timings.len());
                for (i, tr) in style.transitions.iter_mut().enumerate() {
                    if let Some(t) = timings.get(i).or(timings.first()) {
                        tr.timing = *t;
                    }
                }
            }
            _ => {
                // Unsupported on purpose for now (transform/animation/…).
            }
        }
    }
}

fn find_var<'a>(vars: &'a [(String, String)], name: &str) -> Option<&'a str> {
    vars.iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn custom_property_wide_value(
    properties: &[PropertyRule],
    name: &str,
    keyword: CssWideKeyword,
    parent_vars: &[(String, String)],
) -> Option<String> {
    let registration = properties.iter().rev().find(|p| p.name == name);
    let initial = registration.and_then(|p| p.initial.clone());
    match keyword {
        CssWideKeyword::Initial => initial,
        CssWideKeyword::Inherit | CssWideKeyword::Unset
            if registration.map(|p| p.inherits).unwrap_or(true) =>
        {
            find_var(parent_vars, name).map(str::to_string).or(initial)
        }
        CssWideKeyword::Inherit | CssWideKeyword::Unset => initial,
    }
}

/// Parse one timing function token.
fn parse_timing(s: &str) -> Option<Timing> {
    let t = s.trim().to_ascii_lowercase();
    match t.as_str() {
        "linear" => Some(Timing::Linear),
        "ease" => Some(Timing::Ease),
        "ease-in" => Some(Timing::EaseIn),
        "ease-out" => Some(Timing::EaseOut),
        "ease-in-out" => Some(Timing::EaseInOut),
        "step-start" => Some(Timing::Steps(1)),
        "step-end" => Some(Timing::Steps(1)),
        _ => {
            if let Some(inner) = t.strip_prefix("steps(").and_then(|r| r.strip_suffix(')')) {
                let n: u32 = inner.split(',').next()?.trim().parse().ok()?;
                return Some(Timing::Steps(n.max(1)));
            }
            if let Some(inner) = t
                .strip_prefix("cubic-bezier(")
                .and_then(|r| r.strip_suffix(')'))
            {
                let nums: Vec<f32> = inner
                    .split(',')
                    .filter_map(|n| n.trim().parse::<f32>().ok())
                    .collect();
                if let [x1, y1, x2, y2] = nums.as_slice() {
                    return Some(Timing::Bezier(*x1, *y1, *x2, *y2));
                }
            }
            None
        }
    }
}

fn parse_iteration(s: &str) -> IterationCount {
    let t = s.trim().to_ascii_lowercase();
    if t == "infinite" {
        IterationCount::Infinite
    } else {
        t.parse::<f32>()
            .ok()
            .filter(|&n| n > 0.0)
            .map(IterationCount::Count)
            .unwrap_or(IterationCount::Count(1.0))
    }
}

fn parse_anim_direction(s: &str) -> AnimDirection {
    match s.trim().to_ascii_lowercase().as_str() {
        "reverse" => AnimDirection::Reverse,
        "alternate" => AnimDirection::Alternate,
        "alternate-reverse" => AnimDirection::AlternateReverse,
        _ => AnimDirection::Normal,
    }
}

fn parse_fill_mode(s: &str) -> FillMode {
    match s.trim().to_ascii_lowercase().as_str() {
        "forwards" => FillMode::Forwards,
        "backwards" => FillMode::Backwards,
        "both" => FillMode::Both,
        _ => FillMode::None,
    }
}

/// Parse an `animation` shorthand list (comma-separated items, each
/// `[name || duration || timing || delay || iteration || direction ||
/// fill || play-state]` in any order; times distinguish duration (first)
/// from delay (second)).
fn parse_animation_list(value: &str) -> Vec<Animation> {
    let mut out = Vec::new();
    for item in split_fn_args(value) {
        let item = item.trim();
        if item.is_empty() || item.eq_ignore_ascii_case("none") {
            continue;
        }
        let mut anim = Animation::default();
        let mut name_set = false;
        let mut times = 0u32;
        // Whitespace tokens, keeping function groups (cubic-bezier/steps)
        // whole.
        for tok in split_animation_tokens(item) {
            let lower = tok.to_ascii_lowercase();
            if let Some(secs) = parse_time(&tok) {
                times += 1;
                if times == 1 {
                    anim.duration_secs = secs.max(0.0);
                } else if times == 2 {
                    anim.delay_secs = secs;
                }
            } else if let Some(t) = parse_timing(&tok) {
                anim.timing = t;
            } else if lower == "infinite" || is_iteration_number(&lower) {
                anim.iteration = parse_iteration(&tok);
            } else if matches!(
                lower.as_str(),
                "normal" | "reverse" | "alternate" | "alternate-reverse"
            ) {
                anim.direction = parse_anim_direction(&tok);
            } else if matches!(lower.as_str(), "forwards" | "backwards" | "both") {
                anim.fill = parse_fill_mode(&tok);
            } else if matches!(lower.as_str(), "running" | "paused") {
                anim.running = lower == "running";
            } else if !name_set {
                anim.name = tok.trim().trim_matches('"').trim_matches('\'').to_string();
                name_set = true;
            }
        }
        out.push(anim);
    }
    out
}

fn is_iteration_number(s: &str) -> bool {
    s.parse::<f32>().ok().is_some_and(|n| n > 0.0)
}

/// Parse a `transition` shorthand list: comma-separated items, each
/// `[property || duration || timing || delay]` in any order; times
/// distinguish duration (first) from delay (second). `none` drops the list.
fn parse_transition_list(value: &str) -> Vec<Transition> {
    let mut out = Vec::new();
    for item in split_fn_args(value) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if item.eq_ignore_ascii_case("none") {
            return Vec::new();
        }
        let mut tr = Transition::default();
        let mut prop_set = false;
        let mut times = 0u32;
        for tok in split_animation_tokens(item) {
            if let Some(secs) = parse_time(&tok) {
                times += 1;
                if times == 1 {
                    tr.duration_secs = secs.max(0.0);
                } else if times == 2 {
                    tr.delay_secs = secs;
                }
            } else if tok
                .trim_start_matches('-')
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
                || tok.eq_ignore_ascii_case("linear")
                || tok.eq_ignore_ascii_case("ease")
                || tok.starts_with("ease-")
                || tok.starts_with("steps(")
                || tok.starts_with("cubic-bezier(")
            {
                if let Some(t) = parse_timing(&tok) {
                    tr.timing = t;
                }
            } else if !prop_set {
                tr.property = tok.trim().trim_matches('"').trim_matches('\'').to_string();
                prop_set = true;
            }
        }
        out.push(tr);
    }
    out
}

/// Longhands set N values but the shorthand/other longhands may have
/// produced fewer (or zero) entries — grow the list with defaults so index
/// pairing behaves like CSS (extra values extend the list too).
fn pad_transitions(list: &mut Vec<Transition>, n: usize) {
    if n > list.len() {
        list.resize(n, Transition::default());
    }
}

/// Split shorthand on whitespace, keeping balanced function groups whole.
fn split_animation_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                    cur.clear();
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Sample one animation's keyframes at `t_secs` (page-load-relative clock).
/// Returns `(property, value)` overrides for `opacity`, `color` and
/// `background-color` (interpolated) — other properties resolve discretely
/// (later frame wins past the midpoint); `transform` keyframes parse but
/// have no layout model and are skipped (documented). Empty outside the
/// active interval (no fill model) or when nothing matches.
pub fn sample_animation(
    anim: &Animation,
    frames: &kitae_css::Keyframes,
    t_secs: f32,
) -> Vec<(String, String)> {
    if anim.name.is_empty() || anim.name != frames.name || anim.duration_secs <= 0.0 {
        return Vec::new();
    }
    if frames.frames.is_empty() {
        return Vec::new();
    }
    let local = t_secs - anim.delay_secs;
    if local < 0.0 {
        return Vec::new();
    }
    let total = match anim.iteration {
        IterationCount::Infinite => f32::INFINITY,
        IterationCount::Count(n) => anim.duration_secs * n,
    };
    if local >= total {
        return Vec::new();
    }
    let iter = (local / anim.duration_secs).floor();
    let mut p = (local - iter * anim.duration_secs) / anim.duration_secs;
    p = p.clamp(0.0, 1.0);
    let reversed = match anim.direction {
        AnimDirection::Normal => false,
        AnimDirection::Reverse => true,
        AnimDirection::Alternate => (iter as u64) % 2 == 1,
        AnimDirection::AlternateReverse => (iter as u64) % 2 == 0,
    };
    if reversed {
        p = 1.0 - p;
    }
    let eased = anim.timing.apply(p);
    // Surrounding keyframes (hold first/last past the edges).
    let mut lo = &frames.frames[0];
    let mut hi = &frames.frames[frames.frames.len() - 1];
    for w in frames.frames.windows(2) {
        if eased >= w[0].offset && eased <= w[1].offset {
            lo = &w[0];
            hi = &w[1];
            break;
        }
    }
    if eased <= frames.frames[0].offset {
        lo = &frames.frames[0];
        hi = &frames.frames[0];
    }
    if eased >= frames.frames[frames.frames.len() - 1].offset {
        lo = &frames.frames[frames.frames.len() - 1];
        hi = lo;
    }
    let span = hi.offset - lo.offset;
    let k = if span > 0.0 {
        ((eased - lo.offset) / span).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let get = |decls: &[kitae_css::Declaration], prop: &str| {
        decls
            .iter()
            .rev()
            .find(|d| d.property == prop)
            .map(|d| d.value.clone())
    };
    let mut props = Vec::new();
    for prop in ["opacity", "color", "background-color"] {
        if let (Some(a), Some(b)) = (get(&lo.declarations, prop), get(&hi.declarations, prop)) {
            if let Some(v) = interp_value(prop, &a, &b, k) {
                props.push((prop.to_string(), v));
            }
        }
    }
    // Discrete properties: later frame wins past the midpoint.
    for decl in hi.declarations.iter().chain(lo.declarations.iter()) {
        let p = decl.property.as_str();
        if matches!(p, "opacity" | "color" | "background-color" | "transform") {
            continue;
        }
        if !props.iter().any(|(q, _)| q == p) {
            let winner = if k >= 0.5 { &hi } else { &lo };
            if let Some(v) = get(&winner.declarations, p) {
                props.push((p.to_string(), v));
            }
        }
    }
    props
}

/// Interpolate one known property between two keyframe values.
fn interp_value(prop: &str, a: &str, b: &str, k: f32) -> Option<String> {
    match prop {
        "opacity" => {
            let (x, y) = (a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?);
            Some(format!("{:.3}", (x + (y - x) * k).clamp(0.0, 1.0)))
        }
        "color" | "background-color" => {
            let (x, y) = (parse_color(a)?, parse_color(b)?);
            let mut out = 0u32;
            for s in [24, 16, 8, 0] {
                let (ca, cb) = (((x >> s) & 0xFF) as f32, ((y >> s) & 0xFF) as f32);
                out |= ((ca + (cb - ca) * k).round().clamp(0.0, 255.0) as u32) << s;
            }
            if out & 0xFF == 0xFF {
                Some(format!("#{:06X}", out >> 8))
            } else {
                Some(format!(
                    "rgba({}, {}, {}, {:.3})",
                    (out >> 24) & 0xFF,
                    (out >> 16) & 0xFF,
                    (out >> 8) & 0xFF,
                    (out & 0xFF) as f32 / 255.0
                ))
            }
        }
        "transform" => interp_transform(a, b, k),
        _ => None,
    }
}

/// Interpolate a `transform` value between two computed strings (B1). Lists
/// with the same shape (same length, same function kinds in order) lerp per
/// component; anything else is discrete (returns `None`). `none` interpolates
/// against the other side's shape with identity values.
fn interp_transform(a: &str, b: &str, k: f32) -> Option<String> {
    const EM: f32 = 16.0; // px already folded; em in stored values is symbolic only
    let parse_side = |s: &str| -> Option<Vec<TransformFn>> {
        let t = s.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("none") {
            return None;
        }
        parse_transform(t, EM).map(|l| l.0)
    };
    let (la, lb) = (parse_side(a), parse_side(b));
    if la.is_none() && lb.is_none() {
        return Some("none".to_string());
    }
    // Pad a missing side with identity functions of the other side's kinds.
    let fill = |list: Option<Vec<TransformFn>>, shape: &[TransformFn]| -> Vec<TransformFn> {
        list.unwrap_or_else(|| shape.iter().map(identity_of).collect())
    };
    let (va, vb) = match (la, lb) {
        (Some(x), Some(y)) => (x, y),
        (Some(x), None) => {
            let shape = x.clone();
            (x, fill(None, &shape))
        }
        (None, Some(y)) => {
            let shape = y.clone();
            (fill(None, &shape), y)
        }
        (None, None) => unreachable!(),
    };
    if va.len() != vb.len() {
        return None;
    }
    let mut out = Vec::with_capacity(va.len());
    for (fa, fb) in va.iter().zip(vb.iter()) {
        out.push(transform_fn_lerp(*fa, *fb, k)?);
    }
    let parts: Vec<String> = out.iter().map(|f| format_transform_fn(f)).collect();
    Some(parts.join(" "))
}

/// Identity function for a given kind (used to interpolate from/to `none`).
fn identity_of(f: &TransformFn) -> TransformFn {
    match f {
        TransformFn::Translate { .. } => TransformFn::Translate {
            x: TransLen::ZERO,
            y: TransLen::ZERO,
        },
        TransformFn::TranslateX(_) => TransformFn::TranslateX(TransLen::ZERO),
        TransformFn::TranslateY(_) => TransformFn::TranslateY(TransLen::ZERO),
        TransformFn::Scale(_, _) => TransformFn::Scale(1.0, 1.0),
        TransformFn::ScaleX(_) => TransformFn::ScaleX(1.0),
        TransformFn::ScaleY(_) => TransformFn::ScaleY(1.0),
        TransformFn::Rotate(_) => TransformFn::Rotate(0.0),
        TransformFn::Skew(_, _) => TransformFn::Skew(0.0, 0.0),
        TransformFn::SkewX(_) => TransformFn::SkewX(0.0),
        TransformFn::SkewY(_) => TransformFn::SkewY(0.0),
        TransformFn::Matrix(_) => TransformFn::Matrix([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
    }
}

/// Lerp two transform functions of the same kind; `None` on kind mismatch.
fn transform_fn_lerp(a: TransformFn, b: TransformFn, k: f32) -> Option<TransformFn> {
    let lerp = |x: f32, y: f32| x + (y - x) * k;
    let len = |x: TransLen, y: TransLen| TransLen {
        px: lerp(x.px, y.px),
        pct: lerp(x.pct, y.pct),
    };
    Some(match (a, b) {
        (TransformFn::Translate { x: ax, y: ay }, TransformFn::Translate { x: bx, y: by }) => {
            TransformFn::Translate {
                x: len(ax, bx),
                y: len(ay, by),
            }
        }
        (TransformFn::TranslateX(x), TransformFn::TranslateX(y)) => {
            TransformFn::TranslateX(len(x, y))
        }
        (TransformFn::TranslateY(x), TransformFn::TranslateY(y)) => {
            TransformFn::TranslateY(len(x, y))
        }
        (TransformFn::Scale(ax, ay), TransformFn::Scale(bx, by)) => {
            TransformFn::Scale(lerp(ax, bx), lerp(ay, by))
        }
        (TransformFn::ScaleX(x), TransformFn::ScaleX(y)) => TransformFn::ScaleX(lerp(x, y)),
        (TransformFn::ScaleY(x), TransformFn::ScaleY(y)) => TransformFn::ScaleY(lerp(x, y)),
        (TransformFn::Rotate(x), TransformFn::Rotate(y)) => TransformFn::Rotate(lerp(x, y)),
        (TransformFn::Skew(ax, ay), TransformFn::Skew(bx, by)) => {
            TransformFn::Skew(lerp(ax, bx), lerp(ay, by))
        }
        (TransformFn::SkewX(x), TransformFn::SkewX(y)) => TransformFn::SkewX(lerp(x, y)),
        (TransformFn::SkewY(x), TransformFn::SkewY(y)) => TransformFn::SkewY(lerp(x, y)),
        (TransformFn::Matrix(a), TransformFn::Matrix(b)) => {
            let mut m = [0.0f32; 6];
            for i in 0..6 {
                m[i] = lerp(a[i], b[i]);
            }
            TransformFn::Matrix(m)
        }
        _ => return None,
    })
}

/// Serialize one transform function back to CSS text for style overrides.
fn format_transform_fn(f: &TransformFn) -> String {
    let tl = |t: &TransLen| -> String {
        if t.pct != 0.0 {
            format!("{}%", fmt_num(t.pct))
        } else {
            format!("{}px", fmt_num(t.px))
        }
    };
    match f {
        TransformFn::Translate { x, y } => {
            format!("translate({}, {})", tl(x), tl(y))
        }
        TransformFn::TranslateX(x) => format!("translateX({})", tl(x)),
        TransformFn::TranslateY(y) => format!("translateY({})", tl(y)),
        TransformFn::Scale(x, y) if (x - y).abs() < f32::EPSILON => {
            format!("scale({})", fmt_num(*x))
        }
        TransformFn::Scale(x, y) => format!("scale({}, {})", fmt_num(*x), fmt_num(*y)),
        TransformFn::ScaleX(x) => format!("scaleX({})", fmt_num(*x)),
        TransformFn::ScaleY(y) => format!("scaleY({})", fmt_num(*y)),
        TransformFn::Rotate(deg) => format!("rotate({}deg)", fmt_num(*deg)),
        TransformFn::Skew(ax, ay) => format!("skew({}deg, {}deg)", fmt_num(*ax), fmt_num(*ay)),
        TransformFn::SkewX(ax) => format!("skewX({}deg)", fmt_num(*ax)),
        TransformFn::SkewY(ay) => format!("skewY({}deg)", fmt_num(*ay)),
        // CSS stores matrix(a,b,c,d,e,f) = row-major [a, c, e, b, d, f].
        TransformFn::Matrix(m) => format!(
            "matrix({}, {}, {}, {}, {}, {})",
            fmt_num(m[0]),
            fmt_num(m[2]),
            fmt_num(m[4]),
            fmt_num(m[1]),
            fmt_num(m[3]),
            fmt_num(m[5])
        ),
    }
}

/// Compact float formatting for serialized interpolation values.
fn fmt_num(v: f32) -> String {
    let r = (v * 1000.0).round() / 1000.0;
    if (r - r.round()).abs() < f32::EPSILON {
        format!("{}", r.round() as i64)
    } else {
        format!("{}", r)
    }
}

/// Public interpolation entry (B1): the transition engine calls this with the
/// two raw computed values and the eased fraction; `None` = discrete property
/// (winner flips at the midpoint per CSS).
pub fn interpolate_style_value(prop: &str, a: &str, b: &str, k: f32) -> Option<String> {
    interp_value(prop, a, b, k)
}

/// Public single-declaration applier (B1): feeds one `property: value` pair
/// into a computed style exactly as the cascade would — the transition
/// engine overrides styles through it so eased values take the same code
/// path authored values do. Unknown properties are a no-op.
pub fn apply_style_value(style: &mut ComputedStyle, property: &str, value: &str) {
    let decl = kitae_css::Declaration {
        property: property.to_string(),
        value: value.to_string(),
        important: false,
    };
    // The cascade's own apply path; lengths resolve em against the element's
    // current font size (matching how the value was first computed).
    StyleEngine::new(&[]).apply(&decl, style, style.font_size, None);
}

/// Public computed-value serializer (B1): renders a computed style's value
/// for `property` back to CSS text so transitions can capture a starting
/// value and re-apply it as an override. `None` = nothing meaningful to
/// serialize (unset transform, no paint, …).
pub fn serialize_style_value(style: &ComputedStyle, property: &str) -> Option<String> {
    match property {
        "opacity" => Some(format!("{}", style.opacity)),
        "color" => Some(format_color(style.color)),
        "background-color" if style.background != 0 => Some(format_color(style.background)),
        "background-color" => None,
        "transform" => {
            // Unset transforms serialize as `none` so none↔list changes can
            // still transition (the interpolator pads identity functions).
            match style.transform.as_ref() {
                Some(list) => {
                    let parts: Vec<String> = list.0.iter().map(format_transform_fn).collect();
                    Some(parts.join(" "))
                }
                None => Some("none".to_string()),
            }
        }
        _ => None,
    }
}

/// 0xRRGGBBAA → CSS text (alpha 255 prints as hex, otherwise rgba()).
fn format_color(c: u32) -> String {
    let (r, g, b) = ((c >> 24) & 0xFF, (c >> 16) & 0xFF, (c >> 8) & 0xFF);
    if c & 0xFF == 0xFF {
        format!("#{:02X}{:02X}{:02X}", r, g, b)
    } else {
        format!(
            "rgba({}, {}, {}, {:.3})",
            r,
            g,
            b,
            (c & 0xFF) as f32 / 255.0
        )
    }
}

/// Parse a CSS time (`2s`, `200ms`) to seconds. Bare numbers are invalid.
fn parse_time(s: &str) -> Option<f32> {
    let t = s.trim().to_ascii_lowercase();
    if let Some(ms) = t.strip_suffix("ms") {
        ms.trim().parse::<f32>().ok().map(|v| v / 1000.0)
    } else if let Some(sec) = t.strip_suffix('s') {
        sec.trim().parse::<f32>().ok()
    } else {
        None
    }
}

/// Split on top-level commas (parens-aware, so `rgba(1, 2, 3, .5)` survives).
fn split_fn_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
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

/// Parse `background` / `background-image` paint functions: gradients and
/// `url()`. Plain colors are NOT handled here (the declaration arm tries
/// `parse_color` first via this returning `None`).
fn parse_background_paint(value: &str, current_color: u32) -> Option<BackgroundPaint> {
    let value = value.trim();
    // The `background` shorthand lets `<color>` come before `<image>`, so the
    // paint function is found by position rather than by prefix — the first
    // one in the layer is the one that paints.
    for name in [
        "linear-gradient",
        "radial-gradient",
        "conic-gradient",
        "url",
    ] {
        let Some(rest) = function_inner(value, name) else {
            continue;
        };
        return match name {
            "linear-gradient" => parse_linear_gradient(&rest, current_color).map(|(a, s)| {
                BackgroundPaint::LinearGradient {
                    angle_deg: a,
                    stops: s,
                }
            }),
            "radial-gradient" => parse_radial_gradient(&rest, current_color).map(|(x, y, r, s)| {
                BackgroundPaint::RadialGradient {
                    cx: x,
                    cy: y,
                    radius: r,
                    stops: s,
                }
            }),
            "conic-gradient" => parse_conic_gradient(&rest, current_color).map(|(a, x, y, s)| {
                BackgroundPaint::ConicGradient {
                    angle_deg: a,
                    cx: x,
                    cy: y,
                    stops: s,
                }
            }),
            _ => {
                let url = rest
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim()
                    .to_string();
                (!url.is_empty()).then_some(BackgroundPaint::Image(url))
            }
        };
    }
    None
}

/// The balanced `(…)` content of the first `name(` in `value` that starts a
/// token — so `url(` matches, while `my-url(` and
/// `repeating-linear-gradient(` (whose tail contains `linear-gradient(`)
/// do not.
fn function_inner(value: &str, name: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let at = lower.match_indices(name).find_map(|(at, _)| {
        let after = at + name.len();
        let boundary = at == 0
            || lower[..at]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        (boundary && lower[after..].starts_with('(')).then_some(after)
    })?;
    let mut depth = 0usize;
    for (i, c) in value[at..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(value[at + 1..at + i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Top-level whitespace tokens of one `background` layer, with `/` split
/// out and function calls (`url(a b.png)`, `linear-gradient(…)`) kept whole
/// so an inner space or comma never ends a token.
fn background_layer_tokens(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for c in value.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            c if depth > 0 => cur.push(c),
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '/' if depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push("/".to_string());
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The box-level parts of a `background` layer: `position [ / size ]`,
/// `repeat` and the two box keywords, in any order and either case. Words
/// containing `(` are the paint (gradient / `url()`), not placement, and
/// are skipped.
fn parse_background_extras(value: &str) -> Option<BackgroundExtras> {
    let mut extras = BackgroundExtras::default();
    let mut position: Vec<String> = Vec::new();
    let mut size: Vec<String> = Vec::new();
    let mut repeat: Vec<String> = Vec::new();
    let mut box_kw: Vec<BgBox> = Vec::new();
    let mut after_slash = false;
    let mut saw_any = false;
    for tok in background_layer_tokens(value) {
        if tok == "/" {
            after_slash = true;
            continue;
        }
        if tok.contains('(') {
            continue; // the paint itself
        }
        let low = tok.to_ascii_lowercase();
        if let Some(b) = parse_bg_box(&low) {
            // The three box keywords name no axis, so they are neither
            // placement nor size: they are taken out of the token stream
            // before the position/size grammars see them (a `left`/`top`
            // keyword is unaffected — only `border-box`, `padding-box` and
            // `content-box` land here).
            box_kw.push(b);
            saw_any = true;
            continue;
        }
        if repeat_from_keyword(&low).is_some() {
            repeat.push(low);
        } else if after_slash {
            size.push(low);
        } else {
            position.push(low);
        }
        saw_any = true;
    }
    if !saw_any {
        return None;
    }
    if !position.is_empty() {
        extras.position = parse_background_position_tokens(&position);
    }
    if !size.is_empty() {
        extras.size = parse_background_size_tokens(&size);
    }
    // `<bg-layer>` carries up to two `<box>` values: the first is
    // `background-origin`, the second `background-clip`; a lone one sets
    // both, so `background: url(x) padding-box` positions in AND paints
    // inside the padding box (what every engine shows). Extra keywords past
    // the second are dropped.
    match box_kw.len() {
        0 => {}
        1 => {
            extras.origin = Some(box_kw[0]);
            extras.clip = Some(box_kw[0]);
        }
        _ => {
            extras.origin = Some(box_kw[0]);
            extras.clip = Some(box_kw[1]);
        }
    }
    // One repeat keyword sets both axes; a pair is horizontal/vertical.
    match repeat.len() {
        1 => extras.repeat = repeat_from_keyword(&repeat[0]),
        2 => {
            extras.repeat = Some(BackgroundRepeat {
                x: single_repeat(&repeat[0])?,
                y: single_repeat(&repeat[1])?,
            })
        }
        _ => {}
    }
    Some(extras)
}

/// `background-origin` / `background-clip`: one `border-box` | `padding-box`
/// | `content-box` keyword.
pub fn parse_bg_box(value: &str) -> Option<BgBox> {
    BgBox::from_keyword(value.trim().to_ascii_lowercase().as_str())
}

/// The `<color>` of a `background` layer: the first token that parses as a
/// color. `parse_color` only matches hex, named colors, `currentcolor` and
/// the `rgb()` / `rgba()` / `color()` functions, so a paint token
/// (`url()`, a gradient) is simply tried and comes back `None`.
fn background_layer_color(layer: &str, current_color: u32) -> Option<u32> {
    background_layer_tokens(layer)
        .into_iter()
        .filter(|t| t != "/")
        .find_map(|t| parse_style_color(&t, current_color))
}

/// The `background` shorthand: a comma-separated layer list, in CSS order
/// (index 0 is the TOP layer). `None` when the value is invalid — the caller
/// keeps the cascaded value.
///
/// A layer's absent components fall back to the *cascaded* scalars rather
/// than the initial value, matching this engine's long-standing shorthand
/// handling (a `background-position` that precedes the shorthand survives
/// it); the spec's reset-to-initial is not implemented. The final layer's
/// `<color>` is the box's `background-color` in the spec — here every layer
/// keeps its own color and paints as its own bottom-to-top step, which is
/// what an author writing `background: #fff, url(x)` means.
fn parse_background_layers(value: &str, style: &ComputedStyle) -> Option<Vec<BackgroundLayer>> {
    let terms = split_fn_args(value);
    if terms.is_empty() {
        return None;
    }
    let seed = BackgroundLayer {
        paint: style.background_paint.clone(),
        color: style.background,
        position: style.background_position,
        size: style.background_size,
        repeat: style.background_repeat,
        origin: BgBox::BorderBox,
        clip: BgBox::BorderBox,
    };
    let mut out = Vec::with_capacity(terms.len());
    for term in &terms {
        out.push(parse_background_layer(term, style.color, &seed)?);
    }
    Some(out)
}

/// One `<bg-layer>`: its paint, its `<color>`, and its placement. `None`
/// when the layer carries nothing the grammar recognizes.
fn parse_background_layer(
    term: &str,
    current_color: u32,
    seed: &BackgroundLayer,
) -> Option<BackgroundLayer> {
    let extras = parse_background_extras(term).unwrap_or_default();
    let mut layer = seed.clone();
    if let Some(p) = extras.position {
        layer.position = p;
    }
    if let Some(s) = extras.size {
        layer.size = s;
    }
    if let Some(r) = extras.repeat {
        layer.repeat = r;
    }
    if let Some(b) = extras.origin {
        layer.origin = b;
    }
    if let Some(b) = extras.clip {
        layer.clip = b;
    }
    // A bare color is the whole layer; the shorthand allows it before the
    // image too (`background: #fff url(x.png)`), where it paints under the
    // artwork.
    let color = background_layer_color(term, current_color);
    if let Some(paint) = parse_background_paint(term, current_color) {
        // A gradient has no separate color of its own: the fast path is its
        // first stop. An image shows its own pixels, so the layer color (or
        // whatever cascaded) is the backdrop behind it.
        layer.color = if layer_is_gradient(&paint) {
            gradient_first_stop(&paint).unwrap_or(0)
        } else {
            color.unwrap_or(layer.color)
        };
        layer.paint = paint;
        return Some(layer);
    }
    if let Some(c) = color {
        layer.paint = BackgroundPaint::Color(c);
        layer.color = c;
        return Some(layer);
    }
    // `none` (or an image that failed to resolve) leaves the layer unpainted.
    if term.trim().eq_ignore_ascii_case("none") {
        layer.paint = BackgroundPaint::Color(0);
        layer.color = 0;
        return Some(layer);
    }
    // A layer made only of placement keywords is legal
    // (`background: padding-box`); nothing recognizable at all is not.
    let placement_only = extras.position.is_some()
        || extras.size.is_some()
        || extras.repeat.is_some()
        || extras.origin.is_some()
        || extras.clip.is_some();
    placement_only.then_some(layer)
}

/// True for the three gradient paints (never `Color` / `Image` / `none`).
fn layer_is_gradient(paint: &BackgroundPaint) -> bool {
    matches!(
        paint,
        BackgroundPaint::LinearGradient { .. }
            | BackgroundPaint::RadialGradient { .. }
            | BackgroundPaint::ConicGradient { .. }
    )
}

/// The CPU fast path for a gradient paint: its first stop's color.
fn gradient_first_stop(paint: &BackgroundPaint) -> Option<u32> {
    match paint {
        BackgroundPaint::LinearGradient { stops, .. }
        | BackgroundPaint::RadialGradient { stops, .. }
        | BackgroundPaint::ConicGradient { stops, .. } => stops.first().map(|s| s.color),
        _ => None,
    }
}

/// `background-image`: a comma-separated list of `none` | paint functions,
/// one per layer. Per spec this longhand sets the image of *every* layer, so
/// the list length replaces the layer count. `None` in the returned vector
/// is a `none` layer.
fn parse_background_image_layers(
    value: &str,
    current_color: u32,
) -> Option<Vec<Option<BackgroundPaint>>> {
    let terms = split_fn_args(value);
    if terms.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(terms.len());
    for term in &terms {
        let term = term.trim();
        if term.eq_ignore_ascii_case("none") {
            out.push(None);
            continue;
        }
        match parse_background_paint(term, current_color) {
            // A plain color is not a valid `background-image`; the
            // declaration is invalid rather than half-applied.
            Some(BackgroundPaint::Color(_)) | None => return None,
            Some(paint) => out.push(Some(paint)),
        }
    }
    Some(out)
}

/// Grow / shrink the layer list to `n` entries, cloning the last layer for
/// the new ones (CSS Backgrounds 3 §2.1: a longhand with fewer values than
/// the image list repeats its last value).
fn resize_background_layers(style: &mut ComputedStyle, n: usize) {
    ensure_background_layers(style);
    let n = n.max(1);
    match style.background_layers.len().cmp(&n) {
        std::cmp::Ordering::Less => {
            let last = style.background_layers.last().cloned().unwrap_or_default();
            style.background_layers.resize(n, last);
        }
        std::cmp::Ordering::Greater => style.background_layers.truncate(n),
        std::cmp::Ordering::Equal => {}
    }
}

/// A per-layer background longhand: a comma-separated list of one value per
/// layer, with a single value repeated across the layers (the usual case).
/// `None` when any term is invalid.
fn background_layer_list<T>(value: &str, parse_one: impl Fn(&str) -> Option<T>) -> Option<Vec<T>> {
    let terms = split_fn_args(value);
    if terms.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(terms.len());
    for term in &terms {
        out.push(parse_one(term)?);
    }
    Some(out)
}

/// Write a per-layer longhand's values onto the layer list: term `i` lands on
/// layer `i` and a shorter list repeats its last value across the rest (CSS
/// Backgrounds 3 §2.1). Extra terms are dropped — the image list owns the
/// layer count.
fn apply_layer_list<T: Copy>(
    style: &mut ComputedStyle,
    values: Vec<T>,
    mut set: impl FnMut(&mut BackgroundLayer, T),
) {
    let Some(last) = values.last().copied() else {
        return;
    };
    for (i, layer) in style.background_layers.iter_mut().enumerate() {
        set(layer, values.get(i).copied().unwrap_or(last));
    }
}

/// Materialize the layer list from its empty (initial) state, so a longhand
/// has a layer to write into. The scalars already describe that first layer,
/// which is why this is a plain default.
fn ensure_background_layers(style: &mut ComputedStyle) {
    if style.background_layers.is_empty() {
        style.background_layers.push(BackgroundLayer::default());
    }
}

/// `background-color`: the box's solid color, which in a layer list belongs
/// to the FINAL (bottom) layer and paints under every image above it. The
/// layer's own paint is left alone when it carries an image or a gradient —
/// the color is that layer's backdrop, not a replacement for it.
fn apply_background_color(style: &mut ComputedStyle, c: u32) {
    style.background = c;
    ensure_background_layers(style);
    let last = style.background_layers.len().saturating_sub(1);
    let layer = &mut style.background_layers[last];
    layer.color = c;
    if matches!(layer.paint, BackgroundPaint::Color(_)) {
        layer.paint = BackgroundPaint::Color(c);
    }
}

/// Mirror the topmost layer back onto the scalar fast-path fields, so every
/// reader that predates the layer list (the CPU rect fast path, the
/// `<body>` canvas clear, the control chrome) keeps seeing the layer a
/// single-layer declaration would have given it.
fn sync_background_scalars(style: &mut ComputedStyle) {
    let Some(top) = style.background_layers.first().cloned() else {
        return;
    };
    style.background_position = top.position;
    style.background_size = top.size;
    style.background_repeat = top.repeat;
    style.background = top.color;
    style.background_paint = top.paint;
}

#[derive(Debug, Default)]
struct BackgroundExtras {
    position: Option<BackgroundPosition>,
    size: Option<BackgroundSize>,
    repeat: Option<BackgroundRepeat>,
    origin: Option<BgBox>,
    clip: Option<BgBox>,
}

fn repeat_from_keyword(kw: &str) -> Option<BackgroundRepeat> {
    let both = |x: BgRepeat, y: BgRepeat| Some(BackgroundRepeat { x, y });
    match kw {
        "repeat" => both(BgRepeat::Repeat, BgRepeat::Repeat),
        "repeat-x" => both(BgRepeat::Repeat, BgRepeat::NoRepeat),
        "repeat-y" => both(BgRepeat::NoRepeat, BgRepeat::Repeat),
        "no-repeat" => both(BgRepeat::NoRepeat, BgRepeat::NoRepeat),
        "space" => both(BgRepeat::Space, BgRepeat::Space),
        "round" => both(BgRepeat::Round, BgRepeat::Round),
        _ => None,
    }
}

fn pos_keyword(kw: &str) -> Option<BgPos> {
    match kw {
        "left" | "top" => Some(BgPos::Start),
        "center" => Some(BgPos::Center),
        "right" | "bottom" => Some(BgPos::End),
        _ => None,
    }
}

/// A `background-position` component: a keyword or a length/percentage.
enum PosPart {
    Keyword(BgPos),
    Offset(Length),
    Percent(f32),
}

fn parse_pos_part(tok: &str) -> Option<PosPart> {
    if let Some(k) = pos_keyword(tok) {
        return Some(PosPart::Keyword(k));
    }
    if let Some(p) = tok.strip_suffix('%') {
        let v: f32 = p.trim().parse().ok()?;
        return v.is_finite().then_some(PosPart::Percent(v));
    }
    parse_length(tok).map(PosPart::Offset)
}

/// `background-position`, by the CSS 3-value grammar: one value (an edge
/// keyword centers the other axis, anything else is horizontal with a
/// centered vertical), two values (horizontal then vertical), or the
/// four-value edge/offset pairs.
pub fn parse_background_position(value: &str) -> Option<BackgroundPosition> {
    let toks: Vec<String> = value
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    parse_background_position_tokens(&toks)
}

fn parse_background_position_tokens(toks: &[String]) -> Option<BackgroundPosition> {
    let parts: Vec<PosPart> = toks.iter().filter_map(|t| parse_pos_part(t)).collect();
    // A token that isn't a position component makes the whole declaration
    // invalid (the caller keeps the cascaded value).
    if parts.len() != toks.len() || parts.is_empty() || parts.len() > 4 {
        return None;
    }
    match parts.as_slice() {
        // One value: an edge keyword fixes its own axis and centers the
        // other; `center` centers both; a length/percentage is horizontal
        // with a centered vertical.
        [one] => {
            let (x, y) = match one {
                PosPart::Keyword(BgPos::Center) => (BgPos::Center, BgPos::Center),
                PosPart::Keyword(_) => match edge_axis(toks[0].as_str())? {
                    (k, true) => (k, BgPos::Center),
                    (k, false) => (BgPos::Center, k),
                },
                other => (pos_offset(other)?, BgPos::Center),
            };
            Some(BackgroundPosition { x, y })
        }
        // Two values: horizontal then vertical, except two keywords in the
        // `top left` order (CSS allows the block-axis keyword first) and the
        // edge+offset pair `right 10px` / `top 10%`.
        [a, b] => {
            let keyword = |p: &PosPart| match p {
                PosPart::Keyword(k) => Some(*k),
                _ => None,
            };
            // `right 10px` / `top 10%`: the leading keyword's axis takes the
            // offset (measured inward from that edge), the other centers.
            let edged = |k: &str, other: &PosPart| -> Option<(BgPos, bool)> {
                let (edge, horizontal) = edge_axis(k)?;
                Some((from_edge(edge, pos_offset(other)?), horizontal))
            };
            let (x_part, y_part) = match (keyword(a), keyword(b)) {
                (Some(_), Some(_)) => {
                    // `top left` — swap when the first keyword names the
                    // block axis and the second the inline axis. `center`
                    // names neither, so the order stands. Any other pairing
                    // (`left left`) is invalid.
                    let axis = |tok: &str| match tok {
                        "center" => Some(None),
                        other => edge_axis(other).map(|(_, horizontal)| Some(horizontal)),
                    };
                    match (axis(&toks[0]), axis(&toks[1])) {
                        (Some(Some(false)), Some(Some(true))) => {
                            (keyword(b).unwrap(), keyword(a).unwrap())
                        }
                        (Some(x), Some(y)) if x != Some(false) && y != Some(true) => {
                            (keyword(a).unwrap(), keyword(b).unwrap())
                        }
                        _ => return None,
                    }
                }
                (Some(BgPos::Center), None) => (BgPos::Center, pos_offset(b)?),
                (None, Some(BgPos::Center)) => (pos_offset(a)?, BgPos::Center),
                (Some(_), None) => {
                    let (part, horizontal) = edged(&toks[0], b)?;
                    if horizontal {
                        (part, BgPos::Center)
                    } else {
                        (BgPos::Center, part)
                    }
                }
                // A trailing keyword is the vertical position, so the first
                // value stays horizontal (`10px bottom`); a horizontal edge
                // keyword in that slot is invalid.
                (None, Some(_)) => {
                    let (_, horizontal) = edged(&toks[1], a)?;
                    if horizontal {
                        return None;
                    }
                    (pos_offset(a)?, keyword(b).unwrap())
                }
                (None, None) => (pos_offset(a)?, pos_offset(b)?),
            };
            Some(BackgroundPosition {
                x: x_part,
                y: y_part,
            })
        }
        // 3–4 values: the 4-value syntax — an edge keyword paired with the
        // offset that follows it, one pair per axis. A trailing lone
        // `center` fills the remaining axis.
        rest => {
            let mut out: [Option<BgPos>; 2] = [None, None]; // 0 = x, 1 = y
            let mut i = 0;
            while i < rest.len() {
                let (kw, horizontal) = edge_axis(toks[i].as_str())?;
                let part = rest
                    .get(i + 1)
                    .and_then(pos_offset)
                    .or(if rest.len() - i == 1 {
                        // `right bottom`: the axis keeps its keyword.
                        Some(BgPos::Center)
                    } else {
                        None
                    })?;
                let axis = if horizontal { 0 } else { 1 };
                if out[axis].is_some() {
                    return None;
                }
                out[axis] = Some(apply_edge_keyword(kw, from_edge(kw, part)));
                i += 2;
            }
            Some(BackgroundPosition {
                x: out[0]?,
                y: out[1]?,
            })
        }
    }
}

/// An edge keyword with the axis it names: `left`/`right` horizontal,
/// `top`/`bottom` vertical. `center` is not an edge (the 4-value syntax
/// rejects it as a pair head).
fn edge_axis(tok: &str) -> Option<(BgPos, bool)> {
    match tok {
        "left" => Some((BgPos::Start, true)),
        "right" => Some((BgPos::End, true)),
        "top" => Some((BgPos::Start, false)),
        "bottom" => Some((BgPos::End, false)),
        _ => None,
    }
}

/// The edge keyword and the offset beside it: the offset wins, measured from
/// the keyword's own edge (`right 10px` = 10px in from the right).
fn apply_edge_keyword(edge: BgPos, part: BgPos) -> BgPos {
    match part {
        BgPos::Offset(_) | BgPos::Inset(_) | BgPos::Percent(_) => part,
        BgPos::Center => edge_percent(edge),
        BgPos::Start | BgPos::End => edge_percent(edge),
    }
}

/// Restate an offset measured from the keyword's edge as one measured from
/// the start edge: a length inverts into [`BgPos::Inset`], a percentage into
/// its complement of the free space.
fn from_edge(edge: BgPos, part: BgPos) -> BgPos {
    if !matches!(edge, BgPos::End) {
        return part;
    }
    match part {
        BgPos::Offset(l) => BgPos::Inset(l),
        BgPos::Percent(p) => BgPos::Percent(100.0 - p),
        other => other,
    }
}

fn edge_percent(edge: BgPos) -> BgPos {
    BgPos::Percent(if matches!(edge, BgPos::End) {
        100.0
    } else {
        0.0
    })
}

/// A position component that is not a keyword.
fn pos_offset(part: &PosPart) -> Option<BgPos> {
    match part {
        PosPart::Offset(l) => Some(BgPos::Offset(*l)),
        PosPart::Percent(v) => Some(BgPos::Percent(*v)),
        PosPart::Keyword(_) => None,
    }
}
/// `background-size`: `cover`, `contain`, or one or two `<length> |
/// percentage | auto>` values (a single value leaves the other axis `auto`).
pub fn parse_background_size(value: &str) -> Option<BackgroundSize> {
    let toks: Vec<String> = value
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    parse_background_size_tokens(&toks)
}

fn parse_background_size_tokens(toks: &[String]) -> Option<BackgroundSize> {
    match toks.len() {
        1 => match toks[0].as_str() {
            "cover" => Some(BackgroundSize::Keyword(BgSizeKeyword::Cover)),
            "contain" => Some(BackgroundSize::Keyword(BgSizeKeyword::Contain)),
            // `auto` alone is the intrinsic size on both axes.
            "auto" => Some(BackgroundSize::Auto),
            _ => Some(BackgroundSize::Axes(bg_size_part(&toks[0])?, BgSize::Auto)),
        },
        2 => Some(BackgroundSize::Axes(
            bg_size_part(&toks[0])?,
            bg_size_part(&toks[1])?,
        )),
        _ => None,
    }
}

fn bg_size_part(tok: &str) -> Option<BgSize> {
    if tok == "auto" {
        return Some(BgSize::Auto);
    }
    if let Some(p) = tok.strip_suffix('%') {
        let v: f32 = p.trim().parse().ok()?;
        return v.is_finite().then_some(BgSize::Percent(v));
    }
    parse_length(tok).map(BgSize::Length)
}

/// `background-repeat`: one keyword, or a horizontal/vertical pair
/// (`repeat no-repeat`).
pub fn parse_background_repeat(value: &str) -> Option<BackgroundRepeat> {
    let toks: Vec<String> = value
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    match toks.len() {
        1 => repeat_from_keyword(&toks[0]),
        2 => {
            let x = single_repeat(&toks[0])?;
            let y = single_repeat(&toks[1])?;
            Some(BackgroundRepeat { x, y })
        }
        _ => None,
    }
}

fn single_repeat(tok: &str) -> Option<BgRepeat> {
    Some(match tok {
        "repeat" => BgRepeat::Repeat,
        "no-repeat" => BgRepeat::NoRepeat,
        "space" => BgRepeat::Space,
        "round" => BgRepeat::Round,
        _ => return None,
    })
}

/// The matching color token of a gradient stop (`#hex`, named, or `rgb()/rgba()`
/// with balanced parens). Returns (color, rest).
fn split_stop_color(stop: &str, current_color: u32) -> Option<(u32, &str)> {
    let stop = stop.trim();
    let lower = stop.to_ascii_lowercase();
    // Functional color: scan to the matching close paren.
    for prefix in ["rgba(", "rgb("] {
        if lower.starts_with(prefix) {
            let mut depth = 0usize;
            for (i, c) in stop.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            let color = parse_style_color(&stop[..=i], current_color)?;
                            return Some((color, stop[i + 1..].trim()));
                        }
                    }
                    _ => {}
                }
            }
            return None;
        }
    }
    let token: String = stop.split_whitespace().next().unwrap_or("").to_string();
    if token.is_empty() {
        return None;
    }
    parse_style_color(&token, current_color).map(|c| (c, stop[token.len()..].trim()))
}

/// Parse gradient stop lists (`red, blue 50%, #fff`): missing offsets are
/// distributed (first 0, last 1, middles interpolated); `%` offsets honored.
fn parse_stops(args: &[String], current_color: u32) -> Option<Vec<ColorStop>> {
    if args.is_empty() {
        return None;
    }
    let mut stops: Vec<(u32, Option<f32>)> = Vec::with_capacity(args.len());
    for a in args {
        let (color, rest) = split_stop_color(a, current_color)?;
        let offset = rest
            .split_whitespace()
            .next()
            .and_then(|t| t.strip_suffix('%'))
            .and_then(|n| n.trim().parse::<f32>().ok())
            .map(|p| (p / 100.0).clamp(0.0, 1.0));
        stops.push((color, offset));
    }
    // Distribute: anchor ends, interpolate runs of missing offsets.
    let n = stops.len();
    if stops[0].1.is_none() {
        stops[0].1 = Some(0.0);
    }
    if stops[n - 1].1.is_none() {
        stops[n - 1].1 = Some(1.0);
    }
    let mut i = 0;
    while i < n {
        if stops[i].1.is_some() {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < n && stops[j].1.is_none() {
            j += 1;
        }
        let (lo, hi) = (stops[i - 1].1.unwrap(), stops[j].1.unwrap());
        let span = (j - i + 1) as f32;
        for (k, slot) in stops[i..j].iter_mut().enumerate() {
            slot.1 = Some(lo + (hi - lo) * ((k + 1) as f32 / span));
        }
        i = j;
    }
    Some(
        stops
            .into_iter()
            .map(|(color, offset)| ColorStop {
                offset: offset.unwrap_or(0.0),
                color,
            })
            .collect(),
    )
}

/// Parse `linear-gradient([angle | to side,] stops)` → (degrees, stops).
/// Degrees are clockwise from "to top" (CSS `0deg` points up).
fn parse_linear_gradient(inner: &str, current_color: u32) -> Option<(f32, Vec<ColorStop>)> {
    let args = split_fn_args(inner);
    if args.is_empty() {
        return None;
    }
    let (angle, rest) = parse_gradient_angle(&args[0])
        .map(|a| (a, &args[1..]))
        .unwrap_or((180.0, &args[..]));
    Some((angle, parse_stops(rest, current_color)?))
}

fn parse_gradient_angle(token: &str) -> Option<f32> {
    let t = token.trim().to_ascii_lowercase();
    if let Some(deg) = t
        .strip_suffix("deg")
        .and_then(|n| n.trim().parse::<f32>().ok())
    {
        return Some(((deg % 360.0) + 360.0) % 360.0);
    }
    if let Some(sides) = t.strip_prefix("to ") {
        let mut x = 0.0f32;
        let mut y = 0.0f32;
        for side in sides.split_whitespace() {
            match side {
                "top" => y += 1.0,
                "bottom" => y -= 1.0,
                "left" => x -= 1.0,
                "right" => x += 1.0,
                _ => return None,
            }
        }
        if x == 0.0 && y == 0.0 {
            return None;
        }
        return Some((x.atan2(y).to_degrees() + 360.0) % 360.0);
    }
    None
}

/// Parse `radial-gradient([circle|ellipse] [radius]? [at x y]?, stops)` →
/// (cx, cy, radius_px, stops). Positions accept `%` and
/// left/center/right/top/bottom keywords; only `circle [length]` radii.
fn parse_radial_gradient(
    inner: &str,
    current_color: u32,
) -> Option<(f32, f32, Option<f32>, Vec<ColorStop>)> {
    let args = split_fn_args(inner);
    if args.is_empty() {
        return None;
    }
    let mut rest = &args[..];
    let mut cx = 0.5f32;
    let mut cy = 0.5f32;
    let mut radius: Option<f32> = None;
    if let Some(first) = rest.first() {
        let f = first.trim().to_ascii_lowercase();
        let is_shape = f.starts_with("circle")
            || f.starts_with("ellipse")
            || f.contains(" at ")
            || f.starts_with("at ");
        if is_shape {
            // Optional explicit circle radius: `circle 10px [at …]`.
            for tok in f.split_whitespace() {
                if let Some(l) = parse_length(tok) {
                    if let Length::Px(px) = l {
                        radius = Some(px.max(0.0));
                    }
                }
            }
            if let Some(at) = f.find(" at ") {
                let pos: Vec<&str> = f[at + 4..].split_whitespace().collect();
                let pos_of = |t: &str| match t {
                    "left" | "top" => Some(0.0),
                    "center" => Some(0.5),
                    "right" | "bottom" => Some(1.0),
                    _ => t
                        .strip_suffix('%')
                        .and_then(|n| n.parse::<f32>().ok())
                        .map(|p| (p / 100.0).clamp(0.0, 1.0)),
                };
                match pos.as_slice() {
                    [x, y] => {
                        cx = pos_of(x).unwrap_or(0.5);
                        cy = pos_of(y).unwrap_or(0.5);
                    }
                    [single] => {
                        // `at center` sets both; a lone keyword sets x.
                        if *single == "center" {
                            cx = 0.5;
                            cy = 0.5;
                        } else {
                            cx = pos_of(single).unwrap_or(0.5);
                        }
                    }
                    _ => {}
                }
            }
            rest = &rest[1..];
        }
    }
    Some((cx, cy, radius, parse_stops(rest, current_color)?))
}

/// Parse `conic-gradient([from angle]? [at x y]?, stops)` →
/// (angle_deg, cx, cy, stops). Stops without offsets sweep evenly over the
/// remaining turn via `parse_stops`; angles are clockwise from "to top".
fn parse_conic_gradient(
    inner: &str,
    current_color: u32,
) -> Option<(f32, f32, f32, Vec<ColorStop>)> {
    let args = split_fn_args(inner);
    if args.is_empty() {
        return None;
    }
    let mut rest = &args[..];
    let mut angle = 0.0f32;
    let mut cx = 0.5f32;
    let mut cy = 0.5f32;
    if let Some(first) = rest.first() {
        let f = first.trim().to_ascii_lowercase();
        let mut consumed = false;
        // `from 45deg` may share the first arg with `at x y`.
        if let Some(from) = f.strip_prefix("from ") {
            if let Some(a) = from
                .split_whitespace()
                .next()
                .and_then(parse_gradient_angle)
            {
                angle = a;
                consumed = true;
            }
        }
        if let Some(at) = f.find(" at ") {
            let pos: Vec<&str> = f[at + 4..].split_whitespace().collect();
            let pos_of = |t: &str| match t {
                "left" | "top" => Some(0.0),
                "center" => Some(0.5),
                "right" | "bottom" => Some(1.0),
                _ => t
                    .strip_suffix('%')
                    .and_then(|n| n.parse::<f32>().ok())
                    .map(|p| (p / 100.0).clamp(0.0, 1.0)),
            };
            match pos.as_slice() {
                [x, y] => {
                    cx = pos_of(x).unwrap_or(0.5);
                    cy = pos_of(y).unwrap_or(0.5);
                    consumed = true;
                }
                [single] => {
                    if *single == "center" {
                        cx = 0.5;
                        cy = 0.5;
                    } else {
                        cx = pos_of(single).unwrap_or(0.5);
                    }
                    consumed = true;
                }
                _ => {}
            }
        }
        if consumed {
            rest = &rest[1..];
        }
    }
    Some((angle, cx, cy, parse_stops(rest, current_color)?))
}

/// Parse `box-shadow`: comma-separated `inset? ox oy [blur] [spread] color`.
fn parse_box_shadows(value: &str, em: f32) -> Vec<BoxShadow> {
    let rem = 16.0f32;
    let len = |s: &str| parse_len_px(s, em, rem).and_then(|l| l.px());
    let mut out = Vec::new();
    for item in split_fn_args(value) {
        let item = item.trim();
        if item.is_empty() || item.eq_ignore_ascii_case("none") {
            continue;
        }
        let mut inset = false;
        let mut lengths: Vec<f32> = Vec::new();
        let mut color: Option<u32> = None;
        for tok in item.split_whitespace() {
            if tok.eq_ignore_ascii_case("inset") {
                inset = true;
                continue;
            }
            if let Some(px) = len(tok) {
                lengths.push(px);
                continue;
            }
            if color.is_none() {
                if let Some(c) = parse_color(tok) {
                    color = Some(c);
                }
            }
        }
        if lengths.len() < 2 {
            continue;
        }
        out.push(BoxShadow {
            offset_x: lengths[0],
            offset_y: lengths[1],
            blur: lengths.get(2).copied().unwrap_or(0.0).max(0.0),
            spread: lengths.get(3).copied().unwrap_or(0.0),
            color: color.unwrap_or(0x000000FF),
            inset,
        });
    }
    out
}

/// D4: `font-feature-settings` list — `"liga"`, `liga`, `"liga" 0`,
/// `liga off`, `"ss01" 2`, comma-separated. A missing value means 1
/// (`on`); unrecognized values fall back to `on` rather than dropping
/// the tag (the pair is harmless if the font lacks the feature).
pub fn parse_font_features(value: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for piece in value.split(',') {
        let piece = piece.trim();
        if piece.is_empty() || piece.eq_ignore_ascii_case("normal") {
            continue;
        }
        let (raw_tag, val) = match piece.split_once(char::is_whitespace) {
            Some((t, rest)) => (t, rest.trim()),
            None => (piece, ""),
        };
        let tag = raw_tag
            .trim_matches(|c| c == '"' || c == '\'')
            .trim()
            .to_string();
        if tag.is_empty() {
            continue;
        }
        let n = match val {
            "" | "on" | "1" => 1u32,
            "off" | "0" => 0u32,
            other => other.parse::<i64>().map_or(1, |n| n.clamp(0, 99) as u32),
        };
        out.push((tag, n));
    }
    out
}

/// D4: `font-variant*` keywords → (OpenType tag requests, small-caps
/// request) for one axis — `shorthand` (`font-variant`), `caps`
/// (`font-variant-caps`) or `ligatures` (`font-variant-ligatures`).
/// The returned `Option<bool>` is `None` on the ligatures axis: that
/// declaration must not touch the caps state. `normal` resets the axis;
/// `none` forces every ligature family off (and caps back to normal).
/// The numeric / east-asian / position axes stay unapplied.
fn parse_font_variant(value: &str, axis: &str) -> (Vec<(String, u32)>, Option<bool>) {
    let mut features: Vec<(String, u32)> = Vec::new();
    let mut small_caps = false;
    let mut ligatures_none = false;
    for kw in value.split_whitespace() {
        match kw {
            "normal" => {
                features.clear();
                small_caps = false;
            }
            "none" => {
                ligatures_none = true;
                small_caps = false;
            }
            "common-ligatures" => features.push(("liga".to_string(), 1)),
            "no-common-ligatures" => features.push(("liga".to_string(), 0)),
            "discretionary-ligatures" => features.push(("dlig".to_string(), 1)),
            "no-discretionary-ligatures" => features.push(("dlig".to_string(), 0)),
            "historical-ligatures" => features.push(("hlig".to_string(), 1)),
            "no-historical-ligatures" => features.push(("hlig".to_string(), 0)),
            "contextual" => features.push(("calt".to_string(), 1)),
            "no-contextual" => features.push(("calt".to_string(), 0)),
            "small-caps" | "all-small-caps" | "petite-caps" | "all-petite-caps" | "unicase" => {
                small_caps = true;
                let tag = if kw.contains("petite") {
                    "pcap"
                } else {
                    "smcp"
                };
                features.push((tag.to_string(), 1));
            }
            "titling-caps" => features.push(("titl".to_string(), 1)),
            _ => {}
        }
    }
    if ligatures_none {
        for t in ["liga", "clig", "dlig", "hlig", "calt"] {
            features.push((t.to_string(), 0));
        }
    }
    let caps = if axis == "ligatures" {
        None
    } else {
        Some(small_caps)
    };
    (features, caps)
}

/// D4: the OpenType tags one `font-variant-*` longhand axis owns — used to
/// replace that axis's requests without touching the others.
fn axis_tags(axis: &str) -> &'static [&'static str] {
    if axis == "caps" {
        &["smcp", "pcap", "titl"]
    } else {
        &["liga", "clig", "dlig", "hlig", "calt"]
    }
}

/// R8: `text-shadow` — comma-separated `<offset-x> <offset-y> <blur>?
/// <color>` shadows (color may also lead). `none` clears.
fn parse_text_shadows(value: &str, em: f32) -> Vec<TextShadow> {
    let rem = 16.0f32;
    let len = |s: &str| parse_len_px(s, em, rem).and_then(|l| l.px());
    let mut out = Vec::new();
    for item in split_fn_args(value) {
        let item = item.trim();
        if item.is_empty() || item.eq_ignore_ascii_case("none") {
            continue;
        }
        let mut lengths: Vec<f32> = Vec::new();
        let mut color: Option<u32> = None;
        for tok in item.split_whitespace() {
            if let Some(px) = len(tok) {
                lengths.push(px);
                continue;
            }
            if color.is_none() {
                if let Some(c) = parse_color(tok) {
                    color = Some(c);
                }
            }
        }
        if lengths.len() < 2 {
            continue;
        }
        out.push(TextShadow {
            offset_x: lengths[0],
            offset_y: lengths[1],
            blur: lengths.get(2).copied().unwrap_or(0.0).max(0.0),
            color: color.unwrap_or(0x000000FF),
        });
    }
    out
}

/// Split on whitespace not nested inside parentheses, so a shorthand value
/// like `4px calc(2px + 3px)` yields two parts.
/// `border-width` component (R1): keywords fold to px, lengths via the
/// normal `len` (em/rem fold, % stays symbolic for layout).
fn border_width_len(s: &str, len: impl Fn(&str) -> Option<Len>) -> Option<Len> {
    match s {
        "thin" => Some(Len::const_px(1.0)),
        "medium" => Some(Len::const_px(3.0)),
        "thick" => Some(Len::const_px(5.0)),
        _ => len(s),
    }
}

/// B12 `text-decoration-style` keyword (case-insensitive).
fn decoration_style_kw(s: &str) -> Option<DecorationStyle> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "solid" => DecorationStyle::Solid,
        "double" => DecorationStyle::Double,
        "dotted" => DecorationStyle::Dotted,
        "dashed" => DecorationStyle::Dashed,
        "wavy" => DecorationStyle::Wavy,
        _ => return None,
    })
}

/// `border-style` component (R1): unknown keywords keep the current value.
fn border_style_kw(s: &str) -> Option<BorderStyle> {
    match s {
        "none" => Some(BorderStyle::None),
        "hidden" => Some(BorderStyle::Hidden),
        "solid" => Some(BorderStyle::Solid),
        "dashed" => Some(BorderStyle::Dashed),
        "dotted" => Some(BorderStyle::Dotted),
        "double" => Some(BorderStyle::Double),
        "groove" => Some(BorderStyle::Groove),
        "ridge" => Some(BorderStyle::Ridge),
        "inset" => Some(BorderStyle::Inset),
        "outset" => Some(BorderStyle::Outset),
        _ => None,
    }
}

/// 1–4 value TRBL expansion (R1): 1 = all, 2 = tb lr, 3 = t lr b, 4 = trbl.
fn border_edges<T: Clone>(v: &str, f: impl Fn(&str) -> Option<T>) -> Option<[T; 4]> {
    let parts = split_top_whitespace(v);
    match parts.len() {
        1 => {
            let x = f(parts[0])?;
            Some([x.clone(), x.clone(), x.clone(), x])
        }
        2 => {
            let tb = f(parts[0])?;
            let lr = f(parts[1])?;
            Some([tb.clone(), lr.clone(), tb, lr])
        }
        3 => {
            let t = f(parts[0])?;
            let lr = f(parts[1])?;
            let b = f(parts[2])?;
            Some([t, lr.clone(), b, lr])
        }
        4 => Some([f(parts[0])?, f(parts[1])?, f(parts[2])?, f(parts[3])?]),
        _ => None,
    }
}

/// Mutable access to one border side by CSS name (R1). Unknown names hit the
/// top side (callers only pass top/right/bottom/left).
fn border_side_mut<'a>(b: &'a mut Border, name: &str) -> &'a mut BorderSide {
    match name {
        "right" => &mut b.right,
        "bottom" => &mut b.bottom,
        "left" => &mut b.left,
        _ => &mut b.top,
    }
}

/// R3/B14: `content` values — concatenations of quoted strings, `attr()`,
/// `counter(name [, type])` and `counters(name , sep [, type])`.
/// `none`/`normal` and unsupported values (url(), quotes) generate nothing.
/// A lone string or attribute collapses to the legacy variants.
fn parse_content_spec(v: &str) -> Option<ContentSpec> {
    let v = v.trim();
    if v.is_empty() || v == "none" || v == "normal" {
        return None;
    }
    let parts = parse_content_parts(v)?;
    match parts.as_slice() {
        [] => None,
        [ContentPart::Text(t)] => Some(ContentSpec::Text(t.clone())),
        [ContentPart::Attr(a)] => Some(ContentSpec::Attr(a.clone())),
        _ => Some(ContentSpec::Parts(parts)),
    }
}

/// B14: tokenize a `content` value into parts. Whitespace between parts is
/// a concatenation operator. Returns `None` on invalid syntax (unbalanced
/// quotes/parens, unknown function, unquoted `counters()` separator) —
/// matching "generates nothing".
fn parse_content_parts(v: &str) -> Option<Vec<ContentPart>> {
    let chars: Vec<char> = v.chars().collect();
    let mut i = 0usize;
    let mut parts = Vec::new();
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        if chars[i] == '"' || chars[i] == '\'' {
            let quote = chars[i];
            i += 1;
            let start = i;
            let mut closed = false;
            while i < chars.len() {
                match chars[i] {
                    '\\' => i += 2,
                    c if c == quote => {
                        closed = true;
                        break;
                    }
                    _ => i += 1,
                }
            }
            if !closed {
                return None;
            }
            let raw: String = chars[start..i].iter().collect();
            i += 1; // closing quote
            parts.push(ContentPart::Text(unescape(&raw)));
            continue;
        }
        // Function: identifier + balanced-paren argument list.
        if !(chars[i].is_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
            return None;
        }
        let id_start = i;
        while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '-' | '_')) {
            i += 1;
        }
        let id: String = chars[id_start..i].iter().collect();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if chars.get(i) != Some(&'(') {
            return None; // bare keywords like open-quote: unsupported
        }
        i += 1;
        let arg_start = i;
        let mut depth = 1usize;
        while i < chars.len() {
            match chars[i] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            return None;
        }
        let args: String = chars[arg_start..i].iter().collect();
        i += 1; // closing paren
        let kw = id.to_ascii_lowercase();
        match kw.as_str() {
            "attr" => {
                let name = args.trim().trim_matches('"').trim_matches('\'');
                if name.is_empty() {
                    return None;
                }
                parts.push(ContentPart::Attr(name.to_string()));
            }
            "counter" => {
                let mut it = args.splitn(2, ',');
                let name = it.next()?.trim();
                if name.is_empty() {
                    return None;
                }
                let style = it
                    .next()
                    .map(|s| list_style_type_kw(s.trim()))
                    .filter(|s| s.is_some())
                    .map(|s| s.unwrap());
                parts.push(ContentPart::Counter {
                    name: name.to_string(),
                    style,
                });
            }
            "counters" => {
                let mut it = args.splitn(3, ',');
                let name = it.next()?.trim();
                let sep = it.next()?.trim();
                if name.is_empty() {
                    return None;
                }
                // The separator must be a string literal.
                if sep.len() < 2
                    || !((sep.starts_with('"') && sep.ends_with('"'))
                        || (sep.starts_with('\'') && sep.ends_with('\'')))
                {
                    return None;
                }
                let style = it
                    .next()
                    .map(|s| list_style_type_kw(s.trim()))
                    .filter(|s| s.is_some())
                    .map(|s| s.unwrap());
                parts.push(ContentPart::Counters {
                    name: name.to_string(),
                    sep: unescape(&sep[1..sep.len() - 1]),
                    style,
                });
            }
            // url(), quotes() and friends: the whole value is unsupported.
            _ => return None,
        }
    }
    Some(parts)
}

/// Strip one layer of quotes and process CSS escapes (R3): `\XXXX` hex
/// escapes (up to 6 digits, one optional whitespace terminator) and escaped
/// quote/backslash pairs.
fn css_unquote(q: &str) -> String {
    unescape(&q[1..q.len() - 1])
}

/// Process CSS escapes in an already-unquoted string body.
fn unescape(inner: &str) -> String {
    let chars: Vec<char> = inner.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        match chars.get(i) {
            None => {}
            Some('\n') => i += 1, // line continuation
            Some(&e) if e.is_ascii_hexdigit() => {
                let mut hex = String::new();
                while i < chars.len() && chars[i].is_ascii_hexdigit() && hex.len() < 6 {
                    hex.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() && chars[i] == ' ' {
                    i += 1;
                }
                if let Some(v) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(v);
                }
            }
            Some(&e) => {
                out.push(e);
                i += 1;
            }
        }
    }
    out
}

fn split_top_whitespace(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            c if c.is_whitespace() => {
                if let Some(st) = start.take() {
                    out.push(&s[st..i]);
                }
            }
            _ => {
                if start.is_none() {
                    start = Some(i);
                }
            }
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

// ---------------------------------------------------------------------------
// B14 — list markers and CSS counters. Counters resolve at style time over
// a document-order scope stack carried on the walk; the resolved marker is
// stamped onto `ComputedStyle::marker_text` so layout only paints.
// ---------------------------------------------------------------------------

/// `list-style-type` / `counter()` style keyword.
fn list_style_type_kw(s: &str) -> Option<ListStyleType> {
    Some(match s.to_ascii_lowercase().as_str() {
        "disc" => ListStyleType::Disc,
        "circle" => ListStyleType::Circle,
        "square" => ListStyleType::Square,
        "none" => ListStyleType::None,
        "decimal" => ListStyleType::Decimal,
        "decimal-leading-zero" => ListStyleType::DecimalLeadingZero,
        "lower-alpha" | "lower-latin" => ListStyleType::LowerAlpha,
        "upper-alpha" | "upper-latin" => ListStyleType::UpperAlpha,
        "lower-roman" => ListStyleType::LowerRoman,
        "upper-roman" => ListStyleType::UpperRoman,
        _ => return None,
    })
}

/// HTML `type` attribute on ol/ul: the single-letter forms take precedence,
/// everything else runs through the keyword table.
fn list_style_type_attr(s: &str) -> Option<ListStyleType> {
    let s = s.trim();
    if s.chars().count() == 1 {
        return match s.chars().next().unwrap() {
            '1' => Some(ListStyleType::Decimal),
            'a' => Some(ListStyleType::LowerAlpha),
            'A' => Some(ListStyleType::UpperAlpha),
            'i' => Some(ListStyleType::LowerRoman),
            'I' => Some(ListStyleType::UpperRoman),
            _ => None,
        };
    }
    list_style_type_kw(s)
}

/// Render counter value `v` in style `t`. Alpha/roman out-of-range values
/// fall back to decimal, like browsers.
pub fn format_counter(t: ListStyleType, v: i32) -> String {
    match t {
        ListStyleType::Disc => "•".into(),
        ListStyleType::Circle => "◦".into(),
        ListStyleType::Square => "▪".into(),
        ListStyleType::None | ListStyleType::Decimal => v.to_string(),
        ListStyleType::DecimalLeadingZero => {
            let s = format!("{:02}", v.abs());
            if v < 0 {
                format!("-{s}")
            } else {
                s
            }
        }
        ListStyleType::LowerAlpha | ListStyleType::UpperAlpha => {
            if v < 1 {
                return v.to_string();
            }
            let mut n = v as u64;
            let mut out = String::new();
            while n > 0 {
                n -= 1;
                let c = (b'a' + (n % 26) as u8) as char;
                out.push(if t == ListStyleType::UpperAlpha {
                    c.to_ascii_uppercase()
                } else {
                    c
                });
                n /= 26;
            }
            out.chars().rev().collect()
        }
        ListStyleType::LowerRoman | ListStyleType::UpperRoman => {
            if v < 1 || v > 3999 {
                return v.to_string();
            }
            const TAB: [(u32, &str); 13] = [
                (1000, "M"),
                (900, "CM"),
                (500, "D"),
                (400, "CD"),
                (100, "C"),
                (90, "XC"),
                (50, "L"),
                (40, "XL"),
                (10, "X"),
                (9, "IX"),
                (5, "V"),
                (4, "IV"),
                (1, "I"),
            ];
            let mut n = v as u32;
            let mut out = String::new();
            for (val, sym) in TAB {
                while n >= val {
                    out.push_str(sym);
                    n -= val;
                }
            }
            if t == ListStyleType::LowerRoman {
                out.to_ascii_lowercase()
            } else {
                out
            }
        }
    }
}

/// Nearest scope defining `name`, innermost first.
fn counter_lookup(scopes: &[HashMap<String, i32>], name: &str) -> Option<i32> {
    scopes.iter().rev().find_map(|s| s.get(name).copied())
}

/// `counter-increment`: first scope (inner→outer) defining `name` is
/// updated in place; with none, the counter is created in the innermost
/// scope (a documented document-order simplification).
fn counter_bump(scopes: &mut [HashMap<String, i32>], name: &str, d: i32) {
    for s in scopes.iter_mut().rev() {
        if let Some(v) = s.get_mut(name) {
            *v += d;
            return;
        }
    }
    if let Some(top) = scopes.last_mut() {
        top.insert(name.to_string(), d);
    }
}

fn counter_value(scopes: &[HashMap<String, i32>], name: &str) -> i32 {
    counter_lookup(scopes, name).unwrap_or(0)
}

/// `counters()`: every scope defining `name`, innermost first, joined.
fn counters_join(
    scopes: &[HashMap<String, i32>],
    name: &str,
    sep: &str,
    t: ListStyleType,
) -> String {
    let vals: Vec<String> = scopes
        .iter()
        .rev()
        .filter_map(|s| s.get(name).copied())
        .map(|v| format_counter(t, v))
        .collect();
    if vals.is_empty() {
        return format_counter(t, 0);
    }
    vals.join(sep)
}

/// Resolve one `content` part against `node`'s attributes and the current
/// counter scopes.
fn resolve_content_part(
    p: &ContentPart,
    dom: &Dom,
    node: NodeId,
    scopes: &[HashMap<String, i32>],
) -> String {
    match p {
        ContentPart::Text(t) => t.clone(),
        ContentPart::Attr(a) => dom
            .element(node)
            .and_then(|e| e.attr(a))
            .unwrap_or("")
            .to_string(),
        ContentPart::Counter { name, style } => format_counter(
            style.unwrap_or(ListStyleType::Decimal),
            counter_value(scopes, name),
        ),
        ContentPart::Counters { name, sep, style } => {
            counters_join(scopes, name, sep, style.unwrap_or(ListStyleType::Decimal))
        }
    }
}

/// `<ol start="N">` (B14): seeds the implicit `list-item` counter.
fn ol_start_attr(dom: &Dom, node: NodeId) -> Option<i32> {
    let el = dom.element(node)?;
    (el.tag == "ol").then(|| ())?;
    el.attr("start")?.trim().parse::<i32>().ok()
}

/// `counter-reset` / `counter-increment` lists: `name [value]` pairs, `none`
/// clears. A non-integer where a value is expected drops the pair.
fn parse_counter_ops(v: &str, default: i32) -> Vec<CounterOp> {
    let mut out = Vec::new();
    let mut it = v.split_whitespace().peekable();
    while let Some(name) = it.next() {
        if name == "none" {
            out.clear();
            continue;
        }
        let val = match it.peek().and_then(|t| t.parse::<i32>().ok()) {
            Some(n) => {
                it.next();
                n
            }
            None => default,
        };
        out.push(CounterOp {
            name: name.to_string(),
            value: val,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// transform (Q2) — value parsing. Lives beside the gradient/box-shadow
// parsers: `Declaration` values are raw strings, so the computed types above
// are filled here with em/rem folded and percentages kept symbolic.
// ---------------------------------------------------------------------------

/// A translate/origin argument: px folded, `%` symbolic.
fn trans_len(s: &str, em: f32) -> Option<TransLen> {
    let rem = 16.0_f32;
    Some(match parse_len_px(s, em, rem)? {
        Len::Px(v) => TransLen { px: v, pct: 0.0 },
        Len::Sum { px, pct } => TransLen { px, pct },
        // min()/max()/clamp() in a transform arg: percentages have no
        // sane border-box meaning until layout, so the folded px wins.
        other => TransLen {
            px: other.px().unwrap_or(0.0),
            pct: 0.0,
        },
    })
}

/// Parse a transform angle: `deg` (also `rad`/`grad`/`turn`, and bare `0`)
/// → degrees.
fn transform_angle(s: &str) -> Option<f32> {
    let t = s.trim().to_ascii_lowercase();
    if let Some(n) = t
        .strip_suffix("deg")
        .and_then(|n| n.trim().parse::<f32>().ok())
    {
        return Some(n);
    }
    if let Some(n) = t
        .strip_suffix("grad")
        .and_then(|n| n.trim().parse::<f32>().ok())
    {
        return Some(n * 0.9);
    }
    if let Some(n) = t
        .strip_suffix("rad")
        .and_then(|n| n.trim().parse::<f32>().ok())
    {
        return Some(n.to_degrees());
    }
    if let Some(n) = t
        .strip_suffix("turn")
        .and_then(|n| n.trim().parse::<f32>().ok())
    {
        return Some(n * 360.0);
    }
    if t == "0" {
        return Some(0.0);
    }
    None
}

/// Parse a `transform` value: a whitespace-separated list of `name(args)`
/// functions (CSS applies them right-to-left; folding to that order happens
/// at layout when the matrix is built). `none`/empty/garbage → `None`.
pub(crate) fn parse_transform(value: &str, em: f32) -> Option<TransformList> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    let mut fns = Vec::new();
    let mut rest = v;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let open = rest.find('(')?;
        let name = rest[..open].trim().to_ascii_lowercase();
        let close = rest[open..].find(')')? + open;
        let f = parse_transform_fn(&name, &rest[open + 1..close], em)?;
        fns.push(f);
        rest = &rest[close + 1..];
    }
    Some(TransformList(fns))
}

/// Parse one `name(args)` transform function. Comma-separated args, with
/// space separation accepted too (CSS Transforms L2 allows it).
fn parse_transform_fn(name: &str, args: &str, em: f32) -> Option<TransformFn> {
    let mut parts = split_fn_args(args);
    if parts.len() == 1 && parts[0].contains(char::is_whitespace) {
        parts = split_top_whitespace(&parts[0])
            .into_iter()
            .map(String::from)
            .collect();
    }
    let n = |i: usize| -> Option<f32> { parts.get(i).and_then(|p| p.trim().parse::<f32>().ok()) };
    match name {
        "translate" => {
            let x = trans_len(parts.first()?.trim(), em)?;
            let y = if parts.len() > 1 {
                trans_len(parts[1].trim(), em)?
            } else {
                TransLen::ZERO
            };
            Some(TransformFn::Translate { x, y })
        }
        "translatex" => Some(TransformFn::TranslateX(trans_len(
            parts.first()?.trim(),
            em,
        )?)),
        "translatey" => Some(TransformFn::TranslateY(trans_len(
            parts.first()?.trim(),
            em,
        )?)),
        "scale" => {
            let sx = n(0)?;
            Some(TransformFn::Scale(
                sx,
                parts.get(1).and_then(|_| n(1)).unwrap_or(sx),
            ))
        }
        "scalex" => Some(TransformFn::ScaleX(n(0)?)),
        "scaley" => Some(TransformFn::ScaleY(n(0)?)),
        "rotate" => Some(TransformFn::Rotate(transform_angle(parts.first()?)?)),
        "skew" => {
            let ax = transform_angle(parts.first()?)?;
            let ay = parts.get(1).and_then(|p| transform_angle(p)).unwrap_or(0.0);
            Some(TransformFn::Skew(ax, ay))
        }
        "skewx" => Some(TransformFn::SkewX(transform_angle(parts.first()?)?)),
        "skewy" => Some(TransformFn::SkewY(transform_angle(parts.first()?)?)),
        "matrix" => {
            if parts.len() != 6 {
                return None;
            }
            // CSS matrix(a,b,c,d,e,f) → row-major [a, c, e, b, d, f].
            let (a, b, c, d, e, f) = (n(0)?, n(1)?, n(2)?, n(3)?, n(4)?, n(5)?);
            Some(TransformFn::Matrix([a, c, e, b, d, f]))
        }
        // B8: 3D functions fold at parse time to their orthographic
        // projection — every engine-side transform is a 2×3 affine matrix
        // applied to planar (z=0) content, and orthographic projection is
        // linear, so P(A·B) = P(A)·P(B): folding each function independently
        // composes exactly. No z position exists after the fold (and no
        // depth sorting / preserve-3d), which is the documented scope.
        "translate3d" => {
            let x = trans_len(parts.first()?.trim(), em)?;
            let y = if parts.len() > 1 {
                trans_len(parts[1].trim(), em)?
            } else {
                TransLen::ZERO
            };
            Some(TransformFn::Translate { x, y }) // z dropped: nothing projects from it
        }
        "scale3d" => {
            let sx = n(0)?;
            Some(TransformFn::Scale(
                sx,
                parts.get(1).and_then(|_| n(1)).unwrap_or(sx),
            ))
        }
        "rotatez" => Some(TransformFn::Rotate(transform_angle(parts.first()?)?)),
        "rotatex" | "rotatey" => {
            let c = transform_angle(parts.first()?)?.to_radians().cos();
            // Axis squash: rotateX flattens y by cos θ, rotateY flattens x.
            Some(if name == "rotatex" {
                TransformFn::Matrix([1.0, 0.0, 0.0, 0.0, c, 0.0])
            } else {
                TransformFn::Matrix([c, 0.0, 0.0, 0.0, 1.0, 0.0])
            })
        }
        "rotate3d" => {
            if parts.len() != 4 {
                return None;
            }
            let (x, y, z) = (n(0)?, n(1)?, n(2)?);
            let t = transform_angle(parts.get(3)?)?;
            let len = (x * x + y * y + z * z).sqrt();
            if len == 0.0 {
                return None;
            }
            let (x, y, z) = (x / len, y / len, z / len);
            if x == 0.0 && y == 0.0 {
                // Pure z axis — keep it as a Rotate so it interpolates by angle.
                return Some(TransformFn::Rotate(if z < 0.0 { -t } else { t }));
            }
            // Rodrigues' formula, top-left 2×2: for planar input the z rows
            // never feed the projected x/y.
            let (s, c) = (t.to_radians().sin(), t.to_radians().cos());
            let ic = 1.0 - c;
            Some(TransformFn::Matrix([
                c + x * x * ic,
                x * y * ic - z * s,
                0.0,
                y * x * ic + z * s,
                c + y * y * ic,
                0.0,
            ]))
        }
        "matrix3d" => {
            if parts.len() != 16 {
                return None;
            }
            let v: Vec<f32> = parts
                .iter()
                .map(|p| p.trim().parse::<f32>().ok())
                .collect::<Option<Vec<_>>>()?;
            // Column-major: x out = (v0, v4, v8, v12), y out = (v1, v5, v9, v13),
            // w = (v3, v7, v11, v15). Exact fold only when x/y don't depend on
            // z (v8 = v9 = 0) and there's no perspective (v3 = v7 = 0); a
            // uniform w scale (v15) divides out.
            if v[8] != 0.0 || v[9] != 0.0 || v[3] != 0.0 || v[7] != 0.0 || v[15] == 0.0 {
                return None;
            }
            let k = 1.0 / v[15];
            Some(TransformFn::Matrix([
                v[0] * k,
                v[4] * k,
                v[12] * k,
                v[1] * k,
                v[5] * k,
                v[13] * k,
            ]))
        }
        // perspective() is genuinely projective (output depends on z after a
        // divide) — no 2×3 matrix represents it; the function stays dropped.
        _ => None,
    }
}

/// Parse `transform-origin`: one or two space-separated positions. Keywords
/// map to percentages (left/top 0, center 50, right/bottom 100); a third
/// (z) component is accepted and ignored — the engine is 2D.
pub(crate) fn parse_transform_origin(value: &str, em: f32) -> Option<TransformOrigin> {
    let parts = split_top_whitespace(value);
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut o = TransformOrigin::default();
    let mut first = parts[0].to_ascii_lowercase();
    match first.as_str() {
        "left" => o.x = TransLen { px: 0.0, pct: 0.0 },
        "right" => {
            o.x = TransLen {
                px: 0.0,
                pct: 100.0,
            }
        }
        "top" => o.y = TransLen { px: 0.0, pct: 0.0 },
        "bottom" => {
            o.y = TransLen {
                px: 0.0,
                pct: 100.0,
            }
        }
        "center" => {}
        _ => o.x = trans_len(parts[0], em)?,
    }
    if let Some(p2) = parts.get(1) {
        first = p2.to_ascii_lowercase();
        match first.as_str() {
            "top" => o.y = TransLen { px: 0.0, pct: 0.0 },
            "bottom" => {
                o.y = TransLen {
                    px: 0.0,
                    pct: 100.0,
                }
            }
            "center" => {}
            // A horizontal keyword after the first position is invalid.
            "left" | "right" => return None,
            _ => o.y = trans_len(p2, em)?,
        }
    }
    Some(o)
}

/// Parse a track list, also extracting B5 named lines: `[a b] 100px [c]` →
/// tracks `[Fixed(100px)]`, names `[(0, "a"), (0, "b"), (1, "c")]` (line i
/// sits before track i). `repeat()` expansion duplicates inner names, which
/// is legal (resolution takes the first match).
fn parse_grid_tracks_named(value: &str) -> (Vec<GridTrack>, Vec<(usize, String)>) {
    let mut v = value.trim().to_string();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return (Vec::new(), Vec::new());
    }
    // `repeat(N, inner)` → inner repeated N times (inner may hold `minmax`).
    v = expand_repeat(&v);
    // Flatten argument commas to spaces; `split_track_tokens` below keeps
    // each `fn(...)` (incl. `minmax(a b)`) in one token.
    v = v.replace(',', " ");
    let mut tracks = Vec::new();
    let mut names: Vec<(usize, String)> = Vec::new();
    let mut line = 0usize;
    let mut bracket = String::new();
    for tok in split_track_tokens(&v).iter().map(String::as_str) {
        // `[a b]` tokenizes as `[a` + `b]`; accumulate bracket groups and
        // emit one name per word on the current line.
        if !bracket.is_empty() {
            bracket.push(' ');
            bracket.push_str(tok);
            if tok.ends_with(']') {
                let inner = bracket[1..bracket.len() - 1].to_ascii_lowercase();
                for name in inner.split_whitespace() {
                    names.push((line, name.to_string()));
                }
                bracket.clear();
            }
            continue;
        }
        if tok.starts_with('[') {
            if tok.ends_with(']') {
                let t = tok[1..tok.len() - 1].to_ascii_lowercase();
                if !t.is_empty() {
                    names.push((line, t));
                }
            } else {
                bracket = tok.to_string();
            }
            continue;
        }
        let lower = tok.to_ascii_lowercase();
        if lower == "auto" {
            tracks.push(GridTrack::Auto);
        } else if lower == "min-content" {
            tracks.push(GridTrack::MinContent);
        } else if lower == "max-content" {
            tracks.push(GridTrack::MaxContent);
        } else if lower == "fit-content" {
            tracks.push(GridTrack::FitContent(None));
        } else if let Some(body) = lower
            .strip_prefix("fit-content(")
            .and_then(|t| t.strip_suffix(')'))
        {
            // C1b: `fit-content(<len>)` — the stretch cap.
            tracks.push(GridTrack::FitContent(parse_length(body.trim())));
        } else if let Some(body) = lower
            .strip_prefix("minmax(")
            .and_then(|t| t.strip_suffix(')'))
        {
            // C6: `minmax(<min>, <max>)` — args whitespace-separated after
            // comma flattening; malformed sides degrade to `auto`.
            let arg_list = split_track_tokens(body);
            let mut args = arg_list.iter().map(String::as_str);
            let a = args.next().unwrap_or("auto");
            let b = args.next().unwrap_or("auto");
            tracks.push(GridTrack::MinMax(min_side(a), max_side(b)));
        } else if lower.ends_with("fr") {
            let num = lower.trim_end_matches("fr").trim();
            let f: f32 = if num.is_empty() {
                1.0
            } else {
                num.parse().unwrap_or(1.0)
            };
            if f > 0.0 {
                tracks.push(GridTrack::Flex(f));
            }
        } else if let Some(l) = parse_length(tok) {
            tracks.push(GridTrack::Fixed(l));
        } else {
            continue;
        }
        line += 1;
    }
    (tracks, names)
}

/// B5 `grid-template-areas`: quoted row strings of space-separated names
/// (`"head head" "side main"`), `.` cells are empty. Rows must agree in
/// width or the declaration is dropped. Rows may use `'` or `"` quotes.
fn parse_grid_template_areas(value: &str) -> Vec<Vec<String>> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut rest = v;
    while let Some(open) = rest.find(['"', '\'']) {
        let q = rest.as_bytes()[open];
        let Some(close) = rest[open + 1..].find(q as char) else {
            return Vec::new();
        };
        let row: Vec<String> = rest[open + 1..open + 1 + close]
            .split_whitespace()
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if row.is_empty() {
            return Vec::new();
        }
        rows.push(row);
        rest = &rest[open + 1 + close + 1..];
    }
    if rows.iter().any(|r| r.len() != rows[0].len()) {
        return Vec::new();
    }
    rows
}

/// Split a grid shorthand on top-level `/` (`1 / 3`, `head / main`).
/// `None` when there's no slash (single-value fallback path).
fn split_grid_shorthand<const N: usize>(value: &str) -> Option<[String; N]> {
    let parts: Vec<&str> = value.split('/').collect();
    if parts.len() != N {
        return None;
    }
    Some(std::array::from_fn(|i| parts[i].trim().to_string()))
}

/// One longhand of a grid placement shorthand: `auto` clears, `span N`
/// folds into the shared span counter (matching the pre-B5 behavior where
/// `grid-column-end: span 2` set `grid-column-span`), a name or line
/// number stores verbatim for layout to resolve.
fn apply_grid_line(slot: &mut String, span: &mut u32, value: &str) {
    let v = value.trim().to_ascii_lowercase();
    if v.is_empty() || v == "auto" {
        slot.clear();
    } else if v.starts_with("span") {
        *span = parse_grid_span(&v);
        slot.clear();
    } else {
        *slot = v;
    }
}

/// When both ends of a placement axis are plain integer lines (`1 / 3`),
/// the span falls out directly — layout still gets the start line for
/// positioning. `None` keeps whatever `span` already holds (auto/name ends).
fn grid_span_between(start: &str, end: &str) -> Option<u32> {
    let s: usize = start.parse().ok()?;
    let e: i64 = end.parse().ok()?;
    let span = if e < 0 {
        -e as usize - s
    } else {
        e as usize - s
    };
    (span > 0).then(|| span as u32)
}

/// C1b: split a track list on whitespace outside `(...)` so function-style
/// sizing tokens (`minmax(0 1fr)`, `fit-content(200px)`) stay whole.
fn split_track_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for ch in s.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// C6: min side of `minmax()`. `fit-content` is illegal there per CSS —
/// approximated as `min-content`.
fn min_side(tok: &str) -> MinSizing {
    if tok == "max-content" {
        return MinSizing::MaxContent;
    }
    if tok == "min-content" || tok.starts_with("fit-content") {
        return MinSizing::MinContent;
    }
    parse_length(tok)
        .map(MinSizing::Len)
        .unwrap_or(MinSizing::Auto)
}

/// C6: max side of `minmax()`.
fn max_side(tok: &str) -> MaxTrack {
    match tok {
        "auto" => MaxTrack::Auto,
        "min-content" => MaxTrack::MinContent,
        "max-content" => MaxTrack::MaxContent,
        "fit-content" => MaxTrack::FitContent(None),
        _ => {
            if let Some(body) = tok
                .strip_prefix("fit-content(")
                .and_then(|t| t.strip_suffix(')'))
            {
                return MaxTrack::FitContent(parse_length(body.trim()));
            }
            if tok.ends_with("fr") {
                let num = tok.trim_end_matches("fr").trim();
                let f: f32 = if num.is_empty() {
                    1.0
                } else {
                    num.parse().unwrap_or(1.0)
                };
                if f > 0.0 {
                    return MaxTrack::Flex(f);
                }
            }
            parse_length(tok)
                .map(MaxTrack::Fixed)
                .unwrap_or(MaxTrack::Auto)
        }
    }
}

fn expand_repeat(s: &str) -> String {
    let mut cur = s.to_string();
    for _ in 0..8 {
        let lower = cur.to_ascii_lowercase();
        let Some(pos) = lower.find("repeat(") else {
            break;
        };
        let chars: Vec<char> = cur.chars().collect();
        // Byte pos == char pos for ASCII prefix, safe here.
        let mut j = pos + "repeat(".len();
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        let num_start = j;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        let count: usize = cur[num_start..j].trim().parse().unwrap_or(0);
        while j < chars.len() && (chars[j].is_whitespace() || chars[j] == ',') {
            j += 1;
        }
        let inner_start = j;
        let mut depth = 1usize;
        let mut k = j;
        while k < chars.len() && depth > 0 {
            match chars[k] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        if depth != 0 || count == 0 {
            break;
        }
        let inner: String = chars[inner_start..k].iter().collect();
        let repeated = std::iter::repeat(inner.trim())
            .take(count.min(24))
            .collect::<Vec<_>>()
            .join(" ");
        let before: String = chars[..pos].iter().collect();
        let after: String = chars[k + 1..].iter().collect();
        cur = format!("{before} {repeated} {after}");
    }
    cur
}

/// Parse a `grid-column` / `grid-row` span: `span 2` → 2, `1 / 3` → 2,
/// `1 / span 2` → 2, plain `2` or `auto` → 1.
fn parse_grid_span(value: &str) -> u32 {
    let lower = value.trim().to_ascii_lowercase();
    if let Some(pos) = lower.find("span") {
        let after = lower[pos + "span".len()..].trim();
        let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        return num.parse::<u32>().unwrap_or(1).clamp(1, 12);
    }
    if lower.contains('/') {
        let parts: Vec<&str> = lower.split('/').collect();
        if parts.len() == 2 {
            let a = parts[0].trim().parse::<i32>().ok();
            let b = parts[1].trim().parse::<i32>().ok();
            if let (Some(a), Some(b)) = (a, b) {
                return (b - a).max(1).min(12) as u32;
            }
        }
        return 1;
    }
    1
}

fn upsert_var(vars: &mut Vec<(String, String)>, name: String, value: String) {
    if let Some(slot) = vars.iter_mut().find(|(k, _)| *k == name) {
        slot.1 = value;
    } else {
        vars.push((name, value));
    }
}

/// Element *queries* — the read-only side of the matching rules.
///
/// The cascade already knows how to ask "does this selector match this
/// node"; the agent plane (a selector target) and any other embedder need
/// the same answer as a list. One implementation, so a `querySelector`-style
/// call and the stylesheet can never disagree about what a selector means.
pub mod query {
    use super::{matches, InteractionState, Selector};
    use kitae_dom::{Dom, NodeId, NodeKind};

    /// Parse one complex selector for a query. `None` when the text is not
    /// a valid selector — never an empty match set, so callers can tell
    /// "no match" from "bad selector".
    pub fn parse(selector_text: &str) -> Option<Selector> {
        kitae_css::parse_selector(selector_text.trim())
    }

    /// Every element matching `selector`, in document order. Interaction
    /// state passes through, so `:hover`/`:focus` selectors answer with
    /// what the embedder currently reports.
    pub fn all(dom: &Dom, selector: &Selector, state: &InteractionState) -> Vec<NodeId> {
        let Some(root) = dom.root() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut ancestors: Vec<NodeId> = Vec::new();
        collect(dom, root, selector, state, &mut ancestors, &mut out);
        out
    }

    /// Parse and match in one call: `None` for a bad selector, `Some(list)`
    /// (possibly empty) otherwise.
    pub fn find(dom: &Dom, selector_text: &str, state: &InteractionState) -> Option<Vec<NodeId>> {
        let selector = parse(selector_text)?;
        Some(all(dom, &selector, state))
    }

    fn collect(
        dom: &Dom,
        node: NodeId,
        selector: &Selector,
        state: &InteractionState,
        ancestors: &mut Vec<NodeId>,
        out: &mut Vec<NodeId>,
    ) {
        if matches!(dom.node(node).kind, NodeKind::Element)
            && matches(selector, dom, node, ancestors, state)
        {
            out.push(node);
        }
        ancestors.push(node);
        for &kid in dom.children(node) {
            collect(dom, kid, selector, state, ancestors, out);
        }
        ancestors.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kitae_html::parse;

    fn styled(html: &str, author: &[&str]) -> (Dom, Vec<(NodeId, ComputedStyle)>) {
        let (dom, _) = parse(html);
        let styles = StyleEngine::new(author).compute(&dom);
        (dom, styles)
    }

    fn find<'a>(styles: &'a [(NodeId, ComputedStyle)], dom: &Dom, tag: &str) -> &'a ComputedStyle {
        styles
            .iter()
            .find(|(n, _)| dom.tag(*n) == tag)
            .map(|(_, s)| s)
            .unwrap_or_else(|| panic!("no {tag} in tree"))
    }

    fn by_id<'a>(styles: &'a [(NodeId, ComputedStyle)], dom: &Dom, id: &str) -> &'a ComputedStyle {
        let node = dom
            .get_element_by_id(id)
            .unwrap_or_else(|| panic!("no #{id}"));
        &styles.iter().find(|(n, _)| *n == node).unwrap().1
    }

    #[test]
    fn ua_sheet_styles_paragraphs_and_heads() {
        let (dom, styles) = styled("<p>hello</p><h1>Big</h1><a href='x'>link</a>", &[]);

        let p = find(&styles, &dom, "p");
        assert_eq!(p.display, Display::Block);
        assert_eq!(p.font_size, 16.0);
        assert_eq!(p.margin.bottom.px(), Some(14.0));

        let h1 = find(&styles, &dom, "h1");
        assert_eq!(h1.font_weight, 700);
        assert!((h1.font_size - 30.4).abs() < 0.01);

        let a = find(&styles, &dom, "a");
        assert_eq!(a.color, 0x0B57D0FF);
        assert!(a.underline);
        assert_eq!(a.link_href.as_deref(), Some("x"));
    }

    #[test]
    fn qtsq_reader_styles_tables_quotes_and_code() {
        let (dom, styles) = styled(
            "<div id='doc' class='qtsq-text'><table id='table'><tr>\
             <th id='head'>H</th><td id='cell'>C</td></tr></table>\
             <blockquote id='quote'>quote</blockquote><code id='inline'>x</code>\
             <pre id='pre'><code id='block'>y</code></pre><img id='image' src='x'></div>",
            &[],
        );
        assert!(by_id(&styles, &dom, "table").border_collapse);
        assert_eq!(
            by_id(&styles, &dom, "head").border.top.style,
            BorderStyle::Solid
        );
        assert_eq!(
            by_id(&styles, &dom, "head").border.top.width.resolve(16.0),
            1.0
        );
        assert_eq!(by_id(&styles, &dom, "head").background, 0xF4F5F7FF);
        assert_eq!(
            by_id(&styles, &dom, "quote").border.left.style,
            BorderStyle::Solid
        );
        assert_eq!(
            by_id(&styles, &dom, "quote")
                .border
                .left
                .width
                .resolve(16.0),
            3.0
        );
        assert!(by_id(&styles, &dom, "inline").background != 0);
        assert_eq!(by_id(&styles, &dom, "block").background, 0);
        assert_eq!(
            by_id(&styles, &dom, "image")
                .max_width
                .unwrap()
                .resolve(400.0),
            400.0
        );
    }

    #[test]
    fn font_shorthand_parses_size_weight_style_and_family() {
        let (dom, styles) = styled(
            r#"<p style="font: italic 700 18px/1.5 Georgia, serif">a</p>
               <p style="font: inherit">b</p>
               <p style="font: 24px monospace">c</p>"#,
            &[],
        );
        let ps: Vec<&ComputedStyle> = styles
            .iter()
            .filter(|(n, _)| dom.tag(*n) == "p")
            .map(|(_, s)| s)
            .collect();
        let (a, b, c) = (ps[0], ps[1], ps[2]);
        assert!((a.font_size - 18.0).abs() < 0.01);
        assert_eq!(a.font_weight, 700);
        assert!(a.italic);
        assert!((a.line_height - 1.5).abs() < 0.01);
        assert_eq!(a.font_family, "Georgia");
        assert!(!a.monospace);
        // `font: inherit` is a no-op — the UA-sheet paragraph defaults stand.
        assert!((b.font_size - 16.0).abs() < 0.01);
        assert_eq!(b.font_weight, 400);
        // Bare family with just a size: the generic-keyword family keeps
        // the engine family but flags monospace.
        assert!(c.monospace);
        assert!((c.font_size - 24.0).abs() < 0.01);
    }

    #[test]
    fn b12_text_decoration_shorthand_and_longhands() {
        let (dom, styles) = styled(
            r#"<p style="text-decoration: underline wavy #f00 2px">a</p>
               <p style="text-decoration: underline; text-decoration-style: dashed">b</p>
               <p style="text-decoration: underline solid red">c</p>
               <p style="text-decoration: underline; text-decoration: none">d</p>"#,
            &[],
        );
        let ps: Vec<&ComputedStyle> = styles
            .iter()
            .filter(|(n, _)| dom.tag(*n) == "p")
            .map(|(_, s)| s)
            .collect();
        let (a, b, c, d) = (ps[0], ps[1], ps[2], ps[3]);
        assert!(a.underline);
        assert_eq!(a.decoration_style, DecorationStyle::Wavy);
        assert_eq!(a.decoration_color, Some(0xFF0000FF));
        assert_eq!(a.decoration_thickness.and_then(|l| l.px()), Some(2.0));
        assert_eq!(b.decoration_style, DecorationStyle::Dashed);
        assert_eq!(
            b.decoration_color, None,
            "longhand style keeps currentColor"
        );
        assert_eq!(c.decoration_style, DecorationStyle::Solid);
        assert_eq!(c.decoration_color, Some(0xFF0000FF));
        assert!(!d.underline, "shorthand `none` clears the line");
    }

    /// B9 mock: fixed ch/ex regardless of font context.
    struct FixedMetrics {
        ch: f32,
        ex: f32,
    }
    impl FontMetrics for FixedMetrics {
        fn ch_px(&self, _f: &str, _s: f32, _w: u32, _i: bool, _m: bool) -> f32 {
            self.ch
        }
        fn ex_px(&self, _f: &str, _s: f32, _w: u32, _i: bool, _m: bool) -> f32 {
            self.ex
        }
    }

    #[test]
    fn b9_ch_ex_fold_via_metrics() {
        let html = "<div id='x' style='width: 10ch; height: 4ex; margin-left: 2ch'>t</div>";
        let (dom, _) = parse(html);
        // No metrics attached: the CSS fallback ratio — both units fold
        // to half the font size (16px font → 8px).
        let plain = StyleEngine::new(&[]).compute(&dom);
        let x = by_id(&plain, &dom, "x");
        assert_eq!(
            x.width.and_then(|sv| sv.len()).and_then(|l| l.px()),
            Some(80.0)
        );
        assert_eq!(
            x.height.and_then(|sv| sv.len()).and_then(|l| l.px()),
            Some(32.0)
        );
        assert_eq!(x.margin.left.px(), Some(16.0));
        // Metrics attached: ch = 9px, ex = 5px.
        let m = StyleEngine::new(&[])
            .with_metrics(Arc::new(FixedMetrics { ch: 9.0, ex: 5.0 }))
            .compute(&dom);
        let x = by_id(&m, &dom, "x");
        assert_eq!(
            x.width.and_then(|sv| sv.len()).and_then(|l| l.px()),
            Some(90.0)
        );
        assert_eq!(
            x.height.and_then(|sv| sv.len()).and_then(|l| l.px()),
            Some(20.0)
        );
        assert_eq!(x.margin.left.px(), Some(18.0));
        // calc() folds too; the gate ignores non-unit lookalikes.
        let (dom2, _) = parse(
            "<div id='c' style='width: calc(1ch + 10px)'>t</div>\
             <div id='n' style='width: 2chars'>t</div>",
        );
        let s = StyleEngine::new(&[])
            .with_metrics(Arc::new(FixedMetrics { ch: 9.0, ex: 5.0 }))
            .compute(&dom2);
        assert_eq!(
            by_id(&s, &dom2, "c")
                .width
                .and_then(|sv| sv.len())
                .and_then(|l| l.px()),
            Some(19.0)
        );
        assert_eq!(
            by_id(&s, &dom2, "n").width,
            None,
            "`2chars` is not a length"
        );
    }

    #[test]
    fn b13_line_break_properties_parse_and_inherit() {
        let (dom, styles) = styled(
            "<div id='p' style='overflow-wrap: break-word; word-break: break-all; \
             text-overflow: ellipsis; hyphens: auto'><span id='c'>x</span></div>",
            &[],
        );
        let p = by_id(&styles, &dom, "p");
        assert_eq!(p.overflow_wrap, OverflowWrap::BreakWord);
        assert_eq!(p.word_break, WordBreak::BreakAll);
        assert_eq!(p.text_overflow, TextOverflow::Ellipsis);
        assert_eq!(p.hyphens, Hyphens::Auto);
        // All four are inherited.
        let c = by_id(&styles, &dom, "c");
        assert_eq!(c.overflow_wrap, OverflowWrap::BreakWord);
        assert_eq!(c.word_break, WordBreak::BreakAll);
        assert_eq!(c.text_overflow, TextOverflow::Ellipsis);
        assert_eq!(c.hyphens, Hyphens::Auto);
        // Legacy aliases + defaults + invalid keywords leave the value.
        let (dom2, s2) = styled(
            "<div id='a' style='word-wrap: break; hyphens: none'>t</div>\
             <div id='b' style='overflow-wrap: nonsense; text-overflow: clip'>t</div>",
            &[],
        );
        let a = by_id(&s2, &dom2, "a");
        assert_eq!(
            a.overflow_wrap,
            OverflowWrap::BreakWord,
            "`word-wrap: break` alias"
        );
        assert_eq!(a.hyphens, Hyphens::None);
        let b = by_id(&s2, &dom2, "b");
        assert_eq!(
            b.overflow_wrap,
            OverflowWrap::Normal,
            "invalid keyword ignored"
        );
        assert_eq!(b.text_overflow, TextOverflow::Clip);
    }

    #[test]
    fn author_css_beats_ua_at_same_specificity() {
        let (dom, styles) = styled(
            "<p class='lead'>x</p>",
            &["p.lead { color: #ff0000; font-size: 20px }"],
        );
        let p = find(&styles, &dom, "p");
        assert_eq!(p.color, 0xFF0000FF);
        assert_eq!(p.font_size, 20.0);
    }

    #[test]
    fn important_beats_inline() {
        let (dom, styles) = styled(
            "<p style=\"color: #00ff00\">x</p>",
            &["p { color: #ff0000 !important }"],
        );
        let p = find(&styles, &dom, "p");
        assert_eq!(p.color, 0xFF0000FF);
    }

    #[test]
    fn inline_style_wins_without_important() {
        let (dom, styles) = styled("<p style=\"margin: 3px 7px\">x</p>", &[]);
        let p = find(&styles, &dom, "p");
        assert_eq!(p.margin.top.px(), Some(3.0));
        assert_eq!(p.margin.right.px(), Some(7.0));
        assert_eq!(p.margin.bottom.px(), Some(3.0));
        assert_eq!(p.margin.left.px(), Some(7.0));
    }

    #[test]
    fn id_beats_class_beats_tag() {
        let (dom, styles) = styled(
            "<p id='a' class='c'>x</p>",
            &[
                "p { color: #111111 }",
                ".c { color: #222222 }",
                "#a { color: #333333 }",
            ],
        );
        let p = find(&styles, &dom, "p");
        assert_eq!(p.color, 0x333333FF);
    }

    #[test]
    fn descendant_selector_matches() {
        let (dom, styles) = styled(
            "<div class='wrap'><span>in</span></div><span>out</span>",
            &[".wrap span { color: #123456 }"],
        );
        let spans: Vec<_> = styles
            .iter()
            .filter(|(n, _)| dom.tag(*n) == "span")
            .map(|(_, s)| s)
            .collect();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].color, 0x123456FF);
        assert_eq!(spans[1].color, 0x1A1D23FF);
    }

    #[test]
    fn child_combinator_requires_exact_parent() {
        let (dom, styles) = styled(
            "<div class='wrap'><span id='direct'>a</span><section><span id='deep'>b</span></section></div>",
            &[".wrap > span { color: #123456 }"],
        );
        assert_eq!(by_id(&styles, &dom, "direct").color, 0x123456FF);
        assert_eq!(by_id(&styles, &dom, "deep").color, 0x1A1D23FF);
    }

    #[test]
    fn sibling_combinators_match_and_index_stays_sound() {
        let (dom, styles) = styled(
            "<ul><li id='a' class='first'>a</li><li id='b'>b</li><li id='c'>c</li></ul>",
            &[
                "li.first + li { color: #123456 }",
                "li.first ~ li { background: #654321 }",
            ],
        );
        assert_eq!(by_id(&styles, &dom, "a").color, 0x1A1D23FF);
        assert_eq!(by_id(&styles, &dom, "b").color, 0x123456FF);
        assert_eq!(by_id(&styles, &dom, "c").color, 0x1A1D23FF);
        assert_eq!(by_id(&styles, &dom, "a").background, 0);
        assert_eq!(by_id(&styles, &dom, "b").background, 0x654321FF);
        assert_eq!(by_id(&styles, &dom, "c").background, 0x654321FF);

        let sheet = kitae_css::parse_stylesheet_environment(
            "h1 + p { color: red } h1 ~ p { color: blue }",
            800.0,
            &EnvironmentState::default(),
        );
        let index = RuleIndex::build(&sheet.rules);
        let (dom, _) = parse("<h1></h1><p id='p'>x</p>");
        let p = dom.get_element_by_id("p").unwrap();
        let candidates = index.candidates(dom.element(p).unwrap(), &vec![0; index.total_anc_keys]);
        assert!(candidates.contains(&0));
        assert!(candidates.contains(&1));

        let (dom, styles) = styled(
            "<section></section><article><p id='inside'>x</p></article>",
            &["section + article p { color: #abcdef }"],
        );
        assert_eq!(by_id(&styles, &dom, "inside").color, 0xABCDEFFF);
    }

    #[test]
    fn rule_index_keeps_is_ancestor_alternatives() {
        let (dom, styles) = styled(
            "<div class='a'><p id='inside'>inside</p></div><p id='outside'>outside</p>",
            &[":is(.a, .b) > p { color: #123456 }"],
        );
        assert_eq!(by_id(&styles, &dom, "inside").color, 0x123456FF);
        assert_eq!(by_id(&styles, &dom, "outside").color, 0x1A1D23FF);
    }

    #[test]
    fn complex_not_matches_through_cascade() {
        let (dom, styles) = styled(
            "<div class='wrapper'><p id='blocked' class='blocked'>x</p><p id='open'>y</p></div>",
            &["p:not(.wrapper > .blocked) { color: #123456 }"],
        );
        assert_eq!(by_id(&styles, &dom, "blocked").color, 0x1A1D23FF);
        assert_eq!(by_id(&styles, &dom, "open").color, 0x123456FF);
    }

    #[test]
    fn not_specificity_uses_only_its_argument() {
        let (dom, styles) = styled(
            "<p id='x' class='a'>x</p>",
            &["p:not(.a) { color: #ff0000 } p.a { color: #0000ff }"],
        );
        assert_eq!(by_id(&styles, &dom, "x").color, 0x0000FFFF);
    }

    #[test]
    fn attribute_selectors_match() {
        let (dom, styles) = styled(
            "<a id='ext' href='https://x.test' target='_blank'>e</a><a id='int' href='/local'>i</a>",
            &[
                "a[target] { color: #111111 }",
                "a[target='_top'] { color: #222222 }",
                "a[href^='https'] { font-weight: 700 }",
            ],
        );
        let ext = by_id(&styles, &dom, "ext");
        assert_eq!(
            ext.color, 0x111111FF,
            "[target] hits, [target='_top'] does not"
        );
        assert_eq!(ext.font_weight, 700, "[href^='https'] prefix match");
        assert_eq!(
            by_id(&styles, &dom, "int").color,
            0x0B57D0FF,
            "no author rule: UA link blue"
        );
    }

    #[test]
    fn structural_pseudo_classes_match() {
        let (dom, styles) = styled(
            "<ul><li id='first'>1</li><li id='mid'>2</li><li id='last'>3</li></ul>",
            &[
                "li:first-child { color: #111111 }",
                "li:last-child { color: #222222 }",
                "li:not(#mid) { font-weight: 700 }",
            ],
        );
        assert_eq!(by_id(&styles, &dom, "first").color, 0x111111FF);
        assert_eq!(by_id(&styles, &dom, "mid").color, 0x1A1D23FF);
        assert_eq!(by_id(&styles, &dom, "last").color, 0x222222FF);
        assert_eq!(by_id(&styles, &dom, "first").font_weight, 700);
        assert_eq!(by_id(&styles, &dom, "mid").font_weight, 400);
        assert_eq!(by_id(&styles, &dom, "last").font_weight, 700);
    }

    #[test]
    fn root_pseudo_and_custom_properties_flow() {
        let (dom, styles) = styled(
            "<p>x</p>",
            &[
                ":root { font-size: 18px; --accent: #ff0000 }",
                "p { color: var(--accent); background: var(--missing, #00ff00) }",
            ],
        );
        let p = find(&styles, &dom, "p");
        assert!(
            (p.font_size - 18.0).abs() < 0.01,
            ":root hits the html element"
        );
        assert_eq!(p.color, 0xFF0000FF);
        assert_eq!(
            p.background, 0x00FF00FF,
            "fallback applies when var is missing"
        );
    }

    #[test]
    fn custom_property_cycles_and_overrides() {
        let (dom, styles) = styled(
            "<div style='--c: var(--c, #123456); color: var(--c)'><p>x</p></div>",
            &["div { --c: #abcdef }"],
        );
        let p = find(&styles, &dom, "p");
        assert_eq!(p.color, 0xABCDEFFF, "inherited var wins over a self-cycle");
    }

    #[test]
    fn custom_property_css_wide_keywords_resolve() {
        let (dom, styles) = styled(
            "<div id='p' style='--x: #123456'><span id='c' style='--x: inherit; color: var(--x)'></span></div>",
            &[],
        );
        assert_eq!(by_id(&styles, &dom, "c").color, 0x123456FF);
    }

    #[test]
    fn custom_property_initial_is_guaranteed_invalid() {
        let (dom, styles) = styled(
            "<div style='--x: #123456'><span id='c' style='--x: initial; color: var(--x, #00ff00)'></span></div>",
            &[],
        );
        assert_eq!(by_id(&styles, &dom, "c").color, 0x00FF00FF);
    }

    #[test]
    fn positioned_and_flex_declarations_parse() {
        let (dom, styles) = styled(
            "<div id='box' style=\"position: fixed; top: 12px; right: 4px; min-height: 40px; max-height: 100px; overflow: hidden; opacity: 0.5; display: flex; flex-direction: column; justify-content: center; gap: 8px\"><span id='kid' style='flex: 2'>x</span></div>",
            &[],
        );
        let box_st = by_id(&styles, &dom, "box");
        assert_eq!(box_st.position, Position::Fixed);
        assert_eq!(box_st.top.map(|l| l.px()), Some(Some(12.0)));
        assert_eq!(box_st.right.map(|l| l.px()), Some(Some(4.0)));
        assert_eq!(box_st.min_height.map(|l| l.px()), Some(Some(40.0)));
        assert_eq!(box_st.max_height.map(|l| l.px()), Some(Some(100.0)));
        assert_eq!(box_st.overflow, Overflow::Hidden);
        assert!((box_st.opacity - 0.5).abs() < 0.001);
        assert_eq!(box_st.display, Display::Flex);
        assert_eq!(box_st.flex_direction, FlexDirection::Column);
        assert_eq!(box_st.justify_content, JustifyContent::Center);
        assert_eq!(box_st.gap.px(), Some(8.0));

        let kid = by_id(&styles, &dom, "kid");
        assert!((kid.flex_grow - 2.0).abs() < 0.001);
        assert_eq!(
            kid.flex_basis,
            Some(SizeVal::Len(Len::ZERO)),
            "`flex: 2` means 2 1 0%"
        );
    }

    #[test]
    fn media_rules_respect_the_viewport() {
        let css = "@media (min-width: 800px) { p { color: #123456 } } @media (max-width: 500px) { p { font-weight: 700 } }";
        let (dom, _) = parse("<p>x</p>");

        let wide = StyleEngine::with_viewport(&[css], 1200.0).compute(&dom);
        let p = find(&wide, &dom, "p");
        assert_eq!(p.color, 0x123456FF);
        assert_eq!(p.font_weight, 400);

        let narrow = StyleEngine::with_viewport(&[css], 400.0).compute(&dom);
        let p = find(&narrow, &dom, "p");
        assert_eq!(p.color, 0x1A1D23FF);
        assert_eq!(p.font_weight, 700);
    }

    #[test]
    fn media_rules_respect_the_environment() {
        let css = "@media (prefers-color-scheme: dark) { p { color: #123456 } } \
                   @media (prefers-reduced-motion: reduce) { p { letter-spacing: 1px } }";
        let (dom, _) = parse("<p>x</p>");

        let light = StyleEngine::with_viewport(&[css], 800.0).compute(&dom);
        let p = find(&light, &dom, "p");
        assert_eq!(p.color, 0x1A1D23FF, "default environment is light");
        assert_eq!(p.letter_spacing, 0.0);

        let dark = StyleEngine::with_environment(
            &[css],
            800.0,
            600.0,
            &EnvironmentState {
                color_scheme: ColorScheme::Dark,
                ..Default::default()
            },
        )
        .compute(&dom);
        let p = find(&dark, &dom, "p");
        assert_eq!(p.color, 0x123456FF, "dark-scheme rule now applies");
        assert_eq!(p.letter_spacing, 0.0, "motion still full");

        let still = StyleEngine::with_environment(
            &[css],
            800.0,
            600.0,
            &EnvironmentState {
                reduced_motion: true,
                ..Default::default()
            },
        )
        .compute(&dom);
        let p = find(&still, &dom, "p");
        assert_eq!(p.color, 0x1A1D23FF, "scheme unchanged");
        assert_eq!(p.letter_spacing, 1.0, "reduced-motion rule now applies");
    }

    #[test]
    fn color_inherits_to_children() {
        let (dom, styles) = styled("<blockquote>outer <b>bold</b></blockquote>", &[]);
        let bq = find(&styles, &dom, "blockquote");
        let b = find(&styles, &dom, "b");
        assert_eq!(b.color, bq.color, "b inside blockquote inherits its color");
        assert_eq!(b.font_weight, 700);
    }

    #[test]
    fn em_font_size_resolves_against_parent() {
        let (dom, styles) = styled(
            "<div style=\"font-size: 20px\"><span style=\"font-size: 0.8em\">x</span></div>",
            &[],
        );
        let span = find(&styles, &dom, "span");
        assert!((span.font_size - 16.0).abs() < 0.01);
    }

    #[test]
    fn var_inside_calc_resolves() {
        let (dom, styles) = styled(
            "<p style=\"--pad: 24px; margin-left: calc(var(--pad) + 2px)\">x</p>",
            &[],
        );
        let p = find(&styles, &dom, "p");
        assert_eq!(p.margin.left, Len::Px(26.0));
    }

    #[test]
    fn calc_in_margin_shorthand() {
        let (dom, styles) = styled("<p style=\"margin: 4px calc(2px + 3px)\">x</p>", &[]);
        let p = find(&styles, &dom, "p");
        assert_eq!(p.margin.top.px(), Some(4.0));
        assert_eq!(p.margin.right.px(), Some(5.0));
        assert_eq!(p.margin.bottom.px(), Some(4.0));
        assert_eq!(p.margin.left.px(), Some(5.0));
    }

    #[test]
    fn percent_width_stays_symbolic_and_height_pct_drops() {
        let (dom, styles) = styled(
            "<div style=\"width: 50%; height: 50%; min-width: calc(100% - 24px)\">x</div>",
            &[],
        );
        let d = find(&styles, &dom, "div");
        assert_eq!(d.width, Some(SizeVal::Len(Len::Sum { px: 0.0, pct: 50.0 })));
        assert_eq!(d.width.unwrap().len().unwrap().resolve(200.0), 100.0);
        assert_eq!(d.height, None, "% height is indefinite → unset");
        assert_eq!(
            d.min_width,
            Some(Len::Sum {
                px: -24.0,
                pct: 100.0
            })
        );
    }

    #[test]
    fn viewport_units_fold_against_the_engine_viewport() {
        let (dom, _) = parse("<div></div>");
        let css = "div { width: 50vw; height: 25vh; margin-left: 10vmin }";
        let wide =
            StyleEngine::with_environment(&[css], 1000.0, 800.0, &Default::default()).compute(&dom);
        let d = find(&wide, &dom, "div");
        assert_eq!(
            d.width.unwrap().len().unwrap().resolve(1000.0),
            500.0,
            "50vw of 1000"
        );
        assert!(
            (d.height.unwrap().len().unwrap().resolve(0.0) - 200.0).abs() < 0.01,
            "25vh of 800"
        );
        // vmin = min(1000, 800) → 10% = 80.
        assert_eq!(d.margin.left.resolve(1000.0), 80.0, "10vmin");
        // vmin flips to the width on a tall viewport.
        let tall =
            StyleEngine::with_environment(&[css], 500.0, 900.0, &Default::default()).compute(&dom);
        let d = find(&tall, &dom, "div");
        assert_eq!(d.margin.left.resolve(500.0), 50.0, "10vmin of 500");
        // Plain parse (no viewport) rejects viewport units rather than
        // folding to zero.
        assert_eq!(crate::parse_len_px("10vw", 16.0, 16.0), None);
    }

    // ---- R7: @layer ----

    #[test]
    fn layer_order_beats_specificity_and_unlayered_wins() {
        let (dom, _) = parse("<p class='x'>t</p>");
        let css = "@layer base, overrides;\n\
                   @layer base { p.x { color: #0000FF } }\n\
                   p.x { color: #FF0000 }";
        let styles = StyleEngine::with_viewport(&[css], 800.0).compute(&dom);
        let p = find(&styles, &dom, "p");
        // Unlayered beats layered regardless of source order.
        assert_eq!(p.color, 0xFF0000FF, "unlayered wins over layered");

        let css = "@layer base { p.x { color: #0000FF } }\n\
                   @layer overrides { p.x { color: #00FF00 } }";
        let styles = StyleEngine::with_viewport(&[css], 800.0).compute(&dom);
        let p = find(&styles, &dom, "p");
        // Later layer wins even though the specificity is identical and the
        // winning rule came first to the cascade? No — 'overrides' is later.
        assert_eq!(p.color, 0x00FF00FF, "later layer wins among layered");
    }

    #[test]
    fn important_inverts_layer_order() {
        let (dom, _) = parse("<p class='x'>t</p>");
        // Layered !important beats unlayered !important (spec inversion).
        let css = "p.x { color: #0000FF !important }\n\
                   @layer late { p.x { color: #FF0000 !important } }";
        let styles = StyleEngine::with_viewport(&[css], 800.0).compute(&dom);
        let p = find(&styles, &dom, "p");
        assert_eq!(p.color, 0xFF0000FF, "layered important beats unlayered");
    }

    #[test]
    fn min_max_clamp_lengths_store_symbolic_terms() {
        let (dom, styles) = styled(
            "<div style=\"width: min(50%, 300px); max-width: clamp(10px, 80%, 200px)\">x</div>",
            &[],
        );
        let d = find(&styles, &dom, "div");
        assert_eq!(d.width.unwrap().len().unwrap().resolve(400.0), 200.0);
        assert_eq!(d.width.unwrap().len().unwrap().resolve(700.0), 300.0);
        assert_eq!(d.max_width.unwrap().resolve(500.0), 200.0);
        assert_eq!(d.max_width.unwrap().resolve(10.0), 10.0);
    }

    #[test]
    fn display_none_hides_head_bits_and_controls() {
        let (dom, styles) = styled("<style>p{}</style><svg></svg><p>x</p>", &[]);
        assert_eq!(find(&styles, &dom, "style").display, Display::None);
        // R6: inline <svg> renders as a replaced element now.
        assert_eq!(find(&styles, &dom, "svg").display, Display::InlineBlock);
    }

    #[test]
    fn form_controls_are_visible_inline_block() {
        let (dom, styles) = styled(
            "<form><input><button>go</button><select></select><textarea></textarea></form>",
            &[],
        );
        assert_eq!(find(&styles, &dom, "input").display, Display::InlineBlock);
        assert_eq!(find(&styles, &dom, "button").display, Display::InlineBlock);
        assert_eq!(find(&styles, &dom, "form").display, Display::Block);
    }

    #[test]
    fn monospace_detection_and_pre() {
        let (dom, styles) = styled(
            "<code>x</code><pre>y</pre>",
            &["code { font-family: Menlo, monospace }"],
        );
        assert!(find(&styles, &dom, "code").monospace);
        let pre = find(&styles, &dom, "pre");
        assert!(pre.white_space_pre);
        assert_ne!(pre.background, 0);
    }

    #[test]
    fn tables_use_table_display() {
        let (dom, styles) = styled("<table><tr><td>a</td><td>b</td></tr></table>", &[]);
        assert_eq!(find(&styles, &dom, "table").display, Display::Table);
        assert_eq!(find(&styles, &dom, "tr").display, Display::TableRow);
        assert_eq!(find(&styles, &dom, "td").display, Display::TableCell);
        assert_eq!(find(&styles, &dom, "td").padding.left.px(), Some(8.0));
    }

    #[test]
    fn grid_tracks_and_spans_parse() {
        let (dom, styles) = styled(
            "<div id='g' style=\"display:grid;grid-template-columns:100px 1fr auto;gap:8px\"><span id='k' style='grid-column:span 2'>x</span></div>",
            &[],
        );
        let g = by_id(&styles, &dom, "g");
        assert_eq!(g.display, Display::Grid);
        assert_eq!(g.grid_template_columns.len(), 3);
        assert!(matches!(g.grid_template_columns[0], GridTrack::Fixed(_)));
        assert!(
            matches!(g.grid_template_columns[1], GridTrack::Flex(f) if (f - 1.0).abs() < 0.001)
        );
        assert!(matches!(g.grid_template_columns[2], GridTrack::Auto));
        assert_eq!(g.gap.px(), Some(8.0));
        assert_eq!(by_id(&styles, &dom, "k").grid_column_span, 2);
    }

    #[test]
    fn grid_repeat_and_line_spans_parse() {
        let (dom, styles) = styled(
            "<div id='g' style=\"display:grid;grid-template-columns:repeat(3, 1fr)\"><span id='a' style='grid-column:1 / 3'>x</span><span id='b' style='grid-row:span 2'>y</span></div>",
            &[],
        );
        let g = by_id(&styles, &dom, "g");
        assert_eq!(g.grid_template_columns.len(), 3);
        assert!(g
            .grid_template_columns
            .iter()
            .all(|t| matches!(t, GridTrack::Flex(_))));
        assert_eq!(by_id(&styles, &dom, "a").grid_column_span, 2);
        assert_eq!(by_id(&styles, &dom, "b").grid_row_span, 2);
    }

    #[test]
    fn grid_template_areas_parse_rows_and_cells() {
        let (dom, styles) = styled(
            "<div id='g' style=\"display:grid;grid-template-areas:'head head' 'side main' '. main'\"></div>",
            &[],
        );
        let g = by_id(&styles, &dom, "g");
        assert_eq!(g.grid_template_areas.len(), 3);
        assert_eq!(g.grid_template_areas[0], ["head", "head"]);
        assert_eq!(g.grid_template_areas[1], ["side", "main"]);
        assert_eq!(g.grid_template_areas[2], [".", "main"]);
    }

    #[test]
    fn grid_named_lines_parse_before_tracks() {
        let (dom, styles) = styled(
            "<div id='g' style=\"display:grid;grid-template-columns:[main-start sidebar-start] 200px [sidebar-end content-start] 1fr [content-end]\"></div>",
            &[],
        );
        let g = by_id(&styles, &dom, "g");
        assert_eq!(g.grid_template_columns.len(), 2);
        // Line 0 precedes track 0, line 1 precedes track 1.
        assert!(g
            .grid_line_names_col
            .contains(&(0, "main-start".to_string())));
        assert!(g
            .grid_line_names_col
            .contains(&(0, "sidebar-start".to_string())));
        assert!(g
            .grid_line_names_col
            .contains(&(1, "sidebar-end".to_string())));
        assert!(g
            .grid_line_names_col
            .contains(&(1, "content-start".to_string())));
        assert!(g
            .grid_line_names_col
            .contains(&(2, "content-end".to_string())));
    }

    #[test]
    fn grid_area_named_and_slash_forms() {
        let (dom, styles) = styled(
            "<span id='a' style='grid-area:main'></span>\
             <span id='b' style='grid-area:1 / 2 / 3 / 4'></span>\
             <span id='c' style='grid-column-start:content-start;grid-row-end:span 2'></span>",
            &[],
        );
        let a = by_id(&styles, &dom, "a");
        assert_eq!(a.grid_area, "main");
        let b = by_id(&styles, &dom, "b");
        assert_eq!(b.grid_row_start, "1");
        assert_eq!(b.grid_column_start, "2");
        assert_eq!(b.grid_row_end, "3");
        assert_eq!(b.grid_column_end, "4");
        // Integer start→end folds into the span like `grid-column: 1 / 3`.
        assert_eq!(b.grid_row_span, 2);
        assert_eq!(b.grid_column_span, 2);
        let c = by_id(&styles, &dom, "c");
        assert_eq!(c.grid_column_start, "content-start");
        assert_eq!(c.grid_row_span, 2);
    }

    #[test]
    fn conic_gradient_parses_angle_center_stops() {
        let (dom, styles) = styled(
            "<div id='a' style='background:conic-gradient(red, blue)'></div>\
             <div id='b' style='background:conic-gradient(from 90deg at 25% 75%, #ff0000 0%, #0000ff 100%)'></div>",
            &[],
        );
        let pa = &by_id(&styles, &dom, "a").background_paint;
        match pa {
            BackgroundPaint::ConicGradient {
                angle_deg,
                cx,
                cy,
                stops,
            } => {
                assert!((angle_deg).abs() < 0.01, "default from-angle 0");
                assert!((cx - 0.5).abs() < 0.01 && (cy - 0.5).abs() < 0.01);
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[1].color, 0x0000FFFF);
            }
            other => panic!("expected conic paint, got {other:?}"),
        }
        let pb = &by_id(&styles, &dom, "b").background_paint;
        match pb {
            BackgroundPaint::ConicGradient {
                angle_deg,
                cx,
                cy,
                stops,
            } => {
                assert!((angle_deg - 90.0).abs() < 0.01);
                assert!((cx - 0.25).abs() < 0.01 && (cy - 0.75).abs() < 0.01);
                assert_eq!(stops[0].color, 0xFF0000FF);
            }
            other => panic!("expected conic paint, got {other:?}"),
        }
    }

    #[test]
    fn object_fit_parses() {
        let (dom, styles) = styled("<img id='a' style='object-fit:cover'><img id='b'>", &[]);
        assert_eq!(by_id(&styles, &dom, "a").object_fit, ObjectFit::Cover);
        assert_eq!(by_id(&styles, &dom, "b").object_fit, ObjectFit::Fill);
    }

    #[test]
    fn border_radius_shorthand_parses() {
        let (dom, styles) = styled(
            "<div id='a' style='border-radius:4px'></div><div id='b' style='border-radius:2px 6px'></div>",
            &[],
        );
        let r = by_id(&styles, &dom, "a").border_radius;
        assert!(matches!(r.top_left, Length::Px(x) if (x - 4.0).abs() < 0.01));
        assert!(matches!(r.bottom_right, Length::Px(x) if (x - 4.0).abs() < 0.01));
        let r = by_id(&styles, &dom, "b").border_radius;
        assert!(matches!(r.top_left, Length::Px(x) if (x - 2.0).abs() < 0.01));
        assert!(matches!(r.top_right, Length::Px(x) if (x - 6.0).abs() < 0.01));
        assert!(matches!(r.bottom_right, Length::Px(x) if (x - 2.0).abs() < 0.01));
    }

    #[test]
    fn linear_gradient_parses_with_angle_and_stops() {
        let (dom, styles) = styled(
            "<div id='a' style='background:linear-gradient(45deg, red, blue)'></div>\
             <div id='b' style='background:linear-gradient(to right, #ff0000 0%, #0000ff 100%)'></div>\
             <div id='c' style='background:linear-gradient(red, green, blue)'></div>",
            &[],
        );
        match &by_id(&styles, &dom, "a").background_paint {
            BackgroundPaint::LinearGradient { angle_deg, stops } => {
                assert!((angle_deg - 45.0).abs() < 0.01);
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[0].offset, 0.0);
                assert_eq!(stops[1].offset, 1.0);
                assert_eq!(stops[0].color, 0xFF0000FF);
            }
            other => panic!("expected linear gradient, got {other:?}"),
        }
        match &by_id(&styles, &dom, "b").background_paint {
            BackgroundPaint::LinearGradient { angle_deg, stops } => {
                assert!((angle_deg - 90.0).abs() < 0.01, "to right = 90deg");
                assert_eq!(stops[0].color, 0xFF0000FF);
            }
            other => panic!("expected linear gradient, got {other:?}"),
        }
        // CPU-approx fast path carries the first stop.
        assert_eq!(by_id(&styles, &dom, "a").background, 0xFF0000FF);
        // Bare colors distribute evenly across three stops.
        match &by_id(&styles, &dom, "c").background_paint {
            BackgroundPaint::LinearGradient { stops, .. } => {
                assert_eq!(stops.len(), 3);
                assert!((stops[1].offset - 0.5).abs() < 0.01);
            }
            other => panic!("expected linear gradient, got {other:?}"),
        }
    }

    #[test]
    fn radial_gradient_and_image_url_parse() {
        let (dom, styles) = styled(
            "<div id='a' style='background:radial-gradient(circle at 25% 75%, red, blue)'></div>\
             <div id='b' style='background-image:url(\"pic.png\")'></div>",
            &[],
        );
        match &by_id(&styles, &dom, "a").background_paint {
            BackgroundPaint::RadialGradient { cx, cy, stops, .. } => {
                assert!(((cx - 0.25).abs() < 0.01) && ((cy - 0.75).abs() < 0.01));
                assert_eq!(stops.len(), 2);
            }
            other => panic!("expected radial gradient, got {other:?}"),
        }
        assert!(matches!(
            &by_id(&styles, &dom, "b").background_paint,
            BackgroundPaint::Image(u) if u == "pic.png"
        ));
    }

    #[test]
    fn box_shadow_parses_offsets_blur_and_color() {
        let (dom, styles) = styled(
            "<div id='a' style='box-shadow:2px 4px 6px rgba(0,0,0,0.5)'></div>\
             <div id='b' style='box-shadow:inset 0 0 0 2px red, 1px 1px black'></div>",
            &[],
        );
        let shadows = &by_id(&styles, &dom, "a").box_shadows;
        assert_eq!(shadows.len(), 1);
        assert_eq!((shadows[0].offset_x, shadows[0].offset_y), (2.0, 4.0));
        assert_eq!(shadows[0].blur, 6.0);
        assert_eq!(shadows[0].color, 0x00000080);
        assert!(!shadows[0].inset);
        let shadows = &by_id(&styles, &dom, "b").box_shadows;
        assert_eq!(shadows.len(), 2);
        assert!(shadows[0].inset);
        assert_eq!(shadows[0].spread, 2.0);
        assert_eq!(shadows[0].color, 0xFF0000FF);
    }

    #[test]
    fn direction_parses_inherits_and_drives_start_end() {
        let (dom, styles) = styled(
            "<div id='a' style='direction:rtl;text-align:start'><span id='b'>x</span></div>\
             <div id='c' style='text-align:end'>y</div>",
            &[],
        );
        let a = by_id(&styles, &dom, "a");
        assert_eq!(a.direction, Direction::Rtl);
        assert_eq!(a.text_align, TextAlign::Right, "start in RTL");
        assert_eq!(
            by_id(&styles, &dom, "b").direction,
            Direction::Rtl,
            "inherited"
        );
        assert_eq!(
            by_id(&styles, &dom, "c").text_align,
            TextAlign::Right,
            "end in LTR"
        );
        // dir attribute flows through the UA sheet.
        let (dom, styles) = styled("<p id='d' dir='rtl'>x</p>", &[]);
        assert_eq!(by_id(&styles, &dom, "d").direction, Direction::Rtl);
    }

    #[test]
    fn vertical_align_parses_all_forms() {
        let (dom, styles) = styled(
            "<span id='a' style='vertical-align:super'>x</span>\
             <span id='b' style='vertical-align:sub'>x</span>\
             <span id='c' style='vertical-align:50%'>x</span>\
             <span id='d' style='vertical-align:3px'>x</span>\
             <span id='e' style='vertical-align:middle'>x</span>\
             <span id='f'>x</span>",
            &[],
        );
        assert_eq!(
            by_id(&styles, &dom, "a").vertical_align,
            VerticalAlign::Super
        );
        assert_eq!(by_id(&styles, &dom, "b").vertical_align, VerticalAlign::Sub);
        assert_eq!(
            by_id(&styles, &dom, "c").vertical_align,
            VerticalAlign::Percent(0.5)
        );
        assert!(matches!(
            by_id(&styles, &dom, "d").vertical_align,
            VerticalAlign::Length(_)
        ));
        assert_eq!(
            by_id(&styles, &dom, "e").vertical_align,
            VerticalAlign::Baseline
        );
        assert_eq!(
            by_id(&styles, &dom, "f").vertical_align,
            VerticalAlign::Baseline
        );
    }

    #[test]
    fn clear_parses_sides() {
        let (dom, styles) = styled(
            "<div id='a' style='clear:left'></div><div id='b' style='clear:both'></div><div id='c'></div>",
            &[],
        );
        assert_eq!(by_id(&styles, &dom, "a").clear, Clear::Left);
        assert_eq!(by_id(&styles, &dom, "b").clear, Clear::Both);
        assert_eq!(by_id(&styles, &dom, "c").clear, Clear::None);
    }

    #[test]
    fn background_first_layer_wins() {
        let (dom, styles) = styled(
            "<div id='a' style='background:linear-gradient(red, blue), url(x.png)'></div>\
             <div id='b' style='background:url(y.png)'></div>",
            &[],
        );
        assert!(
            matches!(
                by_id(&styles, &dom, "a").background_paint,
                BackgroundPaint::LinearGradient { .. }
            ),
            "first layer paints"
        );
        assert_eq!(
            by_id(&styles, &dom, "a").background,
            0xFF0000FF,
            "first-stop approx"
        );
        assert!(matches!(
            &by_id(&styles, &dom, "b").background_paint,
            BackgroundPaint::Image(u) if u == "y.png"
        ));
        // The whole list survives in CSS order, and the scalars mirror the
        // topmost layer.
        let a = by_id(&styles, &dom, "a");
        assert_eq!(a.background_layers.len(), 2);
        assert!(a.background_layers[0].is_gradient());
        assert_eq!(a.background_layers[1].image_src(), Some("x.png"));
        let b = by_id(&styles, &dom, "b");
        assert_eq!(b.background_layers.len(), 1);
        assert_eq!(b.background_layers[0].image_src(), Some("y.png"));
    }

    /// The `background` shorthand takes a comma-separated layer list; each
    /// layer keeps its own image, color and placement (CSS Backgrounds 3 §3).
    #[test]
    fn background_shorthand_layer_list_keeps_each_layers_own_values() {
        let (dom, styles) = styled(
            "<div id='a' style='background: url(a.png) no-repeat right 10px bottom 20px / 30px 40px              padding-box, linear-gradient(red, blue) content-box, #0f0'></div>\
             <div id='b'></div>",
            &[],
        );
        let a = by_id(&styles, &dom, "a");
        let layers = &a.background_layers;
        assert_eq!(layers.len(), 3, "{layers:#?}");
        // Layer 0: its own image, position, size, repeat and origin.
        assert_eq!(layers[0].image_src(), Some("a.png"));
        assert_eq!(layers[0].position.x, BgPos::Inset(Length::Px(10.0)));
        assert_eq!(layers[0].position.y, BgPos::Inset(Length::Px(20.0)));
        assert_eq!(
            layers[0].size,
            BackgroundSize::Axes(
                BgSize::Length(Length::Px(30.0)),
                BgSize::Length(Length::Px(40.0))
            )
        );
        assert_eq!(layers[0].repeat.x, BgRepeat::NoRepeat);
        assert_eq!(layers[0].repeat.y, BgRepeat::NoRepeat);
        assert_eq!(layers[0].origin, BgBox::PaddingBox);
        assert_eq!(
            layers[0].clip,
            BgBox::PaddingBox,
            "a lone box keyword sets both"
        );
        assert_eq!(
            layers[0].color, 0,
            "an image layer with no color is transparent"
        );
        // Layer 1: a gradient whose color is its first stop, clipped to the
        // content box, with the default placement.
        assert!(layers[1].is_gradient());
        assert_eq!(layers[1].color, 0xFF0000FF);
        assert_eq!(layers[1].origin, BgBox::ContentBox);
        assert_eq!(layers[1].clip, BgBox::ContentBox);
        assert_eq!(layers[1].position, BackgroundPosition::default());
        assert_eq!(layers[1].size, BackgroundSize::Auto);
        assert_eq!(layers[1].repeat, BackgroundRepeat::default());
        // Layer 2: the final layer is just a color.
        assert_eq!(layers[2].paint, BackgroundPaint::Color(0x00FF00FF));
        assert_eq!(layers[2].color, 0x00FF00FF);
        // The scalar fast path mirrors the TOP layer.
        assert_eq!(a.background_position, layers[0].position);
        assert_eq!(a.background_size, layers[0].size);
        assert_eq!(a.background_repeat, layers[0].repeat);
        assert_eq!(a.background, 0, "the top image layer has no solid color");
        assert!(matches!(&a.background_paint, BackgroundPaint::Image(u) if u == "a.png"));
        // A style no background declaration touched keeps the empty list:
        // one initial layer, no allocation, and the scalar defaults.
        let b = by_id(&styles, &dom, "b");
        assert!(b.background_layers.is_empty());
        assert_eq!(b.background_paint, BackgroundPaint::Color(0));
        assert_eq!(b.background_position, BackgroundPosition::default());
    }

    /// `background-origin` / `background-clip`: the per-layer box keywords,
    /// as a longhand (every layer) or inside a `background` layer.
    #[test]
    fn background_origin_and_clip_pick_the_paint_box() {
        let (dom, styles) = styled(
            "<div id='a' style='background: #111, #222 content-box, #333 padding-box, \
             #444 content-box padding-box'></div>\
             <div id='b' style='background:url(x.png) border-box'></div>\
             <div id='c' style='background-origin: content-box; background-clip: padding-box'></div>\
             <div id='d' style='background-image: url(a.png), url(b.png); \
             background-origin: content-box, padding-box'></div>",
            &[],
        );
        let a = by_id(&styles, &dom, "a").background_layers.clone();
        assert_eq!(a[0].origin, BgBox::BorderBox);
        assert_eq!(a[0].clip, BgBox::BorderBox);
        // One `<box>` sets origin AND clip; two set them in order.
        assert_eq!(
            a[1].origin,
            BgBox::ContentBox,
            "a lone box keyword sets both"
        );
        assert_eq!(a[1].clip, BgBox::ContentBox);
        assert_eq!(a[2].origin, BgBox::PaddingBox);
        assert_eq!(a[2].clip, BgBox::PaddingBox);
        assert_eq!(
            a[3].origin,
            BgBox::ContentBox,
            "the first box is the origin"
        );
        assert_eq!(a[3].clip, BgBox::PaddingBox, "the second is the clip");
        // `border-box` in a layer is the initial value of both.
        let b = by_id(&styles, &dom, "b").background_layers[0].clone();
        assert_eq!(b.origin, BgBox::BorderBox);
        assert_eq!(b.clip, BgBox::BorderBox);
        // The longhands write every layer, and do not leak into the scalars.
        let c = by_id(&styles, &dom, "c").background_layers.clone();
        assert_eq!(c[0].origin, BgBox::ContentBox);
        assert_eq!(c[0].clip, BgBox::PaddingBox);
        // A per-layer longhand list assigns one box per layer — the image
        // list owns the layer count (CSS Backgrounds 3 §2.1).
        let d = by_id(&styles, &dom, "d").background_layers.clone();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].origin, BgBox::ContentBox);
        assert_eq!(d[1].origin, BgBox::PaddingBox);
    }

    /// `background-image` is a list too, and (per spec) sets the image of
    /// every layer — so its length replaces the layer count.
    #[test]
    fn background_image_longhand_sets_every_layer() {
        let (dom, styles) = styled(
            "<div id='a' style='background-position: 5px; background-image: url(a.png), url(b.png)              no-repeat'></div>\
             <div id='b' style='background-image: url(c.png)'></div>\
             <div id='c' style='background-image: none'></div>",
            &[],
        );
        let a = by_id(&styles, &dom, "a").background_layers.clone();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].image_src(), Some("a.png"));
        assert_eq!(a[1].image_src(), Some("b.png"));
        // The cascaded position survived (this engine's shorthand merge) and
        // the scalar mirrors the top layer.
        assert_eq!(a[0].position.x, BgPos::Offset(Length::Px(5.0)));
        assert_eq!(a[1].position.x, BgPos::Offset(Length::Px(5.0)));
        assert_eq!(
            by_id(&styles, &dom, "a").background_position.x,
            BgPos::Offset(Length::Px(5.0))
        );
        let b = by_id(&styles, &dom, "b").background_layers.clone();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].image_src(), Some("c.png"));
        // `none` clears the image and keeps the color.
        let c = by_id(&styles, &dom, "c");
        assert_eq!(c.background_layers[0].image_src(), None);
        assert_eq!(c.background, 0);
    }

    /// A comma inside a function is not a layer separator, and one bad layer
    /// invalidates the whole declaration.
    #[test]
    fn background_layer_commas_and_invalid_layers() {
        let (dom, styles) = styled(
            "<div id='a' style='background: rgba(1, 2, 3, 0.5), url(a.png)'></div>\
             <div id='b' style='background: #123456, bogus'></div>",
            &[],
        );
        let a = by_id(&styles, &dom, "a").background_layers.clone();
        assert_eq!(a.len(), 2, "the rgba() comma stays inside the layer");
        assert_eq!(a[0].paint, BackgroundPaint::Color(0x01020380));
        assert_eq!(a[1].image_src(), Some("a.png"));
        // The invalid layer leaves the cascaded (initial) value alone.
        let b = by_id(&styles, &dom, "b");
        assert!(b.background_layers.is_empty());
        assert_eq!(b.background, 0);
    }

    /// `visibility` is inherited, and a descendant can turn painting back
    /// on inside a hidden subtree (CSS Visibility 3 §2.1).
    #[test]
    fn visibility_is_inherited_and_overridable() {
        let (dom, styles) = styled(
            "<div id='p' style='visibility:hidden'><span id='c'>text</span>\
             <span id='v' style='visibility:visible'>shown</span></div>\
             <div id='q' style='visibility:collapse'></div>\
             <div id='d'></div>",
            &[],
        );
        assert_eq!(by_id(&styles, &dom, "p").visibility, Visibility::Hidden);
        assert_eq!(
            by_id(&styles, &dom, "c").visibility,
            Visibility::Hidden,
            "inherited by descendants"
        );
        assert_eq!(
            by_id(&styles, &dom, "v").visibility,
            Visibility::Visible,
            "a descendant re-enables painting"
        );
        assert_eq!(by_id(&styles, &dom, "q").visibility, Visibility::Collapse);
        assert_eq!(
            by_id(&styles, &dom, "d").visibility,
            Visibility::Visible,
            "initial value"
        );
        // A selector cascade + css-wide keywords drive it like any other
        // inherited property.
        let (dom, styles) = styled(
            "<div id='p'><i id='c'>t</i></div>",
            &["div { visibility: hidden } i { visibility: visible }"],
        );
        assert_eq!(by_id(&styles, &dom, "c").visibility, Visibility::Visible);
        let (dom, styles) = styled(
            "<div id='p' style='visibility:hidden'><i id='c'>t</i></div>",
            &["i { visibility: initial }"],
        );
        assert_eq!(
            by_id(&styles, &dom, "c").visibility,
            Visibility::Visible,
            "`initial` beats the inherited value"
        );
        let (dom, styles) = styled(
            "<div id='p' style='visibility:hidden'><i id='c'>t</i></div>",
            &["i { visibility: inherit }"],
        );
        assert_eq!(
            by_id(&styles, &dom, "c").visibility,
            Visibility::Hidden,
            "`inherit` keeps it hidden"
        );
        // An unknown keyword is ignored, not treated as a reset.
        let (dom, styles) = styled(
            "<div id='p' style='visibility:hidden'></div>",
            &["div { visibility: ghost }"],
        );
        assert_eq!(by_id(&styles, &dom, "p").visibility, Visibility::Hidden);
    }

    #[test]
    fn background_position_parses_the_css_grammar() {
        let p = |v: &str| parse_background_position(v).expect(v);
        // One value: an edge keyword centers the other axis, a length is
        // horizontal with a centered vertical.
        assert_eq!(p("left").x, BgPos::Start);
        assert_eq!(p("left").y, BgPos::Center);
        assert_eq!(p("top").x, BgPos::Center);
        assert_eq!(p("top").y, BgPos::Start);
        assert_eq!(p("bottom").x, BgPos::Center);
        assert_eq!(p("bottom").y, BgPos::End);
        assert_eq!(p("center").x, BgPos::Center);
        assert_eq!(p("center").y, BgPos::Center);
        assert_eq!(p("25px").x, BgPos::Offset(Length::Px(25.0)));
        assert_eq!(p("25px").y, BgPos::Center);
        assert_eq!(p("25%").x, BgPos::Percent(25.0));
        // Two values: horizontal then vertical, `top left` included.
        assert_eq!(p("right bottom").x, BgPos::End);
        assert_eq!(p("right bottom").y, BgPos::End);
        assert_eq!(p("bottom right").x, BgPos::End);
        assert_eq!(p("bottom right").y, BgPos::End);
        assert_eq!(p("10px 20px").x, BgPos::Offset(Length::Px(10.0)));
        assert_eq!(p("10px 20px").y, BgPos::Offset(Length::Px(20.0)));
        assert_eq!(p("center top").x, BgPos::Center);
        assert_eq!(p("center top").y, BgPos::Start);
        assert_eq!(p("2em 50%").x, BgPos::Offset(Length::Em(2.0)));
        assert_eq!(p("2em 50%").y, BgPos::Percent(50.0));
        // The 4-value edge/offset form, either order. An offset beside
        // `right`/`bottom` is measured inward from that edge (`Inset`),
        // beside `left`/`top` from the start (`Offset`).
        let q = p("right 10px bottom 20px");
        assert_eq!(q.x, BgPos::Inset(Length::Px(10.0)));
        assert_eq!(q.y, BgPos::Inset(Length::Px(20.0)));
        let q = p("bottom 20px right 10px");
        assert_eq!(q.x, BgPos::Inset(Length::Px(10.0)));
        assert_eq!(q.y, BgPos::Inset(Length::Px(20.0)));
        let q = p("left 10px top 20px");
        assert_eq!(q.x, BgPos::Offset(Length::Px(10.0)));
        assert_eq!(q.y, BgPos::Offset(Length::Px(20.0)));
        // A percentage beside an edge keyword complements into the free
        // space (`right 10%` == `left 90%`).
        let q = p("right 10% bottom 0%");
        assert_eq!(q.x, BgPos::Percent(90.0));
        assert_eq!(q.y, BgPos::Percent(100.0));
        // A lone trailing keyword keeps its own axis (an edge keyword
        // normalizes to the same percentage of the free space).
        let q = p("left 10px bottom");
        assert_eq!(q.x, BgPos::Offset(Length::Px(10.0)));
        assert_eq!(q.y, BgPos::Percent(100.0));
        // The 2-value edge+offset form, which the CSS grammar allows: the
        // named axis takes the offset, the other centers.
        let q = p("right 10px");
        assert_eq!(q.x, BgPos::Inset(Length::Px(10.0)));
        assert_eq!(q.y, BgPos::Center);
        let q = p("bottom 20px");
        assert_eq!(q.x, BgPos::Center);
        assert_eq!(q.y, BgPos::Inset(Length::Px(20.0)));
        // Case-insensitive; invalid values are rejected (the caller keeps
        // the cascaded value).
        assert_eq!(p("CENTER TOP").x, BgPos::Center);
        assert_eq!(p("CENTER TOP").y, BgPos::Start);
        assert_eq!(p("LEFT CENTER").x, BgPos::Start);
        assert_eq!(p("LEFT CENTER").y, BgPos::Center);
        assert!(parse_background_position("").is_none());
        assert!(parse_background_position("nonsense").is_none());
        assert!(parse_background_position("10px center left").is_none());
        assert!(parse_background_position("left left").is_none());
        assert!(parse_background_position("10px 20px 30px 40px 50px").is_none());
    }

    #[test]
    fn background_size_and_repeat_parse() {
        let s = |v: &str| parse_background_size(v).expect(v);
        assert_eq!(s("auto"), BackgroundSize::Auto);
        assert_eq!(s("cover"), BackgroundSize::Keyword(BgSizeKeyword::Cover));
        assert_eq!(
            s("contain"),
            BackgroundSize::Keyword(BgSizeKeyword::Contain)
        );
        assert_eq!(
            s("50%"),
            BackgroundSize::Axes(BgSize::Percent(50.0), BgSize::Auto)
        );
        assert_eq!(
            s("30px 40px"),
            BackgroundSize::Axes(
                BgSize::Length(Length::Px(30.0)),
                BgSize::Length(Length::Px(40.0))
            )
        );
        assert_eq!(
            s("50% auto"),
            BackgroundSize::Axes(BgSize::Percent(50.0), BgSize::Auto)
        );
        assert_eq!(
            s("AUTO 10PX"),
            BackgroundSize::Axes(BgSize::Auto, BgSize::Length(Length::Px(10.0)))
        );
        assert!(parse_background_size("enormous").is_none());
        assert!(parse_background_size("10px 20px 30px").is_none());
        assert!(parse_background_size("cover 10px").is_none());

        let r = |v: &str| parse_background_repeat(v).expect(v);
        let both = |x: BgRepeat, y: BgRepeat| BackgroundRepeat { x, y };
        assert_eq!(r("repeat"), both(BgRepeat::Repeat, BgRepeat::Repeat));
        assert_eq!(r("no-repeat"), both(BgRepeat::NoRepeat, BgRepeat::NoRepeat));
        assert_eq!(r("REPEAT-X"), both(BgRepeat::Repeat, BgRepeat::NoRepeat));
        assert_eq!(r("repeat-y"), both(BgRepeat::NoRepeat, BgRepeat::Repeat));
        assert_eq!(r("space"), both(BgRepeat::Space, BgRepeat::Space));
        assert_eq!(r("round"), both(BgRepeat::Round, BgRepeat::Round));
        assert_eq!(
            r("repeat no-repeat"),
            both(BgRepeat::Repeat, BgRepeat::NoRepeat)
        );
        assert!(parse_background_repeat("diagonal").is_none());
        assert!(parse_background_repeat("repeat repeat repeat").is_none());
    }

    #[test]
    fn background_longhands_and_shorthand_cascade() {
        let (dom, styles) = styled(
            "<div id='a' style='background-position: right 10px bottom 20px; \
                    background-size: cover; background-repeat: repeat-x'></div>\
             <div id='b'></div>\
             <div id='c'></div>",
            &[
                "#b { background: url(t.png) no-repeat center center / 50% 25% }",
                "#c { background: url(u.png) round space }",
            ],
        );
        let a = by_id(&styles, &dom, "a");
        assert_eq!(a.background_position.x, BgPos::Inset(Length::Px(10.0)));
        assert_eq!(a.background_position.y, BgPos::Inset(Length::Px(20.0)));
        assert_eq!(
            a.background_size,
            BackgroundSize::Keyword(BgSizeKeyword::Cover)
        );
        assert_eq!(a.background_repeat.x, BgRepeat::Repeat);
        assert_eq!(a.background_repeat.y, BgRepeat::NoRepeat);

        // The shorthand carries position/size/repeat for the layer, and the
        // paint still comes from the same value.
        let b = by_id(&styles, &dom, "b");
        assert!(matches!(&b.background_paint, BackgroundPaint::Image(u) if u == "t.png"));
        assert_eq!(b.background_position.x, BgPos::Center);
        assert_eq!(b.background_position.y, BgPos::Center);
        assert_eq!(
            b.background_size,
            BackgroundSize::Axes(BgSize::Percent(50.0), BgSize::Percent(25.0))
        );
        assert_eq!(b.background_repeat.x, BgRepeat::NoRepeat);
        assert_eq!(b.background_repeat.y, BgRepeat::NoRepeat);

        let c = by_id(&styles, &dom, "c");
        assert_eq!(c.background_repeat.x, BgRepeat::Round);
        assert_eq!(c.background_repeat.y, BgRepeat::Space);

        // A shorthand layer with a `url()` and a bare color keeps working.
        let (dom, styles) = styled(
            "<div id='a'></div>",
            &["#a { background: #123456 url(x.png) }"],
        );
        let a = by_id(&styles, &dom, "a");
        assert!(matches!(&a.background_paint, BackgroundPaint::Image(u) if u == "x.png"));
        // The background longhands are NOT inherited.
        let (dom, styles) = styled(
            "<div id='p' style='background-size:cover'><span id='c'>t</span></div>",
            &[],
        );
        assert_eq!(
            by_id(&styles, &dom, "c").background_size,
            BackgroundSize::Auto,
            "background-* box properties do not inherit"
        );
        // `all: initial` resets them along with everything else.
        let (dom, styles) = styled(
            "<div id='a' style='all: initial'></div>",
            &["div { background-size: cover; visibility: hidden }"],
        );
        assert_eq!(
            by_id(&styles, &dom, "a").background_size,
            BackgroundSize::Auto
        );
        assert_eq!(by_id(&styles, &dom, "a").visibility, Visibility::Visible);
    }

    #[test]
    fn engine_collects_font_faces_from_sheets() {
        let engine = StyleEngine::new(&[
            "@font-face { font-family: A; src: url(a.woff2); }",
            "p { color: red }",
            "@font-face { font-family: B; src: url(b.ttf); }",
        ]);
        let faces = engine.font_faces();
        assert_eq!(faces.len(), 2);
        assert_eq!(faces[0].family, "A");
        assert_eq!(faces[1].srcs[0].url, "b.ttf");
    }

    /// B11: faces of one family stay distinct and the selector picks the
    /// face a run's weight/style asks for.
    #[test]
    fn engine_selects_font_face_by_weight_and_style() {
        let engine = StyleEngine::new(&[
            "@font-face { font-family: Duplex; src: url(reg.ttf); font-weight: 400; }",
            "@font-face { font-family: Duplex; src: url(bold.ttf); font-weight: 700; }",
            "@font-face { font-family: Duplex; src: url(ital.ttf); font-weight: 400; font-style: italic; }",
            "@font-face { font-family: Lone; src: url(lone.ttf); font-weight: 700; }",
        ]);
        assert_eq!(engine.font_faces().len(), 4, "every face kept");
        let reg = engine.font_face_for("Duplex", 400, false).unwrap();
        assert_eq!(reg.font_weight, 400);
        assert_eq!(reg.srcs[0].url, "reg.ttf");
        let bold = engine.font_face_for("Duplex", 700, false).unwrap();
        assert_eq!(bold.srcs[0].url, "bold.ttf");
        // Family lookup is case-insensitive, like the rest of the cascade.
        assert_eq!(
            engine
                .font_face_for("duplex", 400, false)
                .map(|f| f.font_weight),
            Some(400)
        );
        // An italic run takes the italic face; an oblique-style request has
        // none declared, so the italic face stands in.
        let ital = engine.font_face_for("Duplex", 400, true).unwrap();
        assert_eq!(ital.font_style, kitae_css::FontStyle::Italic);
        assert_eq!(ital.srcs[0].url, "ital.ttf");
        assert_eq!(
            engine
                .font_face_for("Duplex", 700, true)
                .map(|f| f.srcs[0].url.as_str()),
            Some("ital.ttf")
        );
        // Between 400 and 700 the nearer face wins.
        assert_eq!(
            engine
                .font_face_for("Duplex", 500, false)
                .map(|f| f.srcs[0].url.as_str()),
            Some("reg.ttf")
        );
        // A family declared at a single weight still serves other weights
        // (matching falls back inside the family instead of dropping the
        // family entirely).
        assert_eq!(
            engine
                .font_face_for("Lone", 400, false)
                .map(|f| f.font_weight),
            Some(700)
        );
        // Undeclared family → no face (the chain falls through).
        assert!(engine.font_face_for("Missing", 400, false).is_none());
    }

    fn styled_state(
        html: &str,
        author: &[&str],
        state: &InteractionState,
    ) -> (Dom, Vec<(NodeId, ComputedStyle)>) {
        let (dom, _) = parse(html);
        let engine = StyleEngine::new(author);
        let styles = engine.compute_with_state(&dom, state);
        (dom, styles)
    }

    #[test]
    fn hover_active_focus_match_state() {
        let html = "<a id='l' href='/x'>go</a><button id='b'>ok</button>";
        let author = [
            "a:hover { color: #ff0000 }",
            "button:active { color: #00ff00 }",
            "a:focus { color: #0000ff }",
        ];
        // Resting state: UA colors only.
        let (dom, styles) = styled_state(html, &author, &InteractionState::default());
        assert_eq!(find(&styles, &dom, "a").color, 0x0B57D0FF);
        // Hover the link.
        let link = dom.get_element_by_id("l").unwrap();
        let state = InteractionState {
            hover: Some(link),
            ..Default::default()
        };
        let (dom, styles) = styled_state(html, &author, &state);
        assert_eq!(by_id(&styles, &dom, "l").color, 0xFF0000FF);
        assert_eq!(
            by_id(&styles, &dom, "b").color,
            0x1A1D23FF,
            "button untouched"
        );
        // Active button + focused link.
        let button = dom.get_element_by_id("b").unwrap();
        let state = InteractionState {
            active: Some(button),
            focused: Some(link),
            ..Default::default()
        };
        let (dom, styles) = styled_state(html, &author, &state);
        assert_eq!(by_id(&styles, &dom, "b").color, 0x00FF00FF);
        assert_eq!(by_id(&styles, &dom, "l").color, 0x0000FFFF);
    }

    #[test]
    fn checked_disabled_and_nth_child_match() {
        let html = "<ul><li id='a'>1</li><li id='b'>2</li><li id='c'>3</li></ul>\
                    <input id='q' type='checkbox' checked><input id='w' disabled>";
        let author = [
            "li:nth-child(2) { color: #123456 }",
            "li:nth-child(odd) { font-weight: 700 }",
            "input:checked { color: #ff0000 }",
            "input:disabled { color: #0000ff }",
            "input:enabled { color: #00ff00 }",
        ];
        let (dom, styles) = styled_state(html, &author, &InteractionState::default());
        assert_eq!(by_id(&styles, &dom, "b").color, 0x123456FF);
        assert_eq!(by_id(&styles, &dom, "a").font_weight, 700);
        assert_eq!(by_id(&styles, &dom, "c").font_weight, 700);
        assert_eq!(by_id(&styles, &dom, "b").font_weight, 400);
        // q matches :checked AND :enabled; :not's inner attr specificity
        // puts :enabled first — both match, enabled wins.
        assert_eq!(by_id(&styles, &dom, "q").color, 0x00FF00FF);
        assert_eq!(by_id(&styles, &dom, "w").color, 0x0000FFFF);
    }

    #[test]
    fn d3_lang_pseudo_and_style_carry_language() {
        // D3: :lang() matches the nearest lang/xml:lang up the ancestor
        // chain, subtag-wise and case-insensitively; ComputedStyle.lang
        // carries the resolved language for hyphenation (D5) and font
        // fallback.
        let html = "<html lang='EN-US'><body>\
                    <p id='plain'>x</p>\
                    <p id='de' lang='de-DE'>x</p>\
                    <section id='zh' xml:lang='zh-Hans-CN'><p id='inner'>x</p></section>\
                    <p id='empty' lang=''>x</p>\
                    </body></html>";
        let author = [
            "p:lang(en) { color: #FF0000 }",
            "p:lang(de) { color: #00FF00 }",
            "p:lang(zh-hans) { color: #0000FF }",
            "#plain { font-family: Custom }",
        ];
        let (dom, styles) = styled_state(html, &author, &InteractionState::default());
        // `en` matches `en-US` (subtag extension), case-insensitively.
        assert_eq!(by_id(&styles, &dom, "plain").color, 0xFF0000FF);
        assert_eq!(by_id(&styles, &dom, "plain").lang.as_deref(), Some("en-us"));
        // The nearest ancestor's lang wins; the de paragraph stopped matching.
        assert_eq!(by_id(&styles, &dom, "de").color, 0x00FF00FF);
        assert_eq!(by_id(&styles, &dom, "de").lang.as_deref(), Some("de-de"));
        // xml:lang participates; the child inherits it.
        assert_eq!(by_id(&styles, &dom, "inner").color, 0x0000FFFF);
        assert_eq!(
            by_id(&styles, &dom, "inner").lang.as_deref(),
            Some("zh-hans-cn")
        );
        // zh-hans is a prefix of zh-hans-cn but NOT of plain zh… and the
        // empty lang is treated as absent (inherits en-us).
        assert_eq!(by_id(&styles, &dom, "empty").lang.as_deref(), Some("en-us"));
        // `zh-hans` does NOT match a bare `zh` element language.
        let author2 = ["p:lang(zh-hans) { color: #0000FF }"];
        let (dom2, styles2) = styled_state(
            "<div lang='zh'><p id='z'>x</p></div><div lang='zh-hans'><p id='h'>x</p></div>",
            &author2,
            &InteractionState::default(),
        );
        assert_eq!(
            by_id(&styles2, &dom2, "z").color,
            0x1A1D23FF,
            "zh ≠ zh-hans"
        );
        assert_eq!(by_id(&styles2, &dom2, "h").color, 0x0000FFFF);
        // No lang anywhere → None.
        let author3 = ["p:lang(en) { color: #FF0000 }"];
        let (dom3, styles3) =
            styled_state("<p id='n'>x</p>", &author3, &InteractionState::default());
        assert_eq!(by_id(&styles3, &dom3, "n").lang, None);
        assert_eq!(by_id(&styles3, &dom3, "n").color, 0x1A1D23FF);
    }

    #[test]
    fn d4_font_features_parse_and_inherit() {
        // D4: `font-feature-settings` tags + values, `font-variant*`
        // keywords, inheritance down the tree, and the reset forms.
        let html = "<body><p id='a'>x<span id='b'>y</span></p>\
                    <p id='c'>z</p><p id='d'>w</p><p id='e'>v</p><p id='g'>u</p></body>";
        let author = [
            // Tag alone / quoted = on; explicit 0 / off = off; numeric
            // values pass through (clamped).
            "#a { font-feature-settings: \"liga\", dlig 1, 'kern' 0 }",
            "#c { font-feature-settings: ss02 3 }",
            // Shorthand spreads across the axes; small-caps also sets the
            // synthesis flag.
            "#d { font-variant: small-caps no-common-ligatures }",
            "#e { font-variant-caps: all-petite-caps; font-variant-ligatures: none }",
            // `normal` on settings clears the list entirely.
            "#g { font-feature-settings: normal }",
        ];
        let (dom, styles) = styled_state(html, &author, &InteractionState::default());
        let a = by_id(&styles, &dom, "a");
        assert_eq!(
            a.font_features,
            vec![
                ("liga".to_string(), 1),
                ("dlig".to_string(), 1),
                ("kern".to_string(), 0),
            ]
        );
        // Inheritance: the span carries the parent's full list.
        assert_eq!(by_id(&styles, &dom, "b").font_features, a.font_features);
        let c = by_id(&styles, &dom, "c");
        assert_eq!(c.font_features, vec![("ss02".to_string(), 3)]);
        // font-variant fills the variant vector, not the settings one.
        let d = by_id(&styles, &dom, "d");
        assert!(d.small_caps);
        assert!(d.font_features.is_empty());
        assert!(d
            .variant_features
            .iter()
            .any(|(t, v)| t == "smcp" && *v == 1));
        assert!(d
            .variant_features
            .iter()
            .any(|(t, v)| t == "liga" && *v == 0));
        // all-petite-caps → pcap + small_caps; `none` forces all five
        // ligature families off.
        let e = by_id(&styles, &dom, "e");
        assert!(e.small_caps);
        assert!(e.variant_features.iter().any(|(t, _)| t == "pcap"));
        for t in ["liga", "clig", "dlig", "hlig", "calt"] {
            assert!(
                e.variant_features.iter().any(|(ft, v)| ft == t && *v == 0),
                "{t} forced off by font-variant-ligatures: none"
            );
        }
        // `normal` clears the list entirely; a plain element (no rules)
        // starts empty with small_caps false.
        assert!(by_id(&styles, &dom, "g").font_features.is_empty());
        assert!(!by_id(&styles, &dom, "c").small_caps);
        assert!(by_id(&styles, &dom, "c").variant_features.is_empty());
    }

    #[test]
    fn not_hover_composes() {
        let html = "<a id='l' href='/x'>go</a>";
        let author = ["a:not(:hover) { color: #123456 }"];
        let (dom, styles) = styled_state(html, &author, &InteractionState::default());
        assert_eq!(by_id(&styles, &dom, "l").color, 0x123456FF);
        let link = dom.get_element_by_id("l").unwrap();
        let state = InteractionState {
            hover: Some(link),
            ..Default::default()
        };
        let (dom, styles) = styled_state(html, &author, &state);
        assert_eq!(
            by_id(&styles, &dom, "l").color,
            0x0B57D0FF,
            "hovered: :not(:hover) drops"
        );
    }

    #[test]
    fn is_and_where_match_any_list_argument() {
        let html = "<p id='a'>one</p><p class='big' id='b'>two</p><span id='c'>s</span>";
        let author = [
            ":is(p, .big) { color: #123456 }",
            ":where(span, .big) { color: #ff00ff }",
        ];
        let (dom, styles) = styled(html, &author);
        // a matches via tag p; b via .big; c matches :where via span.
        assert_eq!(by_id(&styles, &dom, "a").color, 0x123456FF);
        assert_eq!(
            by_id(&styles, &dom, "b").color,
            0x123456FF,
            ":is wins the tie (later rule)"
        );
        assert_eq!(by_id(&styles, &dom, "c").color, 0xFF00FFFF);

        // Arguments may carry combinators and pseudo-classes.
        let html = "<div><span id='d'>x</span></div><em id='e'>y</em>";
        let author = [":is(div > span, em:hover) { color: #0000ff }"];
        let (dom, styles) = styled_state(html, &author, &InteractionState::default());
        assert_eq!(
            by_id(&styles, &dom, "d").color,
            0x0000FFFF,
            "Child combinator inside :is"
        );
        assert_eq!(
            by_id(&styles, &dom, "e").color,
            0x1A1D23FF,
            "unhovered em: no match"
        );
    }

    #[test]
    fn is_takes_most_specific_argument_where_takes_none() {
        let html = "<p id='a' class='k'>x</p>";
        // :is(#z, p) on tag p contributes the #z id bucket → beats .k (0,1,0).
        let author = ["p.k { color: #111111 }", ":is(#z, p) { color: #222222 }"];
        let (dom, styles) = styled(html, &author);
        assert_eq!(
            by_id(&styles, &dom, "a").color,
            0x222222FF,
            ":is = most specific arg"
        );

        // Same shape with :where contributes (0,0,0) → the class rule wins
        // even though it comes first.
        let author = ["p.k { color: #111111 }", ":where(#z, p) { color: #222222 }"];
        let (dom, styles) = styled(html, &author);
        assert_eq!(
            by_id(&styles, &dom, "a").color,
            0x111111FF,
            ":where = zero specificity"
        );
    }

    #[test]
    fn nth_of_type_family_matches_against_tag_groups() {
        let html = "<h1 id='h'>t</h1><p id='p1'>1</p><span id='s1'>a</span>\
                    <p id='p2'>2</p><span id='s2'>b</span><p id='p3'>3</p>";
        let author = [
            "p:nth-of-type(2) { color: #123456 }",
            "p:nth-of-type(odd) { font-weight: 700 }",
            "h1:first-of-type { color: #0000ff }",
            "span:last-of-type { color: #00ff00 }",
            "h1:only-of-type { font-style: italic }",
        ];
        let (dom, styles) = styled(html, &author);
        // of-type counts per tag: p2 is the 2nd p even though spans interleave.
        assert_eq!(by_id(&styles, &dom, "p2").color, 0x123456FF);
        assert_eq!(by_id(&styles, &dom, "p3").color, 0x1A1D23FF);
        assert_eq!(by_id(&styles, &dom, "p1").font_weight, 700);
        assert_eq!(by_id(&styles, &dom, "p3").font_weight, 700);
        assert_eq!(by_id(&styles, &dom, "p2").font_weight, 400);
        assert_eq!(by_id(&styles, &dom, "h").color, 0x0000FFFF);
        assert_eq!(by_id(&styles, &dom, "s2").color, 0x00FF00FF);
        assert_eq!(by_id(&styles, &dom, "s1").color, 0x1A1D23FF);
        assert!(by_id(&styles, &dom, "h").italic, "h1:only-of-type matches");
    }

    #[test]
    fn animation_shorthand_parses() {
        let (dom, styles) = styled(
            "<div id='a' style='animation: fade 2s ease-in-out 0.5s infinite alternate'></div>\
             <div id='b' style='animation-name: slide; animation-duration: 3s'></div>",
            &[],
        );
        let a = &by_id(&styles, &dom, "a").animations;
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "fade");
        assert!((a[0].duration_secs - 2.0).abs() < 0.01);
        assert!((a[0].delay_secs - 0.5).abs() < 0.01);
        assert_eq!(a[0].timing, Timing::EaseInOut);
        assert_eq!(a[0].iteration, IterationCount::Infinite);
        assert_eq!(a[0].direction, AnimDirection::Alternate);
        let b = &by_id(&styles, &dom, "b").animations;
        assert_eq!(b.len(), 1, "longhands compose an animation alone");
        assert_eq!(b[0].name, "slide");
        assert!((b[0].duration_secs - 3.0).abs() < 0.01);
    }

    #[test]
    fn timing_curves_behave() {
        assert_eq!(Timing::Linear.apply(0.3), 0.3);
        assert_eq!(Timing::Linear.apply(0.0), 0.0);
        assert_eq!(Timing::Linear.apply(1.0), 1.0);
        // Ease-in-out is symmetric and passes through the middle.
        assert!((Timing::EaseInOut.apply(0.5) - 0.5).abs() < 0.02);
        assert!(Timing::EaseIn.apply(0.5) < 0.5, "starts slow");
        assert!(Timing::EaseOut.apply(0.5) > 0.5, "starts fast");
        assert_eq!(Timing::Steps(4).apply(0.0), 0.0);
        assert!((Timing::Steps(4).apply(0.3) - 0.25).abs() < 0.01);
        // Custom bezier endpoints pin.
        assert_eq!(Timing::Bezier(0.2, 0.8, 0.4, 0.9).apply(0.0), 0.0);
        assert_eq!(Timing::Bezier(0.2, 0.8, 0.4, 0.9).apply(1.0), 1.0);
    }

    #[test]
    fn samples_opacity_and_color_over_keyframes() {
        use kitae_css::{KeyframeBlock, Keyframes};
        let decl = |p: &str, v: &str| kitae_css::Declaration {
            property: p.into(),
            value: v.into(),
            important: false,
        };
        let frames = Keyframes {
            name: "fade".into(),
            frames: vec![
                KeyframeBlock {
                    offset: 0.0,
                    declarations: vec![decl("opacity", "0"), decl("color", "red")],
                },
                KeyframeBlock {
                    offset: 1.0,
                    declarations: vec![decl("opacity", "1"), decl("color", "blue")],
                },
            ],
        };
        let anim = Animation {
            name: "fade".into(),
            duration_secs: 2.0,
            delay_secs: 0.0,
            timing: Timing::Linear,
            iteration: IterationCount::Count(1.0),
            direction: AnimDirection::Normal,
            fill: FillMode::None,
            running: true,
        };
        // Before delay / after end: no overrides (no fill model).
        assert!(sample_animation(&anim, &frames, -0.1).is_empty());
        assert!(sample_animation(&anim, &frames, 2.5).is_empty());
        let mid = sample_animation(&anim, &frames, 1.0);
        let get = |p: &str| mid.iter().find(|(q, _)| q == p).map(|(_, v)| v.clone());
        assert_eq!(get("opacity").as_deref(), Some("0.500"));
        assert_eq!(
            get("color").as_deref(),
            Some("#800080"),
            "red↔blue midpoint"
        );
        // Alternate direction mirrors the second iteration.
        let mut alt = anim.clone();
        alt.direction = AnimDirection::Alternate;
        alt.iteration = IterationCount::Count(2.0);
        let v = sample_animation(&alt, &frames, 3.0);
        let get = |p: &str| v.iter().find(|(q, _)| q == p).map(|(_, v)| v.clone());
        assert_eq!(get("opacity").as_deref(), Some("0.500"));
        // Name mismatch samples nothing.
        let mut other = anim.clone();
        other.name = "nope".into();
        assert!(sample_animation(&other, &frames, 1.0).is_empty());
    }

    // ------------------------------------------------------------------
    // B1: transition parsing + value interpolation.
    // ------------------------------------------------------------------

    fn style_of(html: &str) -> ComputedStyle {
        let (dom, _) = kitae_html::parse(html);
        StyleEngine::new(&[])
            .compute(&dom)
            .into_iter()
            .next()
            .unwrap()
            .1
    }

    #[test]
    fn transition_shorthand_and_longhands_parse() {
        let s = style_of(
            "<div style='transition: opacity 300ms ease-in, transform 2s .5s linear'>x</div>",
        );
        assert_eq!(s.transitions.len(), 2);
        assert_eq!(s.transitions[0].property, "opacity");
        assert!((s.transitions[0].duration_secs - 0.3).abs() < 1e-4);
        assert_eq!(s.transitions[0].timing, Timing::EaseIn);
        assert!((s.transitions[0].delay_secs).abs() < 1e-4);
        assert_eq!(s.transitions[1].property, "transform");
        assert!((s.transitions[1].duration_secs - 2.0).abs() < 1e-4);
        assert!((s.transitions[1].delay_secs - 0.5).abs() < 1e-4);
        assert_eq!(s.transitions[1].timing, Timing::Linear);

        // Longhands: property list grows the list to pair by index.
        let s = style_of(
            "<div style='transition-duration: 1s, .2s; transition-property: opacity, color'>x</div>",
        );
        assert_eq!(s.transitions.len(), 2);
        assert_eq!(s.transitions[0].property, "opacity");
        assert!((s.transitions[0].duration_secs - 1.0).abs() < 1e-4);
        assert_eq!(s.transitions[1].property, "color");
        assert!((s.transitions[1].duration_secs - 0.2).abs() < 1e-4);

        // `none` drops; bare property names default to 0s (instant).
        assert!(style_of("<div style='transition: none'>x</div>")
            .transitions
            .is_empty());
        let s = style_of("<div style='transition: opacity'>x</div>");
        assert_eq!(s.transitions.len(), 1);
        assert!((s.transitions[0].duration_secs).abs() < 1e-4);

        // Not inherited. compute() emits post-order: the <p> first, then
        // its <div> parent.
        let (dom, _) = kitae_html::parse("<div style='transition: opacity 1s'><p>t</p></div>");
        let styles = StyleEngine::new(&[]).compute(&dom);
        assert!(styles[1].1.transitions.len() == 1, "on the div");
        assert!(styles[0].1.transitions.is_empty(), "p resets to initial");
    }

    #[test]
    fn transition_property_matching_covers_all() {
        assert!(transition_matches("all", "opacity"));
        assert!(transition_matches("", "background-color"));
        assert!(transition_matches("opacity", "opacity"));
        assert!(transition_matches("OPACITY", "opacity"), "case-insensitive");
        assert!(!transition_matches("opacity", "color"));
    }

    #[test]
    fn interpolate_style_value_numbers_colors_transforms() {
        // Numeric lerp.
        assert_eq!(
            interpolate_style_value("opacity", "0", "1", 0.25).as_deref(),
            Some("0.250")
        );
        // Color lerp keeps rgba form when alpha < 1 on either side.
        let v = interpolate_style_value("color", "#000000", "#ffffff", 0.5).unwrap();
        assert_eq!(v, "#808080");
        // Same-shape transform lists lerp per component.
        let v = interpolate_style_value(
            "transform",
            "translate(0px, 0px) rotate(0deg)",
            "translate(100px, 50px) rotate(90deg)",
            0.5,
        )
        .unwrap();
        assert_eq!(v, "translate(50px, 25px) rotate(45deg)");
        // `none` interpolates against identity of the other side's shape.
        let v = interpolate_style_value("transform", "none", "scale(2)", 0.5).unwrap();
        assert_eq!(v, "scale(1.5)");
        // Mismatched shapes are discrete.
        assert!(
            interpolate_style_value("transform", "scale(2)", "translate(10px, 0px)", 0.5).is_none()
        );
        // Unknown property stays discrete.
        assert!(interpolate_style_value("margin-top", "0px", "10px", 0.5).is_none());
    }

    // ------------------------------------------------------------------
    // Q2: transform / transform-origin parsing + computed application.
    // ------------------------------------------------------------------

    fn style_with(transform_decl: &str) -> ComputedStyle {
        let (dom, _) = kitae_html::parse(&format!("<div style='{}'>x</div>", transform_decl));
        StyleEngine::new(&[])
            .compute(&dom)
            .into_iter()
            .next()
            .unwrap()
            .1
    }

    #[test]
    fn transform_functions_parse_to_computed_list() {
        let s = style_with("transform: translate(10px, 20%) rotate(45deg) scale(2) skewX(15deg)");
        let t = s.transform.as_ref().expect("transform parsed");
        assert_eq!(t.0.len(), 4);
        // translate: px folded, % symbolic.
        assert_eq!(
            t.0[0],
            TransformFn::Translate {
                x: TransLen { px: 10.0, pct: 0.0 },
                y: TransLen { px: 0.0, pct: 20.0 }
            }
        );
        assert_eq!(t.0[1], TransformFn::Rotate(45.0));
        assert_eq!(t.0[2], TransformFn::Scale(2.0, 2.0));
        assert_eq!(t.0[3], TransformFn::SkewX(15.0));
    }

    #[test]
    fn transform_shorthands_and_matrix_row_major() {
        let s = style_with("transform: matrix(1, 2, 3, 4, 5, 6) translateY(2em) scaleX(3)");
        let t = s.transform.as_ref().unwrap();
        // matrix(a,b,c,d,e,f) → row-major [a, c, e, b, d, f].
        assert_eq!(t.0[0], TransformFn::Matrix([1.0, 3.0, 5.0, 2.0, 4.0, 6.0]));
        // em folds at computed time against the element font size (16px).
        assert_eq!(
            t.0[1],
            TransformFn::TranslateY(TransLen { px: 32.0, pct: 0.0 })
        );
        assert_eq!(t.0[2], TransformFn::ScaleX(3.0));
        // scale(s) mirrors into y; rotate accepts rad/grad/turn too.
        let s2 = style_with("transform: scale(2, 0.5) rotate(0.25turn)");
        let t2 = s2.transform.as_ref().unwrap();
        assert_eq!(t2.0[0], TransformFn::Scale(2.0, 0.5));
        assert_eq!(t2.0[1], TransformFn::Rotate(90.0));
    }

    #[test]
    fn transform_3d_folds_to_orthographic_projection() {
        // rotate3d about z is just rotate; rotateX/Y squash the other axis.
        let t = style_with("transform: rotate3d(0, 0, 1, 90deg)")
            .transform
            .unwrap();
        assert_eq!(t.0[0], TransformFn::Rotate(90.0));
        let t = style_with("transform: rotateX(60deg)").transform.unwrap();
        assert!(matches!(&t.0[0], TransformFn::Matrix(m) if (m[4] - 0.5).abs() < 1e-5));
        let t = style_with("transform: rotateY(60deg)").transform.unwrap();
        assert!(matches!(&t.0[0], TransformFn::Matrix(m) if (m[0] - 0.5).abs() < 1e-5));
        // translate3d drops z; scale3d mirrors into y like scale().
        let t = style_with("transform: translate3d(10px, 20px, 5px)")
            .transform
            .unwrap();
        assert_eq!(
            t.0[0],
            TransformFn::Translate {
                x: TransLen { px: 10.0, pct: 0.0 },
                y: TransLen { px: 20.0, pct: 0.0 }
            }
        );
        let t = style_with("transform: scale3d(2, 3, 0.5)")
            .transform
            .unwrap();
        assert_eq!(t.0[0], TransformFn::Scale(2.0, 3.0));
        // rotate3d about a tilted axis: Rodrigues' top-left 2×2. At 90° about
        // (1,1,0)/√2 the cos term vanishes and with z=0 the [n]× skew leaves
        // the 2×2 — all four entries are xy·(1−cos) = 0.5.
        let t = style_with("transform: rotate3d(1, 1, 0, 90deg)")
            .transform
            .unwrap();
        assert!(matches!(&t.0[0], TransformFn::Matrix(m)
            if m.iter().zip([0.5, 0.5, 0.0, 0.5, 0.5, 0.0]).all(|(a, b)| (a - b).abs() < 1e-5)));
        // matrix3d folds when x/y don't read z and there's no perspective;
        // CSS column-major (v0..v3 = first column) → row-major 2×3.
        // [2 0 0 10; 0 3 0 20; 0 0 1 0; 0 0 0 1] → Matrix([2, 0, 10, 0, 3, 20]).
        let m3 = "transform: matrix3d(2, 0, 0, 0, 0, 3, 0, 0, 0, 0, 1, 0, 10, 20, 0, 1)";
        let t = style_with(m3).transform.unwrap();
        assert_eq!(
            t.0[0],
            TransformFn::Matrix([2.0, 0.0, 10.0, 0.0, 3.0, 20.0])
        );
        // Perspective columns or z-coupled x/y can't fold — dropped entirely.
        assert!(style_with(
            "transform: matrix3d(1, 0, 0, 0.1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1)"
        )
        .transform
        .is_none());
        assert!(
            style_with("transform: matrix3d(1, 0, 0, 0, 0, 1, 0, 0, 1, 0, 1, 0, 0, 0, 0, 1)")
                .transform
                .is_none()
        );
        // A uniform w scale divides out.
        let t = style_with("transform: matrix3d(2, 0, 0, 0, 0, 2, 0, 0, 0, 0, 1, 0, 0, 0, 0, 2)")
            .transform
            .unwrap();
        assert_eq!(t.0[0], TransformFn::Matrix([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]));
    }

    #[test]
    fn transform_none_and_garbage_reset_to_unset() {
        assert!(style_with("transform: none").transform.is_none());
        assert!(style_with("transform: rotate(garbage)").transform.is_none());
        // 3D folds to its orthographic projection (B8); only genuinely
        // projective functions (`perspective`) stay unsupported → none.
        assert!(style_with("transform: perspective(100px)")
            .transform
            .is_none());
        // Missing arg → dropped.
        assert!(style_with("transform: translate()").transform.is_none());
        // Inherited default: no property → None.
        assert!(style_with("color: red").transform.is_none());
    }

    #[test]
    fn transform_origin_parses_keywords_percent_and_px() {
        let s = style_with("transform-origin: left bottom");
        assert_eq!(
            s.transform_origin,
            TransformOrigin {
                x: TransLen { px: 0.0, pct: 0.0 },
                y: TransLen {
                    px: 0.0,
                    pct: 100.0
                },
            }
        );
        let s2 = style_with("transform-origin: 20px 25%");
        assert_eq!(
            s2.transform_origin,
            TransformOrigin {
                x: TransLen { px: 20.0, pct: 0.0 },
                y: TransLen { px: 0.0, pct: 25.0 },
            }
        );
        // `center` = 50%, the initial; a lone keyword covers one axis.
        let s3 = style_with("transform-origin: center");
        assert_eq!(s3.transform_origin, TransformOrigin::default());
        let s4 = style_with("transform-origin: top");
        assert_eq!(s4.transform_origin.y, TransLen { px: 0.0, pct: 0.0 });
        assert_eq!(s4.transform_origin.x, TransLen { px: 0.0, pct: 50.0 });
        // Garbage → default 50% 50%.
        let s5 = style_with("transform-origin: nonsense");
        assert_eq!(s5.transform_origin, TransformOrigin::default());
    }

    #[test]
    fn transform_is_not_inherited_and_single_arg_translate() {
        let (dom, _) = kitae_html::parse("<div style='transform: translate(5px)'><p>x</p></div>");
        let styles = StyleEngine::new(&[]).compute(&dom);
        let div = &styles.iter().find(|(n, _)| dom.tag(*n) == "div").unwrap().1;
        let t = div.transform.as_ref().expect("div transformed");
        // translate(5px) == translate(5px, 0).
        assert_eq!(
            t.0[0],
            TransformFn::Translate {
                x: TransLen { px: 5.0, pct: 0.0 },
                y: TransLen::ZERO
            }
        );
        // The <p> child has no transform of its own.
        let p = &styles.iter().find(|(n, _)| dom.tag(*n) == "p").unwrap().1;
        assert!(p.transform.is_none());
    }

    // ── A10: constraint validation ──────────────────────────────────────

    /// The `control_invalid` predicate: required-missing, type formats,
    /// min/max — and the empty-optional-is-valid rule.
    #[test]
    fn control_invalid_predicate() {
        let (dom, _) = parse(
            "<input id='a' required value=''>\
                             <input id='b' required value='x'>\
                             <input id='c' type='email' value='bad'>\
                             <input id='d' type='email' value='a@b.co'>\
                             <input id='e' type='number' value='5' min='0' max='3'>\
                             <input id='f'>\
                             <input id='g' type='date' value='2026-09-06'>\
                             <input id='h' type='date' value='2026-13-01'>\
                             <textarea id='i' required></textarea>",
        );
        for (id, want) in [
            ("a", true),  // required + empty
            ("b", false), // required + filled
            ("c", true),  // bad email
            ("d", false), // good email
            ("e", true),  // 5 > max 3
            ("f", false), // optional empty = valid
            ("g", false), // good date
            ("h", true),  // month 13
            ("i", true),  // required empty textarea
        ] {
            let n = dom.get_element_by_id(id).unwrap();
            assert_eq!(
                control_invalid(&dom, n),
                want,
                "control #{id} ({}): got {}",
                id,
                control_invalid(&dom, n)
            );
        }
    }

    /// `:invalid` / `:valid` match through the cascade (the UA error border
    /// is the layout-side proof; here an author rule proves matching).
    #[test]
    fn invalid_pseudo_matches_through_cascade() {
        let (dom, styles) = styled(
            "<input id='bad' required value=''>\
             <input id='good' required value='x'>",
            &["input:invalid { background: #d93025 }\
               input:valid { background: #188038 }"],
        );
        assert_eq!(
            by_id(&styles, &dom, "bad").background,
            0xD93025FF,
            "invalid matches the empty required input"
        );
        assert_eq!(
            by_id(&styles, &dom, "good").background,
            0x188038FF,
            "valid matches the filled input"
        );
    }

    /// The `pattern` regex subset: literals, classes with shorthands and
    /// negation, alternation, groups, quantifiers with bounds, anchors —
    /// full-string matched. Unparseable patterns are ignored (fail-open).
    #[test]
    fn pattern_attribute_validation() {
        let cases: &[(&str, &str, bool)] = &[
            // [A-Z]{3}\d{2} — the classic
            ("[A-Z]{3}\\d{2}", "ABC12", true),
            ("[A-Z]{3}\\d{2}", "abc12", false),
            ("[A-Z]{3}\\d{2}", "ABC123", false), // anchored: extra digit
            // alternation
            ("ab|cd", "cd", true),
            ("ab|cd", "ad", false),
            // negated class + +
            ("[^0-9]+", "abc", true),
            ("[^0-9]+", "a1c", false),
            // optional quantifier
            ("a+b?c", "aac", true),
            ("a+b?c", "aaacb", false), // b then c, not cb
            // counted range
            ("[a-z ]{3,6}", "hi", false),
            ("[a-z ]{3,6}", "hi there", false), // 8 chars
            ("[a-z ]{3,6}", "hi the", true),
            // shorthand word class
            ("\\w+", "user_1", true),
            ("\\w+", "user 1", false),
            // dot
            ("a.c", "abc", true),
            ("a.c", "ac", false),
            // unparseable pattern → ignored, value stays valid
            ("[unclosed", "anything", true),
            ("a)b", "ab", true),
        ];
        for (pat, val, want) in cases {
            assert_eq!(
                pattern_matches(pat, val),
                *want,
                "pattern {pat:?} vs {val:?}"
            );
        }
    }

    /// The validation bubble texts: reason follows the spec's priority
    /// (value-missing → type/format → range → pattern); empty when valid.
    #[test]
    fn control_invalid_messages() {
        let (dom, _) = parse(
            "<input id='a' required value=''>\
             <input id='b' type='email' value='nope'>\
             <input id='c' type='number' value='9' max='3'>\
             <input id='d' type='number' value='cat'>\
             <input id='e' value='x' pattern='[A-Z]{3}'>\
             <input id='f' value='XYZ'>\
             <select id='s' required><option value=''>pick</option></select>\
             <textarea id='t' required></textarea>",
        );
        let msg = |id: &str| {
            let n = dom.get_element_by_id(id).unwrap();
            control_invalid_message(&dom, n)
        };
        assert_eq!(msg("a"), "Please fill out this field.");
        assert_eq!(msg("b"), "Please enter an email address.");
        assert_eq!(msg("c"), "Value must be less than or equal to 3.");
        assert_eq!(msg("d"), "Please enter a number.");
        assert_eq!(msg("e"), "Please match the format requested.");
        assert_eq!(msg("f"), "", "pattern satisfied → no message");
        assert_eq!(msg("s"), "Please select an item in the list.");
        assert_eq!(msg("t"), "Please fill out this field.");
    }

    // --- B3: :has() relational selector ---

    #[test]
    fn has_matches_descendant_and_child_args() {
        let (dom, styles) = styled(
            "<div id='a'><section><p>x</p></section></div><div id='b'><img></div>",
            &[
                "div:has(p) { color: #D93025FF }",
                "div:has(> section) { background: #188038FF }",
            ],
        );
        // `a` holds a <p> two levels down → the descendant form matches.
        assert_eq!(by_id(&styles, &dom, "a").color, 0xD93025FF);
        // `b` holds only an <img> → no.
        assert_ne!(by_id(&styles, &dom, "b").color, 0xD93025FF);
        // `> section` anchors on the direct child → `a` matches.
        assert_eq!(by_id(&styles, &dom, "a").background, 0x188038FF);
    }

    #[test]
    fn has_child_anchor_rejects_nested_match() {
        let (dom, styles) = styled(
            "<div id='wrap'><section><p>x</p></section></div>",
            &[
                "div:has(> p) { color: #D93025FF }",
                "div:has(section p) { background: #188038FF }",
            ],
        );
        // <p> is nested under <section> → the child-anchored form fails…
        assert_ne!(by_id(&styles, &dom, "wrap").color, 0xD93025FF);
        // …while the two-compound descendant argument matches.
        assert_eq!(by_id(&styles, &dom, "wrap").background, 0x188038FF);
    }

    #[test]
    fn has_anchor_must_sit_below_the_scope() {
        // Relative selectors are :scope-anchored: `article img` requires
        // the <article> BELOW the div. Here it is the div's parent → no match.
        let (dom, styles) = styled(
            "<article><div id='c'><img></div></article>",
            &["div:has(article img) { color: #D93025FF }"],
        );
        assert_ne!(by_id(&styles, &dom, "c").color, 0xD93025FF);
    }

    #[test]
    fn has_specificity_adds_most_specific_argument() {
        // `.card:has(img)` = (0,1,1) beats plain `.card` (0,1,0).
        let (dom, styles) = styled(
            "<div class='card'><img></div>",
            &[
                ".card { color: #1A73E8FF }",
                ".card:has(img) { color: #D93025FF }",
            ],
        );
        assert_eq!(find(&styles, &dom, "div").color, 0xD93025FF);
    }

    // --- B3: container queries ---

    fn container_info(node: NodeId, names: &[&str], width: f32) -> (u32, ContainerInfo) {
        (
            node.0,
            ContainerInfo {
                names: names.iter().map(|s| s.to_string()).collect(),
                width,
                height: 0.0,
            },
        )
    }

    #[test]
    fn container_query_applies_by_nearest_container_width() {
        // The container is a PARENT of the element being styled, so the
        // condition applies to the descendant. (A self-query would be a
        // cycle — the spec applies @container rules to descendants only.)
        let (dom, _) = styled(
            "<div id='outer' class='card'><div id='inner' class='card'><p id='p'>x</p></div></div>",
            &[
                ".card { container-type: inline-size }",
                "@container (min-width: 300px) { p { color: #D93025FF } }",
            ],
        );
        let outer = dom.get_element_by_id("outer").unwrap();
        let inner = dom.get_element_by_id("inner").unwrap();
        let p = dom.get_element_by_id("p").unwrap();
        let engine = StyleEngine::new(&[
            ".card { container-type: inline-size }",
            "@container (min-width: 300px) { p { color: #D93025FF } }",
        ]);
        // Narrow inner container (200px) is the nearest → condition false.
        let info: HashMap<u32, ContainerInfo> = [
            container_info(outer, &[], 1280.0),
            container_info(inner, &[], 200.0),
        ]
        .into_iter()
        .collect();
        let styles = engine.compute_with_containers(&dom, &InteractionState::default(), &info);
        assert_ne!(by_id(&styles, &dom, "p").color, 0xD93025FF);
        // Widen the inner container past the boundary → matches.
        let info: HashMap<u32, ContainerInfo> = [
            container_info(outer, &[], 1280.0),
            container_info(inner, &[], 320.0),
        ]
        .into_iter()
        .collect();
        let styles = engine.compute_with_containers(&dom, &InteractionState::default(), &info);
        assert_eq!(by_id(&styles, &dom, "p").color, 0xD93025FF);
        // Drop inner → outer (1280) is the nearest, condition holds.
        let info: HashMap<u32, ContainerInfo> =
            [container_info(outer, &[], 1280.0)].into_iter().collect();
        let styles = engine.compute_with_containers(&dom, &InteractionState::default(), &info);
        assert_eq!(by_id(&styles, &dom, "p").color, 0xD93025FF);
        // Sanity: bare p (no containers anywhere) leaves the UA color.
        let _ = p;
    }

    #[test]
    fn container_named_query_targets_the_named_container() {
        // The query names `card` (outer, 800px); the inner container is
        // unnamed (CSS sets the name only on the outer wrapper) so the
        // named lookup skips it and finds the outer instead.
        let (dom, _) = styled(
            "<div id='outer' class='card'><div id='inner' class='box'><p id='p'>x</p></div></div>",
            &[
                ".card { container-type: inline-size; container-name: card }",
                ".box { container-type: inline-size }",
                "@container card (min-width: 300px) { p { color: #D93025FF } }",
            ],
        );
        let outer = dom.get_element_by_id("outer").unwrap();
        let inner = dom.get_element_by_id("inner").unwrap();
        let engine = StyleEngine::new(&[
            ".card { container-type: inline-size; container-name: card }",
            ".box { container-type: inline-size }",
            "@container card (min-width: 300px) { p { color: #D93025FF } }",
        ]);
        let info: HashMap<u32, ContainerInfo> = [
            container_info(outer, &["card"], 800.0),
            container_info(inner, &[], 100.0),
        ]
        .into_iter()
        .collect();
        let styles = engine.compute_with_containers(&dom, &InteractionState::default(), &info);
        assert_eq!(by_id(&styles, &dom, "p").color, 0xD93025FF);
        // Removing the outer leaves only an unnamed 100px container → the
        // named query can't find its target and the rule does not apply.
        let info: HashMap<u32, ContainerInfo> =
            [container_info(inner, &[], 100.0)].into_iter().collect();
        let styles = engine.compute_with_containers(&dom, &InteractionState::default(), &info);
        assert_ne!(by_id(&styles, &dom, "p").color, 0xD93025FF);
    }

    #[test]
    fn container_condition_boundary_and_max_width() {
        // The container is the <div>; @container targets the inner <p>.
        let css = "@container (min-width: 400px) { p { color: #D93025FF } }";
        let (dom, _) = styled(
            "<div class='card' id='wrap'><p id='p'>x</p></div>",
            &[".card { container-type: inline-size }", css],
        );
        let wrap = dom.get_element_by_id("wrap").unwrap();
        let engine = StyleEngine::new(&[".card { container-type: inline-size }", css]);
        let mk = |w: f32| {
            let info: HashMap<u32, ContainerInfo> =
                [container_info(wrap, &[], w)].into_iter().collect();
            engine.compute_with_containers(&dom, &InteractionState::default(), &info)
        };
        assert_ne!(by_id(&mk(399.9), &dom, "p").color, 0xD93025FF);
        assert_eq!(by_id(&mk(400.0), &dom, "p").color, 0xD93025FF);

        let css_max = "@container (max-width: 200px) { p { background: #188038FF } }";
        let engine_max = StyleEngine::new(&[".card { container-type: inline-size }", css_max]);
        let mk_max = |w: f32| {
            let info: HashMap<u32, ContainerInfo> =
                [container_info(wrap, &[], w)].into_iter().collect();
            engine_max.compute_with_containers(&dom, &InteractionState::default(), &info)
        };
        assert_eq!(by_id(&mk_max(200.0), &dom, "p").background, 0x188038FF);
        assert_ne!(by_id(&mk_max(201.0), &dom, "p").background, 0x188038FF);
    }

    #[test]
    fn cqw_folds_against_nearest_container_else_viewport() {
        // The container is the <div>; the cqw length is on the inner <p>.
        let css = "p { font-size: 10cqw }";
        let (dom, _) = styled(
            "<div id='wrap' class='card'><p id='p'>x</p></div>",
            &[".card { container-type: inline-size }", css],
        );
        let wrap = dom.get_element_by_id("wrap").unwrap();
        let engine = StyleEngine::new(&[".card { container-type: inline-size }", css]);
        // No container in the walk → small-viewport fallback: 10% of 1280.
        let styles =
            engine.compute_with_containers(&dom, &InteractionState::default(), &HashMap::new());
        assert!((by_id(&styles, &dom, "p").font_size - 128.0).abs() < 0.01);
        // With a 400px container → 40px.
        let info: HashMap<u32, ContainerInfo> =
            [container_info(wrap, &[], 400.0)].into_iter().collect();
        let styles = engine.compute_with_containers(&dom, &InteractionState::default(), &info);
        assert!((by_id(&styles, &dom, "p").font_size - 40.0).abs() < 0.01);
    }

    #[test]
    fn cqh_folds_against_viewport_height() {
        // No container needed: block-axis units use viewport height
        // directly, even on a bare element.
        let (dom, styles) = styled("<p id='p'>x</p>", &["p { font-size: 10cqh }"]);
        // Headless viewport height 600 → 60px.
        assert!((by_id(&styles, &dom, "p").font_size - 60.0).abs() < 0.01);
    }

    #[test]
    fn uses_containers_flag_tracks_sheets() {
        assert!(
            StyleEngine::new(&["@container (min-width: 100px) { p { color: red } }"])
                .uses_containers()
        );
        assert!(StyleEngine::new(&["p { width: 10cqw }"]).uses_containers());
        assert!(!StyleEngine::new(&["p { width: 10px }"]).uses_containers());
    }

    // --- B4: filter / backdrop-filter ---

    #[test]
    fn filter_blur_and_scalar_chain_resolves() {
        let (dom, styles) = styled(
            "<div id='a'>x</div>",
            &["div { filter: blur(8px) brightness(1.2) }"],
        );
        let s = by_id(&styles, &dom, "a");
        let fl = s.filter.as_ref().expect("filter is set");
        assert_eq!(fl.0.len(), 2);
        assert!(matches!(fl.0[0], FilterFn::Blur(8.0)));
        assert!(matches!(fl.0[1], FilterFn::Brightness(1.2)));
    }

    #[test]
    fn filter_none_clears_the_effect() {
        let (dom, styles) = styled(
            "<div id='a'>x</div>",
            &["div { filter: blur(8px) }", "div#a { filter: none }"],
        );
        assert!(by_id(&styles, &dom, "a").filter.is_none());
    }

    #[test]
    fn backdrop_filter_is_independent_from_filter() {
        let (dom, styles) = styled(
            "<div id='a'>x</div>",
            &["div { filter: blur(2px); backdrop-filter: grayscale(0.5) }"],
        );
        let s = by_id(&styles, &dom, "a");
        assert_eq!(s.filter.as_ref().unwrap().0.len(), 1);
        assert_eq!(s.backdrop_filter.as_ref().unwrap().0.len(), 1);
        assert!(matches!(
            s.backdrop_filter.as_ref().unwrap().0[0],
            FilterFn::Grayscale(0.5)
        ));
    }

    #[test]
    fn drop_shadow_color_defaults_to_currentcolor() {
        // No color in the author rule → cascade fills it from the
        // element's own `color` (UA default 0x000000FF here).
        let (dom, styles) = styled(
            "<div id='a'>x</div>",
            &["div { color: #D93025FF; filter: drop-shadow(2px 4px 6px) }"],
        );
        let s = by_id(&styles, &dom, "a");
        let fl = s.filter.as_ref().unwrap();
        match fl.0[0] {
            FilterFn::DropShadow {
                dx,
                dy,
                blur,
                color,
            } => {
                assert_eq!((dx, dy, blur), (2.0, 4.0, 6.0));
                assert_eq!(color, 0xD93025FF, "uses the element's text color");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn drop_shadow_explicit_color_is_kept() {
        let (dom, styles) = styled(
            "<div id='a'>x</div>",
            &["div { color: #D93025FF; filter: drop-shadow(2px 4px 6px #188038FF) }"],
        );
        let s = by_id(&styles, &dom, "a");
        match s.filter.as_ref().unwrap().0[0] {
            FilterFn::DropShadow { color, .. } => {
                assert_eq!(color, 0x188038FF);
            }
            _ => unreachable!(),
        }
    }

    // -----------------------------------------------------------------------
    // B14 — list markers and CSS counters.
    // -----------------------------------------------------------------------

    #[test]
    fn b14_format_counter_forms() {
        assert_eq!(format_counter(ListStyleType::LowerRoman, 4), "iv");
        assert_eq!(format_counter(ListStyleType::LowerRoman, 14), "xiv");
        assert_eq!(format_counter(ListStyleType::UpperRoman, 1999), "MCMXCIX");
        assert_eq!(format_counter(ListStyleType::LowerAlpha, 2), "b");
        assert_eq!(format_counter(ListStyleType::UpperAlpha, 28), "AB");
        assert_eq!(format_counter(ListStyleType::DecimalLeadingZero, 7), "07");
        assert_eq!(format_counter(ListStyleType::Decimal, 0), "0");
        assert_eq!(format_counter(ListStyleType::Disc, 3), "•");
        // Out-of-range alpha/roman fall back to decimal.
        assert_eq!(format_counter(ListStyleType::LowerRoman, 0), "0");
        assert_eq!(format_counter(ListStyleType::LowerRoman, 4000), "4000");
    }

    #[test]
    fn b14_default_ol_ul_numbering() {
        let (dom, styles) = styled(
            "<ul><li>u</li></ul><ol><li>o1</li><li id='o2'>o2</li></ol>",
            &[],
        );
        assert_eq!(find(&styles, &dom, "li").marker_text.as_deref(), Some("•"));
        assert_eq!(
            by_id(&styles, &dom, "o2").marker_text.as_deref(),
            Some("2.")
        );
    }

    #[test]
    fn b14_list_style_parse_and_marker_stamping() {
        let css = "ul { list-style: lower-alpha inside } \
                   ol { list-style-type: decimal-leading-zero }";
        let (dom, styles) = styled(
            "<ul><li>x</li></ul><ol><li>a</li><li id='b'>c</li></ol>",
            &[css],
        );
        let li = find(&styles, &dom, "li");
        assert_eq!(li.list_style_type, ListStyleType::LowerAlpha);
        assert_eq!(li.list_style_position, ListStylePosition::Inside);
        assert_eq!(li.marker_text.as_deref(), Some("a."));
        assert_eq!(
            by_id(&styles, &dom, "b").marker_text.as_deref(),
            Some("02.")
        );
    }

    #[test]
    fn b14_counter_reset_increment_and_content() {
        let css = ".root { counter-reset: chapter 2 section 0 } \
                   .ch { counter-increment: chapter } \
                   .ch::before { content: 'Ch ' counter(chapter) } \
                   .sec { counter-increment: section 3 } \
                   .sec::before { content: counter(chapter) '.' counter(section) }";
        let (dom, styles) = styled(
            "<div class='root'><div class='ch' id='c'>x</div>\
             <div class='sec' id='s'>y</div></div>",
            &[css],
        );
        let c = by_id(&styles, &dom, "c");
        assert_eq!(c.before.as_ref().unwrap().text, "Ch 3");
        let s = by_id(&styles, &dom, "s");
        assert_eq!(s.before.as_ref().unwrap().text, "3.3");
    }

    #[test]
    fn b14_counters_join_and_own_increment() {
        let css = ".outer { counter-reset: sec 1 } \
                   .inner { counter-increment: sec } \
                   .inner::before { content: counters(sec, '-') } \
                   .solo { counter-increment: q } \
                   .solo::before { content: counter(q) }";
        let (dom, styles) = styled(
            "<div class='outer' id='o1'><div class='inner' id='i1'>a</div></div>\
             <div class='outer' id='o2'><div class='inner' id='i2'>b</div></div>\
             <div class='outer' id='m'><div class='outer' id='n'>\
             <div class='inner' id='i3'>c</div></div></div>\
             <div class='solo' id='q1'>d</div>",
            &[css],
        );
        // Each scope tree counts independently (sibling lists restart).
        assert_eq!(
            by_id(&styles, &dom, "i1").before.as_ref().unwrap().text,
            "2"
        );
        assert_eq!(
            by_id(&styles, &dom, "i2").before.as_ref().unwrap().text,
            "2"
        );
        // Nested same-named scopes join innermost-first.
        assert_eq!(
            by_id(&styles, &dom, "i3").before.as_ref().unwrap().text,
            "2-1"
        );
        // An increment without a reset still counts, document-order style.
        assert_eq!(
            by_id(&styles, &dom, "q1").before.as_ref().unwrap().text,
            "1"
        );
    }

    #[test]
    fn b14_content_counter_syntax() {
        assert!(parse_content_spec("counter(n, upper-roman)").is_some());
        assert_eq!(
            parse_content_spec("counters(s, '.', lower-alpha)"),
            Some(ContentSpec::Parts(vec![ContentPart::Counters {
                name: "s".into(),
                sep: ".".into(),
                style: Some(ListStyleType::LowerAlpha),
            }]))
        );
        // Unsupported functions invalidate the whole value; a lone string or
        // attr collapses to the legacy variants (R3).
        assert!(parse_content_spec("url(x) counter(n)").is_none());
        assert!(parse_content_spec("open-quote").is_none());
        assert_eq!(
            parse_content_spec("'hi'"),
            Some(ContentSpec::Text("hi".into()))
        );
        assert_eq!(
            parse_content_spec("attr(x)"),
            Some(ContentSpec::Attr("x".into()))
        );
    }

    // ------------------------------------------------------------------
    // B15: `all` / CSS-wide keywords / @property / @scope / snap.
    // ------------------------------------------------------------------

    #[test]
    fn current_color_resolves_after_cascade_for_paint_properties() {
        let (dom, styles) = styled(
            "<div id='x' style='background-color: currentColor; color: #123456; fill: currentColor; stroke: currentColor; background-image: linear-gradient(currentColor, #0000ff)'></div>",
            &[],
        );
        let x = by_id(&styles, &dom, "x");
        assert_eq!(x.color, 0x123456FF);
        assert_eq!(x.background, 0x123456FF);
        assert_eq!(x.fill, Some(0x123456FF));
        assert_eq!(x.stroke, Some(0x123456FF));
        match &x.background_paint {
            BackgroundPaint::LinearGradient { stops, .. } => {
                assert_eq!(stops[0].color, 0x123456FF);
            }
            other => panic!("expected gradient, got {other:?}"),
        }
    }

    #[test]
    fn css_wide_keywords_reset_inherit_and_unset() {
        let (dom, styles) = styled(
            "<div id='parent' style='color: #112233; background: #445566; display: block; margin: 4px'>\
             <p id='current' style='color: #778899; background-color: currentColor'></p>\
             <p id='inherited' style='background-color: inherit; display: inherit; margin: inherit'></p>\
             <p id='initial' style='color: initial; background-color: initial; display: initial; margin: initial'></p>\
             <p id='unset' style='color: unset; background-color: unset; display: unset'></p></div>",
            &[],
        );
        let parent = by_id(&styles, &dom, "parent");
        let current = by_id(&styles, &dom, "current");
        assert_eq!(current.color, 0x778899FF);
        assert_eq!(current.background, 0x778899FF);
        let inherited = by_id(&styles, &dom, "inherited");
        assert_eq!(inherited.background, parent.background);
        assert_eq!(inherited.display, parent.display);
        assert_eq!(inherited.margin.top.px(), parent.margin.top.px());
        let initial = by_id(&styles, &dom, "initial");
        assert_eq!(initial.color, ComputedStyle::default().color);
        assert_eq!(initial.background, 0);
        assert_eq!(initial.display, Display::Inline);
        assert_eq!(initial.margin.top.px(), Some(0.0));
        let unset = by_id(&styles, &dom, "unset");
        assert_eq!(unset.color, parent.color);
        assert_eq!(unset.background, 0);
        assert_eq!(unset.display, Display::Inline);
    }

    #[test]
    fn all_keywords_apply_to_non_inherited_properties() {
        let (dom, styles) = styled(
            "<div id='parent' style='display: block; color: #123456; margin: 3px; background: #654321'>\
             <p id='inherit'>x</p><p id='unset'>y</p></div>",
            &["#inherit { all: inherit } #unset { all: unset }"],
        );
        let parent = by_id(&styles, &dom, "parent");
        let inherited = by_id(&styles, &dom, "inherit");
        assert_eq!(inherited.display, parent.display);
        assert_eq!(inherited.color, parent.color);
        assert_eq!(inherited.margin.top.px(), parent.margin.top.px());
        assert_eq!(inherited.background, parent.background);
        let unset = by_id(&styles, &dom, "unset");
        assert_eq!(unset.display, Display::Inline);
        assert_eq!(unset.color, parent.color);
        assert_eq!(unset.margin.top.px(), Some(0.0));
        assert_eq!(unset.background, 0);
    }

    fn rgb(v: &str) -> u32 {
        style_with(&format!("color: {v}")).color
    }

    #[test]
    fn b15_all_shorthand_resets_earlier_keeps_later() {
        let red = rgb("rgb(255,0,0)");
        let green = rgb("rgb(0,255,0)");

        // Earlier declarations are wiped; later ones survive.
        let (dom, styles) = styled(
            "<p id='x'>t</p>",
            &["#x { color: rgb(255,0,0); font-size: 30px; all: initial; color: rgb(0,255,0); }"],
        );
        let s = by_id(&styles, &dom, "x");
        assert_eq!(s.color, green, "later color survives `all`");
        assert_eq!(s.font_size, 16.0, "earlier font-size is reset to initial");

        // With no later color, the reset falls to the ComputedStyle base.
        let (dom2, st2) = styled(
            "<p id='x'>t</p>",
            &["#x { color: rgb(255,0,0); all: initial; }"],
        );
        let s2 = by_id(&st2, &dom2, "x");
        assert_ne!(s2.color, red);
        assert_eq!(s2.color, ComputedStyle::default().color);

        // `direction` is exempt: `all` never resets it (spec).
        let (dom3, st3) = styled("<p id='x'>t</p>", &["#x { direction: rtl; all: initial; }"]);
        assert_eq!(by_id(&st3, &dom3, "x").direction, Direction::Rtl);

        // A non-initial keyword keeps the inherited base (no wipe of the
        // base itself), but still consumes earlier declarations.
        let (dom4, st4) = styled(
            "<p id='x'>t</p>",
            &["#x { color: rgb(255,0,0); all: inherit; }"],
        );
        assert_eq!(
            by_id(&st4, &dom4, "x").color,
            ComputedStyle::default().color
        );
    }

    #[test]
    fn b15_revert_keyword_falls_through_cascade() {
        let red = rgb("rgb(255,0,0)");
        let green = rgb("rgb(0,255,0)");

        // `revert` as a value drops the declaration: the next rule wins.
        let (dom, styles) = styled(
            "<p id='x'>t</p>",
            &[
                "#x { color: rgb(255,0,0); }",
                "p { color: rgb(0,255,0); color: revert; }",
            ],
        );
        assert_eq!(
            by_id(&styles, &dom, "x").color,
            red,
            "reverted decl falls to #x rule"
        );

        // At the top of the cascade it still falls to the lower author rule.
        let (dom2, st2) = styled(
            "<p id='x'>t</p>",
            &["p { color: rgb(0,255,0); }", "#x { color: revert; }"],
        );
        assert_eq!(by_id(&st2, &dom2, "x").color, green);

        // `revert-layer` behaves the same here (single author layer): the
        // reverted value drops, the earlier rule wins.
        let (dom3, st3) = styled(
            "<p id='x'>t</p>",
            &[
                "#x { color: rgb(255,0,0); }",
                "p { color: rgb(0,255,0); color: revert-layer; }",
            ],
        );
        assert_eq!(by_id(&st3, &dom3, "x").color, red);
    }

    #[test]
    fn b15_at_property_registration_semantics() {
        let blue = rgb("rgb(0,0,255)");
        let red = rgb("rgb(255,0,0)");
        let (dom, styles) = styled(
            "<div id='p'><span id='c'>t</span></div>",
            &[
                "@property --accent { syntax: \"<color>\"; inherits: false; initial-value: rgb(0,0,255); }",
                "#p { --accent: rgb(255,0,0); color: var(--accent); }",
                "#c { color: var(--accent); }",
            ],
        );
        // Own value wins on the setter.
        assert_eq!(by_id(&styles, &dom, "p").color, red);
        // Non-inherited registration: the child sees the registered initial,
        // not the parent's value (guaranteed-value semantics).
        assert_eq!(by_id(&styles, &dom, "c").color, blue);

        // An inherited registration passes the parent value down.
        let (dom2, st2) = styled(
            "<div id='p'><span id='c'>t</span></div>",
            &[
                "@property --shared { syntax: \"<color>\"; inherits: true; initial-value: rgb(0,0,255); }",
                "#p { --shared: rgb(255,0,0); }",
                "#c { color: var(--shared); }",
            ],
        );
        assert_eq!(by_id(&st2, &dom2, "c").color, red);

        // Registered initial takes precedence over a var() fallback.
        let (dom3, st3) = styled(
            "<p id='x'>t</p>",
            &[
                "@property --accent { syntax: \"<color>\"; inherits: false; initial-value: rgb(0,0,255); }",
                "#x { color: var(--accent, rgb(0,255,0)); }",
            ],
        );
        assert_eq!(by_id(&st3, &dom3, "x").color, blue);
    }

    #[test]
    fn b15_scope_gate_at_cascade() {
        let red = rgb("rgb(255,0,0)");
        let (dom, styles) = styled(
            "<div class='card' id='root'><p id='a'>x</p>\
             <div class='stop'><p id='b'>x</p></div></div>\
             <div><p id='c'>x</p></div>",
            &["@scope (.card) to (.stop) { p { color: rgb(255,0,0); } }"],
        );
        // Inside the scope: styled.
        assert_eq!(by_id(&styles, &dom, "a").color, red);
        // Inside the `to` boundary (and at it): out.
        assert_ne!(by_id(&styles, &dom, "b").color, red);
        // Outside the root: out.
        assert_ne!(by_id(&styles, &dom, "c").color, red);

        // The scope root itself is in its own scope.
        let (dom2, st2) = styled(
            "<div class='card' id='root'>x</div>",
            &["@scope (.card) { div { color: rgb(255,0,0); } }"],
        );
        assert_eq!(by_id(&st2, &dom2, "root").color, red);
    }

    #[test]
    fn b15_snap_and_overscroll_parse() {
        let s = style_with(
            "scroll-snap-type: block proximity; scroll-snap-align: center; \
             overscroll-behavior: contain; overscroll-behavior-y: none",
        );
        assert_eq!(s.snap_axis, SnapAxis::Y);
        assert!(s.snap_proximity);
        assert_eq!(s.snap_align, (SnapAlign::Center, SnapAlign::Center));
        assert_eq!(s.overscroll_x, OverscrollBehavior::Contain);
        assert_eq!(
            s.overscroll_y,
            OverscrollBehavior::None,
            "longhand overrides shorthand"
        );

        // Mandatory is the default strictness; `both` sets both axes.
        let s2 = style_with("scroll-snap-type: both");
        assert_eq!(s2.snap_axis, SnapAxis::Both);
        assert!(!s2.snap_proximity);

        // Strictness without an axis is invalid → no snapping.
        let s3 = style_with("scroll-snap-type: mandatory");
        assert_eq!(s3.snap_axis, SnapAxis::None);

        // `none` resets.
        let s4 = style_with("scroll-snap-type: none");
        assert_eq!(s4.snap_axis, SnapAxis::None);
        assert!(!s4.snap_proximity);

        // Two align tokens: inline then block.
        let s5 = style_with("scroll-snap-align: start end");
        assert_eq!(s5.snap_align, (SnapAlign::Start, SnapAlign::End));

        // Legacy single-axis overscroll forms.
        let s6 = style_with("overscroll-behavior: x");
        assert_eq!(s6.overscroll_x, OverscrollBehavior::Contain);
        assert_eq!(s6.overscroll_y, OverscrollBehavior::Auto);

        // Defaults are Auto / none.
        let s7 = style_with("color: red");
        assert_eq!(s7.overscroll_x, OverscrollBehavior::Auto);
        assert_eq!(s7.snap_axis, SnapAxis::None);
    }

    #[test]
    fn b7_clip_path_computes_non_inherited() {
        // `clip-path` folds em now, keeps % symbolic for layout.
        let s = style_with("clip-path: circle(50% at center)");
        assert!(matches!(
            s.clip_path,
            Some(ClipShape::Circle {
                r: ShapeRadius::Len(Len::Sum { px: 0.0, pct: 50.0 }),
                ..
            })
        ));
        let s = style_with("clip-path: inset(10px 0 round 4px)");
        assert!(matches!(s.clip_path, Some(ClipShape::Inset { .. })));
        // Initial value is none; `none` clears a set clip.
        assert!(style_with("color: red").clip_path.is_none());
        assert!(style_with("clip-path: circle(10px); clip-path: none")
            .clip_path
            .is_none());
        // Not inherited: the child of a clipped div starts clean.
        let (dom, _) = kitae_html::parse("<div style='clip-path: circle(10px)'><p>x</p></div>");
        let styles = StyleEngine::new(&[]).compute(&dom);
        let child = styles.iter().find(|(id, _)| dom.tag(*id) == "p").unwrap();
        assert!(child.1.clip_path.is_none());
    }

    #[test]
    fn b8_mix_blend_and_isolation_computes_non_inherited() {
        // All 16 keywords fold; unknown values keep the initial normal.
        assert_eq!(
            style_with("mix-blend-mode: multiply").mix_blend_mode,
            MixBlend::Multiply
        );
        assert_eq!(
            style_with("mix-blend-mode: color-dodge").mix_blend_mode,
            MixBlend::ColorDodge
        );
        assert_eq!(
            style_with("mix-blend-mode: LUMINOSITY").mix_blend_mode,
            MixBlend::Luminosity
        );
        assert_eq!(
            style_with("mix-blend-mode: bogus").mix_blend_mode,
            MixBlend::Normal
        );
        assert_eq!(style_with("color: red").mix_blend_mode, MixBlend::Normal);
        // `normal` clears a set mode; wire table matches the header.
        assert_eq!(
            style_with("mix-blend-mode: screen; mix-blend-mode: normal").mix_blend_mode,
            MixBlend::Normal
        );
        assert_eq!(MixBlend::Normal.wire(), 0);
        assert_eq!(MixBlend::Luminosity.wire(), 15);
        // isolation: only `isolate` sets the flag.
        assert!(style_with("isolation: isolate").isolate);
        assert!(!style_with("isolation: auto").isolate);
        assert!(!style_with("color: red").isolate);
        // Neither property inherits.
        let (dom, _) = kitae_html::parse(
            "<div style='mix-blend-mode: multiply; isolation: isolate'><p>x</p></div>",
        );
        let styles = StyleEngine::new(&[]).compute(&dom);
        let child = styles.iter().find(|(id, _)| dom.tag(*id) == "p").unwrap();
        assert_eq!(child.1.mix_blend_mode, MixBlend::Normal);
        assert!(!child.1.isolate);
    }

    #[test]
    fn b5_multicol_longhands_and_columns_shorthand_compute() {
        // Column count: positive integers win, `auto` resets.
        assert_eq!(style_with("column-count: 3").column_count, Some(3));
        assert_eq!(
            style_with("column-count: 3; column-count: auto").column_count,
            None
        );
        assert_eq!(style_with("column-count: 0").column_count, None);
        assert_eq!(style_with("column-count: bogus").column_count, None);
        // Column width: folded lengths, `auto` → None.
        assert_eq!(
            style_with("column-width: 160px")
                .column_width
                .map(|l| l.resolve(0.0)),
            Some(160.0)
        );
        assert!(style_with("column-width: auto").column_width.is_none());
        // `columns` shorthand: `<integer> || <length> | auto | normal`,
        // order-free, and it resets the longhand it does not mention.
        let s = style_with("columns: 2");
        assert_eq!(s.column_count, Some(2));
        assert!(s.column_width.is_none());
        let s = style_with("columns: 160px");
        assert_eq!(s.column_width.map(|l| l.resolve(0.0)), Some(160.0));
        assert_eq!(s.column_count, None);
        let s = style_with("columns: 12em 3");
        assert_eq!(s.column_count, Some(3));
        assert_eq!(s.column_width.map(|l| l.resolve(0.0)), Some(192.0)); // 12em @ 16px
        let s = style_with("column-count: 4; columns: auto");
        assert_eq!(s.column_count, None);
        // Column gap: `None` is `normal` (layout folds to 1em).
        assert_eq!(
            style_with("column-gap: 20px")
                .column_gap
                .map(|l| l.resolve(0.0)),
            Some(20.0)
        );
        assert!(style_with("column-gap: normal").column_gap.is_none());
        assert_eq!(
            style_with("gap: 30px").column_gap.map(|l| l.resolve(0.0)),
            Some(30.0)
        );
        assert!(style_with("row-gap: 30px").column_gap.is_none());
        // None of the three inherit.
        let (dom, _) = kitae_html::parse(
            "<div style='column-count: 3; column-width: 100px; column-gap: 10px'><p>x</p></div>",
        );
        let styles = StyleEngine::new(&[]).compute(&dom);
        let child = styles.iter().find(|(id, _)| dom.tag(*id) == "p").unwrap();
        assert_eq!(child.1.column_count, None);
        assert!(child.1.column_width.is_none());
        assert!(child.1.column_gap.is_none());
    }

    #[test]
    fn b4_writing_mode_computes_inherited_and_folds_logical() {
        // Keywords compute; unknown values keep the (inherited) mode.
        assert_eq!(
            style_with("writing-mode: vertical-rl").writing_mode,
            WritingMode::VerticalRl
        );
        assert_eq!(
            style_with("writing-mode: Sideways-LR").writing_mode,
            WritingMode::SidewaysLr
        );
        assert_eq!(
            style_with("writing-mode: bogus").writing_mode,
            WritingMode::HorizontalTb
        );
        // `writing-mode` inherits; the logical slots do not.
        let (dom, _) = kitae_html::parse(
            "<div style='writing-mode: vertical-lr; inline-size: 40px'><p>x</p></div>",
        );
        let styles = StyleEngine::new(&[]).compute(&dom);
        let child = styles.iter().find(|(id, _)| dom.tag(*id) == "p").unwrap();
        assert_eq!(child.1.writing_mode, WritingMode::VerticalLr);
        assert!(child.1.logical.is_empty());
        // Horizontal modes: inline-size aliases width (order-independent —
        // the fold runs after the whole cascade).
        let s = style_with("inline-size: 40px");
        assert_eq!(
            s.width.and_then(|sv| sv.len()).map(|l| l.resolve(0.0)),
            Some(40.0)
        );
        let s = style_with("inline-size: 40px; writing-mode: horizontal-tb");
        assert_eq!(
            s.width.and_then(|sv| sv.len()).map(|l| l.resolve(0.0)),
            Some(40.0)
        );
        // Vertical/sideways: inline-size aliases HEIGHT, block-size width,
        // and the min/max pairs follow the same axis swap.
        let s = style_with("writing-mode: vertical-rl; inline-size: 40px; block-size: 10px");
        assert_eq!(
            s.height.and_then(|sv| sv.len()).map(|l| l.resolve(0.0)),
            Some(40.0)
        );
        assert_eq!(
            s.width.and_then(|sv| sv.len()).map(|l| l.resolve(0.0)),
            Some(10.0)
        );
        let s =
            style_with("writing-mode: vertical-rl; min-inline-size: 20px; max-block-size: 30px");
        assert_eq!(s.min_height.map(|l| l.resolve(0.0)), Some(20.0));
        assert_eq!(s.max_width.map(|l| l.resolve(0.0)), Some(30.0));
        // Logical wins over a physical twin in the same origin…
        let s = style_with("width: 100px; inline-size: 20px");
        assert_eq!(
            s.width.and_then(|sv| sv.len()).map(|l| l.resolve(0.0)),
            Some(20.0)
        );
        // …and in horizontal-tb `block-size` is the height axis, so it
        // does not conflict with width at all.
        let s = style_with("width: 100px; block-size: 20px");
        assert_eq!(
            s.width.and_then(|sv| sv.len()).map(|l| l.resolve(0.0)),
            Some(100.0)
        );
        assert_eq!(
            s.height.and_then(|sv| sv.len()).map(|l| l.resolve(0.0)),
            Some(20.0)
        );
        // …and in a vertical subtree inline-size does NOT touch width.
        let s = style_with("writing-mode: vertical-rl; width: 100px; inline-size: 40px");
        assert_eq!(
            s.width.and_then(|sv| sv.len()).map(|l| l.resolve(0.0)),
            Some(100.0)
        );
        assert_eq!(
            s.height.and_then(|sv| sv.len()).map(|l| l.resolve(0.0)),
            Some(40.0)
        );
        // Insets map through the axis model.
        let s = style_with("inset-inline-start: 12px");
        assert_eq!(s.left.map(|l| l.resolve(0.0)), Some(12.0));
        let s = style_with("writing-mode: vertical-rl; inset-inline-start: 12px");
        assert_eq!(s.top.map(|l| l.resolve(0.0)), Some(12.0));
        assert!(s.left.is_none());
        // Two-value shorthands: first = start, second = end.
        let s = style_with("margin-inline: 5px 10px");
        assert_eq!(s.margin.left.resolve(0.0), 5.0);
        assert_eq!(s.margin.right.resolve(0.0), 10.0);
        // Single value sets both sides; block shorthands map to top/bottom
        // horizontally.
        let s = style_with("padding-block: 7px");
        assert_eq!(s.padding.top.resolve(0.0), 7.0);
        assert_eq!(s.padding.bottom.resolve(0.0), 7.0);
        // Block-start in a vertical subtree lands on the right edge.
        let s = style_with("writing-mode: vertical-rl; margin-block-start: 4px");
        assert_eq!(s.margin.right.resolve(0.0), 4.0);
        assert_eq!(s.margin.top.resolve(0.0), 0.0);
        // Rtl swaps the inline start/end pair (horizontal modes only).
        let s = style_with("direction: rtl; inline-size: 44px");
        assert_eq!(
            s.width.and_then(|sv| sv.len()).map(|l| l.resolve(0.0)),
            Some(44.0)
        );
        let s = style_with("direction: rtl; padding-inline-start: 9px");
        assert_eq!(s.padding.right.resolve(0.0), 9.0);
        let s = style_with("direction: rtl; writing-mode: vertical-rl; padding-inline-start: 9px");
        assert_eq!(s.padding.top.resolve(0.0), 9.0);
    }

    #[test]
    fn d6_text_orientation_computes_and_inherits() {
        // Keywords compute (case-insensitive); unknown values keep the
        // inherited orientation.
        assert_eq!(
            style_with("text-orientation: upright").text_orientation,
            TextOrientation::Upright
        );
        assert_eq!(
            style_with("text-orientation:  Sideways").text_orientation,
            TextOrientation::Sideways
        );
        assert_eq!(
            style_with("text-orientation: bogus").text_orientation,
            TextOrientation::Mixed
        );
        // Default is `mixed` with no declaration at all.
        assert_eq!(style_with("").text_orientation, TextOrientation::Mixed);
        // Inherited: children of a vertical box pick up its orientation but
        // can override; an unstyled root stays Mixed.
        let (dom, _) = kitae_html::parse(
            "<div style='text-orientation: upright'><p id=a style='text-orientation: sideways'>x</p>\
             <p id=b>y</p></div>",
        );
        let styles = StyleEngine::new(&[]).compute(&dom);
        assert_eq!(
            by_id(&styles, &dom, "a").text_orientation,
            TextOrientation::Sideways
        );
        assert_eq!(
            by_id(&styles, &dom, "b").text_orientation,
            TextOrientation::Upright
        );
    }

    #[test]
    fn b7_mask_shape_appoximates_to_clip_and_prefers_path() {
        // A mask shape feeds the same clip slot; gradients contribute nothing.
        let s = style_with("-webkit-mask-image: ellipse(30% 40%)");
        assert!(matches!(s.mask, Some(ClipShape::Ellipse { .. })));
        let s = style_with("mask: radial-gradient(black, transparent)");
        assert!(s.mask.is_none());
        // clip-path wins when both are present (documented single-clip rule):
        // the layout resolver takes `clip_path.or(mask)`.
        let s = style_with("clip-path: polygon(0 0, 100% 0, 0 100%); mask: circle(5px)");
        assert!(matches!(s.clip_path, Some(ClipShape::Polygon { .. })));
        assert!(matches!(s.mask, Some(ClipShape::Circle { .. })));
    }

    // ---- C1/C1c: intrinsic sizing keywords through the style pipeline ----
    #[test]
    fn c1_intrinsic_keyword_cascade() {
        assert_eq!(
            style_with("width: min-content").width,
            Some(SizeVal::MinContent)
        );
        assert_eq!(
            style_with("width: MAX-CONTENT").width,
            Some(SizeVal::MaxContent)
        );
        let s = style_with("width: fit-content(200px)");
        assert!(
            matches!(&s.width, Some(SizeVal::FitContent(Some(l))) if l.px() == Some(200.0)),
            "{:?}",
            s.width
        );
        assert_eq!(
            style_with("height: max-content").height,
            Some(SizeVal::MaxContent)
        );
        assert_eq!(
            style_with("height: 50%").height,
            None,
            "% height stays indefinite → auto"
        );
        assert_eq!(style_with("width: auto").width, None);
        let s = style_with("flex: 1 1 min-content");
        assert_eq!(s.flex_basis, Some(SizeVal::MinContent));
        assert_eq!(
            style_with("flex-basis: max-content").flex_basis,
            Some(SizeVal::MaxContent)
        );
    }

    #[test]
    fn c1b_grid_intrinsic_minmax_tracks_parse() {
        let (dom, styles) = styled(
            "<div id='g' style=\"display:grid;grid-template-columns:min-content max-content fit-content(60px) minmax(50px, 1fr)\"></div>",
            &[],
        );
        let g = by_id(&styles, &dom, "g");
        let t = &g.grid_template_columns;
        assert_eq!(t.len(), 4);
        assert!(matches!(t[0], GridTrack::MinContent));
        assert!(matches!(t[1], GridTrack::MaxContent));
        assert!(matches!(t[2], GridTrack::FitContent(Some(_))));
        assert!(matches!(
            t[3],
            GridTrack::MinMax(MinSizing::Len(_), MaxTrack::Flex(f))
                if (f - 1.0).abs() < 0.001
        ));
        // `minmax(0, 1fr)` keeps a 0 floor (the old collapse-to-auto bug).
        let (dom2, styles2) = styled(
            "<div id='g' style=\"display:grid;grid-template-columns:repeat(2, minmax(0, 1fr))\"></div>",
            &[],
        );
        let g2 = by_id(&styles2, &dom2, "g");
        assert_eq!(g2.grid_template_columns.len(), 2);
        assert!(g2.grid_template_columns.iter().all(|tr| matches!(
            tr,
            GridTrack::MinMax(MinSizing::Len(l), MaxTrack::Flex(f))
                if l.resolve(16.0, 16.0, 0.0) == 0.0 && (f - 1.0).abs() < 0.001
        )));
        // `minmax(auto, min-content)` + bare `fit-content` round-trip.
        let (dom3, styles3) = styled(
            "<div id='g' style='display:grid;grid-template-rows:minmax(auto, min-content) fit-content'></div>",
            &[],
        );
        let g3 = by_id(&styles3, &dom3, "g");
        assert!(matches!(
            g3.grid_template_rows[0],
            GridTrack::MinMax(MinSizing::Auto, MaxTrack::MinContent)
        ));
        assert!(matches!(
            g3.grid_template_rows[1],
            GridTrack::FitContent(None)
        ));
    }

    #[test]
    fn c2_aspect_ratio_parses() {
        let (dom, styles) = styled(
            "<div id='a' style='aspect-ratio: 16/9'></div>\
             <div id='b' style='aspect-ratio: 2'></div>\
             <div id='c' style='aspect-ratio: auto'></div>\
             <div id='d' style='aspect-ratio: banana'></div>\
             <div id='e' style='aspect-ratio: auto 16/9'></div>\
             <div id='f' style='aspect-ratio: AUTO 2'></div>\
             <div id='g' style='aspect-ratio: auto banana'></div>\
             <div id='h' style='aspect-ratio: autox'></div>\
             <div id='i' style='aspect-ratio: 0/2'></div>\
             <div id='j' style='aspect-ratio: 2 / 0'></div>",
            &[],
        );
        let r = |id: &str| by_id(&styles, &dom, id).aspect_ratio;
        assert!((r("a").unwrap().ratio - 16.0 / 9.0).abs() < 0.001);
        assert!(!r("a").unwrap().auto_keyword);
        assert!((r("b").unwrap().ratio - 2.0).abs() < 0.001);
        assert_eq!(r("c"), None);
        assert_eq!(r("d"), None);
        // `auto <ratio>` carries the same number plus the keyword that lets an
        // intrinsic ratio win (the layout side reads `auto_keyword`).
        assert!((r("e").unwrap().ratio - 16.0 / 9.0).abs() < 0.001);
        assert!(r("e").unwrap().auto_keyword);
        // The keyword is ASCII case-insensitive, like every CSS keyword.
        assert!((r("f").unwrap().ratio - 2.0).abs() < 0.001);
        assert!(r("f").unwrap().auto_keyword);
        // Everything after a valid `auto` prefix still has to parse, and a
        // word that merely starts with `auto` is not the keyword.
        assert_eq!(r("g"), None);
        assert_eq!(r("h"), None);
        // A zero component is not a ratio.
        assert_eq!(r("i"), None);
        assert_eq!(r("j"), None);
    }

    #[test]
    fn c3_break_props_parse_and_inherit() {
        let (dom, styles) = styled(
            "<div id='p' style='break-inside: avoid; page-break-before: always; orphans: 3; widows: 4'><span id='k'>x</span></div>",
            &[],
        );
        let p = by_id(&styles, &dom, "p");
        assert_eq!(p.break_inside, BreakInside::Avoid);
        assert_eq!(p.break_before, BreakAction::Page); // `always` alias
        assert_eq!(p.orphans, 3);
        assert_eq!(p.widows, 4);
        // `orphans`/`widows` inherit; the `break-*` properties do not.
        let k = by_id(&styles, &dom, "k");
        assert_eq!(k.orphans, 3);
        assert_eq!(k.widows, 4);
        assert_eq!(k.break_inside, BreakInside::Auto);
        assert_eq!(k.break_before, BreakAction::Auto);
    }

    #[test]
    fn c5_table_props_parse_and_inherit() {
        let (dom, styles) = styled(
            "<table id='t' style='border-collapse: collapse; border-spacing: 10px 20px; caption-side: bottom'><caption id='c'>cap</caption><tr id='r'><td>x</td></tr></table>",
            &[],
        );
        let t = by_id(&styles, &dom, "t");
        assert!(t.border_collapse);
        assert_eq!(t.border_spacing, (10.0, 20.0));
        assert_eq!(t.caption_side, CaptionSide::Bottom);
        // Collapse + spacing inherit to the rows…
        let r = by_id(&styles, &dom, "r");
        assert!(r.border_collapse);
        assert_eq!(r.border_spacing, (10.0, 20.0));
        // …but caption-side applies to the caption box only (not inherited):
        // the caption element itself is unset here.
        assert_eq!(r.caption_side, CaptionSide::Top);
        assert_eq!(by_id(&styles, &dom, "c").caption_side, CaptionSide::Top);

        // Single-value spacing duplicates; `%` is invalid and keeps the
        // inherited value; `separate` resets the default.
        let (dom2, styles2) = styled(
            "<table id='s' style='border-spacing: 8px'><tr><td id='d' style='border-spacing: 4%; border-collapse: separate'>x</td></tr></table>",
            &[],
        );
        let s = by_id(&styles2, &dom2, "s");
        assert!(!s.border_collapse);
        assert_eq!(s.border_spacing, (8.0, 8.0));
        let d = by_id(&styles2, &dom2, "d");
        assert_eq!(d.border_spacing, (8.0, 8.0)); // `%` rejected
        assert!(!d.border_collapse);
    }

    #[test]
    fn c6_grid_flow_and_subgrid_parse() {
        let (dom, styles) = styled(
            "<div id='g' style='grid-auto-flow: column dense'><span id='k'>x</span></div>\
             <div id='r' style='grid-auto-flow: row'></div>\
             <div id='bad' style='grid-auto-flow: sideways'></div>\
             <div id='s' style='grid-template-columns: subgrid'></div>\
             <div id='sr' style='grid-template-rows: SUBGRID'></div>\
             <div id='t' style='grid-template-columns: subgrid; grid-template-columns: 100px'></div>",
            &[],
        );
        let g = by_id(&styles, &dom, "g");
        assert!(g.grid_flow_column);
        assert!(g.grid_flow_dense);
        // Grid-flow properties do not inherit.
        let k = by_id(&styles, &dom, "k");
        assert!(!k.grid_flow_column);
        assert!(!k.grid_flow_dense);
        let r = by_id(&styles, &dom, "r");
        assert!(!r.grid_flow_column);
        assert!(!r.grid_flow_dense);
        // An unknown token invalidates the whole declaration.
        let bad = by_id(&styles, &dom, "bad");
        assert!(!bad.grid_flow_column);
        assert!(!bad.grid_flow_dense);
        // `subgrid` flags the axis and clears any track list.
        let s = by_id(&styles, &dom, "s");
        assert!(s.grid_subgrid_columns);
        assert!(s.grid_template_columns.is_empty());
        let sr = by_id(&styles, &dom, "sr");
        assert!(sr.grid_subgrid_rows);
        // A later explicit template resets the flag.
        let t = by_id(&styles, &dom, "t");
        assert!(!t.grid_subgrid_columns);
        assert_eq!(t.grid_template_columns.len(), 1);
    }

    /// I1: the rule index must never drop a rule that can match — for every
    /// element, every rule with a matching selector has to appear in
    /// `candidates`, and candidates stay in source order.
    #[test]
    fn i1_rule_index_candidates_superset_of_matchers() {
        let author = "\
            div p { color: red } \
            ul > li { color: green } \
            .card { color: blue } \
            #main { color: yellow } \
            * { color: black } \
            li:nth-child(2n) { color: purple } \
            section :is(h1, span.x) { color: teal } \
            p:not(.lead) { color: maroon } \
            div:has(img) { color: navy } \
            a:hover { color: fuchsia } \
            aside { color: olive }";
        let sheet =
            kitae_css::parse_stylesheet_environment(author, 1280.0, &EnvironmentState::default());
        let index = RuleIndex::build(&sheet.rules);
        let (dom, _) = parse(
            "<div id='main'><p class='lead'>a</p><p>b</p><ul><li>1</li><li>2</li></ul>\
             <section><span class='x'>s</span><article><img></article></section></div>\
             <div class='card'><a href='#'>l</a></div>",
        );
        let state = InteractionState::default();
        let mut chain: Vec<NodeId> = Vec::new();
        let mut implications = 0;
        fn visit(
            dom: &Dom,
            node: NodeId,
            chain: &mut Vec<NodeId>,
            rules: &[Rule],
            index: &RuleIndex,
            state: &InteractionState,
            implications: &mut usize,
        ) {
            let Some(el) = dom.element(node) else { return };
            // Ancestor presence per interned key: a count > 0 means some
            // chain element carries the key (as tag, class or id).
            let mut counts = vec![0u32; index.total_anc_keys];
            for (name, k) in &index.anc_keys {
                if chain.iter().any(|&a| {
                    let anc = dom.element(a).unwrap();
                    anc.tag == *name
                        || anc.id() == Some(name.as_str())
                        || anc.classes().any(|c| c == name.as_str())
                }) {
                    counts[*k as usize] = 1;
                }
            }
            let cand = index.candidates(el, &counts);
            for (ri, rule) in rules.iter().enumerate() {
                if rule
                    .selectors
                    .iter()
                    .any(|sel| matches(sel, dom, node, chain, state))
                {
                    assert!(
                        cand.contains(&(ri as u32)),
                        "rule {ri} can match {} but is not a candidate",
                        el.tag
                    );
                    *implications += 1;
                }
            }
            chain.push(node);
            for &c in dom.children(node) {
                visit(dom, c, chain, rules, index, state, implications);
            }
            chain.pop();
        }
        let root = dom.root().unwrap();
        visit(
            &dom,
            root,
            &mut chain,
            &sheet.rules,
            &index,
            &state,
            &mut implications,
        );
        assert!(
            implications >= 10,
            "test went vacuous: {implications} checks"
        );
        // Candidates come back ascending — source order preserved.
        let body = dom.get_element_by_id("main").unwrap();
        let el = dom.element(body).unwrap();
        let cand = index.candidates(el, &vec![0u32; index.total_anc_keys]);
        assert!(cand.windows(2).all(|w| w[0] < w[1]));
    }

    /// I1: cascade results are identical through the index — specificity
    /// and source order decide exactly as before.
    #[test]
    fn i1_indexed_cascade_matches_scan_semantics() {
        // Specificity beats order; equal specificity → later rule wins.
        let (dom, styles) = styled(
            "<div><p id='x'>t</p></div><p id='y'>u</p>",
            &["p { color: red } div p { color: green } p { color: blue }"],
        );
        assert_eq!(by_id(&styles, &dom, "x").color, 0x008000FF);
        assert_eq!(by_id(&styles, &dom, "y").color, 0x0000FFFF);
        // :is / :not / :has route through the index without loss.
        let (dom, styles) = styled(
            "<section><h2 id='h'>t</h2><p id='p1'>a</p><p class='lead' id='p2'>b</p></section>\
             <div id='d'><img></div>",
            &[
                ":is(h2, h3) { color: teal }",
                "p:not(.lead) { color: maroon }",
                "div:has(img) { color: navy }",
            ],
        );
        assert_eq!(by_id(&styles, &dom, "h").color, 0x008080FF);
        assert_eq!(by_id(&styles, &dom, "p1").color, 0x800000FF);
        assert_ne!(by_id(&styles, &dom, "p2").color, 0x800000FF);
        assert_eq!(by_id(&styles, &dom, "d").color, 0x000080FF);
        // nth-child counting intact through the index.
        let (dom, styles) = styled(
            "<ul id='u'><li id='a'>1</li><li id='b'>2</li><li id='c'>3</li></ul>",
            &["li:nth-child(2n) { color: purple }"],
        );
        assert_ne!(by_id(&styles, &dom, "a").color, 0x800080FF);
        assert_eq!(by_id(&styles, &dom, "b").color, 0x800080FF);
        assert_ne!(by_id(&styles, &dom, "c").color, 0x800080FF);
    }

    /// I1: the embedder-facing sensitivity predicate — hover/active/focus
    /// anywhere in a selector (including :not/:is/:has arguments) counts;
    /// other dynamic pseudos do not.
    #[test]
    fn i1_uses_interaction_state_detects_hover_active_focus() {
        assert!(!StyleEngine::new(&["p { color: red }"]).uses_interaction_state());
        assert!(StyleEngine::new(&["a:hover { color: red }"]).uses_interaction_state());
        assert!(StyleEngine::new(&["button:active { color: red }"]).uses_interaction_state());
        assert!(StyleEngine::new(&["input:focus { color: red }"]).uses_interaction_state());
        assert!(StyleEngine::new(&["a:not(:hover) { color: red }"]).uses_interaction_state());
        assert!(StyleEngine::new(&[":is(div, :hover) a { color: red }"]).uses_interaction_state());
        assert!(StyleEngine::new(&["div:has(a:focus) { color: red }"]).uses_interaction_state());
        // Non-interaction dynamic pseudos don't count.
        assert!(!StyleEngine::new(&["input:checked { color: red }"]).uses_interaction_state());
        assert!(!StyleEngine::new(&["li:nth-child(2n) { color: red }"]).uses_interaction_state());
    }
}
