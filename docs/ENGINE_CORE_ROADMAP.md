# Kitae Engine Core — Full Completion Roadmap

> **Superseded in ordering, 2026-10-15:** the owner pivot
> (`AGENTIC_PIVOT.md`) makes the standard-JS/agentic program the current
> focus — see `ROADMAP.md` §The current plan (Tracks A–E). The
> engine-completion items below remain valid engineering targets, taken
> opportunistically where the Track-A conformance panel demands them;
> first-vision engine phases get no new standalone starts.
>
> Status: PLANNING. Written 2026-09-03, after the engine-core goal (slices A–I)
> verified complete — see `ENGINE_CORE_STATUS.md` for what exists today
> (152 tests green, DisplayList v2, FFI frame v3).
>
> **Implementation-level detail now lives in two standalone plans:**
> - **QTSQ native support (Phase J):** [`QTSQ_INTEGRATION_PLAN.md`](QTSQ_INTEGRATION_PLAN.md)
> - **All remaining engine-core features (Phases K–P):** [`ENGINE_FEATURES_PLAN.md`](ENGINE_FEATURES_PLAN.md)
>
> This doc is the high-level roadmap; those two are the build-ready specs.
> This doc covers everything that remains: the second-tier engine features,
> the **wgpu/Metal renderer**, and a new first-class addition — **built-in
> native support for the QTSQ format** (`/Users/priyanshuboruah/projects/black-hole-disk`),
> the user's own binary format for images, text, audio, and video.

---

## 0. Guiding rules (unchanged, apply to every phase)

1. **No JavaScript, no JS engines — ever.** The engine's endpoint is the display list.
2. **No WASM — ever.**
3. Pure Rust engine. C++ (and C) enter **only as link-in libraries behind a C
   ABI** — libqtsq, codecs, HarfBuzz — never owning a thread, loop, or window.
   Orchestration always lives in Rust (`CPP_ROLE.md`).
4. macOS-only native frontend (Swift Kitae.app); Makefile + swiftc, no .xcodeproj.
5. The name **Kitae** never changes.
6. Every slice ships with tests and stays `cargo test --workspace` green.

---

## Phase J — QTSQ native format (the user's format, first-class)

**Goal:** Kitae renders `.qtsq` files natively — images, text, audio, video —
exactly like it renders PNG or HTML today. QTSQ becomes a *media type of the
engine*, not a plugin.

**Library:** `libqtsq.a` (C, MIT) from the black-hole-disk project, linked
statically into `kitae_media`-style bindings. A new crate `kitae_qtsq` wraps
the C API in safe Rust (matches the CPP_ROLE "library author, not a program"
rule: per-call contexts, no threads of its own; engine threads call it).

### J1 — `kitae_qtsq` bindings crate
- Build: Makefile step compiles/links `libqtsq.a` (arm64) into the workspace
  (pattern: how `libkitae_core.a` flows into the app today).
- Bindings over `qtsq.h`: context create/free, `qtsq_compress*` (not needed by
  the browser initially), `qtsq_decompress`, `qtsq_decompress_pixel16`,
  `qtsq_decompress_splat_only`, `qtsq_decompress_audio_float/hq`,
  `qtsq_video_decoder_*` (open, frame_count, `get_frame`).
- Safe API: `QtsqFile::probe(bytes) -> Option<QtsqInfo>` (magic `QTS\x01`,
  strategy/data-type/width/height/channels/encrypted — parse the fixed header
  in Rust from `qtsq_format.h`, no decode), `decode_image() -> Rgba {w, h, buf}`,
  `decode_text() -> String`, `audio_info() -> (sample_rate, channels, frames)`,
  `video_open() -> VideoDecoder {fps, w, h, frame_count}`.
- Tests: hand-crafted + real `.qtsq` fixtures from the black-hole-disk `data/`
  and `demos/` folders (image, text, audio, video one each).
