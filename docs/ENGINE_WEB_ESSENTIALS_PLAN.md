# Kitae Web Essentials — Plan (closing the real functional gaps)

> **STATUS 2026-09-05: COMPLETE.** Every slice Q1–Q14 (minus §7 deferrals) is
> implemented and green — 404 tests workspace-wide, zero failures. Q11 web
> fonts verified visually (registered face vs Helvetica control); Q5 sticky
> verified by unit tests + wire round-trip (GUI scroll-verify deferred —
> injected events were landing on unrelated windows); Q4/Q14 covered by
> layout tests. Implementation details and deviations are recorded in
> `ENGINE_CORE_STATUS.md` §3 ("Web Essentials").

> Closes the verified gap list between "engine-core goal complete" (D–P shipped,
> see `ENGINE_CORE_STATUS.md`) and full-browser-engine behavior for the
> reader-web. Companion to `QTSQ_INTEGRATION_PLAN.md` (features ①–④) and
> `ENGINE_FEATURES_PLAN.md` (K–P).
> Slice prefix: **Q**. Gaps verified by grep audit 2026-09-04.

---

## 0. Ground rules (unchanged, from the standing decisions)

1. **No JavaScript, no WASM — ever.** The endpoint is the display list.
2. **Pure Rust first.** C++ appears only as link-in media codecs + shaping
   (`CPP_ROLE.md`). Every dependency this plan adds is pure Rust.
3. **The display list is the contract.** One version bump (v5 → v6) covers all
   paint-contract changes; slices are ordered so the bump lands once.
4. Every slice: `cargo test --workspace` green (286-test baseline grows), new
   unit tests per crate, visual verification via the screencapture recipe for
   anything the user can see.
5. This plan slots in **after QTSQ ① (password prompt, in flight) and
   ② (Save-Page-as-QTSQ)** land, or interleaves slice-by-slice if the user
   prefers.

---

## 1. Gap inventory (verified 2026-09-04)

| # | Gap | Where it bites today | Slice |
|---|---|---|---|
| 1 | `calc()` / `min()` / `max()` / `clamp()` | `calc(100% - 24px)` mis-parses or resolves wrong | Q1 ✅ |
| 2 | Static CSS `transform` (2D) | `transform` only exists inside animation sampling; no painted rotation/translation/scale | Q2 ✅ |
| 3 | `letter-spacing`, `text-transform` | ignored everywhere | Q3 ✅ |
| 4 | `flex-wrap` (multi-line flex) | single-line flex runs only | Q4 ✅ |
| 5 | `position: sticky` | approximated as relative | Q5 ✅ |
| 6 | `prefers-color-scheme` / `prefers-reduced-motion` | viewport-width is the only media condition | Q6 ✅ |
| 7 | `:is()` / `:where()` / `:nth-of-type` family | only `:not` inside @supports; nth-child only | Q7 ✅ |
| 8 | GIF pixel decode (+ animation) | headers only — GIFs render as placeholders | Q8 ✅ |
| 9 | WebP decode | unsupported → placeholder | Q9 ✅ |
| 10 | SVG images | unsupported → placeholder | Q10 ✅ |
| 11 | woff/woff2 web fonts | CoreText can't parse them; only TTF/OTF URLs work | Q11 ✅ |
| 12 | CJK/ideographic line breaking (UAX #14) | word-based wrap only; CJK runs don't wrap correctly | Q13 ✅ |
| 13 | `<iframe>` | parsed as raw text, body leaks into page | Q14 ✅ |
| 14 | AVIF, `:has()`, container queries | — | deferred (§7) |

---

## 2. Phase Q-A — CSS completeness (no new dependencies)

### Q1 — `calc()` / `min()` / `max()` / `clamp()`
- **Do:** `Length` gains a sum-tree form: `Length::Calc(Vec<CalcTerm>)` where a
  term is `px | em | rem | % (+/-)` and `Length::Fn(min|max|clamp, Vec<Length>)`.
  Parse in `kitae_css::parse_length` (function-aware declaration splitting —
  today `parse_declarations` splits on `;` only, but shorthand paths split on
  spaces/commas; audit those). Resolution stays two-stage exactly like today:
  em/rem resolve at style time against the parent font-size; `%` resolves at
  layout time against the containing block. `min/max/clamp` evaluate after
  their arguments resolve.
- **Files:** `kitae_css` (Length type, parser, Display), `kitae_style`
  (resolve call sites), `kitae_layout` (percent-basis resolution at use sites:
  width/height/margin/padding/inset/flex-basis/gap).
- **Scope guard:** only the properties listed resolve calc; everything else
  keeps today's behavior (documented).
- **Tests:** nested `calc(calc() + 1px)`, `var()` inside calc, `%`+`px` mixes
  against a fixed-width container, min/max/clamp edge evaluation; existing
  286 untouched.
- **Acceptance:** `width: calc(100% - 24px)` inside a padded parent lays out
  identical to an explicit pixel equivalent.
- **Est:** 4–6 days. Risk: the Length type ripple — contained by keeping
  resolved-px storage in `ComputedStyle` and resolving late in layout only for
  the listed properties.

### Q2 — static CSS `transform` (2D)
- **Do:** `ComputedStyle.transform: Option<TransformList>` (translate/
  translateX/Y, scale/scaleX/Y, rotate, skewX/Y, matrix; `%` = border box).
  Layout applies post-layout: the element's own paint items (Rect, TextRuns,
  DisplayImage) carry a 2×3 matrix. v1 composes transforms down the tree;
  `transform-origin` supported (percent/keywords).
