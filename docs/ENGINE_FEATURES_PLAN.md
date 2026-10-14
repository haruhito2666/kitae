# Engine Core — Remaining Features & Functions Plan

> Standalone, detailed plan for everything left in the **engine core** after
> the A–I goal completed (152 tests green — see `ENGINE_CORE_STATUS.md`).
> This is Phase-set K–P of `ENGINE_CORE_ROADMAP.md` expanded into an
> implementation-level spec: real image decode, the wgpu/Metal renderer,
> text shaping/bidi, CSS & layout completeness, network hardening, and
> find-in-page + accessibility.
>
> QTSQ format support has its own plan: `QTSQ_INTEGRATION_PLAN.md` (Phase J).
> The two share one piece of plumbing — the decoded-raster path (J3 ↔ K3) —
> and are designed to be built back-to-back.
>
> Standing rules that bind every stage: no JS/WASM ever; C/C++ only as
> link-in libraries behind a C ABI (per `CPP_ROLE.md`); display list is the
> contract; every stage ships tests and keeps `cargo test --workspace` green.

---

## Phase K — Real image decode (PNG/JPEG pixels, not just headers)

**Today:** `image_dimensions` reads PNG/GIF/JPEG *headers*; `DisplayImage`
items are placeholders. **Goal:** actual RGBA pixels flow to the shells.

### K1 — PNG decode (pure Rust)
- `crates/kitae_media` (new crate, Rust-only initially): `png` crate decode →
  RGBA8; `flate2`/`zlib-rs` already policy-approved.
- `image_dimensions` switches from header-math to `kitae_media::dimensions`
  (one call site, same signature).
- Interlaced/16-bit PNGs: accept via the crate, no special handling.
- ICC/gamma: ignore (reader-browser nicety, documented).
- **Tests:** generated fixtures (gray, RGB, RGBA, palette, interlaced) decode
  to expected pixels; `image_dimensions` parity.
- **Estimate: 1 wk.**

### K2 — JPEG decode
- Default: **`libjpeg-turbo` behind the C ABI** in `kitae_media`
  (`native/jpeg/` vendored, build.rs link, hand-written FFI: `turbojpeg`
  init/decompress-to-RGBA — the API is small and stable).
- Escape hatch: if a pure-Rust `zune-jpeg` passes the same fixture tests at
  acceptable speed, take it first and keep the C library off the table — the
  seam (`kitae_media::decode_image`) is identical either way.
- **Tests:** fixtures (baseline, progressive, 4:2:0/4:4:4 chroma, grayscale);
  CMYK JPEGs → documented fallback to header-dims + placeholder.
- **Estimate: 1–2 wk.**

### K3 — Pixel plumbing (shared with QTSQ J3)
- `kitae_core` decoded-raster cache: `Url → Arc<RgbaImage>` (LRU, ~64 entries,
  same shape as the HTTP cache; entries dropped on navigate-away).
- FFI frame format: images graduate from `{src}` to `{src → resolved pixels}`.
  Wire change is append-only: each `DisplayImage` gains a payload id; a
  side-table in the frame carries id → RGBA bytes. Frame format version bumps;
  Swift `PageCanvasView` and SDL update in the same commit.
- Memory rule: the frame carries *references* (ids), the cache carries *bytes*;
  the embedder asks for pixels only for images currently on screen (scroll
  window), like everything else in the mailbox design.
- **Estimate: 1–1.5 wk.**

### K4 — object-fit with real pixels
- Contain/cover/scale-down become crop+letterbox math at paint time (UV or
  source-rect, per backend); `fill` stretches. Layout math (which box the
  image occupies) is already correct and untouched.
- **Estimate: 0.5 wk.**

**K total: 3.5–5 wk.**

---

## Phase L — wgpu renderer (Metal backend) + CPU fallback

**Goal:** the agreed Servo-style path — wgpu over Metal as the primary
painter of the display list; the CPU/SDL rasterizer stays the permanent
software fallback (and the regression gate).

### L1 — `kitae_render_gpu` foundation
- New crate: wgpu instance/device/surface; one instanced quad pipeline (solid
  color, rounded-corner-ready SDF in the fragment shader from day one);
  scissor-rect clipping; page-background clear.
- Input: `DisplayList` as-is (rects → instance buffer; no scene graph, no
  retention — rebuild instance buffers each frame, the list is small).
- Parity target: everything `kitae_render` (SDL) draws today, provable by the
  golden tests in L6.
- **Estimate: 2 wk.**

### L2 — Text: glyph atlas
- Core Text (via Swift `kitae_core` FFI extension or `core-text` crate —
  prefer the **`core-text` Rust crate** so the GPU crate stays pure Rust)
  rasterizes glyphs → atlas texture (r8, padded, LRU eviction).
- Per-`TextRun`: shape-free glyph id + position lookup (kerning/shaping stays
  Phase M; the atlas stores *rendered* glyphs, layout already supplies
  positions).
- Instanced glyph quads over the atlas; subpixel positioning off (grayscale AA
  first).
