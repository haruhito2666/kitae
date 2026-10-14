# B7 · Media codecs decision point

> Status: **DECIDED 2026-09-09 — Option 2, shipped** (user directive
> "now complete all of F at once"). The engine side stayed pure Rust:
> `kitae_media::video` demuxes MP4 and WebM/MKV into compressed samples +
> extradata, and the macOS host app decodes H.264/HEVC/VP9 through
> VideoToolbox (`app/Sources/WebVideoDecoder.swift`) — the platform's own
> framework, so no C codec library was vendored and `CPP_ROLE.md` was never
> exercised. AV1/VP8 fall back to the placeholder frame (VideoToolbox has
> no AV1 decoder on this OS target; a future slice could add dav1d).
> AVIF image decode remains deferred separately (parity-plan §B).

## What needs decoding

A browser has to decode four things from the network:

| Container | Codec | Common source | Royalty status |
|---|---|---|---|
| `.mp4` (ISO BMFF) | H.264 / AVC | Most user videos on the web | **Patent-encumbered** (MPEG-LA) |
| `.mp4` | H.265 / HEVC | Apple/iPhone 4K HDR | Patent-encumbered (multiple pools) |
| `.mp4` | AV1 | YouTube, Netflix, Vimeo newer | Royalty-free (AOMedia) |
| `.webm` (Matroska) | VP9 | YouTube, WebM archives | Royalty-free (Google) |
| `.webm` | AV1 | YouTube newer | Royalty-free |
| Image `.avif` | AV1 stills | Modern web | Royalty-free |
| Image `.webp` | WebP / VP8 | Older modern web | Royalty-free |
| Image `.jpg/.png/.gif` | JPEG / PNG / GIF | Everywhere | Royalty-free (PNG/GIF), JPEG baseline OK |

## Decoder matrix (what we have, what we'd add)

| Format | Decoded by | Where it lives | Notes |
|---|---|---|---|
| JPEG | zune-jpeg (pure Rust) | `crates/kitae_media` | Works today |
| PNG | `image` crate | `crates/kitae_media` | Works today |
| GIF | `image` crate | `crates/kitae_media` | Works today (animated too via Q8) |
| WebP | `image` crate | `crates/kitae_media` | Works today (still + anim) |
| AVIF | None today | would need `dav1d` via `kitae_media` C++ wrapper | Royalty-free; ~250k LOC C |
| VP9 | None today | would need `libvpx` via `kitae_media` C++ wrapper | Royalty-free |
| AV1 video | None today | would need `dav1d` via `kitae_media` C++ wrapper | Royalty-free |
| H.264 | None today | would need FFmpeg + libx264 OR VideoToolbox on macOS | **Patent-encumbered** |
| H.265 | None today | would need `libde265` + licensing | **Patent-encumbered** |
| AAC audio | None today | would need `fdkaac` or FFmpeg | **Patent-encumbered** |

## Three options

### Option 1 — Royalty-free only (recommended for first cut)

Ship `kitae_media` with:
- `dav1d` (AV1 stills + AV1 video) — single decoder covers both AVIF
  and `.webm` (AV1).
- `libvpx` (VP9) — covers the older `.webm` archive.
- Ogg/Opus audio (royalty-free, Xiph).
- H.264/H.265/AAC are NOT decoded. Pages with `<video src="foo.mp4">`
  show the engine's existing "media-not-supported" UI (a card explaining
  the format). macOS users can still use Safari for those.

**Cost**: 1–2 weeks of integration (`cmake` crate, wrapper crate,
engine hookup), all under the existing C++ policy.

**Coverage**: ~85% of the public web (YouTube AV1, Vimeo AV1, most
podcast .webm archives, modern AVIF images). The 15% is the
long-tail of legacy `.mp4` (H.264) — mostly user uploads older than
~2018.

**Risk**: The decision is REVERSIBLE. Once `kitae_media` exists, adding
an H.264 shim is a sub-system, not a re-architecture. macOS can use
VideoToolbox hardware decoding (no licensing work), which is the path
we'd take if Option 1 turns out to leave too much on the table.

### Option 2 — Royalty-free + H.264 via macOS VideoToolbox

