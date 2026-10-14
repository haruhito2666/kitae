# QTSQ Native Support in Kitae — Implementation Plan

> **STATUS (2026-09-25):** J1–J6 are implemented, including interactive QTSQ
> video transport. Current integration uses frame wire v20 for the host
> boundary, a 64-entry/192 MiB shared QTSQ/web raster cache, an 8-entry/64 MiB
> QTSQ video decoder/source cache, and a purge path that clears raster, video,
> media, and network state. The four selected features (password prompt,
> Save-Page-as-QTSQ, tensor visualization, and ML-DSA-65 signature verify) are
> shipped. Automated validation is 1149 workspace tests passed (3 ignored),
> including 33 `kitae_qtsq` tests, plus 9 native contracts. Supported video
> interaction, Markdown typography, cache purge/reload, and the broader manual
> release matrix remain pending.
>
> **Deployment:** app/native contracts target arm64 macOS 15.0 and the current
> executable reports `minos 15.0`. This verifies the local target, not a
> clean-machine or distribution release.
>
> Live detail and retained legacy-corpus history: **`QTSQ_STATUS.md`**. The
> acceptance text and J-stage details below remain the original plan; passing
> automation is not presented as completion of their manual GUI clauses.
>
> Standalone, detailed plan for one thing: making Kitae render and play the
> user's own **QTSQ** format (`.qtsq` — Quantum Tensor Sequence) natively.
> Source project: `/Users/priyanshuboruah/projects/black-hole-disk`
> (MIT, C, ships `libqtsq.a` + a clean C ABI in `include/qtsq.h`).
>
> This plan is self-contained: Phase J of `ENGINE_CORE_ROADMAP.md` expanded
> into an implementation-level spec. Everything else in Kitae is out of scope
> here.

---

## 1. What QTSQ is (from the actual project)

- Binary container, magic bytes `QTS\x01` (`QTSQ_MAGIC_0..3`), versions 1–3
  (`QTSQ_VERSION == 3`), file extension `.qtsq`.
- One file = one payload, chosen by a **strategy** enum
  (`qtsq_strategy_t` in `qtsq_format.h`) and a **data type** enum
  (`qtsq_data_type_t`):
  - **Images**: `STRATEGY_TRIANGLE` (mesh + splat), `STRATEGY_SPLAT` /
    `SPLAT_TSUBU` (Gaussian splats), `STRATEGY_PIXEL` (exact pixels, delta
    filter + zlib), `STRATEGY_IONIC` (fast I/O), `STRATEGY_4DGS`/`SURFEL`
    (3D/4D — out of scope for a browser), data types
    `IMAGE_GRAY/RGB/RGBA`.
  - **Text**: dictionary compression (`compress_text`, `compress_json`,
    `compress_csv` — all round-trip through `decompress_text`).
  - **Audio**: `STRATEGY_PHOTON` (HQ Gabor atoms) and `STRATEGY_OPUS`
    (Creative default, lossy); decode via `qtsq_decompress_audio_float`,
    `qtsq_decompress_audio_hq`, `qtsq_decompress_audio`; header carries
    `sample_rate` (uint32 in `qtsq_header_t`).
  - **Video**: frame-based encoder/decoder API —
    `qtsq_video_decoder_open` / `qtsq_video_decoder_get_frame(dec, frame_index,
    &pixels, &w, &h)` / `qtsq_video_decoder_frame_count`.
  - **Tensors / brain state**: `STRATEGY_TENSOR` (f32/f16/f8/f4/raw) — the
    browser shows these as an info page, it does not compute.
  - **Provenance/encryption flags** exist (`QTSQ_FLAG_*`, `encrypted` field in
    `qtsq_info()` output). Encrypted files the engine cannot decrypt render as
    an info page — never a hang, never a blank.
- The Python wheel (`pip install qtsq`) is irrelevant to Kitae — Kitae talks to
  the **C library** directly, same as the plugin apps do.

**Scope of "support" in Kitae:** images decode to RGBA and paint; text/JSON/CSV
decode and display as text pages; audio decodes to PCM and plays; video decodes
frame-by-frame and plays with a real clock. Tensors get an honest info page.