- **Estimate: 2–3 wk.**

### L3 — Images as textures
- `DisplayImage`/video frames upload → texture cache (id-keyed, matches K3's
  side-table); bilinear scaling; object-fit = UV math from K4.
- **Estimate: 1 wk.**

### L4 — Paint upgrades (GPU + CPU where cheap)
- `border-radius`: SDF fragment path on GPU; CPU backend draws rounded rects
  via scanline approximation (both wired to one
  `ComputedStyle::border_radius` field added here).
- Gradients: linear/radial paint servers (data model lands in N4; this stage
  renders them) — gradient stop buffer + per-quad interpolation.
- `box-shadow`: blurred-quad passes (dual-kawase or a pre-blurred sprite; the
  simplest correct thing first).
- **Estimate: 2–3 wk.**

### L5 — Integration
- Kitae.app: `PageCanvasView` gains a `CAMetalLayer` mode; **Settings toggle
  "Render with GPU"** (default on once goldens pass) — the toggle doubles as
  the permanent proof of the CPU-fallback story.
- SDL shell: unchanged CPU path (regression gate).
- Present loop: GPU crate draws the latest display list on demand (frame
  mailbox semantics preserved; no engine-thread coupling).
- **Estimate: 1.5 wk.**

### L6 — Golden-image tests
- Offline harness: render a corpus of display lists through **both** backends
  → downsample to a small hash grid → assert parity within tolerance; runs in
  CI-equivalent (`cargo test`), Metal via a headless device where available.
- **Estimate: 1 wk (spread across L1–L5, listed separately for honesty).**

**L total: 8–10 wk.**

---

## Phase M — Text shaping, bidi, international text

**Goal:** typography-first for all scripts, not just Latin.

