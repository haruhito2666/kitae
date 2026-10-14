# C++ in Kitae — Its Role Now That Swift Owns the UI

> Status: the Swift frontend **is implemented** (Kitae.app in `app/`; the
> engine lives in `crates/kitae_core` behind a C ABI). What this doc
> describes — C++ returning as codec/shaper *libraries* — is still the plan
> for when images and advanced text shaping land.

## The decision

The frontend UI moves to **Swift** (AppKit/SwiftUI + Core Text). C++ is no
longer the window/paint shell. It does not disappear from the project — but
its role shrinks to one thing:

> **C++ is a library author, not a program.** It ships codecs and shapers as
> small static libraries behind a C ABI, linked and orchestrated by Rust.

The dividing line is the one the project already has:

- **Rust (main)** — the engine: networking, HTML, CSS, style, layout,
  history, content policy, orchestration. All decisions live here.
- **Swift** — the frontend: window, chrome UI, event handling, and page
  painting via Core Text. Swift owns the main loop and calls *into* Rust.
- **C++ (libraries only)** — best-in-class native codecs/shapers that have no
  equal in pure Rust yet. It never owns a thread, a loop, or a window again.

## Why keep any C++ at all?

Because three decades of C++ library work is still unmatched in exactly two
domains that matter to a browser:

| Domain | C++ library (the reference) | Pure-Rust alternative | Verdict |
|---|---|---|---|
| JPEG decoding | `libjpeg-turbo` (SIMD, decades of fuzzing) | `zune-jpeg` / `image` (good for stills) | C++ for speed + robustness |
| PNG / WebP / GIF | `libpng`, `libwebp` | `image` crate (fine for PNG/GIF) | C++ for WebP; PNG optional |
| Video (later) | `dav1d` (AV1), FFmpeg | none production-grade | C++ clearly |
| **Text shaping** | **HarfBuzz** (used by Chrome, Firefox, Safari) | `rustybuzz` (good, less complete) | C++ for full script coverage |
| HTTP / TLS | — | `reqwest` + `rustls` (already excellent, in use) | Rust, obviously |
| URL / IDNA | — | `url` + `icu4x` (in use) | Rust |
| Compression | zlib | `zlib-rs` / `flate2` (parity) | Rust |

**The two real C++ modules for Kitae:**

1. **Image decoding** (`kitae_media`): JPEG first — it is what most of the
   still-text web actually serves. PNG/WebP follow, video only if ever.
2. **Text shaping** (`kitae_shape`): HarfBuzz. For a *typography-first*
   browser this is the highest-value C++ dependency: correct Arabic,
   Devanagari, Bengali/Assamese, emoji, ligatures, and contextual kerning —
   things SDL2_ttf (and the current shell) simply cannot do.

Everything else stays pure Rust. We are not adding C++ for HTTP, parsing,
compression, SIMD, or "performance" — Rust meets or beats it there.

## What happens to `native/` when Swift lands

`native/kitae_renderer.cpp` (the SDL2 shell) gets **retired to
`native/sdl2-shell/`** as a reference implementation and portable fallback:

- Keep it building while the Swift app catches up (it is the only way to run
  Kitae today).
- If Kitae ever targets Linux/Windows, this shell is the cross-platform
  frontend — same engine, same ABI, different renderer.
- Once the Swift app reaches parity and Kitae is macOS-only, delete it.

## The integration pattern (same as today, smaller)

Every C++ module follows the exact pattern the SDL2 shell already proved:

```
crates/kitae_media/            # Rust wrapper crate (safe API)
  build.rs                     # cmake crate builds the C++ lib
  src/lib.rs                   # safe types + extern "C" declarations
native/media/                  # C++ sources (CMakeLists.txt, static lib)
native/include/kitae_media.h   # the contract — stable C ABI
```

Concrete example — the image module:

```c
// native/include/kitae_media.h  (contract, stable)
// Decodes JPEG bytes into RGBA8. Returns 0 on success.
// Output buffer is caller-allocated: out_size = width*height*4.
int kitae_decode_jpeg(const uint8_t* bytes, size_t len,
                      uint8_t* out, size_t out_size,
                      int* out_width, int* out_height);
```

```rust
// crates/kitae_media/src/lib.rs  (safe Rust wrapper)
pub fn decode_jpeg(bytes: &[u8]) -> Result<Image, MediaError> {
    // two-call pattern: ask dimensions, allocate, decode
    // all unsafe confined here; callers never see pointers
}
```

Rules for every C++ module (these are the project's FFI laws, learned from
the `push_text` dangling-pointer bug):

1. **All `unsafe` lives in the wrapper crate.** Engine code sees only safe
   Rust types.
2. **Strings and buffers are copied across the boundary** — the C++ side
   never keeps a pointer it did not copy or is not given ownership of.
3. **The header is the contract.** Comment it like `kitae_renderer.h` is:
   units, ownership, lifetime, error codes.
4. **Layout integration.** `kitae_layout`'s `TextMeasurer` trait gains a
   `shape()` path: Swift's Core Text measures plain text; HarfBuzz shapes
   complex runs. The layout engine does not know or care which is which.

## Stack summary

```
┌───────────────────────────── Swift app (frontend) ─────────────────────┐
│  NSWindow + native chrome, NSEvent input, Core Text page painting,     │
│  native scrollbar, dark mode, accessibility. Owns the main loop.       │
└──────────────────────────────┬─────────────────────────────────────────┘
                               │ C ABI (display list in, events out)
┌──────────────────────────────▼─────────────────────────────────────────┐
│                       Rust engine (the main program)                   │
│  kitae_net · kitae_dom/html · kitae_css/style · kitae_layout · kitae   │
│  — no JavaScript engine, no interpreter, no scripting of any kind —    │
└───────────┬───────────────────────────────────────┬────────────────────┘
            │ C ABI (small static libs)             │ pure Rust crates
┌───────────▼────────────┐              ┌───────────▼────────────────────┐
│  C++ library modules   │              │  reqwest/rustls · url/icu4x ·  │
│  kitae_media: libjpeg- │              │  zlib-rs · everything else     │
│  turbo, libwebp, dav1d │              │  — pure Rust, no C++ needed —  │
│  kitae_shape: HarfBuzz │              └────────────────────────────────┘
└────────────────────────┘
```

## Build order (when the Swift work starts)

1. Rust-as-library facade: `kitae` crate gains `crate-type = ["staticlib"]`
   plus an `extern "C"` API (`kitae_core_navigate`, `kitae_core_events`,
   `kitae_core_render(w, h) -> DisplayList`). The existing event loop is
   reused internally.
2. Swift app skeleton: window, chrome, Core Text renderer consuming the
   display list; measurement via `CTFont` behind the same `TextMeasurer`
   idea.
3. `native/` → `native/sdl2-shell/`; keep it green in CI until Swift parity.
4. C++ module #1: `kitae_media` (JPEG) — unlocks images.
5. C++ module #2: `kitae_shape` (HarfBuzz) — unlocks real typography.