- **DisplayList:** `DISPLAY_LIST_VERSION` 5 → **6** — Rect/TextRun/DisplayImage
  gain an optional non-identity matrix (sparse: flag-bit + 6 f32s only when
  present). This slice owns the bump; Q8 rides it (see sequencing).
- **Renderers:** kitae_render (SDL: rect paths via transformed quad edges;
  text via per-run CTM), kitae_render_gpu (instance matrix in the quad
  pipeline + glyph quads), Swift `PageCanvasView` (CGContext CTM per item
  group), Metal path passes the matrix into the instance buffer.
- **Tests:** parser cases; layout emits matrix for transformed subtree only;
  GPU/CPU parity with a rotated box (extends the existing
  `gpu_matches_cpu_reference` gate); visual verify (rotated card in Kitae.app).
- **Acceptance:** `rotate(45deg)` + `translate(10px, 20%)` render identically
  in SDL, Swift CPU, Swift GPU.
- **Est:** 1.5–2 wk. Biggest paint-contract change in the plan — scheduled
  alone, after the quick wins.

### Q3 — `letter-spacing` + `text-transform` (quick win)
- **Do:** `text-transform` (uppercase/lowercase/capitalize/none) applied at
  text collection in layout **before** measuring, so wrapping widths are
  correct. `letter-spacing` threads through the `TextMeasurer`/`Shaper` seam
  (`measure(text, size, weight, monospace, spacing)`), added per character
  after the last-glyph advance (CSS: no trailing spacing); `WidthCache` key
  gains the spacing field.
- **Decision recorded:** find-in-page indexes the **transformed** text (what
  the user sees). Browsers match source text; revisit if it ever matters.
- **Tests:** transformed wrap widths; spacing ≠ 0 vs WidthCache hit;
  capitalized headings; pre blocks unaffected by text-transform.
- **Est:** 2–3 days.

### Q4 — `flex-wrap`
- **Do:** two-axis flex: wrap items into lines by hypothetical main size when
  `flex-wrap: wrap|wrap-reverse`; each line flexes independently (grow/shrink
  per line); `align-content` distributes leftover cross space (default
  stretch). Extends the existing two-pass measure→paint flex engine.
