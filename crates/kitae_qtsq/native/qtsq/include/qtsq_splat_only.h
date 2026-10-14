/*
 * QTSQ Pure Splat Image Compression Engine
 * Represents entire images as dense 2D Gaussian Splat fields.
 * No triangle mesh, no Delaunay — splats only.
 *
 * Completely independent from the triangle+splat hybrid engine
 * (qtsq_gravitational.c / qtsq_splat2d.c).
 *
 * Author: Haruhito / QTSQ Project
 */

#ifndef QTSQ_SPLAT_ONLY_H
#define QTSQ_SPLAT_ONLY_H

#include <stdint.h>
#include <stddef.h>
#include "qtsq_format.h"
#include "qtsq_splat2d.h"  /* splat2d_t struct only */

/* ── Full-Image Splat Fitting ──
 * Fits splats to the ENTIRE image (all regions, no fg/bg split).
 * Multi-resolution: coarse (8×8) → medium (4×4) → fine (2×2) blocks.
 * Returns number of splats placed (up to max_splats). */
uint32_t splat_only_fit_image(const uint8_t *pixels,
                               uint32_t width, uint32_t height,
                               uint8_t channels,
                               splat2d_t *out_splats,
                               uint32_t max_splats);

/* ── Gradient Descent Refinement ──
 * Optimizes splat parameters (position, scale, color, rotation, alpha)
 * against the target image to minimize residual error.
 * Multi-threaded, with jittered multi-view support. */
void splat_only_refine(splat2d_t *splats, uint32_t count,
                        const uint8_t *target,
                        uint32_t width, uint32_t height,
                        uint8_t channels,
                        int iters, int num_views);

/* ── Pure Splat Renderer ──
 * Renders splat field to RGB buffer. No fg_mask — renders everywhere.
 * Sorts by area (large first) for correct compositing order.
 * Multi-threaded. */
void splat_only_render(const splat2d_t *splats, uint32_t count,
                        uint8_t *rgb, uint32_t width, uint32_t height,
                        uint8_t channels);

/* ── GSP4 Binary Format ──
 * Standalone splat-only container (self-contained, no TRM3 wrapper).
 * Format: "GSP4" | width(4) | height(4) | count(4) | planar splat data
 * Caller owns the returned buffer. */
int splat_only_pack(const splat2d_t *splats, uint32_t count,
                     uint32_t width, uint32_t height,
                     uint8_t **out_data, size_t *out_size);

int splat_only_unpack(const uint8_t *data, size_t size,
                       splat2d_t **out_splats, uint32_t *out_count,
                       uint32_t *out_width, uint32_t *out_height);

/* ── Compression API ── */
int qtsq_compress_splat_compact(qtsq_context_t *ctx,
                                 const uint8_t *pixels,
                                 uint32_t width, uint32_t height,
                                 uint8_t channels,
                                 size_t source_size);

int qtsq_compress_splat_hd(qtsq_context_t *ctx,
                             const uint8_t *pixels,
                             uint32_t width, uint32_t height,
                             uint8_t channels,
                             size_t source_size);

int qtsq_compress_splat_lossless(qtsq_context_t *ctx,
                                   const uint8_t *pixels,
                                   uint32_t width, uint32_t height,
                                   uint8_t channels,
                                   size_t source_size);

/* ── Decompression API ──
 * Completely independent from qtsq_decompress_gravitational.
 * Detects GSP4 magic and renders splats directly. */
int qtsq_decompress_splat_only(qtsq_context_t *ctx,
                                 uint8_t **out_data, size_t *out_size);

#endif /* QTSQ_SPLAT_ONLY_H */
