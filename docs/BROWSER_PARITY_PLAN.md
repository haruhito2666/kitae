# Kitae Browser Parity Plan — implementation census and remaining gates

> **Status: REFRESHED 2026-09-25 — all 74 A–I implementation rows are
> shipped.** The A11 visual range-selection/IME tail and B3 full
> scroll-semantics tail are now implemented. Manual GUI verification and
> external release work remain separate gates; implementation completion is
> not GUI or release sign-off. Wire is v20; v20 adds the per-media-item
> `controls` byte used by native web-video chrome.
> Written 2026-09-05 as an itemized census of everything left to make Kitae
> comparable with V8/Chrome/Chromium: **74 concrete items in 9 buckets
> (A–I)** (the original "75" was an arithmetic slip — the bucket tables sum
> to 74), effort-sized,
> with the two buckets that are *not* on the list (the JS platform —
> permanently out by decision — and the conformance long tail) stated
> explicitly.
> Baseline at writing: 439 tests green, ~49k authored lines (39.6k Rust +
> 9.2k Swift), Web Essentials (Q1–Q14) + HTML/CSS completion (R1–R8)
> shipped. **Current validation checkpoint (2026-09-25):** serialized and
> default-parallel full workspace sweeps pass 1149 tests (3 ignored); 9 native
> contract tests, the app bundle, workspace check/formatting, and dependency
> checks pass. The dated checkpoints below preserve the earlier census.
> At the 2026-09-08 checkpoint, batches
> 1+2 + A1 + A11a/A11b + G3 shipped, the finish pass
> closed A6/A7/A10/A12 (`::backdrop`+modal focus, range interaction,
> `pattern`+bubbles, 2D MathML `math_walk`) and shipped A11b
> contenteditable editing; the 2026-09-08 CSS pass landed B4
> (writing-mode + logical flow), B5 (multi-column), B7+B8 (clip-path/
> mask + blend/isolation, wire v18), B9's ch/ex measurer tail, and
> B11–B15 — that checkpoint left only A11's range-selection/IME tail and
> B3's live-GUI scroll-verify (58/74 shipped; buckets A–G were closed to their
> then-known tails) +
> all eight of the RP-B1..B8 UI-library slices shipped
> end-to-end at the engine level (see
> `UI_LIBRARY_PLAN.md` §Track B status). Scoreboard in §0.5.
> **2026-09-09:** buckets C and D closed in single passes, then bucket E
> (8 items, first bucket with the no-exclusion sweep), **bucket F
> (5 items — F1 symphonia audio, F2/F3 MP4+WebM demux with host-side
> VideoToolbox decode per the approved Option 2, F4 in-page video chrome,
> F5 WebVTT captions; wire v20)** closed the same day.
> **2026-09-10: bucket G (8 items — G1 HTTP/2 + `Response.protocol`,
> G2 HTTP/3/QUIC via quinn+h3 on ring, G4 Basic/Digest auth + credential
> provider, G5 range/resumable downloads with progress hooks, G6 RFC 6455
> WebSockets, G7 SSE, G8 preconnect/prefetch + RFC 9218 priorities; all
> in `kitae_net`, no wire bump) closed — Phase 2 is fully done.**
> Companion docs: `ENGINE_CORE_STATUS.md` (progress record),
> `ENGINE_HTML_CSS_COMPLETION_PLAN.md` (shipped gap-closing),
> `UI_LIBRARY_PLAN.md`, `BRIDGE_FEATURE.md`,
> `MEDIA_CODECS_DECISION.md` (B7 decision doc).
> **2026-09-06:** the UI-library roadmap (`UI_LIBRARY_PLAN.md`, "React
> parity" section) enumerates the subset of these items that gate
> building complex/visual sites with kitae_ui — the RP-B1..B8 slices
> (transitions, overflow panes, :has()/container queries, filters, grid
> completion, text completion, media codecs, 3D rendering audit) are the
> priority order for *this* plan's CSS/layout buckets. Note: Grid,
> gradients, box-shadow, @keyframes, and fixed/sticky were verified
> present in the tree on 2026-09-06 and are NOT on the RP-B list.
> **2026-09-07:** Track B (RP-B1..B8) shipped. B2/B3/B4 added wire
> sections v16 (per-item filter chains) and v17 (scroll containers).
> B6 limited to ch/ex parse + measurer-fallback document; writing-mode
> and multi-column remained open at that checkpoint. B7's decision doc
> presented Option 2; it was implemented in the 2026-09-08 CSS pass.
> **2026-09-08 (CSS pass):** bucket B implemented to completion except
> B3's live-GUI verification: B4 + B5 + B7 + B8 + B9(tail) + B11–B15
> landed; frame wire now v18 (B7 clip/mask + B8 blend/isolation share
> the sparse per-item section). Baseline 620 → 735.
> **2026-09-09 (bucket C):** C. Layout complete — C1 intrinsic sizing
> (`ENGINE_INTRINSIC_SIZING_PLAN.md`), C2 `aspect-ratio`, C3 fragmentation
> (forced breaks consumed by multicol; paged media deferred), C4 abs
> static position, C5 table polish, C6 true CSS §8.5 sparse flow +
> `dense` + column-subgrid v1. Baseline 735 → 767; no wire bump.
> Deviations recorded per-row in the §C table.
> **2026-09-09 (bucket D):** D. Text & fonts complete — D1 shaping
> audit + combining-mark fallback fix, D2 ZWJ emoji clusters, D3
> `lang`/`:lang()`, D4 `font-feature-settings`/`font-variant*`, D5
> hyphenation dictionaries (en-us embedded), D6 `text-orientation`
> vertical glyph stance. Baseline 767 → 789; no wire bump.
> Deviations recorded per-row in the §D table.

## 0. Scope framing

"Comparable" is scoped honestly:

- **In scope**: every feature whose behavior is expressible without script —
  rendering, layout, text, network, media decode, and browser-layer UX the
  engine or app can own.
- **Out by standing decision** (not counted as gaps): the JS platform — V8
  itself and every API that exists only as a script surface. Any future
  interactivity goes through kitae_ui/kitae_live's declarative patch
  protocol, never WASM or JS.
- **Not listable**: the conformance/performance long tail (WPT has millions
  of assertions; item I5 is the programmatic version of it).

Effort legend: **S** ≤ half a slice · **M** = 1–2 slices · **L** ≥ 3 slices
(a "slice" ≈ one focused session of implementation + tests, the working unit
of Q/R plans).

## 0.5 Progress scoreboard (updated 2026-09-25)

| Bucket | Done | Partial | Open | Left |
|---|---|---|---|---|
| A. HTML & DOM (12) | 12 | 0 | 0 | — (A11 range selection/IME implemented; manual GUI check remains a release gate) |
| B. CSS (15) | 15 | 0 | 0 | — (B3 nested scrolling implemented; manual GUI check remains a release gate) |
| C. Layout (6) | 6 | 0 | 0 | — (closed 2026-09-09; per-row deviations below) |
| D. Text & fonts (6) | 6 | 0 | 0 | — (closed 2026-09-09; per-row deviations below) |
| E. Rendering/GPU (8) | 8 | 0 | 0 | — (closed 2026-09-09, first bucket with the full workspace sweep incl. `kitae_render_gpu`; per-row deviations below) |
| F. Media (5) | 5 | 0 | 0 | — (closed 2026-09-09; per-row deviations below — F2's AV1 piece stays open as a future slice, VideoToolbox has no AV1 decoder on this OS target) |
| G. Network (8) | 8 | 0 | 0 | — (closed 2026-09-10; per-row deviations below — zstd deferred (C dep), the downloads UI is H1, the credential-prompt UI is H6, remote H3 needs a public host) |
| H. Browser layer (8) | 8 | 0 | 0 | — (closed 2026-09-10; AX semantics extended 2026-09-25; per-row deviations below) |
| I. Perf & robustness (6) | 6 | 0 | 0 | — (closed 2026-09-10; I5 is a recorded *baseline* scorecard, not high conformance: tokenizer 100.0%, tree 60.1%, all three CSS subsets 100.0%) |
| **Total (74)** | **74 ✅** | **0 🚧** | **0 ⏳** | **manual GUI, conformance long tail, and release gates remain separate** |

The earlier effort-weighted estimate was **~94.5 of ~97 units (≈97%)**;
that is historical sizing, not the current row census. The current census is
**74/74 implementation rows shipped**: Phase 1 is 39/39, Phase 2 E/F/G is
21/21, and Phase 3 H/I is 14/14. This does not turn the documented per-row
deviations or the manual GUI/release gates into completed work.

## 0.6 Current cross-cutting implementation (2026-09-25)

These shipped changes are not additional A–I census rows:

- **Frame wire v20:** the media-item `controls` byte and eight web-video C
  exports support native `<video controls>`; QTSQ viewer media keeps its
  separate docked transport path.
- **Settings, privacy, and HSTS:** Settings → Privacy is wired to every
  engine through FFI policy/search commands. HTTPS-only navigation, the
  insecure-navigation confirmation, DNT, third-party-cookie blocking, and
  the selectable search provider are implemented. App defaults are HTTPS-only
  off, insecure-HTTP warning on, third-party-cookie blocking on, and DNT on.
  HTTPS responses populate a capped HSTS store (expiry,
  `includeSubDomains`, subdomain matching, and Page Info deletion); HSTS
  snapshots are persisted in Application Support, with no preload list.
- **Deep links:** `Info.plist` registers `kitae`; `kitae://history` and
  `kitae://open?url=…&newTab=…` are handled, with only HTTP(S)/approved
  `about:` targets accepted.
- **SPA query handling:** `kitae_live` preserves the request path *and query*
  for SPA seeds; `Route::spa`, `request_path()`, and `kitae_ui::Router` provide
  in-page navigation/history plus decoded route/query parameters. This is
  renderer state, not native address-bar/history synchronization; Redirect
  frames remain the full-page redirect mechanism.
- **A11 editing finish:** form controls and plain-text `contenteditable` hosts
  now carry anchor/head selection ranges, point selection, Shift-range caret
  movement, visual highlight geometry, replacement-aware typing/deletes, and
  marked-text composition update/commit/cancel through FFI commands 43–51.
  `PageCanvasView` is the AppKit `NSTextInputClient`; the clean display list
  remains storage-safe and decoration is non-accumulating. `contenteditable`
  is still the documented plain-text v1 model, not structured rich text.
- **B3 nested scrolling:** layout records horizontal/vertical content extents,
  `auto`/`scroll` mode, and nearest scroll parent in the v20-compatible tail;
  `PageCanvasView` keeps x/y offsets, clips/translates cumulative ancestors,
  chains wheel remainders, drags both axes, and applies scroll context before
  per-item transforms. Pages with nested containers bypass GPU tile
  presentation. Manual GUI validation is still open.
- **AX semantics:** the tree now maps 44 stable roles (wire ids 0–43),
  associated labels, values/min/max, checked/expanded/selected/disabled,
  heading levels, and live politeness. The host maps these into AppKit
  semantics, synthesizes press actions plus slider/spin-button increments,
  announces polite/assertive
  live changes, and rebuilds frames in nested-scroll screen coordinates. A
  native contract pins the optional v20 `AXS1` tail and its legacy-frame
  fallback. This is scoped AX support, not a claim of full platform AX
  conformance; manual AX verification remains open.
- **QTSQ raster/video caches:** QTSQ and web images share a final-URL-keyed
  cache with stable payload IDs, 64-entry/192 MiB eviction, newest pinning,
  and purge; QTSQ video decoder/source state is separately capped at eight
  entries/64 MiB.
- **macOS 15 target:** the Makefile exports `MACOSX_DEPLOYMENT_TARGET=15.0`,
  the bundle declares `LSMinimumSystemVersion=15.0`, and the current arm64
  executable reports `minos 15.0`. Native contract tests compile for
  `arm64-apple-macos15.0`; this does not complete clean-machine or
  distribution validation.

## Phase 1 — Reader-web rendering parity (A/B/C/D ≈ 2–3 months)

Priority order within the phase: B3 first (biggest real-page win), then
C1 (unlocks flex/grid correctness), then B1+B2 together (shared cascade
pre-pass work). *2026-09-25 update: all 39 Phase 1 implementation rows are
shipped, including the A11 range-selection/IME and B3 full scroll-semantics
finishes. Manual GUI verification remains a separate release gate.*

### A. HTML & DOM — 12 items

| # | Item | Note | Eff | Status |
|---|---|---|---|---|
| A1 | Full-spec HTML5 tree construction | Adoption-agency algorithm, complete "in body" insertion modes, spec-exact foreign content. | L | ✅ 2026-09-06 (html5lib battery green) |
| A2 | `<template>` content semantics | Inert document fragment; matters for preparsing templates. | S | ✅ 2026-09-06 |
| A3 | `<picture>` + `srcset`/`sizes` | Pick the best candidate at DOM resolve time. | M | ✅ 2026-09-06 (`sizes` deferred) · 2026-09-26: the page's `EnvironmentState` now reaches layout, so `<source media>` is evaluated against the live media type and `prefers-*` answers (print runs select their print sources); the `<img>`'s own `media` attribute is honored as a gate on the element (wire unchanged — still v20) |
| A4 | `<colgroup>`/`<col>` column sizing | Parsed today, ignored by table layout. | M | ✅ 2026-09-06 |
| A5 | `<details>`/`<summary>` toggle | Engine owns the click → toggle → relayout (no JS needed). | M | ✅ 2026-09-06 |
| A6 | `<dialog>` incl. `::backdrop` | `open` attribute rendering + top-layer concept. | M | ✅ 2026-09-08 (card + `::backdrop` + modal focus/Tab-trap + Esc stack + backdrop click gate) |
| A7 | Form control UIs: date/time, color, range, file | Visual + interaction, engine-side. | L | ✅ 2026-09-08 (slider/swatch/pill chrome + type-aware sizes; range track-click + focused stepping) |
| A8 | `<progress>`/`<meter>` rendering | | S | ✅ 2026-09-06 |
| A9 | `<datalist>`, `<output>` | | S | ✅ 2026-09-06 |
| A10 | Constraint validation UI | `required`/`pattern`/type-mismatch messages. | M | ✅ 2026-09-08 (live `:invalid`/`:valid` + UA error border + submit gate + validation bubble; `pattern` via a regex-subset engine) |
| A11 | Editing: `contenteditable`, caret/selection rendering, IME | The biggest no-JS interactive feature left; needs a real editing engine. | L | ✅ 2026-09-25 (A11a form-control caret/typing + A11b plain-text `contenteditable`; A11c anchor/head and point selection, non-accumulating highlight rects, Shift-range movement, replacement typing/deletes, and AppKit marked-text update/commit/cancel are implemented. Contenteditable remains the documented plain-text v1 model; manual GUI validation is still required.) |
| A12 | MathML | Reads as foreign content today; renders nothing. | L | ✅ 2026-09-08 (linearized tokens + 2D `math_walk`: mfrac/msqrt/mover/munder/msub/msup/msubsup/mrow, semantics) |

### B. CSS — 15 items

Track B of `UI_LIBRARY_PLAN.md` (RP-B1..B8, shipped 2026-09-07) covered
B1/B2/B3/B6/B10 fully and B9 partially — see §Track B status there. The
2026-09-08 CSS pass closed the rest of this table; the 2026-09-25 host pass
closed B3's remaining scroll-metadata and interaction semantics. Manual GUI
verification remains outside the implementation census.

| # | Item | Note | Eff | Status |
|---|---|---|---|---|
| B1 | `:has()` | Upward matching; needs a pre-pass over the style tree. | L | ✅ 2026-09-07 (two-pass resolve, descendant+child args) |
| B2 | Container queries + `@container` | Needs containment-context size plumbing. | L | ✅ 2026-09-07 (`@container` named/unnamed, cqw/cqh/cqi/cqb) |
| B3 | Scrollable overflow (`auto`/`scroll`) | Nested scroll containers with own offsets, cumulative clipping/translation, input routing, and scrollbars. **Biggest real-page win.** | L | ✅ 2026-09-25 (wire-v17 containers plus a v20-compatible tail carry x/y content extents, `auto`/`scroll` mode, and nearest parent. The host keeps horizontal/vertical offsets, clips/translates cumulative ancestors, chains wheel remainders, drags both-axis scrollbars, composes scroll before item transforms, and disables GPU tile presentation for nested pages. Transformed link hit-testing remains a documented engine approximation. Manual GUI verification remains a release gate.) |
| B4 | `writing-mode` + logical flow | Reshapes the line model. | L | ✅ 2026-09-08 (`vertical-rl`/`vertical-lr`/`sideways-rl`/`sideways-lr` via rigid-rotation stamp carried by the v12 transform tail — no wire bump; logical size/inset/margin/padding longhands + `*-inline`/`*-block` shorthands folded post-cascade, order-independent, logical wins physical twin; inherited same-mode children share the parent's stamp via `rot_ctx`; `vertical-lr` ≈ `vertical-rl` in v1 — no rigid transform expresses lr's pairing; glyph-level vertical typesetting = D6) |
| B5 | Multi-column | Fragmentation subsystem. | M | ✅ 2026-09-08 (`column-count`/`column-width` count estimation + greedy balance, `column-gap`, `columns` shorthand; rule/r span deferred) |
| B6 | Filters, `backdrop-filter` | Needs offscreen buffers. | M | ✅ 2026-09-07 (full filter chain parse→style→layout→wire v16; host applies via CIFilter; backdrop-filter parsed, host-side) |
| B7 | `clip-path`, `mask` | | M | ✅ 2026-09-08 (circle/ellipse/inset/polygon/`path()` resolved to page space; innermost wins; `mask` falls back to clip; wire v18 sparse clip section; host installs per-item clips) · 2026-09-26: `background-clip` now resolves against the same machinery — `border-box`/`padding-box`/`content-box` per background layer, padding-box radii scaled down proportionally, emitted on the existing per-item clip field (no wire change) |
| B8 | Blend modes, `isolation` | | M | ✅ 2026-09-08 (all 16 modes + `isolation` group ids on the v18 wire; host flattens isolate runs through an offscreen layer; first-writer group semantics) |
| B9 | `ch`/`ex` units | Needs the measurer at length-resolve time. | S–M | ✅ 2026-09-08 (tail closed: embedder attaches the real measurer via `with_metrics`; ch/ex fold against font metrics at style time, 0.5em CSS fallback otherwise) |
| B10 | `transition` + property animation coverage | State-change timeline; `@keyframes` exists. | M | ✅ 2026-09-07 (RP-B1, wire v9) |
| B11 | `@font-face` `unicode-range`, `font-display`, variable fonts | | M | ✅ 2026-09-08 (`unicode-range` install filter — out-of-range face never installed; `font-display: optional` skips the fetch; variable-font axis mapping noted as follow-up) |
| B12 | Full `text-decoration` (color, thickness, style) | | S | ✅ 2026-09-08 (`text-decoration` shorthand + color/thickness/style/line longhands; overline/line-through already from R2) |
| B13 | `text-overflow: ellipsis`, `overflow-wrap`, `word-break`, `hyphens` | | M | ✅ 2026-09-08 (ellipsis truncation incl. mid-word cuts, `overflow-wrap: break-word`/`anywhere`, `word-break: break-all`, soft-hyphen reveal + `hyphens`; dictionary hyphenation = D5) |
| B14 | Counters + full `list-style` | Only the basic disc marker exists. | M | ✅ 2026-09-08 (`counter-reset`/`increment`, `counters()`, format forms, `content: counter(...)`, `list-style` type/position/none, marker stamping, nested-list numbering + `type`/`start` attrs) |
| B15 | Long-tail at-rules/values: `@scope`, `@property`, `@supports selector()`, `all`, `revert`, `overscroll-behavior`, `scroll-snap` | | M | ✅ 2026-09-08 (all seven: `@scope` cascade gate, `@property` registration + initial-value semantics, `@supports selector()`, `all` shorthand reset, `revert` cascade fall-through, `overscroll-behavior`/`scroll-snap` parse + store) · 2026-09-26: the paint-side long tail landed too — `visibility: hidden|collapse|visible` (a hidden box hides its subtree, a descendant may re-show, space is still reserved, `collapse` also drops table rows; links/AX entries follow), full `background-position` (1–4 value, incl. edge insets) / `-size` (`cover`/`contain`/length/percent) / `-repeat` (incl. `space`/`round`) as ordinary `DisplayImage` tiles (no wire change), **comma-separated multi-layer `background`** (bottom-up paint, per-layer longhand lists) with `background-origin`/`-clip`, and **block-axis media features** — `orientation`, `height`/`min-`/`max-height`, MQ4 range syntax — now resolving against the real viewport box in `@media`, `@import … <media>`, `<style media>`/`<link media>`, `<source media>` and `<img media>` (this also fixed an `@import url(x.css) (media)` bug that swallowed the media list into the target URL) |

### C. Layout — 6 items

| # | Item | Note | Eff | Status |
|---|---|---|---|---|
| C1 | Intrinsic sizing: `min-content`/`max-content`/`fit-content` | Grid treats them as `auto` today; unlocks flex/grid correctness. | L | ✅ 2026-09-09 (full `ENGINE_INTRINSIC_SIZING_PLAN.md`: `SizeVal` keyword + clamp resolve in `kitae_css`; block width — min = longest word, max = whole run, `fit-content()` clamp, nested = widest child, replaced = natural size, padding/border chrome included; grid keyword + `fit-content()` tracks; `flex-basis:` keywords size items; `height:` keywords behave as `auto` per plan) |
| C2 | `aspect-ratio` | | S | ✅ 2026-09-09 (derives box height from the resolved width when `height` is `auto`; `w/h` + bare ratio; content-box math + clamps tested) · 2026-09-26: the `auto <ratio>` form parses (the keyword keeps a box's own ratio and falls back to the authored number), and the ratio also sizes a replaced-ish box on whichever axis is `auto` — definite width, definite height, and both-auto against the intrinsic size; still no wire change (the box size is an existing display-list field) |
| C3 | Fragmentation: `break-inside`, orphans/widows, paged media | Dependency for H3 (print). | L | ✅ 2026-09-09 (`break-before`/`after`/`inside` + `page-break-*` aliases + `orphans`/`widows` (inherit) through the cascade; forced breaks start new multicol units; **paged media deferred** — properties are parsed and carried, so H3 has what it needs) |
| C4 | Abs-positioning spec edges (static position, percent heights vs auto, orthogonal flow) | | M | ✅ 2026-09-09 (abs box with no edges lands at its static position in the flow, container origin standing in for the flow cursor; percent-edges model already resolved against the containing block; orthogonal flow rides the B4 vertical path) |
| C5 | Table polish: `rowspan>12`, border-collapse models, `caption-side` | | M | ✅ 2026-09-09 (`rowspan` clamps to the table height, not 12; `border-spacing: h [v]` leading + inter-row bands under `separate`, forced to 0 under `collapse` — conflict-resolution simplified, not full CSS winner model; `caption-side: bottom` paints below the grid, `left`/`right`/`block-start` collapse to `top`) |
| C6 | Subgrid + advanced grid | Masonry is experimental even in Chrome — optional. | M | ✅ 2026-09-09 (true CSS §8.5 **sparse** auto-placement — monotonic row-major cursor, plain `auto` items never backfill a hole; `grid-auto-flow: dense` backfills from the origin; `minmax()` floors applied after the `fr` distribution; `grid-template-columns: subgrid` v1 = nearest grid ancestor's column template + line names + gap; **`column` flow and row-subgrid parsed + carried but placement stays row-major; masonry out**) |

### D. Text & fonts — 6 items

Shipped 2026-09-09 in one pass. Shared substrate: `kitae_shape` owns
segmentation/shaping; the layout text feeder consumes it word-by-word, so
D-items ride the existing run pipeline with no wire bump.

| # | Item | Note | Eff | Status |
|---|---|---|---|---|
| D1 | Complex-script shaping coverage audit | rustybuzz + per-script fallback exists; Indic conjuncts/Arabic joining edges need a conformance pass. | M | ✅ 2026-09-09 (audit battery: Arabic joining forms + lam-alef, Devanagari conjuncts + reph, matra positioning through rustybuzz; **fix shipped**: script-fallback runs now carry combining marks (Mn) with their base instead of dropping them into a second fallback pass) |
| D2 | Emoji: ZWJ sequences, COLRv1/CBDT | Color emoji via Core Text partially works already. | M | ✅ 2026-09-09 (ZWJ chains — family/flag/skin-tone — shaped as single grapheme clusters in the fallback shaper; COLRv1/CBDT glyph paint stays on the Core Text platform path) |
| D3 | `lang`-based font + feature selection, `:lang()` | | S–M | ✅ 2026-09-09 (`lang` attr inherits through the cascade; `:lang()` matches inherited value incl. prefix forms (`en` vs `en-GB`); first lang-gated consumer = D5 hyphenation; per-language font fallback selection remains coarse) |
| D4 | `font-feature-settings`, `font-variant*` | | M | ✅ 2026-09-09 (`font-feature-settings` tag/val list + `font-variant-ligatures`/`-caps`/`-numeric`/`-east-asian`/`-position` parsed to shaping features; numeric/east-asian/position axes stored but unapplied — only liga/caps reach rustybuzz; no wire bump) |
| D5 | Hyphenation dictionaries | Pairs with B13. | M | ✅ 2026-09-09 (`hyphenate.rs` over `hyphenation 0.8.4`, **en-us dictionary embedded**; `hyphens: auto` breaks at the last fitting dictionary point and paints the visible hyphen; gated on overflow + `lang` unset or `en*`; **uppercase words never hyphenate; other languages need dictionary data added**) |
| D6 | Vertical text glyph handling | Pairs with B4. | M | ✅ 2026-09-09 (`text-orientation: mixed\|upright\|sideways`, inherited, horizontal-tb ignores it; `mixed`/`upright` split runs per upright char — CJK set in `mixed` — and counter-rotate about the glyph's advance center so the B4 box stamp composes to an identity linear part; **no vmtx: vertical glyphs use horizontal advances and baselines hang from the flow position; decorations stay word-level line-shaped; links unrotated**) |

## Phase 2 — Engine depth (E/F/G ≈ 3–5 months)

### E. Rendering / GPU — 8 items

| # | Item | Note | Eff | Status |
|---|---|---|---|---|
| E1 | Family-aware glyph advances in `kitae_render_gpu` | Known Q11 follow-up; GPU text measures family-blind. | S | ✅ 2026-09-09 (glyph quads carry the styled font family into the scene; Q11's family-aware measurement feeds GPU-path glyphs — engine measures, painter no longer re-measures) |
| E2 | Sticky/transform correctness in the GPU painter | | S | ✅ 2026-09-09 (sticky side-list parsed onto the scene, `SceneSticky::shift` mirrors layout exactly, per-item device-y shift folded into the painter transform; sparse v12 transforms on all painter items) |
| E3 | Text blur (per-run shadow field on the wire) | | S | ✅ 2026-09-09 (plan-sanctioned wire bump to **v19**: per-text blur f32; CPU painter = padded mask + box blur + composite, GPU = 5-copy axis-offset approximation; Swift side composites NSShadow) |
| E4 | Tiled rasterization + dirty-rect compositing | Scroll/reraster performance at scale. | L | ✅ 2026-09-09 (`tiles.rs` TileCache: viewport-space CSS-px tile grid, per-tile FNV signature over intersecting paint items with sticky-expanded footprints; unchanged tiles blit; assembly bit-exact vs the whole-surface render at scale 1 & 2, incl. scrolled sticky scenes. The bit-exactness bar surfaced and fixed a real translational-purity bug — gradient evaluation is now box-relative, matching the GPU WGSL) |
| E5 | Compositing layers, `will-change`, promoted scrolling | | L | ✅ 2026-09-09 (engine: `will-change` promotes via the B8 overwrite-group stamp in layout; painter: `layers.rs` whole-layer raster cache keyed on raw frame bytes + render state — unchanged frames blit without parse/raster, `kru_gpu_clear_layers` for out-of-band invalidation. Per-group layer isolation itself stays host-side with blend/group (B8 wire decision); scrolling promotes = the v17 scroll-container side-list) |
| E6 | Subpixel AA / LCD text modes | | M | ✅ 2026-09-09 (CPU painter = per-channel 3-tap coverage at 1/(3·scale) with straight-alpha decomposition; GPU stays grayscale (documented deviation); scene flag + host override `kru_gpu_set_lcd_text`) |
| E7 | Color management (ICC profiles, P3, HDR) | | M | ✅ 2026-09-09 web-level scope (`color(srgb …)` stored as-is, `color(display-p3 …)` converted to sRGB through the CSS Color 4 matrices, `srgb-linear` handled; a98-rgb/prophoto/rec2020/xyz parse as unsupported; ICC-tagged images + HDR headroom remain open by scope note) |
| E8 | Complex clip stacks (rounded-clip intersection, clip-path on GPU) | | M | ✅ 2026-09-09 (scene clips parsed onto rects/images/shadows — circle/ellipse/inset+radii/polygon — CPU tests at the inverse-mapped sample point; GPU encodes two vec4 instance attrs + WGSL discard; intersections flatten to per-item clips, no full stack on GPU) |

Per-row GPU deviations recorded (documented, all have CPU-fallback behavior):
conic-gradient paints solid on the GPU path; filter chains are walked but
host-applied (CIFilter); blend/group stay host-side (B8 wire decision);
polygon clips are CPU-only; GPU inset shadows are skipped and GPU shadows
ignore clips; glyph blur is the 5-copy approximation on GPU; LCD text is
grayscale on GPU; clip-on-identity-items anchoring is device-local on the
CPU painter (exact only at origin 0 / scale 1 — transformed items are
unaffected).

### F. Media — 5 items

F2 got its decision document on 2026-09-07 (`MEDIA_CODECS_DECISION.md`):
Option 1 royalty-free only (dav1d+libvpx), Option 2 = Option 1 + H.264 via
macOS VideoToolbox (**recommended**, ~99% coverage, no extra licensing),
Option 3 full FFmpeg (rejected: kitchen-sink, LGPL risk). **Option 2 was
approved on 2026-09-09** (directive "now complete all of F at once") and
bucket F shipped the same day — the Rust demux stayed pure Rust (the C
rule was never exercised; VideoToolbox is the platform's own framework).

| # | Item | Note | Eff | Status |
|---|---|---|---|---|
| F1 | Web audio via symphonia (pure Rust) | mp3/flac/vorbis/wav/m4a for plain `<audio>`; Opus remains deferred. | M | ✅ 2026-09-09 (kitae_media audio.rs: Symphonia probe/decode → duration/channels/rate; every supported format re-encodes to CAF via the existing QTSQ `write_caf` pipeline (`kitae_f1_{pid}_audio.caf`) so the docked AVAudioPlayer bar and the in-page chrome band both transport; fetch gated on first layout via `web_media_tried`; `<audio controls>` paints the same chrome band geometry as video. Symphonia 0.5 has no shipped Opus decoder feature, so Opus is a recorded deferral.) |
| F2 | H.264/VP9/AV1 video | **Decision point**: C libraries (dav1d/openh264) are allowed by `CPP_ROLE.md`, or wait for pure-Rust. | L | ✅ 2026-09-09 (**Option 2 approved by directive "now complete all of F at once"**): Rust side = kitae_media video.rs MP4 (stts/stss/stsz/stsc/stco/co64 + avcC/hvcC/vpcC/av1C extradata) + WebM/MKV (EBML lacing, CodecPrivate) demux — pure Rust, no C added; host side = macOS VideoToolbox decode (H.264/HEVC parameter-set format descriptions, VP9 plain description + vpcC extension atom; VP8/AV1 degrade to the placeholder frame) in the new `WebVideoDecoder.swift`, BGRA→RGBA swizzle, and a `WebVideoDriver` clock (decode → `kitae_web_video_feed` → `mediaPlay` nudge so the engine republishes with the raster stamped). 8 C exports in wire v20. dav1d/AV1 stays open as a future slice (VideoToolbox has no AV1 decoder on this OS target). |
| F3 | WebM/MKV container + demux | Pairs with F2. | M | ✅ 2026-09-09 (EBML/SimpleBlock parsing with lacing in video.rs, VP9 CodecPrivate → extradata, fixture-tested; MP4 in the same module) |
| F4 | Full native `<video>` control chrome | Beyond the QTSQ player UI. | M | ✅ 2026-09-09 (engine-side `paint_media_chrome`: play/pause glyph, scrubber + thumb, time label, backdrop; engine click hit-test routes transport; **wire v20 `controls` flag per media item**; host skips the docked video bar for controlled page video, while control-less QTSQ videos keep it) · 2026-09-26: the video **attribute** surface ships too — `poster` (fetched/decoded/cached, painted under the frame), `muted`/`autoplay`, `loop` (rewinds on the host's end-of-stream report), `preload=none|metadata|auto` (deferred fetch until playback), `playsinline` (inert, as it must be), `crossorigin` (only `use-credentials` fails closed), and `<video>/<audio><source>` selection: the first `<source>` whose `src` **decodes** wins, `media`-gated against the live viewport and environment, with the element's own `src` last (wire unchanged — still v20) |
| F5 | WebVTT captions/subtitles | | M | ✅ 2026-09-09 (kitae_media vtt.rs parser: cues, timestamps, speaker/voice tags stripped, NOTE/STYLE skipped; engine fetches the `<track>` alongside the source and overlays the active cue on the video band via decorate_caret's caption overlay) |

### G. Network — 8 items

| # | Item | Note | Eff | Status |
|---|---|---|---|---|
| G1 | HTTP/2 (HPACK, multiplexing) | | L | ✅ 2026-09-10 (reqwest `http2` feature on — hyper's h2 handles HPACK/framing/multiplexing; `Response.protocol` reports "HTTP/1.1"/"HTTP/2"/"HTTP/3" so embedders can verify; h2c prior-knowledge exercised in tests; TLS-ALPN negotiation rides rustls) |
| G2 | HTTP/3 + QUIC | | L | ✅ 2026-09-10 (`fetch_h3`/`fetch_h3_with` over quinn + h3 0.0.7 with the **ring** crypto provider per `CPP_ROLE.md` — no aws-lc-rs C build; `runtime-tokio` feature required even for manual `block_on` because quinn's endpoint construction resolves its driver through `default_runtime()`, and endpoints must be built inside the runtime that drives them; self-signed-QUIC test server + protocol "HTTP/3" verified; chrome-remote H3 needs a public host — loopback only for now) |
| G3 | Response decompression: brotli (crate already in the tree via WOFF2), zstd, full gzip coverage | | S | ✅ 2026-09-08 (reqwest `brotli`+`deflate` features on; transparent decode proven end-to-end vs a loopback server serving real gz/br/deflate bodies; **zstd deferred**: reqwest's zstd links C `zstd-sys`, violates `CPP_ROLE.md` — pure-Rust ruzstd has no reqwest integration) |
| G4 | Auth flows: Basic/Digest challenges, credential prompts | | M | ✅ 2026-09-10 (auth.rs: automatic `Authorization: Basic` on first request when credentials are set, RFC 2617 Digest on 401 `WWW-Authenticate: Digest` — md5 algorithm with qop=auth, cnonce/nc — plus an embedder **credential provider** (`set_auth_provider`) invoked on 401 challenges so the browser layer can prompt; declining provider surfaces the 401 as an ordinary response; no UI — the prompt itself is H6's job) |
| G5 | Range requests, resumable downloads, download-manager hooks | | M | ✅ 2026-09-10 (download.rs: `fetch_range` with `Range`/`If-Range`; `download_to_file` resumes `.part` files via a `.meta` ETag sidecar (206 appends, 200 restarts), progress hook after headers + each 16 KiB chunk returning `Cancelled` to abort while keeping the part, dedicated `download_client()` without a total timeout; transfer errors surface with their full `source()` chain because reqwest's top-level Display hides the root cause. The downloads UI/manager itself is H1) |
| G6 | WebSocket protocol | Embeddable without the JS API. | M | ✅ 2026-09-10 (ws.rs RFC 6455 client: opening handshake with Sec-WebSocket-Accept verification, client-masked text/binary frames, fragmentation reassembly, automatic Pong for Ping, polite Close handshake, ws+wss (rustls); one real bug found by the tests — frame data legally arrives coalesced with the 101's final CRLF, the handshake reader now returns the over-read bytes instead of discarding them; embeddable as `ws_connect`/`WsStream`, no JS surface) |
| G7 | Server-Sent Events | | S | ✅ 2026-09-10 (sse.rs: `text/event-stream` pump over the streaming response — `data:`/`id:`/`event:`/`retry:` parsing, multi-line data, `Last-Event-ID` replayed on reconnect, comment keep-alives skipped; clean EOF drains the pending buffer before returning None so a final dispatch whose blank line sits in the buffer is not lost) |
| G8 | Speculative preconnect/prefetch + request priorities | | M | ✅ 2026-09-10 (warm.rs: `preconnect` (DNS+TCP(+TLS) warm-up on a spawned thread, cached per-origin), `prefetch` into a small warm cache served by `fetch_cached`, and RFC 9218-style priority hints plumbed through the fetch helpers; loopback tests cover warm hits and origin dedup) |

## Phase 3 — Browser layer + robustness (H/I ≈ 2–3 months, implementation closed)

### H. Browser layer (Kitae.app) — 8 items

| # | Item | Note | Eff | Status |
|---|---|---|---|---|
| H1 | Downloads UI + progress | Swift downloads center + popover over the kitae_net download registry (live progress, dedup'd filenames). | M | ✅ 2026-09-10 |
| H2 | File upload (open panel → multipart) | `KITAE_CMD_SET_FILE_FILES` (36) feeds engine-side `file` inputs; the host picks via panel and submit builds a real multipart body. | M | ✅ 2026-09-10 |
| H3 | Print to PDF | Host-side `PagePrinter` (⌘P + palette) writes the current frame to PDF. | M | ✅ 2026-09-10; print media 2026-09-26 (`KITAE_CMD_SET_MEDIA` = 52: the engine re-lays the page out for paper — `@media print` applies, `@media screen` does not, `prefers-*` answer as a print device — and flags the frame it publishes as v20 `flags` bit2; the host lays out at the paper's printable width, prints that frame through `NSPrintOperation`, then puts screen media back) |
| H4 | AX parity: actions, live regions, full roving focus | `aria-live` maps to wire role 11; explicit roles/labels, associated form labels, value/min/max, checked/expanded/selected/disabled, heading level, and politeness are rebuilt per frame. Pressable roles synthesize
clicks; slider/spin-button roles synthesize increment/decrement actions;
live-region label diffs post polite/assertive announcements; and Tab/⇧Tab
roving focus uses `focusNext`/`focusPrev`. The host AX cache is rebuilt for frames and nested-scroll offsets. | L | ✅ 2026-09-10, extended 2026-09-25 (44 stable roles, optional v20 `AXS1` semantics tail with legacy fallback, native decode contract); manual AX/GUI verification pending |
| H5 | Read-only inspector (DOM + computed CSS overlay) | ⌘-click picks an element (`KITAE_CMD_INSPECT_PICK`/`INSPECT_INFO`, 38/39); the engine reports tag, attributes and computed style to the read-only panel. | M | ✅ 2026-09-10 |
| H6 | Autofill + password UI (Keychain) | Engine serves Basic/Digest 401 sign-in pages and detects submitted login forms (`KITAE_CMD_SET_AUTH`, 37); the host fills from and saves to the Keychain. | M | ✅ 2026-09-10 |
| H7 | Session restore, history polish | `SessionStore` persists non-transient tabs to session.json (restore on launch, save on terminate); Reopen Closed Tab ⌘⇧T. | S | ✅ 2026-09-10 |
| H8 | Per-tab crash isolation | Decision resolved to the reload-on-panic option: FFI `catch_unwind` guards recover the engine and the tab shows a recovery page with Reload — no multiprocess model. | L | ✅ 2026-09-10 |

### I. Performance & robustness — 6 items

| # | Item | Note | Eff | Status |
|---|---|---|---|---|
| I1 | Style invalidation sets + descendant-tag filters | Kills full-recascade on dynamic pages. | L | ✅ 2026-09-10 (kitae_style `RuleIndex` — selectors filed under the keys of their rightmost compound (tag/class/id; `:is()`/`:where()` arguments contribute theirs), keyless selectors in `universal`, ancestor-key demand filtered by the walk's per-key ancestor counts; `candidates` returns a proven superset and the cascade's `matches` stays the exact decision. Plus the hover fast path: `uses_interaction_state()` (hover/active/focus anywhere, including inside `:not()`/`:is()`/`:has()`) cached on the page, so hover/active changes skip restyle+relayout entirely on sheets that key off no interaction pseudo) |
| I2 | Incremental everything (LayoutNG-style): layout fragments reused across relayouts | | L | ✅ 2026-09-10 (kitae_dom per-node `rev` counters; kitae_layout `FragmentCache` keyed by node rev + containing width, with ancestor-chain invalidation — a leaf change reflows only the dirty chain; kitae_core `layout_cached` wired into the engine relayout path so unchanged subtrees reuse their fragments) |
| I3 | Image decode-on-demand + purgeable caches | | M | ✅ 2026-09-10 (shared QTSQ/web URL-keyed decoded RGBA cache — 64-entry/192 MiB byte budget, oldest-first eviction, newest pinned; `purge_image_caches()` clears image, web-media, and network caches; pressure observability counters) · 2026-09-26: safe in-crate decoders for **BMP, ICO (incl. the multi-image directory) and APNG** (`kitae_media/src/{bmp,ico,apng}.rs`, size/frame caps, fail-closed on every truncation), and APNG plays through the existing animation store — a still raster for the image path plus the frame sequence in the side table, so `<img>` animates like GIF |
| I4 | Fuzzing program (HTML/CSS/URL/QTSQ) | | M | ✅ 2026-09-10, scope clarified 2026-09-25 (`fuzz_html`/`fuzz_css`/`fuzz_urls`/`fuzz_qtsq` deterministic mutation programs run in the normal sweep. Image/font decoders have corpus/unit and fail-closed guards but no dedicated seeded mutation target; `docs/FUZZING.md` records that boundary.) |
| I5 | Web Platform Tests conformance harness + gap-driven fixes | The honest scorecard. | L | ✅ 2026-09-10, remeasured 2026-09-25 (`docs/WPT_SCORECARD.md`: tokenizer **6256/6256 (100.0%)**, tree-construction **1072/1784 (60.1%)**, CSS color-hex **81/81**, color-keyword **160/160**, An+B **128/128**; harnesses assert recorded baselines so regressions turn the sweep red. This is measurement of five vendored subsets, not a full-WPT conformance claim. Gap-driven fixes include strict An+B, the full CSS Color 3 keyword table, and template-content inertness.) |
| I6 | Security hardening: strict fetch policies, URL-parse fuzz parity, sandboxing review | | L | ✅ 2026-09-10 (mixed-content blocking, URL-parse fuzz parity, and sandboxing review recorded in H8; subsequent settings/privacy work adds persisted HSTS, HTTPS-only navigation, DNT, and third-party-cookie policy through FFI; see `SECURITY_AUDIT.md`) |

## 4. The two buckets that are not on this list

1. **The JS platform** — V8 itself plus the thousands of web APIs that exist
   only as script surfaces (DOM manipulation, events, fetch/XHR, workers,
   service workers, storage, canvas/WebGL/WebGPU contexts, extensions,
   devtools protocol). By feature mass this is the **majority of what
   Chromium is**, and it is permanently out by the standing design decision.
   "Comparable to V8" in the literal sense is unreachable by definition; in
   the agreed sense — the embeddable-engine role V8 plays for the JS
   ecosystem — Kitae already holds it architecturally (fetch → parse →
   style → layout → display list behind a C/Rust embedder API).
2. **The conformance/performance long tail** — beyond I5, millions of WPT
   assertions and 15 years of optimization. Not a list; it's the ocean.

## 5. Sequencing summary

| Phase | Buckets | Items | Shipped | Remaining outside the census |
|---|---|---|---|---|
| 1. Reader-web rendering parity | A/B/C/D | 39 | 39 | manual A11/B3 GUI validation |
| 2. Engine depth | E/F/G | 21 | 21 | documented row deviations only |
| 3. Browser layer + robustness | H/I | 14 | 14 | manual AX and release validation |
| **Total** | | **74** | **74 ✅** | **manual GUI, conformance long tail, and external release gates** |

No A–I implementation row remains partial or open. The next work is validation
rather than another census row: manually exercise A11 selection/IME, B3 nested
scrolling, AX, and the browser/QTSQ surfaces listed below; then complete the
external release gates. The WPT/performance long tail (§4.2) remains outside
this finite census and is not presented as finished.

### Final validation gate (2026-09-25)

- Serialized and default-parallel `cargo test --workspace`: 1149 passed, 3
  ignored; `make native-test`: 6/6; `make`: app bundle up to date;
  `cargo check --workspace`, formatting, and `tools/deps-audit.sh`: pass.
  The built arm64 executable reports `minos 15.0`.
- Manual GUI validation remains for A11 range selection/IME, B3 nested
  scrolling and transforms, AX roles/actions/live announcements, Settings/
  Privacy/HSTS persistence, registered deep links, v20 video controls, QTSQ
  cache purge, Markdown typography, and supported QTSQ video interaction.
- Release, package publication, git/tag, signing/distribution, and clean-machine
  validation remain external/manual gates.

## 6. Ground rules (unchanged)

1. No JavaScript, no WASM — ever. The endpoint is the display list.
2. Pure Rust first; vendored C/C++ stays behind `CPP_ROLE.md` (currently the
   QTSQ decoder), while shaping uses `rustybuzz`.
3. Wire bumps land with the Swift decoder bump in the same change.
4. Every item: tests green, docs updated, visual verification for anything
   the user can see.