- **Estimate: 1–1.5 wk.**

### J2 — sniffing + navigation
- `kitae_net`: content-type mapping `.qtsq → application/x-qtsq`; magic-byte
  sniffing joins the existing HTML/binary gate (QTSQ is a *known binary*, not
  rejected).
- `kitae_core`: a QTSQ navigation synthesizes a viewer page (same mechanism as
  today's direct-image pages):
  - image payload → `<img>`-style page with the decoded raster,
  - text/JSON/CSV payload → text page (`decode_text`, JSON pretty-printed),
  - audio payload → media element page (see J4),
  - video payload → media element page (see J5),
  - tensor/unknown strategy → info page (strategy, size, dims) — honest
    fallback, never a blank page.
- Tests: navigate-to-`.qtsq` for each payload kind at the `Engine` facade level.
- **Estimate: 1 wk.**

### J3 — QTSQ images inside pages
- `<img src="photo.qtsq">`: `image_dimensions` learns the QTSQ header (parse
  w/h from bytes, no decode); `layout_image`/render path gains a decoded-raster
  source alongside PNG/GIF/JPEG headers.
- The FFI frame v3 `DisplayImage` items carry decoded pixels; Swift already
  draws image placeholders — upgrade to real bitmaps (this overlaps with
  Phase K's general image work; do the plumbing once for both).
- **Estimate: 1 wk.**

### J4 — QTSQ audio playback
- Architecture (keeps the display-list contract): audio is **decoded by the
  engine, played by the embedder**. The viewer page paints a native player
  placeholder (a `Rect` + text run like today's form controls) plus a
  `MediaHandle` published through `PublicState`/FFI.
- New FFI commands (15–17): `media_open(url)`, `media_play/pause/seek`,
  `media_state`. Engine decodes on demand (`qtsq_decompress_audio_hq` → f32
  interleaved PCM) in the tab's engine thread; PCM handed to Swift via a
  small ring-buffer FFI or written to a temp CAF file for AVFoundation.
- Swift: `QTSQAudioPlayerView` (AVFoundation/CoreAudio), play/pause/scrub UI in
  the content card.
- **Estimate: 2 wk.**

### J5 — QTSQ video playback
- Same split: engine owns decode, embedder owns the clock.
- DisplayList v3: new `DisplayMedia { id, x, y, w, h, kind, state }` item
  (`DISPLAY_LIST_VERSION` 2 → 3; non-exhaustive contract handles this).
- Engine: `qtsq_video_decoder_get_frame(i)` → RGBA; embedder drives
  `media_seek(t)` / frame stepping at the container fps; the engine paints the
  current frame as a `DisplayImage` inside the `DisplayMedia`'s rect.
- Swift: `QTSQVideoPlayerView` (CAMetalLayer or CALayer bitmap swap), transport
  controls; SDL shell draws the current frame rect (test harness).
- **Estimate: 2–3 wk.**

**Phase J total: ~7–9 wk.** Acceptance: open any of the four payload kinds
from a URL or file, see/hear it, with tests green at every layer
(bindings → core synthesis → layout → FFI → Swift).

---

## Phase K — Real image decode (PNG/JPEG pixels, not just headers)

**Goal:** `DisplayImage` items carry actual pixels for ordinary web images.

- K1: PNG decode in pure Rust (`image` crate or a minimal inflate + unfilter —
  prefer the `png` crate for auditability; `zlib-rs`/`flate2` is already in
  policy). ICC/gamma ignored initially.
  **Estimate: 1 wk.**
- K2: JPEG decode — per `CPP_ROLE.md`, C++ `libjpeg-turbo` behind the C ABI in
  `kitae_media` (or `zune-jpeg` first if we want to stay pure-Rust for one more
  phase; decide at implementation, both fit the seam).
  **Estimate: 1–2 wk.**
- K3: Pixel plumbing — decoded RGBA cache keyed by URL in `kitae_core`; FFI
  frame carries bitmaps (shared with J3's plumbing); Swift `PageCanvasView`
  draws bitmaps; SDL shell blits them.
  **Estimate: 1–1.5 wk.**
- K4: `object-fit` real behavior (contain/cover crop math happens at paint with
  real pixels instead of the current placeholder logic).
  **Estimate: 0.5 wk.**

**Phase K total: ~3.5–5 wk.**

---

## Phase L — wgpu renderer (Metal backend) + CPU fallback

**Goal:** the agreed long-term render path (Servo-style): the display list is
the contract, wgpu renders it on Metal; the CPU rasterizer remains the
permanent software fallback.

- L1: `kitae_render_gpu` crate — wgpu device/surface, pipeline for solid-rect
  quads (instanced), scissor-rect clipping, page-background clear. Parity
  target: everything `kitae_render` (SDL) draws today.
  **Estimate: 2 wk.**
- L2: Glyph atlas — Core Text rasterizes glyphs (Swift) **or** a Rust glyph
  rasterizer; atlas texture + per-glyph instanced quads. Text is the hard 80%.
  **Estimate: 2–3 wk.**
- L3: Images as textures — `DisplayImage`/video frames upload as GPU textures;
  `object-fit` becomes UV math; bilinear scaling.
  **Estimate: 1 wk.**
- L4: Paint upgrades unlocked by GPU (also implemented in the CPU path where
  cheap): `border-radius` (SDF shader + CPU rounded-rect approximation),
  linear/radial gradients (paint-server registry in kitae_style/layout),
  `box-shadow` (blurred quad passes).
  **Estimate: 2–3 wk.**
- L5: Integration — SDL shell keeps CPU path (regression gate, always green);
  Kitae.app gets a `CAMetalLayer` canvas view toggled by a Settings switch
  (GPU on/off proves the fallback story); present loop synced to the tab's
  engine thread mailbox.
  **Estimate: 1.5 wk.**
- Tests: golden-image tests (render same display list through both backends,
  compare downsampled hashes), plus per-feature unit tests.

**Phase L total: ~8–10 wk.**

---

## Phase M — Text shaping, bidi, international text

**Goal:** typography-first for *all* scripts, not just Latin.

- M1: `kitae_shape` crate — HarfBuzz behind the C ABI (per `CPP_ROLE.md`; if
  we want pure-Rust first, `rustybuzz` slots behind the same trait — decide at
  implementation). Shaped runs replace per-char measurement behind
  `TextMeasurer`: clusters, ligatures, kerning, marks.
  **Estimate: 3 wk.**
- M2: Font collection — Core Text font enumeration/selection per CSS
  `font-family` fallback chains; per-script font fallback (Arabic → Geeza Pro,
  Devanagari → Devanagari MT, emoji → Apple Color Emoji).
  **Estimate: 2 wk.**
- M3: Unicode Bidirectional Algorithm (UBA) for line runs; `direction: rtl`
  CSS property (parse → style → line layout → text-align flip).
  **Estimate: 2 wk.**
- M4: `vertical-align` (sub/super/baseline shifts on inline runs).
  **Estimate: 0.5 wk.**

**Phase M total: ~7–8 wk.**

---

## Phase N — CSS & layout completeness

- N1: `clear: left/right/both` for floats (float registry already exists —
  push flow cursor below matching floats). **0.5 wk**
- N2: Table `rowspan` (row model gains cell-height spanning; documented
  limitation removed). **1 wk**
- N3: Full HTML entity table (complete WHATWG named-entity set generated from
  the spec list; replaces the ~50-entry subset). **0.5 wk**
- N4: Gradients + `background-image` in CSS parsing/style (paint lands in L4;
  this is the data model: `BackgroundPaint { Color, LinearGradient(..),
  RadialGradient(..), Image(url) }`). **1 wk**
- N5: `@font-face` (font URL fetch via kitae_net cache, registration into the
  font collection from M2). **1–1.5 wk**
- N6: More selectors/pseudo-classes: `:hover`/`:active`/`:focus` (engine-side
  *state set* the embedder updates — `engine.set_hover(node)`; styles recompute
  through the G incremental path), `:checked`, `:disabled`,
  `:nth-child(An+B)`. **1.5 wk**
- N7: `@supports` condition evaluation (parser already skips the block —
  evaluate and keep). **0.5 wk**
- N8: Declarative animations, Kitae-style: `@transition`/`@keyframes` subset
  on *transform/opacity/color* only, evaluated by the embedder (no script, no
  timeline thread in the engine — the embedder interpolates and republishes
  display lists). Fits the no-JS/WASM rules. **2 wk**

**Phase N total: ~8–9 wk.**

---

## Phase O — Network hardening

- O1: POST — `method`/`body` in `FetchConfig`-level API + form
  `method=post`/`enctype` in `submit_form` (file upload out of scope).
  **1 wk**
- O2: Cookie jar (in-memory, per-host, Secure/HttpOnly respected, expired on
  restart) with opt-out config. **1 wk**
- O3: HTTP caching correctness pass — ETag/Last-Modified revalidation on top
  of the existing LRU + max-age. **1 wk**
- O4: Response streaming into the parser for progressive first paint of large
  HTML (engine thread already owns the loop; chunked parse hook). **1.5 wk**

**Phase O total: ~4.5 wk.**

---

## Phase P — Find-in-page + accessibility

- P1: `find_in_page(query) -> Vec<Rect>` — text-run scan, match rects for
  highlight painting (embedder overlays; engine publishes match count/index).
  **1 wk**
- P2: Accessibility tree — DOM → AX node list (role from tag: heading/link/
  button/image/textbox), bounds from layout boxes, published via FFI; Swift
  bridges to NSAccessibility. **2 wk**

**Phase P total: ~3 wk.**

---

## Sequencing & estimate

| Order | Phase | Why this order | Estimate |
|---|---|---|---|
| 1 | **J. QTSQ native format** | User's own feature; J3 shares image plumbing with K | 7–9 wk |
| 2 | **K. Real image decode** | Needed by J3 pixels, L3 textures | 3.5–5 wk |
| 3 | **L. wgpu/Metal renderer** | Unlocks border-radius/gradients/shadows paint (N4/N8 visuals) | 8–10 wk |
| 4 | **M. Shaping + bidi** | Highest-value typography work; benefits from L's glyph atlas | 7–8 wk |
| 5 | **N. CSS/layout completeness** | N1–N3 are engine-only and could interleave earlier if wanted | 8–9 wk |
| 6 | **O. Network hardening** | Independent; can interleave anywhere after J | 4.5 wk |
| 7 | **P. Find + a11y** | Polish on top of final layout/interaction | 3 wk |

**Total: roughly 41–48 focused weeks.** J and K share plumbing (do them
back-to-back); L and M both touch text (L2's glyph atlas is a prerequisite
structure M reuses). O and P are independent and can slot into gaps.

**Deliberately out of scope (standing decisions):** JS engines, WASM, page
WebGPU API, Kitae Bridge (post-kitae_live), kitae_ui (separate track per
`UI_LIBRARY_PLAN.md`).

---

## Test & acceptance discipline (all phases)

- Every slice: unit tests in the owning crate + at least one `Engine`
  facade-level integration test; `cargo test --workspace` stays green.
- J: fixture `.qtsq` files committed under `crates/kitae_qtsq/testdata/`.
- L: dual-backend golden tests (CPU vs GPU hash parity) + a Settings toggle to
  prove the CPU fallback path survives.
- M: shaping tests use real system fonts on the dev Mac (Core Text owns
  faces); golden-width tests per script.
- Renderer-facing features (L4, N4, N8) get a Swift-side visual verification
  pass using the established screencapture recipe.