### M1 — `kitae_shape` crate
- Default: **HarfBuzz behind the C ABI** (vendored, like K2's libjpeg-turbo).
  Escape hatch: `rustybuzz` (pure Rust) behind the same trait if HarfBuzz
  linking drags — the seam is `Shaper::shape(text, font, size) -> ShapedRun`
  (glyph ids, advances, offsets, clusters).
- `TextMeasurer` gains a shaped path: `measure_run` replaces per-char math for
  real shells; the Mock keeps its arithmetic for tests.
- **Estimate: 3 wk.**

### M2 — Font collection & fallback
- Core Text enumerates families; CSS `font-family` chains resolve through it;
  per-script fallback (Arabic → Geeza Pro, Devanagari → Devanagari MT, CJK →
  Hiragino, emoji → Apple Color Emoji) via Core Text cascade.
- Color emoji: rendered as image strikes into the atlas (L2 mechanism).
- **Estimate: 2 wk.**

### M3 — Bidirectional text
- Unicode Bidirectional Algorithm (UBA) on line runs (`unicode-bidi` crate —
  pure Rust, policy-clean) reorders glyph runs; `direction: rtl` property end
  to end: parse → `ComputedStyle.direction` → line layout order →
  `text-align` start/end flip.
- **Estimate: 2 wk.**

### M4 — `vertical-align`
- sub/super/baseline-shift on inline runs; line-box math already tracks
  max ascent/descent, so this is a per-word offset in `PendingWord` + paint.
- **Estimate: 0.5 wk.**

**M total: 7–8 wk.**

---

## Phase N — CSS & layout completeness

### N1 — `clear: left/right/both`
- Float registry exists; `clear` consults it and pushes the box's flow cursor
  below matching floats (the containment loop already does this implicitly —
  this makes it explicit and per-side).
- **Estimate: 0.5 wk.**

### N2 — Table `rowspan`
- Table row model gains cell height-spanning: pass 1 assigns base rows per
  cell (`rowspan` attr, cap 12 like colspan), pass 2 sizes spanned rows so the
  cell fits. Documented limitation removed.
- **Estimate: 1 wk.**

### N3 — Full HTML entity table
- Replace `entities.rs`'s ~50-entry subset with the complete WHATWG
  named-entity set (~2,200), generated from the spec's JSON into a static
  table; keep longest-first matching (the `&#…;` numeric path already exists).
- **Estimate: 0.5 wk.**

### N4 — Background paint model
- `BackgroundPaint { Color(u32), LinearGradient(..), RadialGradient(..),
  Image(Url) }` in `ComputedStyle`; CSS parser accepts gradient functions and
  `background-image: url()`.
- Layout resolves paint geometry (angle/extent math at box level); actual
  pixels render in L4 (CPU approximation before that: solid color from the
  first stop — graceful, documented).
- **Estimate: 1 wk.**

### N5 — `@font-face`
- Parser keeps the block (it's currently skipped); style engine registers
  `family → {url, format}`; the font collection (M2) fetches via the net
  cache, registers into Core Text (`CTFontManagerRegisterFontsForURL`), and
  uses it in the chain. Display-list contract unaffected.
- **Estimate: 1–1.5 wk.**

### N6 — Dynamic pseudo-classes & `:nth-child`
- Engine-side *interaction state set*: `engine.set_hover(node)/
  set_active(node)` (embedder sends from mouse events; focus already tracked).
  Style recomputation rides the G incremental path (dirty-chain recompute —
  hover changes dirty only the affected subtrees).
- Selectors: `:hover`, `:active`, `:focus`, `:checked` (state set lookup),
  `:disabled`, `:nth-child(An+B)` (static parse + match), `:not()` already
  exists.
- **Estimate: 1.5 wk.**

### N7 — `@supports`
- Condition evaluator alongside `@media`'s (property: value support table);
  parser already delimits the block.
- **Estimate: 0.5 wk.**

### N8 — Declarative animations (Kitae-style, no script)
- `@keyframes` + `animation` on **transform/opacity/color/background only**;
  CSS parses, the *embedder* interpolates: engine publishes keyframe data in
  frames, embedder animates published display lists (re-publish per tick, no
  engine-thread timeline, no script). Honors `Theme.motionEnabled` reduce-
  motion on the Swift side for free.
- **Estimate: 2 wk.**

**N total: 8–9.5 wk.**

---

## Phase O — Network hardening

### O1 — POST
- `kitae_net`: `fetch_with(method, url, headers, body)` alongside `fetch`/
  `fetch_cached` (POST responses never cached); `FetchConfig` untouched.
- `submit_form`: `method=post` + `enctype=application/x-www-form-urlencoded`
  body building; `multipart/form-data` **out of scope** (documented).
- **Estimate: 1 wk.**

### O2 — Cookie jar
- In-memory, per-host: `Set-Cookie` parsing (name=value, Path, Domain, Secure,
  HttpOnly, Expires/Max-Age), sent on same-host requests; Secure not sent over
  http; jar dies with the process (no persistence — a reader browser has no
  cross-session cookie story by design).
- Opt-out via `FetchConfig { cookies: bool }`.
- **Estimate: 1 wk.**

### O3 — Conditional requests
- Cache entries store `ETag`/`Last-Modified`; revalidation on `max-age`
  expiry: `If-None-Match`/`If-Modified-Since` → 304 serves the cached body.
- **Estimate: 1 wk.**

### O4 — Streaming parse
- Response body streams into the tree builder in chunks (builder already
  processes text incrementally behind `TreeBuilder`); first paint fires when
  the body's first meaningful chunk parses (progressive `content_height`
  updates via the normal frame path).
- Fallback: non-streamed path stays for tests.
- **Estimate: 1.5 wk.**

**O total: 4.5 wk.**

---

## Phase P — Find-in-page + accessibility

### P1 — Find-in-page
- `Engine::find_in_page(query) -> Vec<Match>` (`Match { rects: Vec<Rect>,
  index }`): walks display-list text runs (already page-space), case-folded
  substring match across run boundaries (adjacent runs on one line merge).
- Frames carry match count + active index; embedder highlights by overlaying
  the rects (Swift) — no display-list change needed for v1.
- **Estimate: 1 wk.**

### P2 — Accessibility tree
- DOM → AX list: role per tag (h1–h6 → heading, a → link, button/input/
  select/textarea → button/textbox/combo, img → image w/ alt, table → table),
  label = text content/placeholder, **bounds from layout** (node → box lookup
  the interaction code already does for links).
- Published via FFI as a side-table in frames (id → role/label/rect/children);
  Swift bridges to NSAccessibility children of the canvas view; `focused_id`
  (exists since slice H) maps to `NSAccessibilityFocusedElement`.
- **Estimate: 2 wk.**

**P total: 3 wk.**

---

## Sequencing, totals, and what's deliberately out

| Order | Phase | Depends on | Estimate |
|---|---|---|---|
| 1 | **K. Real image decode** | (shared plumbing lands with QTSQ J3 — do J first) | 3.5–5 wk |
| 2 | **L. wgpu/Metal renderer** | K3 textures; L2 atlas reused by M | 8–10 wk |
| 3 | **M. Shaping + bidi** | L2 atlas, M2 feeds N5 | 7–8 wk |
| 4 | **N. CSS/layout completeness** | N4 renders via L4; N6 rides G; N1–N3/N7 standalone (can interleave into any gap) | 8–9.5 wk |
| 5 | **O. Network hardening** | independent — can interleave anywhere | 4.5 wk |
| 6 | **P. Find + a11y** | final layout/interaction shape | 3 wk |

**Total: ~35–40 focused weeks for the engine core proper** (plus QTSQ's
7.5–9.5 in its own plan; J/K share the image plumbing which is why K assumes
J's cache/FFI work exists).

**Out of scope by standing decision (never planned here):** JS engines, WASM,
page WebGPU API, Kitae Bridge (post-kitae_live), kitae_ui (separate track,
`UI_LIBRARY_PLAN.md`), `multipart/form-data`, ICC color management.

**Suggested interleavings if you want visible wins earlier:** N1+N3+N7 (two
weeks, pure engine, no renderer dependency) can slot in after J; O1–O3 any
time after J.
