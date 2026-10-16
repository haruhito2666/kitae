/* kitae_headless.h — the pure-Rust page rasterizer, for hosts that link it.
 *
 * The app links libkitae_headless.a and makes one call at startup:
 *
 *     if (kitae_headless_install_raster(engine, 2.0)) { … }
 *
 * which builds the rasterizer over the system's own fonts, at `scale` device
 * pixels per CSS pixel, and hands the engine its vtable (see
 * kitae_engine_set_raster in kitae_core.h for the function contract). Until
 * that call the agent's `screenshot` and `pdf` verbs answer with
 * "no page rasterizer is installed in this session…" — never a blank image.
 *
 * No C is involved in the rasterizing itself: fontdb finds the user's fonts,
 * ttf-parser reads glyph outlines, tiny-skia fills them, and the engine's own
 * CPU compositor paints the display list underneath. The Rust module doc
 * (crates/kitae_headless/src/lib.rs) states the honest limits of v1 — no
 * shaping, raster PDF pages, no subpixel positioning.
 *
 * This header is standalone: it needs no other kitae header. */

#ifndef KITAE_HEADLESS_H
#define KITAE_HEADLESS_H

/* The engine handle, forward-declared: the host passes the pointer it got
 * from kitae_engine_new() straight through, so the type stays opaque here
 * (and this header keeps working on its own — including from Swift, where the
 * parameter arrives as OpaquePointer). */
struct KitaeEngine;

#ifdef __cplusplus
extern "C" {
#endif

/* Install this rasterizer on `engine` (an engine from kitae_engine_new).
 * `scale` is device pixels per CSS pixel: 2.0 matches a retina screenshot,
 * 1.0 keeps the pixels — and the memory — down. Values outside [0.1, 4.0] are
 * clamped.
 *
 * Returns 1 when a rasterizer was installed and 0 when the system has no
 * usable fonts or `engine` is NULL; the agent verbs then keep their
 * "no rasterizer" error, which names the reason. The rasterizer is released
 * when the engine is destroyed. */
int kitae_headless_install_raster(struct KitaeEngine *engine, double scale);

#ifdef __cplusplus
}
#endif

#endif /* KITAE_HEADLESS_H */