- **Tests:** wrapped rows at fixed container width, wrap-reverse order,
  align-content: flex-start/center/space-between, line cross-sizing with
  tallest item.
- **Est:** 3–4 days.

### Q5 — `position: sticky`
- **Do:** stop approximating sticky as relative. Layout keeps the element in
  flow but emits a **StickyItem** side-list entry
  `{node, base_y, container_top, container_bottom, offset}`. The engine
  applies stickiness at **frame-publish time** using the current `scroll_y`
  (clamp base_y into `[container_top + offset, container_bottom - height -
  offset]`) — publish-without-relayout already exists, so sticky costs zero
  extra layouts while scrolling. `left/right` horizontal sticky: out of v1
  (vertical-only, documented).
- **Files:** `kitae_style` (parse sticky → own `Position` variant),
  `kitae_layout` (side-list + containment bounds from nearest scroll
  ancestor), `kitae_core` (publish-time translate).
- **Tests:** sticky header inside a tall container clamps at both ends;
  resize re-derives ranges; scroll + republish moves the item without
  relayout (repaint counter unchanged).
- **Est:** 3–5 days.

### Q6 — `prefers-color-scheme` / `prefers-reduced-motion` / `prefers-contrast`
- **Do:** `eval_media_condition` matches named features with values
  (`(prefers-color-scheme: dark)` etc.). Style engine gains an
  `EnvironmentState { color_scheme: Auto|Light|Dark, reduced_motion: bool,
  contrast: Normal|More }` — **the app already has exactly these settings**
  (appearance system/light/dark + reduce-motion in SettingsWindow), so this
  wires real pages to the existing toggles. Engine re-computes styles on
  environment change riding the N6 `compute_with_state` machinery; new FFI
  command 24 `KITAE_CMD_SET_ENVIRONMENT` (bits: scheme, motion, contrast).
  Auto = follow `NSApp.effectiveAppearance` on the Swift side.
- **Tests:** media feature evaluation matrix; style recompute on environment
  flip (hover-style invalidation precedent); UA dark palette unaffected.
- **Acceptance:** a page with `@media (prefers-color-scheme: dark)` re-renders
  when the app's appearance setting changes, no reload.
- **Est:** 2–3 days.

### Q7 — `:is()` / `:where()` + `:nth-of-type` family
- **Do:** selector parser accepts functional selector lists
  (`:is(a, .b > c)`, `:where(...)`); specificity: `:is()` = its most specific
  argument, `:where()` = zero. Matching: any-argument match with the existing
  combinator matcher. `:nth-of-type/:first-of-type/:last-of-type/:only-of-type`
  ride the existing `parse_nth` + sibling scan (same machinery as
  `:nth-child`).
- **Tests:** specificity precedence (`:where` never counts), `:is` list with
  combinators, of-type variants against mixed sibling lists.
- **Est:** 3–4 days.

---

## 3. Phase Q-B — real-web media & fonts (all-new deps, all pure Rust)

### Q8 — GIF still decode + first-class animation
- **Dep:** `gif` crate (pure Rust, maintained).
- **Do:** `kitae_media::decode_image` dispatches GIF magic → composited first
  frame → RGBA (joins the PNG/JPEG path). `decode_frames(bytes)` returns
  `Vec<Frame { rgba, delay_ms }>` with proper disposal-method compositing and
  loop count. Guards: `MAX_PIXELS × frame_count`, frame cap (≤512), duration
  floor 20 ms per frame.
- **Wire (rides v6):** raster cache stores frame 0; new C ABI
  `kitae_raster_frames(id) -> (count, delays…)` + `kitae_raster_frame(id, n,
  buf)` mirroring the existing `kitae_qtsq_raster` pull. `DisplayImage` gains
  `animated: bool`. The **embedder drives timing** (N8 rule: no engine
  timeline) — Swift presents frames on a CADisplayLink-paced timer, SDL steps
  on a fixed tick.