Same as Option 1, plus: route H.264 through `VideoToolbox` (`CMVideoFormatDescriptionCreateFromH264ParameterSets` + `VTDecompressionSessionDecodeFrame`). This is **free** (no per-unit royalties because Apple pays MPEG-LA), hardware-accelerated, and the canonical macOS path.

**Cost**: +3–5 days on top of Option 1 (Swift-side wrapper; FFI
from the engine's media item into CoreMedia). Note that this only
helps macOS — Linux/Windows builds of `kitae_media` still wouldn't
have H.264, but those aren't on the roadmap today.

**Coverage**: ~99% of the public web. The only holes are HEVC and
unusual legacy formats.

**Risk**: VideoToolbox is private-ish CoreMedia, well-supported on
macOS 10.9+. The wrapper is straightforward but requires the Swift
host (not the Rust engine) to do the decode — the engine still
hands a media item to the host and the host decodes + uploads
frames back. This is the pattern Q3/Q8 already use.

### Option 3 — Full FFmpeg (everything)

Ship FFmpeg with all codecs, including libx264 (LGPL build) and
AAC. Covers every media format the web has ever shipped.

**Cost**: +1 week of build wiring (FFmpeg's CMake is a beast), ~80 MB
of additional binary, ongoing maintenance for security advisories.

**Coverage**: 100%.

**Risk**: FFmpeg ships a lot of code that we don't need (filters,
muxers, demuxers, network protocols, deprecated codecs). The
maintenance burden on a small team is real. GPL/LGPL licensing
implications are not free either — distributing a static build
puts the project under copyleft terms if the linking is "intimate"
enough. Building everything as `.so`/`.dylib` loaded at runtime is
the workaround, but that's not how our static-lib pattern works.

## Recommendation

**Option 2**: start with the royalty-free stack (Option 1) because it
delivers ~85% of the public web at low cost, then add H.264 via
VideoToolbox when the need surfaces. Skip FFmpeg.

The reasoning:
- The C++ policy in `CPP_ROLE.md` explicitly scopes C++ to "best-in-class
  native codecs that have no equal in pure Rust" — `dav1d` fits; FFmpeg is
  a kitchen-sink we don't need.
- We have no evidence today that the 15% of H.264-only media matters for
  our users. YouTube, Vimeo, and the major podcast archives have all
  shipped AV1/VP9 alternatives. The remaining H.264-only content is
  legacy user uploads.
- macOS is the only target. VideoToolbox removes the licensing AND the
  patent encumbrance for H.264 cleanly. The engine/host split already
  handles the decode-on-host pattern.

If we discover the AV1-only stack is too narrow (e.g. a partner site is
unreachable because it serves only H.264), Option 2 is a 3–5 day add-on
on top of Option 1's foundation. We keep the option open without paying
for it now.

## Action items, if Option 2 is approved

1. Add `dav1d` and `libvpx` as C++ static libs under
   `native/media/{dav1d,vpx}/` with a `CMakeLists.txt` each.
2. New Rust wrapper `crates/kitae_media_v2/` (or extend the existing
   `kitae_media` crate) with `decode_avif`, `decode_av1_video`,
   `decode_vp9_video`. The wrapper's FFI matches the pattern in
   `kitae_renderer.cpp` today (caller-allocated output buffers, integer
   return codes).
3. Engine integration: extend `EngineFrame::media` to carry a
   `codec: MediaCodec` enum and an `is_animated: bool`. Layout still
   emits one paint per decoded frame; the host owns frame stepping
   (Q8 already).
4. H.264 via VideoToolbox: Swift-side shim in `QTSQAudioPlayerView.swift`
   or a new `VideoDecoder.swift`. Engine hands the host a media item
   with codec=H264 and frame-byte payload; host runs
   `VTDecompressionSessionDecodeFrame` and pushes the decoded image
   back through the existing `kitae_qtsq_raster_for_id` channel.
5. Tests: a single tiny AV1 video fixture (50–100 KB), a single AVIF
   image fixture, an MP4 with H.264 (only the macOS build needs to
   decode it). All in `tests/fixtures/media/`.

## Decision needed

Pick 1, 2, or 3. (Default if no answer: 2 — start with royalty-free, add
VideoToolbox H.264 in the same push.)
