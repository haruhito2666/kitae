/*
 * QTSQ Metal GPU Engine — Public C API
 * Provides a bridge from QTSQ's C core to macOS Metal compute shaders.
 * Author: Haruhito
 */

#ifndef QTSQ_METAL_H
#define QTSQ_METAL_H

#include <stdint.h>
#include <stddef.h>
#include "qtsq.h"
#include "qtsq_splat2d.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque context holding MTLDevice, MTLCommandQueue, and pipeline states */
typedef struct qtsq_metal_ctx qtsq_metal_ctx_t;

/* Initialize the Metal engine (compiles shaders, creates queues) */
qtsq_metal_ctx_t* qtsq_metal_init(void);

/* Retrieve a globally shared Metal context for utility invocations */
qtsq_metal_ctx_t* qtsq_metal_global(void);

/* Free all Metal resources */
void qtsq_metal_destroy(qtsq_metal_ctx_t *ctx);

/* 
 * Barycentric Triangle Rasterization [GPU Compute]
 * output: WxH RGB buffer
 * blur_map: WxH single-channel buffer for blur depth 
 */
int qtsq_metal_rasterize(qtsq_metal_ctx_t *ctx,
                         const tri_vertex_t *verts, uint32_t nv,
                         const tri_face_t *faces, uint32_t nf,
                         uint32_t w, uint32_t h, uint8_t ch,
                         uint8_t *output, uint8_t *blur_map, const uint8_t *tier_mask);

/* 
 * 3x3 Box Blur with Masking [GPU Compute] 
 * Uses blur_map to apply variable-strength blurring over sharp triangle edges.
 */
int qtsq_metal_blur(qtsq_metal_ctx_t *ctx,
                    const uint8_t *input, uint8_t *output,
                    uint32_t w, uint32_t h, uint8_t ch,
                    const uint8_t *blur_map);

/*
 * Gaussian Splat Rendering [GPU Compute]
 * Alpha blends 2D/3D splats onto the output RGB buffer.
 */
int qtsq_metal_render_splats(qtsq_metal_ctx_t *ctx,
                             const splat2d_t *splats, uint32_t count,
                             uint32_t w, uint32_t h, uint8_t ch,
                             uint8_t *output, const uint8_t *fg_mask);

/*
 * Fast Splat-Centric Rendering [GPU Compute — Two-Pass]
 * Optimized for real-time video: O(splats × radius²) instead of O(pixels × splats).
 * Pass 1: Each splat stamps its Gaussian onto atomic accumulation buffers.
 * Pass 2: Each pixel normalizes accumulated color by total weight.
 */
int qtsq_metal_render_splats_fast(qtsq_metal_ctx_t *ctx,
                                   const splat2d_t *splats, uint32_t count,
                                   uint32_t w, uint32_t h, uint8_t ch,
                                   uint8_t *output);

/*
 * Sobel Edge Detection Gradient [GPU Compute]
 * Calculates gradient magnitudes for vertex placement saliency.
 */
int qtsq_metal_sobel(qtsq_metal_ctx_t *ctx,
                     const uint8_t *pixels, float *gradient,
                     uint32_t w, uint32_t h, uint8_t ch);

/*
 * Fast Pixel Format Conversion (BGRA -> RGB) [GPU Compute]
 * Used directly by the frame extractor to bypass slow CPU loops.
 */
int qtsq_metal_bgra_to_rgb(qtsq_metal_ctx_t *ctx,
                           const uint8_t *bgra, uint8_t *rgb,
                           uint32_t w, uint32_t h, uint32_t stride);

/*
 * Frame Diff for P-frame Encoding [GPU Compute]
 * Computes per-pixel delta between current and reference frames.
 * Delta is bias-encoded: 128 = no change, >128 = brighter, <128 = darker.
 * Returns change_ratio: fraction of pixels that changed beyond threshold (0.0 - 1.0).
 * Use this to decide if a P-frame is worth it or if we should promote to I-frame.
 */