- **Tests:** still GIF decode; disposal restore-to-background/previous cases;
  delay table; frame-cap guard; Swift timer presents ≥2 frames (visual).
- **Est:** 4–6 days.

### Q9 — WebP (static)
- **Dep:** `image-webp` (pure Rust decode, lossy VP8 + lossless).
- **Do:** RIFF/WEBP magic → decode_image; dimensions from the VP8/VP8L
  header. Animated WebP → first frame, documented.
- **Est:** 1–2 days.

### Q10 — SVG images (static, safe subset)
- **Dep:** `resvg` (usvg + tiny-skia, pure Rust). **Decision flagged:** this is
  the largest dependency the workspace has taken (roxmltree, tiny-skia, …);
  it renders statically with no script/SMIL execution **by construction**
  (usvg drops them at parse), which keeps the no-JS purity auditable.
  Hand-rolling a minimal SVG renderer was rejected: weeks of work, poor
  fidelity.
- **Do:** `kitae_media::decode_svg(bytes, target_w, target_h) -> RGBA` (raster
  at requested size × scale; cache keyed `(svg_id, w, h)` in the raster
  cache); dimensions from the usvg tree (`viewBox`/`width`/`height`).
  `<img src=…svg>` and `background-image: url(…svg)` (via the existing
  `BackgroundPaint::Image`) both route through it.
- **Tests:** viewBox-only sizing, explicit px sizing, `<img>` + background
  paths, a known-gradient fixture rendered to expected average color.
- **Acceptance:** an SVG logo renders crisply at two different layout sizes.
- **Est:** 3–4 days.

### Q11 — woff/woff2 web fonts
- **Today:** `@font-face` fetches the URL and registers the file with
  CoreText — TTF/OTF work, **woff/woff2 (what the web actually serves)
  silently fail**.
- **Do:** decompress at fetch, register in-memory: WOFF1 (per-table zlib —
  trivial) then WOFF2 (brotli via the pure-Rust `brotli` crate + table
  transform reconstruction per the WOFF2 spec: `glyf`/`loca`/`hmtx`
  transforms, collection handling). Reconstructed sfnt stays in memory →
  register via `CGDataProvider` + `CGFont` (no temp files), feeding the
  existing `kitae_core` registry path (`CTFontManagerRegisterFontsForURL` is
  URL-scoped; the CGFont path avoids it).
- **Stages:** 11a WOFF1 (1 day) → 11b WOFF2 untransformed tables (2 days) →
  11c transformed glyf/loca (2–3 days). A font that fails reconstruction
  falls back to the next `src:` entry (spec behavior).
- **Tests:** round-trip a known TTF→woff2 fixture, glyph advance equality
  against the source TTF; malformed woff2 falls back cleanly.
- **Est:** 5–8 days total.

---

## 4. Phase Q-C — text & misc

