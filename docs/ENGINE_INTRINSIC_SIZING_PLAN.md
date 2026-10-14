# C1 — Intrinsic sizing: min-content / max-content / fit-content

Parity plan item **C1** (bucket C, L-size, "unlocks flex/grid correctness").
Written 2026-09-08 after the G3 wrap-up. Status: **SHIPPED 2026-09-09** —
C1a/C1b/C1c landed with the rest of bucket C (tests `c1_*`/`c1b_*` in
`kitae_layout`, `c1_intrinsic_keyword_cascade`/`c1b_grid_intrinsic_minmax_tracks_parse`
in `kitae_style`, `c1_parse_size_val_*` in `kitae_css`; see the §C table in
`BROWSER_PARITY_PLAN.md` for the scope + deviations).

## 0. Survey (verified 2026-09-08)

- **One touchpoint today**: `kitae_style/src/lib.rs:4815` — the grid-template
  parser collapses `min-content`/`max-content` into `GridTrack::Auto`.
  `fit-content` appears nowhere.
- **`Len`** (`kitae_css:1904`) is a pure numeric type (`Px`/`Sum`/`Min`/`Max`/
  `Clamp`) — keywords don't fit it. ComputedStyle size fields are
  `Option<Len>` (`kitae_style:713–718`); `None` = auto.
- **Width parsing** (`kitae_style:3098–3125`): `width`/`height`/`min-*`/`max-*`
  via `len(v)`/`len_px(v)`; unknown tokens (`min-content`) already fall to
  `None` → behave as auto. So today the keywords are silently auto.
- **Read sites are few**: `style.width` is consumed at exactly two places in
  `kitae_layout` — `:1634` (block width: `resolve(avail).min(avail)`) and
  `:4636` (replaced-element path). `flex_basis` reads at `:2484/:2492/:2664`.
  Grid track sizing at `:3665–3700` (`Fixed`/`Flex`/`Auto` arms; auto tracks
  sized by `content_main_size` + padding).
- **Measurement infra exists**: `self.tm.measure_family(text, size, weight,
  italic, mono, family)` measures a string's full width; `content_main_size`
  (`:3837`) sums all subtree text (crude max-content); `collect_text` (`:3853`)
  walks a subtree with paint styles. UAX #14 line breaks exist (Q13) but the
  measurer path here is whitespace-based.

## 1. Design

### 1.1 Representation (kitae_css)

```rust
/// A `width`/`height` value: a length or an intrinsic keyword.
pub enum SizeVal {
    Len(Len),
    MinContent,
    MaxContent,
    /// `fit-content` keyword = clamp(min-content, available, max-content);
    /// `fit-content(<len>)` = min(<len>, max-content) floored at min-content.
    FitContent(Option<Len>),
}
```

- `ComputedStyle.width`/`height`: `Option<Len>` → `Option<SizeVal>`
  (`None` still = auto). **v1 scope: keywords on `width`/`height` only** —
  `min-*`/`max-*` keep `Option<Len>` (keyword there is rare; ignore).
- Parser: in the `"width"`/`"height"` arms, match the lowercase token for
  `min-content`/`max-content`/`fit-content` (+ `fit-content(...)` via the
  paren-aware length splitter from R5/calc) before falling through to `len(v)`.