int qtsq_metal_frame_diff(qtsq_metal_ctx_t *ctx,
                           const uint8_t *current, const uint8_t *reference,
                           uint8_t *delta, float *change_ratio,
                           uint32_t w, uint32_t h, uint8_t ch, uint32_t threshold);

/*
 * Region Classification [GPU Compute]
 * Thresholds Sobel gradient into binary mask:
 *   mask=1 → triangle zone (detail), mask=0 → splat zone (smooth)
 */
int qtsq_metal_classify_regions(qtsq_metal_ctx_t *ctx,
                                 const float *gradient, uint8_t *mask,
                                 uint32_t w, uint32_t h, float threshold);

/*
 * GPU Delaunay Triangulation [GPU Compute + JFA]
 * Runs the full Jump Flooding Algorithm and extracts Delaunay triangles.
 * vertex_pos: float2 array of vertex positions (x, y)
 * tris_out: pre-allocated buffer for triangle indices (3 uint32 per tri)
 * num_tris_out: receives the number of triangles generated
 * max_tris: capacity of tris_out buffer
 */
int qtsq_metal_gpu_delaunay(qtsq_metal_ctx_t *ctx,
                             const float *vertex_pos, uint32_t num_vertices,
                             uint32_t *tris_out, uint32_t *num_tris_out,
                             uint32_t max_tris, uint32_t w, uint32_t h);

/*
 * Triangle Color Fitting [GPU Compute]
 * For each vertex, sample source pixel color (3x3 neighborhood average).
 * colors_out: packed RGB bytes, 3 bytes per vertex
 */
int qtsq_metal_fit_triangle_colors(qtsq_metal_ctx_t *ctx,
                                    const float *vertex_pos, uint32_t num_vertices,
                                    const uint8_t *source, uint8_t *colors_out,
                                    uint32_t w, uint32_t h, uint8_t ch);

/*
 * Splat Fitting for Smooth Regions [GPU Compute]
 * Fits Gaussian Splats in mask=0 (smooth) regions.
 * splat_pos: float2 array of candidate splat positions
 * splats_out: receives fitted splat data (pos, color, radius, alpha)
 *   Format per splat: [float x][float y][uchar r,g,b][uchar radius][uchar alpha] = 12 bytes
 * num_active_out: receives number of active (alpha>0) splats
 */
int qtsq_metal_fit_splats(qtsq_metal_ctx_t *ctx,
                           const float *splat_pos, uint32_t num_splats,
                           const uint8_t *source, const uint8_t *mask,
                           const float *gradient,
                           uint8_t *splats_out, uint32_t *num_active_out,
                           uint32_t w, uint32_t h, uint8_t ch);

/*
 * Splat Color Sampling [GPU Compute]
 * For each splat, averages source pixels within its radius to compute color.
 * splat_packed: [uint16 x, uint16 y, uint8 radius] × num_splats (5 bytes each)
 * colors_out: [uint8 r, g, b] × num_splats
 */
int qtsq_metal_splat_color_sample(qtsq_metal_ctx_t *ctx,
                                   const uint8_t *splat_packed, uint32_t num_splats,
                                   const uint8_t *source, uint8_t *colors_out,
                                   uint32_t w, uint32_t h, uint8_t ch);

/*
 * Hybrid Triangle + Splat Rendering [GPU Compute — 3-Pass]
 * For real-time video: triangles cover ~80% (detail), splats cover ~20% (smooth).
 * Pass 1: Rasterize triangles via compute kernel → tri_buffer
 * Pass 2: Stamp splats into accumulation buffers
 * Pass 3: Composite — splat zones override, triangle zones passthrough
 * Uses persistent GPU buffers (allocated once, reused per frame).
 */
int qtsq_metal_render_hybrid(qtsq_metal_ctx_t *ctx,
                              const tri_vertex_t *verts, uint32_t num_verts,
                              const tri_face_t *faces, uint32_t num_faces,
                              const splat2d_t *splats, uint32_t num_splats,
                              uint32_t w, uint32_t h, uint8_t ch,
                              uint8_t *output);

#ifdef __cplusplus
}
#endif

#endif /* QTSQ_METAL_H */