---

## 2. Architecture (one rule decides everything)

> **The engine decodes; the embedder plays.**

- The display list stays the engine's only visual contract. A `.qtsq` page
  looks like any other page: rects, texts, `DisplayImage`s, links.
- Audio/video *state* (playing/paused/position) is not a visual — it travels
  through `PublicState`/FFI like focus and selection already do.
- No decode threads are added anywhere. Decoding happens on the tab's existing
  engine thread, on demand, synchronously; long operations (full video open)
  are the embedder's problem to pace through commands.

This keeps the no-JS/no-WASM purity, keeps `DISPLAY_LIST_VERSION` honest, and
means the SDL shell can regression-test everything except actual sound.

---

## 3. Work breakdown

### Stage J1 — `kitae_qtsq` bindings crate (foundation)

**Deliverable:** a new workspace crate `crates/kitae_qtsq` that wraps
`libqtsq.a` in safe Rust.

- Build plumbing:
  - `native/` gains a Makefile step that builds black-hole-disk's `libqtsq.a`
    for arm64 (its own Makefile already emits `libqtsq.a`; Kitae just needs
    `make -C ../black-hole-disk libqtsq.a` or a vendored copy — decision at
    implementation, vendored-copy-into-`native/qtsq/` is the default so the
    browser build never depends on the sibling repo's state).
  - `build.rs` emits `cargo:rustc-link-lib=static=qtsq` +
    `cargo:rustc-link-search=native/qtsq/`, plus `-lz` (PIXEL strategy uses
    zlib; black-hole-disk's Makefile already handles its own deps like Opus).
- Hand-written FFI (no bindgen — the surface is small and stable):
  - `qtsq_context_t` create/free guarded in a `QtsqContext` RAII type.
  - Header probe **in pure Rust** (no C call needed): parse magic + version +
    strategy + data type + w/h/channels/encrypted straight from
    `qtsq_format.h`'s documented layout. This is what the net layer and
    `image_dimensions` will call — it never decodes.
- Safe API:
  ```rust
  pub struct QtsqInfo { pub strategy: QtsqStrategy, pub data_type: QtsqDataType,
                        pub width: u32, pub height: u32, pub channels: u32,
                        pub encrypted: bool, pub original_size: u64 }
  pub fn probe(bytes: &[u8]) -> Option<QtsqInfo>;           // header only
  pub fn decode_image(bytes: &[u8]) -> Result<RgbaImage, QtsqError>; // RGBA8
  pub fn decode_text(bytes: &[u8]) -> Result<String, QtsqError>;
  pub fn audio_info(bytes: &[u8]) -> Option<QtsqAudioInfo>;  // rate, channels, frames
  pub fn decode_audio(bytes: &[u8]) -> Result<PCMAudio, QtsqError>;  // f32 interleaved
  pub struct VideoDecoder { /* owns qtsq_video_decoder_t */ }
  impl VideoDecoder {
      pub fn open(bytes: &[u8]) -> Result<Self, QtsqError>;
      pub fn dimensions(&self) -> (u32, u32);
      pub fn frame_count(&self) -> u32;
      pub fn frame(&mut self, index: u32) -> Result<RgbaImage, QtsqError>;
  }
  ```
- `QtsqError` enum: `NotQtsq`, `UnsupportedStrategy`, `Encrypted`, `CApi(i32)`,
  `BadAudio`, `BadVideo`.
- **Tests:** fixture files. Generate once with the Python wheel
  (`qtsq.compress` an image, `compress_text`, an audio file, a tiny 4-frame
  video) and commit under `crates/kit_qtsq/testdata/` — one fixture per
  strategy Kitae supports. Unit tests: probe returns correct w/h/strategy;
  decode_image matches a reference PNG pixel-for-pixel where lossless
  (PIXEL strategy); decode_text round-trips; video frame(0) dims match;
  encrypted file → `QtsqError::Encrypted`; truncated file → error not crash;
  fuzz-lite loop (random byte flips on a fixture) must never panic.
- **Estimate: 1–1.5 wk.**

### Stage J2 — Sniffing + navigation (`.qtsq` as a first-class document)

**Deliverable:** navigating to a `.qtsq` URL renders a viewer page, same UX
shape as today's direct-image pages.

- `kitae_net`:
  - Extension → mime map gains `.qtsq → application/x-qtsq`.
  - The binary-payload gate (`is_html`/`is_plain_text`/`is_image`) gains
    `is_qtsq`: probe the header; QTSQ is a *known binary*, not a rejection.
- `kitae_core` (where direct-image navigation already synthesizes a page):
  - `application/x-qtsq` → dispatch on `probe().data_type`:
    - `IMAGE_*` → synthesize the existing `<img>`-placeholder page; the image
      stage (J3) upgrades it to real pixels.
    - text strategies → text page (JSON gets pretty-printed; CSV/JSON also get
      a `<pre>` wrap and the standard reader margins).
    - audio → media element page (J4's placeholder).
    - video → media element page (J5's placeholder).
    - tensor / brain / unknown / encrypted → **info page**: strategy name,
      data type, dimensions, original size, encryption flag — generated as
      plain HTML so it styles like every other page.
  - `page_title` for `.qtsq` navigations = filename.
- **Tests** at the `Engine` facade (embedder.rs pattern): navigate to each
  fixture kind → frame contains the expected placeholder/title text; info page
  for a tensor fixture; history back/forward works on a `.qtsq` navigation.
- **Estimate: 1 wk.**

### Stage J3 — QTSQ images inside pages

**Deliverable:** `<img src="photo.qtsq">` and direct-image pages show real
pixels.

- `kitae_layout::image_dimensions` learns the QTSQ header (call
  `kitae_qtsq::probe` after PNG/GIF/JPEG checks — header parse, no decode).
- `kitae_core` gains a small **decoded-raster cache**: `Url → (w, h, Arc<Vec<u8>>RGBA)`;
  layout/render paths already carry `DisplayImage { x, y, w, h, src, alt, fit }`
  — this stage makes the *shell* able to resolve `src` → pixels for QTSQ and
  (in the shared plumbing, Phase K) for web formats.
- FFI frame v3 already carries `DisplayImage`s; if pixels-in-frame lands here
  instead of Phase K, bump the frame format (append-only, versioned) — the
  Swift side already draws image placeholders, so the change is
  placeholder→bitmap behind the same item.
- **Tests:** layout test `qtsq_img_gets_real_dimensions`; core test that a
  page with a `.qtsq` img publishes a `DisplayImage` whose src resolves in the
  raster cache; FFI round-trip test of the image bytes.
- **Estimate: 1 wk.**

### Stage J4 — QTSQ audio playback

**Deliverable:** navigate to an audio `.qtsq` → native player UI → sound.

- Engine side:
  - Media viewer page paints a `Rect`-based player placeholder (same technique
    as form controls: bordered box, label = filename + duration) + a link-ish
    hit area.
  - `PublicState` gains `media: Option<MediaInfo>` (`MediaInfo { url, kind:
    Audio|Video, duration, rate, channels }`), published with frames.
  - New FFI commands **15 `media_open`, 16 `media_play` (payload: play/pause/
    seek seconds), 17 `media_state` (query)**; engine responds by decoding
    (`decode_audio` → f32 interleaved) and streaming PCM into a small
    lock-free ring buffer exposed over the FFI (`kitae_media_pcm_read(buf)`
    for Swift) — *or*, simpler v1: engine writes decoded PCM to a temp CAF
    file and hands the path to Swift (AVFoundation plays it natively; zero
    audio code in Rust). **Decision at implementation: CAF-file handoff is
    the default for v1** (fewer moving parts, macOS-native, trivially
    replaceable by the ring buffer later).
  - Play/pause/seek state machine lives in the embedder; the engine just
    decodes what it's asked to.
- Swift side:
  - `QTSQAudioPlayerView` (AppKit): transport controls (play/pause, scrubber,
    time labels) themed by `Theme.swift`; AVFoundation `AVAudioPlayer` over the
    handed-off CAF.
- **Tests:** core — audio fixture navigation publishes `MediaInfo` with the
  fixture's rate/channels; media_open/play state transitions; FFI round-trip
  of the command. Swift side verified visually (screencapture recipe).
- **Estimate: 2 wk.**

### Stage J5 — QTSQ video playback

**Deliverable:** navigate to a video `.qtsq` → transport UI → playing frames.

- DisplayList contract: **v3** — new item
  `DisplayMedia { id, x, y, w, h, kind, state: (playing, position_secs) }`;
  `DISPLAY_LIST_VERSION 2 → 3`. Non-exhaustive enum/structs keep embedders
  compiling; Swift + SDL update their decoders together.
- Engine side:
  - `VideoDecoder` opened lazily on first `media_open` for the tab's URL;
    `media_play`/`seek` commands advance `frame_index = position × fps`
    (fps = frame_count / duration from the container; duration published in
    `MediaInfo`).
  - The current frame paints as a `DisplayImage` inside the `DisplayMedia`
    rect — video is literally images plus a clock, reusing J3's raster path.
- Swift side:
  - `QTSQVideoPlayerView`: `CALayer` bitmap swap (or `CAMetalLayer` after
    Phase L) + transport controls; drives the clock with `CADisplayLink`-
    style timer, sends `media_play/seek` commands.
- SDL shell: draws the current frame rect (test harness for the pipeline
  without audio hardware).
- **Tests:** layout — `DisplayMedia` item geometry; core — frame stepping
  math, `MediaInfo` for video fixtures, FFI command round-trips; a
  deterministic 3-frame fixture with distinct solid colors lets tests assert
  frame(0/1/2) pixel identity end-to-end.
- **Estimate: 2–3 wk.**

### Stage J6 — Shell polish (small, closes the story)

- Kitae.app: `.qtsq` file open via Finder/Open panel (NSDocument-style
  `openFile:` → engine navigate `file://`), Quick-Look-style icon in the
  palette, "Export to PNG…" share-sheet entry for image payloads (decode →
  NSBitmapImageRep → save).
- SDL shell: `kitae test.qtsq` opens the viewer page (same code path as
  `kitae http://…`).
- **Estimate: 0.5–1 wk.**

---

## 4. Total, acceptance, and risks

| Stage | Content | Estimate |
|---|---|---|
| J1 | `kitae_qtsq` bindings + fixtures | 1–1.5 wk |
| J2 | Sniffing + synthesized viewer pages | 1 wk |
| J3 | QTSQ images (real pixels) | 1 wk |
| J4 | Audio playback (engine decodes, AVFoundation plays) | 2 wk |
| J5 | Video playback (DisplayList v3 + clock) | 2–3 wk |
| J6 | Shell polish (Finder open, export, SDL CLI) | 0.5–1 wk |
| | **Total** | **~7.5–9.5 wk** |

**Original acceptance target (preserved, not a completion claim):** from a
cold app, opening any of the four payload kinds — by URL, by `<img>`/embed
inside a page, or by Finder double-click — shows/hears/plays it; tensors and
encrypted files show an info page;
every stage ships unit tests and `cargo test --workspace` stays green; SDL
shell can play the video fixture end-to-end without audio hardware.

**Risks & mitigations:**
- *Header layout drift across QTSQ versions* → the pure-Rust probe pins to
  `QTSQ_VERSION == 3` and fails closed with `UnsupportedStrategy`/info page;
  never guesses.
- *libqtsq build coupling* → vendored snapshot under `native/qtsq/` decouples
  the browser build from the sibling repo; upstream refresh is a deliberate
  Makefile action.
- *Audio handoff latency* (CAF file per open) → fine for v1; the ring-buffer
  path is a drop-in upgrade that changes no contracts.
- *Video frame stepping cost* (`get_frame` is random-access) → cap seek
  granularity to container fps; if long files stall the engine thread, decode
  in `media_open`-sized batches between commands (still no extra threads —
  the engine thread just yields between command polls).
- *Strategy zoo* (4DGS, surfel, gravitational…) → out of scope by rule: only
  image/text/audio/video strategies are rendered; everything else gets the
  info page. Adding one later = one `match` arm + fixture.