### Q13 — UAX #14 line breaking (CJK + kinsoku)
- **Dep:** `unicode-linebreak` (pure Rust, generated UAX #14 tables).
- **Do:** `kitae_shape` gains `line_breaks(text) -> impl Iterator<(usize,
  BreakOpportunity)>`; kitae_layout's wrap path switches from word-splitting
  to break-opportunity segmentation — Latin words stay whole segments, CJK
  breaks per character, kinsoku (never break before `、。」`, never after
  `「`) falls out of the UAX #14 classes automatically. Feeds both the normal
  inline path and `feed_pre` wrap mode. WidthCache segments by break
  opportunity.
- **Golden gate:** pure-Latin text produces **byte-identical** display-list
  output to today's word path (regression test before switching).
- **Est:** 4–6 days.
- **Acceptance:** a CJK paragraph wraps character-wise without breaking
  before closing punctuation; mixed Latin/CJK lines break correctly at both
  kinds of boundaries.

### Q14 — `<iframe>` placeholder box (optional, decision point)
- **Do:** stop leaking iframe raw-text into the page; render a bordered
  placeholder box (height 150, "embedded frame — <src host>" label) in the
  display list. Nested browsing contexts stay **out of scope** (they are a
  whole engine subsystem, not a gap this plan should paper over).
- **Est:** 1–2 days. User decides keep-or-drop.

---

## 5. Sequencing

| Order | Slice | Why here | Wire impact |
|---|---|---|---|
| 1 | Q3 letter-spacing/text-transform | quick win, unblocks real stylesheets | none |
| 2 | Q1 calc/min/max/clamp | highest CSS frequency | none |
| 3 | Q6 prefers-* | app settings already exist — product synergy | FFI cmd 24 |
| 4 | Q7 :is/:where + of-type | selector completeness | none |
| 5 | Q13 UAX #14 breaks | fixes all CJK wrapping; golden-gated | none |
| 6 | Q9 WebP | tiny, independent | none |
| 7 | Q2 transform | owns the v5→v6 bump; schedule alone | **v6** |
| 8 | Q8 GIF + animation | rides the same v6 bump | **v6** |
| 9 | Q10 SVG | big dep, needs its own review | none |
| 10 | Q5 sticky | publish-time logic, no wire change | none |
| 11 | Q11 woff/woff2 | staged 11a→11c | none |
| 12 | Q4 flex-wrap | self-contained layout work | none |
| 13 | Q14 iframe placeholder | optional, user decision | none |

**Milestones:**
- **M-Q1 (CSS quick wins: Q3+Q1+Q6+Q7)** — real stylesheets land visibly
  better. ~2 wk.
- **M-Q2 (text: Q13)** — reader is CJK-ready. ~1 wk.
- **M-Q3 (media: Q9+Q2+Q8, one v6 bump)** — GIF/WebP/transform with a single
  wire change. ~3–3.5 wk.
- **M-Q4 (Q10+Q5+Q11+Q4)** — SVG, sticky, web fonts, multi-line flex. ~2.5 wk.

**Total:** ~7.5–9.5 wk solo — same scale as the J–P plans.

---

## 6. New dependencies (policy audit)

| Crate | Purpose | Policy check |
|---|---|---|
| `gif` | GIF still + animation decode | pure Rust ✅ |
| `image-webp` | WebP (VP8/VP8L) decode | pure Rust ✅ |
| `resvg` (+usvg, tiny-skia) | static SVG raster | pure Rust ✅, no script execution by construction |
| `unicode-linebreak` | UAX #14 break tables | pure Rust ✅ |
| `brotli` | WOFF2 decompression | pure Rust ✅ (keeps C++ at "media codecs only") |

No JS engines, no WASM targets, no C++ additions.

---

## 7. Deferred (deliberate)

- **AVIF** — only realistic decode is dav1d (C); reader-web AVIF ships JPEG
  fallbacks. Revisit on demand.
- **`:has()`** — upward matching breaks the top-down cascade assumption;
  needs a matching-strategy redesign of its own.
- **Container queries** — second layout dependency; after flex-wrap/containment
  maturity.
- **3D transforms, filters/backdrop-filter, blend modes** — paint-stack work
  beyond v1 transforms; separate plan if needed.

---

## 8. Acceptance gates (whole plan)

1. `cargo test --workspace` green after every slice (286-test baseline grows;
   render_gpu stays in the run).
2. Latin line-breaking output unchanged (golden gate on Q13).
3. Single display-list bump v5→v6 landing with Q2+Q8 together; Swift +
   SDL + GPU all updated in the same slice that bumps.
4. Visual verification (screencapture recipe) for every user-visible slice:
   calc layout, rotated elements, dark-scheme flip, GIF/SVG/WebP pages,
   sticky scroll, CJK paragraphs, web-font pages.
5. `docs/ENGINE_CORE_STATUS.md` updated per milestone; this doc's inventory
   table annotated with ✅ as slices land.
