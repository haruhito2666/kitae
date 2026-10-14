/*
 * qtsq_triangle_only.h — Pure Triangle Mesh Compression (No Splats)
 *
 * Provides triangle-only compression at three quality levels.
 * Output is a pure Delaunay triangle mesh (TRM3 format) with NO
 * Gaussian splat overlay. Designed for DTCN neural network input.
 *
 * Author: Priyanshu Boruah / QTSQ Project
 */

#ifndef QTSQ_TRIANGLE_ONLY_H
#define QTSQ_TRIANGLE_ONLY_H

#include "qtsq_format.h"

/* ══════════════════════════════════════════════════════════════
 *  Pure Triangle-Only Compression API
 *
 *  All four modes produce TRM3 format (vertices only, no splats).
 *  The decoder performs Delaunay triangulation on the vertices and
 *  rasterizes the mesh with barycentric RGB interpolation.
 *
 *  Quality Levels:
 *    Compact  —  25K-400K vertices, 30-40% of original size
 *    HD       —  200K-3M vertices, 70-85% of original size
 *    Lossless —  500K-6M vertices, near-lossless quality
 *    Pixel    —  W×H vertices (1:1), pixel-perfect, for ML training
 * ══════════════════════════════════════════════════════════════ */

int qtsq_compress_triangle_compact(qtsq_context_t *ctx,
                                    const uint8_t *pixels,
                                    uint32_t width, uint32_t height,
                                    uint8_t channels, size_t source_size);

int qtsq_compress_triangle_hd(qtsq_context_t *ctx,
                                const uint8_t *pixels,
                                uint32_t width, uint32_t height,
                                uint8_t channels, size_t source_size);

int qtsq_compress_triangle_lossless(qtsq_context_t *ctx,
                                      const uint8_t *pixels,
                                      uint32_t width, uint32_t height,
                                      uint8_t channels, size_t source_size);

int qtsq_compress_triangle_pixel(qtsq_context_t *ctx,
                                   const uint8_t *pixels,
                                   uint32_t width, uint32_t height,
                                   uint8_t channels, size_t source_size);

/* ══════════════════════════════════════════════════════════════
 *  Adaptive Recursive Triangle Subdivision API
 *
 *  After initial Delaunay triangulation, measures per-triangle
 *  color error against the original image. Triangles exceeding
 *  error_threshold are recursively subdivided by inserting edge
 *  midpoint vertices and splitting into 4 child triangles.
 *
 *  Output is TRM5 format (vertices + faces + depth per face).
 *  Decoder skips Delaunay — faces are pre-computed.
 *
 *  Parameters:
 *    max_depth        — max subdivision levels (1-4, default 3)
 *    error_threshold  — MSE threshold per pixel (default 15.0)
 * ══════════════════════════════════════════════════════════════ */

int qtsq_compress_triangle_adaptive(qtsq_context_t *ctx,
                                     const uint8_t *pixels,
                                     uint32_t width, uint32_t height,
                                     uint8_t channels, size_t source_size,
                                     uint8_t max_depth,
                                     float error_threshold);

/* Decompress: same as gravitational — Delaunay + barycentric rasterization.
 * Pixel mode uses direct vertex→pixel write (no interpolation).
 * TRM5 (adaptive) uses pre-computed faces — no Delaunay needed.
 * Uses the existing qtsq_decompress_gravitational() since TRM3/5 format is shared. */

/* ══════════════════════════════════════════════════════════════
 *  Artist Controls — Fine-grained mesh parameters
 *
 *  For artists who want explicit control over the mesh style.
 *  These functions create a triangle mesh with custom parameters
 *  rather than using the preset quality tiers.
 *
 *  Parameters:
 *    density          — vertex budget as fraction of pixels (0.01-1.0)
 *                       0.01 = sparse/faceted, 1.0 = pixel-perfect
 *    edge_sensitivity — how much to concentrate vertices on edges (0.0-1.0)
 *                       0.0 = uniform grid, 1.0 = maximum edge focus
 *    edge_threshold   — Sobel gradient threshold for edge detection (0-255)
 *                       lower = more edges detected, higher = only strong edges
 * ══════════════════════════════════════════════════════════════ */

int qtsq_compress_triangle_custom(qtsq_context_t *ctx,
                                   const uint8_t *pixels,
                                   uint32_t width, uint32_t height,
                                   uint8_t channels,
                                   float density,
                                   float edge_sensitivity,
                                   float edge_threshold);

/* ══════════════════════════════════════════════════════════════
 *  Preset Styles — Common artistic looks
 * ══════════════════════════════════════════════════════════════ */

/* Low-poly look: sparse mesh, strong edge focus */
int qtsq_compress_triangle_lowpoly(qtsq_context_t *ctx,
                                    const uint8_t *pixels,
                                    uint32_t width, uint32_t height,
                                    uint8_t channels);

/* Smooth/faceted: medium density, low edge sensitivity */
int qtsq_compress_triangle_smooth(qtsq_context_t *ctx,
                                   const uint8_t *pixels,
                                   uint32_t width, uint32_t height,
                                   uint8_t channels);

#endif /* QTSQ_TRIANGLE_ONLY_H */
