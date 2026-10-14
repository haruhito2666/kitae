# Kitae HTML/CSS Completion Plan — closing the audited gap list (slices R1+)

> **STATUS 2026-09-05: COMPLETE.** R1–R8 all landed and green — 439 tests
> workspace-wide, zero failures. R5 note: `ch`/`ex` units moved to the
> deferred table (correct support needs the measurer at resolve time).
> Progress record lives in `ENGINE_CORE_STATUS.md` §3.
> Audit basis: grep audit 2026-09-05, after the Web Essentials plan (Q1–Q14)
> completed. Non-negotiables unchanged: no JS/WASM ever, pure Rust, display
> list is the contract.

## 0. Scope framing

"Full support" is scoped to what a **no-script reader engine** can mean:
every CSS/HTML feature whose behavior can be expressed in the static display
list, plus declarative interactivity the engine can own (details toggle,
form controls). JS-dependent platform APIs (events, fetch/XHR, storage,
canvas drawing, custom elements) are out **by design**, not gaps. This plan
closes the remaining *static-rendering* gaps and documents the rest as
explicit engineering decisions rather than silent absences.

## 1. Slices

### R1 — Borders + outline ✅/⏸ (highest visual impact)
- **Do:** `border-width` / `border-color` / `border-style` per side + all
  shorthands (`border`, `border-top`, …); parse `thin/medium/thick`;
  styles `solid` (exact), `dashed`/`dotted` (painted as segment runs — the
  display list is rect-based, so dashes are small rects; no wire change),
  others (`double/groove/…`) fall back to solid. Paint the 4 edges in
  `layout_static_box` inside the border box (background draws inside
  padding-box per CSS — adjust the background geometry when borders exist).
  `outline` (+ `outline-offset`): painted as an outline rect around the
  border edge, does not affect layout.
- **Tests:** shorthand expansion, per-side override, dashed segmentation
  count, outline not shifting flow, border vs background geometry.

### R2 — text-decoration variants
- **Do:** `text-decoration-line` (underline/overline/line-through + none),
  shorthands; paint overline above the ascent, line-through at the middle.
  `text-decoration-color`/`thickness` accepted, thickness approximated.
- **Tests:** each line paints at the right offset; shorthand parses.

### R3 — ::before / ::after generated content
- **Do:** selector parser keeps a trailing pseudo-element; style engine
  computes a per-element pseudo style; `content` supports `none`, strings
  (with escapes), `attr(x)`. Layout emits the generated text as an inline
  run at the element's content start/end using the pseudo's own
  color/font/weight. `url()` images and `counter()` out of v1 (documented).
- **Tests:** string prefix/suffix render with pseudo styling; attr();
  elements without content: render nothing; cascade specificity for pseudo
  rules.

### R4 — @import
- **Do:** kitae_core's sheet collector recursively resolves `@import` at
  collect time (same fetch path as `<link>` stylesheets; depth cap 8, cycle
  guard by URL); media-qualified imports evaluate their condition first.
- **Tests:** nested import chain, cycle does not hang, imported rules apply.

### R5 — viewport + text units
- **Do:** `vw/vh/vmin/vmax` resolve against the layout viewport (style
  computation gains the viewport size); `ch` resolves at layout time via the
  measurer ("0" advance), `ex` ≈ 0.5em.
- **Tests:** 50vw box width, ch-width monospace column, unit round-trips.

### R6 — inline <svg>
- **Do:** remove `svg` from the display:none sheet; `layout_static_box`
  gains an `svg` arm: serialize the element subtree to SVG source (dom
  serializer), render through resvg (already a dep for SVG images) into the
  shared decoded-raster cache (hash by source), emit a `DisplayImage`
  payload entry sized by width/height attrs or CSS (default 300×150).
- **Tests:** inline icon SVG renders a raster at the element box; `<path>`
- only and attribute-driven geometry both work; nested-html inside svg
  ignored.

### R7 — @layer
- **Do:** `@layer a, b` declarations + layered rule blocks; cascade sort key
  gains the layer order (unlayered wins over layered, first-declared layer
  lowest). `!important` inverts across layers (spec behavior).
- **Tests:** layer order beats specificity, unlayered beats layered,
  important inversion.

### R8 — text-shadow (wire-free approximation)
- **Do:** parse `text-shadow` (offsets, blur accepted, color); layout paints
  duplicate runs at the offset behind the run in the shadow color (crisp —
  no blur; blur needs a per-run field on the wire, out of v1). Multi-shadow
  supported (last behind first).
- **Tests:** offset duplicate exists with the shadow color; no shadow =
  no extra runs.

## 2. Deferred (decision + reason, not silent)

| Item | Reason |
|---|---|
| Full HTML5 adoption-agency error recovery | Parser is deliberately "lite"; real-page error recovery already works; spec-exact recovery is weeks of edge-case work for invisible behavior |
| `:has()` | Upward matching breaks the one-pass cascade; needs a pre-pass architecture change (own slice if ever) |
| Container queries | Needs containment-context size plumbing through style/layout (own slice) |
| Scrollable overflow (`auto`/`scroll`) | A nested-scroll subsystem (per-container offsets, input routing, scrollbars), not a paint property |
| `writing-mode` / vertical text | Reshapes the entire line model for a niche reader-web case |
| CSS multi-column | Fragmentation subsystem |
| Filters / backdrop-filter / blend modes / clip-path / mask | New paint pipeline surface; some need offscreen buffers |
| AVIF / H.264 / WebM video | Require C/C++ codec libraries (dav1d/openh264) — allowed by `CPP_ROLE.md` but a build-weight decision, deferred until pure-Rust options or user request |
| Web video/audio (non-QTSQ) | Audio could ride symphonia (pure Rust) — candidate slice R9 if requested |
| `transition` property | Needs a state-change animation timeline; `@keyframes` already covers declarative cases |
| `ch`/`ex` units | Correct resolution needs the measurer ("0"/"x" advances) at length-resolve time — the style layer has no font access; threading it through every `Len::resolve` site is its own slice |
| `<canvas>` drawing | No script, no drawing commands — meaningless without JS (stays hidden) |
| `<template>` content semantics | Hidden already; inert-content semantics matter only with script |

## 3. Sequencing

R1 → R2 (paint completeness, no wire changes) → R6 (inline SVG, icons
everywhere) → R3 (generated content) → R4 → R5 → R7 → R8. R1–R8 add no wire
version bumps (rects/runs/image payloads only), so the Swift app needs no
decoder work for this plan.
