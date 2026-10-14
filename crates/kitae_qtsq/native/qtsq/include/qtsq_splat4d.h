/*
 * QTSQ 4D Continuous Gaussian Splat Engine
 * Part of QTSQ — 4DGS Velocity-Based Video Codec
 * Author: Haruhito
 *
 * Each Gaussian is a persistent particle with:
 *   - Position + velocity (continuous motion)
 *   - Lifespan (birth/death within segment)
 *   - Base color + temporal color drift
 *
 * This replaces discrete per-frame encoding with a continuous
 * mathematical representation — enabling:
 *   - Resolution independence (render at any pixel density)
 *   - Arbitrary framerate (evaluate at any timestamp)
 *   - Instant random access (no I-frame chasing)
 *   - Zero blocking artifacts (no DCT, no pixel grid)
 *   - Perfect gradient rendering (Gaussians ARE gradients)
 */

#ifndef QTSQ_SPLAT4D_H
#define QTSQ_SPLAT4D_H

#include <stdint.h>
#include <stddef.h>

/* ══════════════════════════════════════════════════════════════
 *  4D Continuous Gaussian Splat Primitive — 24 bytes packed
 *
 *  Velocity is Q8.8 fixed-point: 8 bits integer + 8 bits fraction
 *    Range: ±127.996 pixels/frame
 *    Precision: 1/256 pixel per frame (~0.004 px)
 *    At 24fps: ±3072 px/second — covers full 1080p width in 0.6s
 *
 *  Color drift is signed int8: ±127 per channel per frame
 *    Applied as: color(t) = base + drift * (t - birth)
 *    Clamped to [0, 255] at render time
 * ══════════════════════════════════════════════════════════════ */
typedef struct {
    /* ── Spatial (birth position + velocity) ── */
    uint16_t x0, y0;          /* birth position in pixels                    */
    int16_t  vx, vy;          /* velocity: Q8.8 fixed-point (px/frame)       */

    /* ── Shape ── */
    uint8_t  sx, sy;           /* sigma spread X, Y (1–255 px)               */
    uint8_t  theta;            /* rotation angle (0–180° → 0–255)            */
    uint8_t  alpha;            /* base opacity (0=transparent, 255=opaque)    */

    /* ── Temporal (lifespan within segment) ── */
    uint16_t birth_frame;      /* frame index when this Gaussian appears      */
    uint16_t death_frame;      /* frame index when this Gaussian disappears   */

    /* ── Color (base + linear temporal drift) ── */
    uint8_t  r, g, b;          /* base color at birth                        */
    int8_t   dr, dg, db;       /* linear color drift per frame (signed)      */

    /* ── Padding for alignment ── */
    uint8_t  _pad[2];          /* reserved (future: quadratic drift, depth)   */
} __attribute__((packed)) splat4d_t;

_Static_assert(sizeof(splat4d_t) == 24, "splat4d_t must be 24 bytes");

/* ── Q8.8 fixed-point helpers ── */
#define Q8_SCALE      256
#define Q8_TO_FLOAT(v)  ((float)(v) / (float)Q8_SCALE)
#define FLOAT_TO_Q8(f)  ((int16_t)((f) * (float)Q8_SCALE))

/* ══════════════════════════════════════════════════════════════
 *  Continuous Splat Evaluation
 *  Computes position and color at arbitrary time t
 * ══════════════════════════════════════════════════════════════ */

/* Check if a Gaussian is alive at time t */
static inline int splat4d_alive(const splat4d_t *sp, float t) {
    return (t >= (float)sp->birth_frame && t < (float)sp->death_frame);
}

/* Evaluate position at time t */
static inline void splat4d_position(const splat4d_t *sp, float t,
                                     float *out_x, float *out_y) {
    float dt = t - (float)sp->birth_frame;
    *out_x = (float)sp->x0 + Q8_TO_FLOAT(sp->vx) * dt;
    *out_y = (float)sp->y0 + Q8_TO_FLOAT(sp->vy) * dt;
}

/* Evaluate color at time t (clamped to [0, 255]) */
static inline void splat4d_color(const splat4d_t *sp, float t,
                                  uint8_t *out_r, uint8_t *out_g, uint8_t *out_b) {
    float dt = t - (float)sp->birth_frame;
    int cr = (int)sp->r + (int)((float)sp->dr * dt);
    int cg = (int)sp->g + (int)((float)sp->dg * dt);
    int cb = (int)sp->b + (int)((float)sp->db * dt);
    *out_r = (uint8_t)(cr < 0 ? 0 : (cr > 255 ? 255 : cr));
    *out_g = (uint8_t)(cg < 0 ? 0 : (cg > 255 ? 255 : cg));
    *out_b = (uint8_t)(cb < 0 ? 0 : (cb > 255 ? 255 : cb));
}

