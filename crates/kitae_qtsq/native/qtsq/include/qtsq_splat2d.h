/*
 * QTSQ 2D Gaussian Splat Engine
 * Part of QTSQ HD v3 — Hybrid Triangle + Splat Compression
 * Author: Haruhito
 *
 * 2D Gaussian splats represent smooth, low-detail background regions
 * (sky, walls, bokeh, shadows) while triangle meshes handle complex
 * foreground subjects (faces, hair, trees, text).
 */

#ifndef QTSQ_SPLAT2D_H
#define QTSQ_SPLAT2D_H

#include <stdint.h>
#include <stddef.h>

/* ── 2D/3D Gaussian Splat Primitive ── 12 bytes packed ──
 * 
 * Phase 8 NOTE: Uses uint16_t x,y + uint8_t sx,sy, quantized to pixel grid.
 * Re-renderable by scaling but NOT truly resolution-independent (would need float).
 * Future V4 format: float32 positions for true vector-like resolution-independence.
 * Current claim: "parametric / re-renderable by scaling, quantized to capture grid."
 */
typedef struct {
    uint16_t x, y;       /* center position (pixels)             */
    uint8_t  sx, sy;     /* sigma spread in X and Y (1–255 px)   */
    uint8_t  theta;      /* rotation angle (0–180° → 0–255)      */
    uint8_t  r, g, b;    /* color                                */
    uint8_t  alpha;      /* opacity (0=transparent, 255=opaque)  */
    uint8_t  z;          /* depth (0=background, 255=foreground) */
} splat2d_t;

/* ── Shape Type Encoding (Stored in top 2 bits of z) ──
 * 00 = Rotated Ellipse (default, sy and theta used)
 * 01 = Circle (isotropic, sy = sx, theta ignored)
 * 10 = Triangle (equilateral, size = sx, rotation = theta)
 * 11 = reserved
 */
#define SPLAT_SHAPE_ELLIPSE   0
#define SPLAT_SHAPE_CIRCLE    1
#define SPLAT_SHAPE_TRIANGLE  2

#define SPLAT_SHAPE(sp)       ((sp)->z >> 6)
#define SPLAT_SET_SHAPE(sp, s) ((sp)->z = ((sp)->z & 0x3F) | (((s) & 0x03) << 6))

/* ── Render splats onto an RGB buffer ──
 * Splats are alpha-blended onto the existing buffer contents.
 * Pixels covered by foreground triangles (fg_mask[i]=1) are skipped.
 * If fg_mask is NULL, splats render everywhere.
 */
void splat2d_render(const splat2d_t *splats, uint32_t count,
                    uint8_t *rgb, uint32_t width, uint32_t height,
                    const uint8_t *fg_mask);

/* ── Fit splats to background regions of an image ──
 * Uses the is_fg[] block map to identify background areas and fits
 * Gaussian splats to represent them efficiently.
 * Returns number of splats placed (up to max_splats).
 */
uint32_t splat2d_fit_background(const uint8_t *pixels,
                                 uint32_t width, uint32_t height,
                                 uint8_t channels,
                                 const uint8_t *is_fg,
                                 uint32_t bw, uint32_t bh,
                                 uint32_t block_size,
                                 splat2d_t *out_splats,
                                 uint32_t max_splats);

/* ── Pack splats into GSP2/GSP3 binary format ──
 * Returns malloc'd buffer. Caller owns it.
 * Format: "GSP2" (11 byte) / "GSP3" (12 byte) magic + count + planar arrays.
 */
int splat2d_pack_gsp2(const splat2d_t *splats, uint32_t count,
                       uint32_t width, uint32_t height,
                       uint8_t **out_data, size_t *out_size);
                       
int splat2d_pack_gsp3(const splat2d_t *splats, uint32_t count,
                       uint32_t width, uint32_t height,
                       uint8_t **out_data, size_t *out_size);

/* ── Unpack splats from binary format ──
 * Allocates the splat array. Caller must free it.
 */
int splat2d_unpack_gsp2(const uint8_t *data, size_t size,
                         splat2d_t **out_splats, uint32_t *out_count,
                         uint32_t *out_width, uint32_t *out_height);

int splat2d_unpack_gsp3(const uint8_t *data, size_t size,
                         splat2d_t **out_splats, uint32_t *out_count,
                         uint32_t *out_width, uint32_t *out_height);

/* ── Estimate depth (z) for 3D splats based on local focus/contrast ── */
void splat2d_estimate_depth(splat2d_t *splats, uint32_t count,
                            const uint8_t *target, uint32_t width, uint32_t height,
                            uint8_t channels);

/* ── Refine splats via Analytical Gradient Descent ──
 * Performs a multi-pass optimization of splat properties (pos, scale, color)
 * against the target image background to minimize residual error.
 */
void splat2d_refine_gradient_descent(splat2d_t *splats, uint32_t count,
                                      const uint8_t *target, uint32_t width, uint32_t height,
                                      uint8_t channels, const uint8_t *is_fg,
                                      int iters);

/* ── Jittered Multi-View Gradient Descent ──
 * Generates num_views jittered copies of the target image in RAM,
 * then runs gradient descent using a different view per iteration.
 * Gives sub-pixel accuracy from a single captured photo.
 */
void splat2d_refine_jittered_multiview(splat2d_t *splats, uint32_t count,
                                        const uint8_t *target, uint32_t width, uint32_t height,
                                        uint8_t channels, const uint8_t *is_fg,
                                        int iters, int num_views);

/* ── Resolution-Independent Render ──
 * Renders splats at an arbitrary output resolution by scaling positions
 * and sigma values from the original capture resolution to the target.
 *
 * @param splats       Splat array (positions in original resolution)
 * @param count        Number of splats
 * @param orig_w       Original image width (capture resolution)
 * @param orig_h       Original image height
 * @param output       Output RGB buffer (target_w * target_h * 3)
 * @param target_w     Target render width
 * @param target_h     Target render height
 */
void splat2d_render_scaled(const splat2d_t *splats, uint32_t count,
                           uint32_t orig_w, uint32_t orig_h,
                           uint8_t *output, uint32_t target_w, uint32_t target_h);

#endif /* QTSQ_SPLAT2D_H */