- `SizeVal::resolve(self, avail, min_w, max_w) -> f32` helper takes the
  measured intrinsic widths as inputs at the layout call site (the enum itself
  can't measure).

### 1.2 Measurement (kitae_layout) — the real work

Two recursive functions over DOM + computed style, no layout side effects:

```rust
fn min_content_width(&self, node: NodeId) -> f32  // narrowest without overflow
fn max_content_width(&self, node: NodeId) -> f32  // widest without wrapping
```

Rules (v1):
- **Text**: max-content = measure whole run (whitespace-collapsed per
  `white-space`); min-content = measure longest word (split on spaces; CJK
  per-grapheme via UAX #14 = deferral).
- **Inline flow** (children of a block): max-content = Σ children max-content
  (single unwrapped line); min-content = max over children min-content
  (approximation: inline boxes can't break internally).
- **Block flow**: both = max over children (each child gets full width).
- **Replaced** (`img`): natural raster width from the cache, else `width`
  attr, else 0; `min`/`max` = same (images don't reflow).
- **Box chrome**: add resolved padding + visible border widths; margins are
  NOT included (they're outside the width).
- **Flex/grid containers**: v1 approximates as block (max of children);
  refined by C1b/C1c once track/item measurement exists.
- **`display:none`** subtrees skipped (reuse `collect_text`'s walk shape).
- Memoize per node within a layout pass (a `HashMap<NodeId, (f32, f32)>` on
  `Layout`) — the same subtree is measured by parent + child passes.

### 1.3 Application

- **Block width** (`:1634`): match `style.width` —
  `MinContent => min_content_width(node)`, `MaxContent => max_content_width`,
  `FitContent(arg) => max(min_c, min(arg.resolve(avail) or avail, max_c))`;
  then clamp by `min_width`/`max_width` and cap at `avail` as today.
- **Block height**: same keywords via `min/max_content_height` = the height
  the subtree takes at the resolved width — v1: only `MaxContent` height is
  honest (single line); `MinContent` height = line count at min-content width
  × line-height. If this gets hairy, ship width-only and note height as a
  follow-up (width is what flex/grid correctness needs).
- **Grid** (C1b): `GridTrack` gains `MinContent`/`MaxContent`/`FitContent(Option<Len>)`;
  parser at `kitae_style:4815` stops collapsing; sizing at `kitae_layout:3665+`
  measures span-1 items with the new fns (auto tracks keep current behavior).
- **Flex** (C1c): `flex-basis: min-content|max-content|fit-content` →
  `SizeVal`; item base size = measured intrinsic width instead of content
  sum at `:2484+`.

## 2. Slices

| Slice | Scope | Eff | Tests |
|---|---|---|---|
| **C1a** | `SizeVal` + parse + `min/max_content_width` + block width application + memo | M | style: parse 4 keyword forms; layout: text block min = longest word, max = full line; nested block = widest child; img natural size; padding/border added; fit-content clamp; `width:min-content` inside a narrow parent caps at avail |
| **C1b** | Grid track variants + item measurement in track sizing | S–M | `grid-template-columns: min-content 1fr max-content` golden widths; fit-content(100px) track |
| **C1c** | flex-basis keywords + intrinsic contribution | S | `flex: 0 0 min-content` item width = longest word |

No wire bump — purely engine-side. Completion bar per standing rule:
`cargo test --workspace --exclude kitae_render_gpu` green + release libs +
Kitae.app rebuilt (and **verify the release build log for `error` lines —
the `| tail` pipeline masks cargo's exit code; see the stale-.a lesson**).

## 3. Deferrals (recorded, not silently dropped)

- `min-width`/`max-width`/`height` keywords beyond v1 (height keywords may
  ship width-only in C1a if the line-count math is ugly).
- CJK-aware min-content (per-grapheme breaks via the Q13 UAX #14 tables).
- Intrinsic contribution of spanning grid items (multi-track distribution) —
  v1 measures span-1 only, same restriction as today's auto tracks.
- `fit-content()` inside min/max shorthand contexts; table-cell intrinsic
  sizing (tables have their own path); writing-mode interactions (B4).

## 4. Gate

`kitae_layout/src/lib.rs` and `kitae_style/src/lib.rs` are user WIP (math_walk
09:01–09:07). Before any edit: poll `stat -f "%m"` until stable AND
`cargo check -p kitae_layout -p kitae_style` clean, then strictly targeted
Edits with re-reads. The G3 app rebuild (blocked on the same WIP) rides
along once the zone clears.