/* ══════════════════════════════════════════════════════════════
 *  Rendering
 * ══════════════════════════════════════════════════════════════ */

/* Render all alive Gaussians at arbitrary time t onto an RGB buffer.
 * This is the core decode operation — O(alive_splats × σ²).
 * Multi-threaded via horizontal strip decomposition. */
void splat4d_render_at_time(const splat4d_t *splats, uint32_t count,
                            float t, uint8_t *rgb,
                            uint32_t width, uint32_t height,
                            uint8_t channels);

/* Render at arbitrary time with custom output resolution.
 * Spatial coordinates scaled by (out_w/src_w, out_h/src_h).
 * This is the RESOLUTION INDEPENDENCE feature. */
void splat4d_render_scaled(const splat4d_t *splats, uint32_t count,
                           float t, uint8_t *rgb,
                           uint32_t src_width, uint32_t src_height,
                           uint32_t out_width, uint32_t out_height,
                           uint8_t channels);

/* ══════════════════════════════════════════════════════════════
 *  Fitting (Encoder)
 * ══════════════════════════════════════════════════════════════ */

/* Fit continuous splats to a sequence of frames.
 * This is the full encoder pipeline:
 *   1. Sobel gradient on frame 0
 *   2. Optical flow between consecutive frames
 *   3. Multi-resolution seed placement
 *   4. Temporal color drift estimation
 *   5. Lifespan assignment
 *
 * frames[i] points to the i-th frame's pixel data (RGB packed).
 * Returns number of splats placed (up to max_splats). */
uint32_t splat4d_fit_sequence(const uint8_t **frames, uint32_t num_frames,
                              uint32_t width, uint32_t height, uint8_t channels,
                              splat4d_t *out_splats, uint32_t max_splats);

/* ══════════════════════════════════════════════════════════════
 *  Refinement (Gradient Descent)
 * ══════════════════════════════════════════════════════════════ */

/* Refine splat parameters via multi-frame gradient descent.
 * Optimizes: position (x0,y0), velocity (vx,vy), sigma (sx,sy),
 * color (r,g,b), color drift (dr,dg,db), and lifespan.
 * Multi-threaded: each thread processes a subset of splats. */
void splat4d_refine(splat4d_t *splats, uint32_t count,
                    const uint8_t **frames, uint32_t num_frames,
                    uint32_t width, uint32_t height, uint8_t channels,
                    int iters);

/* ══════════════════════════════════════════════════════════════
 *  Densification
 * ══════════════════════════════════════════════════════════════ */

/* Split high-error splats into children for better coverage.
 * Children inherit parent velocity/drift. Returns new total count. */
uint32_t splat4d_densify(splat4d_t *splats, uint32_t count,
                          const uint8_t **frames, uint32_t num_frames,
                          uint32_t width, uint32_t height, uint8_t channels,
                          uint32_t max_capacity);

/* ══════════════════════════════════════════════════════════════
 *  Binary Format — Planar Packing for 'CONT' Segments
 * ══════════════════════════════════════════════════════════════ */

/* Pack splat4d array into planar binary format (for zlib).
 * Returns malloc'd buffer, caller owns it. */
int splat4d_pack(const splat4d_t *splats, uint32_t count,
                 uint32_t width, uint32_t height,
                 uint16_t seg_frames, uint16_t source_fps,
                 uint8_t **out_data, size_t *out_size);

/* Unpack planar binary format back to splat4d array.
 * Allocates the splat array, caller must free it. */
int splat4d_unpack(const uint8_t *data, size_t size,
                   splat4d_t **out_splats, uint32_t *out_count,
                   uint32_t *out_width, uint32_t *out_height,
                   uint16_t *out_seg_frames, uint16_t *out_source_fps);

/* ══════════════════════════════════════════════════════════════
 *  Motion Estimation (Internal, exposed for testing)
 * ══════════════════════════════════════════════════════════════ */

/* Compute dense optical flow between two frames via block matching.
 * Returns malloc'd arrays mvx[] and mvy[] of size (w/block)*(h/block).
 * Values are in pixels (float). Caller frees both. */
int splat4d_optical_flow(const uint8_t *frame_a, const uint8_t *frame_b,
                         uint32_t width, uint32_t height, uint8_t channels,
                         uint32_t block_size, uint32_t search_radius,
                         float **out_mvx, float **out_mvy,
                         uint32_t *out_bw, uint32_t *out_bh);

#endif /* QTSQ_SPLAT4D_H */
