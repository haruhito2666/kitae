/*
 * QTSQ 4D Gaussian Splatting Engine
 * Full 3DGS/4DGS renderer with SH color, EWA rasterization, PLY import.
 *
 * Implements:
 *   Kerbl et al. "3D Gaussian Splatting for Real-Time Radiance Field
 *   Rendering", SIGGRAPH 2023
 *   Wu et al. "4D Gaussian Splatting for Real-Time Dynamic Scene
 *   Rendering", CVPR 2024
 *
 * Author: Haruhito
 */

#ifndef QTSQ_4DGS_H
#define QTSQ_4DGS_H

#include "qtsq_format.h"
#include <stdint.h>
#include <stddef.h>

/* ══════════════════════════════════════════════════════════════
 *  Gaussian4D — Full 4DGS Gaussian Primitive (256 bytes)
 *
 *  Implements the standard 3DGS representation extended with
 *  4D temporal fields (Wu et al. 2024).
 *
 *  Color = SH_eval(view_dir, sh_coeffs) → clamped [0, 1]
 *  Alpha = sigmoid(opacity) × exp(−½ (x−μ)^T Σ^{-1} (x−μ))
 *  Σ    = R S S^T R^T  (covariance in world space)
 * ══════════════════════════════════════════════════════════════ */
typedef struct {
    /* ── Position (world space) ── */
    float   x, y, z;             /* 12 B  — center μ_xyz                          */

    /* ── Anisotropic 3D scale (stored as log for optimization) ── */
    float   sx, sy, sz;          /* 24 B  — exp(sx)=scale_x, etc.                */

    /* ── Rotation quaternion (w, x, y, z) ── */
    float   qw, qx, qy, qz;      /* 40 B  — orientation of the Gaussian           */

    /* ── Opacity (stored as logit for optimization) ── */
    float   opacity;             /* 44 B  — σ(opacity) ∈ [0,1] after sigmoid      */

    /* ── Spherical Harmonics — degree 0–3, 48 floats ──
     *    sh[0..2]     = f_dc  (degree 0, base RGB)
     *    sh[3..17]    = f_rest R channel (degree 1–3, 15 coeffs)
     *    sh[18..32]   = f_rest G channel
     *    sh[33..47]   = f_rest B channel                                         */
    float   sh[48];              /* 236 B — SH coefficients for view-dep. color    */

    /* ── 4D temporal fields (Wu et al. 2024) ── */
    float   t_center;            /* 240 B — temporal mean μ_t                      */
    float   t_scale;             /* 244 B — temporal std-dev σ_t (0 = static)      */
    float   wx, wy, wz;          /* 256 B — space-time coupling ω                  */
} gaussian_4d_t;

_Static_assert(sizeof(gaussian_4d_t) == 256, "gaussian_4d_t must be 256 bytes");

/* ══════════════════════════════════════════════════════════════
 *  Camera (pinhole model)
 * ══════════════════════════════════════════════════════════════ */
typedef struct {
    float   width, height;       /* image dimensions in pixels                     */
    float   fx, fy;              /* focal lengths                                  */
    float   cx, cy;              /* principal point                                */
    float   z_near, z_far;       /* clipping planes                                */

    /* World-to-Camera transforms */
    float   R[9];                /* 3×3 rotation matrix (column-major)             */
    float   t[3];                /* translation                                    */
} gs_camera_t;

/* Convenience: construct identity camera looking down -Z */
gs_camera_t gs_camera_default(float width, float height);
void gs_camera_lookat(gs_camera_t *cam,
                       float eye_x, float eye_y, float eye_z,
                       float target_x, float target_y, float target_z,
                       float up_x, float up_y, float up_z);

/* ══════════════════════════════════════════════════════════════
 *  Gaussian Model (collection of Gaussians)
 * ══════════════════════════════════════════════════════════════ */
typedef struct {
    gaussian_4d_t  *gaussians;
    uint32_t         count;
    uint32_t         capacity;

    /* Precomputed bounding-box for culling */
    float            aabb_min[3];
    float            aabb_max[3];
} gs_model_t;

void gs_model_init(gs_model_t *model);
void gs_model_free(gs_model_t *model);
int  gs_model_add(gs_model_t *model, const gaussian_4d_t *g);
int  gs_model_append_array(gs_model_t *model, const gaussian_4d_t *arr, uint32_t count);

/* ══════════════════════════════════════════════════════════════
 *  PLY Import — Parse standard 3DGS / 4DGS .ply files
 *
 *  Reads vertex properties: x,y,z, f_dc_[0-2], f_rest_*,
 *  opacity, scale_[0-2], rot_[0-3], and optional 4D fields.
 *
 *  Supports binary little-endian and ASCII PLY.
 * ══════════════════════════════════════════════════════════════ */
int gs_model_load_ply(gs_model_t *model, const char *filepath);

/* ══════════════════════════════════════════════════════════════
 *  SH Evaluation — Compute view-dependent color
 *
 *  view_dir: normalized 3D vector from Gaussian to camera
 *  Returns RGB in [0, 1] range
 * ══════════════════════════════════════════════════════════════ */
void gs_eval_sh(const gaussian_4d_t *g, const float view_dir[3],
                float *out_r, float *out_g, float *out_b);

/* ══════════════════════════════════════════════════════════════
 *  Rasterizer — EWA Splatting with Tile-Based Sorting
 *
 *  Renders all Gaussians to an RGBA8 framebuffer at time t.
 *
 *  output: pre-allocated buffer of size width×height×4 (RGBA)
 *  t:      time value (only affects Gaussians with t_scale > 0)
 *
 *  Returns 0 on success, -1 on error.
 * ══════════════════════════════════════════════════════════════ */
int gs_render(const gs_model_t *model, const gs_camera_t *cam,
              float t, uint8_t *output);

/* ══════════════════════════════════════════════════════════════
 *  Binary Packing — For .qtsq container storage
 * ══════════════════════════════════════════════════════════════ */

/* Pack gaussian_4d_t array into binary (planar-friendly).
 * Format: [u32 count][gaussian_4d_t × count] = 4 + 256·N bytes */
int gs_4d_pack(const gaussian_4d_t *gaussians, uint32_t count,
               uint8_t **out_data, size_t *out_size);

/* Unpack back to gaussian_4d_t array */
int gs_4d_unpack(const uint8_t *data, size_t size,
                 gaussian_4d_t **out_gaussians, uint32_t *out_count);

/* ══════════════════════════════════════════════════════════════
 *  Container helpers — read/write .qtsq with 4DGS models
 * ══════════════════════════════════════════════════════════════ */

/* Write a 4DGS model to a .qtsq file with zlib-compressed streams */
int gs_container_write(const char *filename, const gs_model_t *model);

/* Read a 4DGS model from a .qtsq file */
int gs_container_read(const char *filename, gs_model_t *model);

#endif /* QTSQ_4DGS_H */
