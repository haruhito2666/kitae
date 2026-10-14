/*
 * QTSQ Surfel — captured 2D Gaussian splat geometry
 *
 * Storage + container serialization for surfels produced by the on-device
 * 2DGS capture feature (Huang et al. 2024 + Yang et al. 2025 unbiased depth).
 * A surfel is a 2D oriented Gaussian disk in WORLD space — distinct from the
 * image-space painterly splat2d (uint16 x,y) and from the volumetric 4DGS
 * Gaussian. The field layout matches the host trainer's surfel_t so a trained
 * scene round-trips through this format without a conversion layer.
 *
 * Parameters are stored in the trainer's UNCONSTRAINED form (log scale, logit
 * opacity, normalized quaternion) so a saved scene can be reloaded and the
 * optimization resumed bit-for-bit.
 *
 * Author: Haruhito
 */

#ifndef QTSQ_SURFEL_H
#define QTSQ_SURFEL_H

#include "qtsq_format.h"
#include "qtsq_tsdf.h"
#include <stdint.h>
#include <stddef.h>

/* ══════════════════════════════════════════════════════════════
 *  qtsq_surfel_t — one 2D oriented Gaussian disk (52 bytes)
 *
 *    center[3]      world position p_k
 *    quat[4]        orientation [w,x,y,z]; columns of R = (t_u, t_v, n)
 *    log_scale[2]   log of the two tangent scales (scale = exp(...))
 *    opacity_logit  alpha = sigmoid(opacity_logit)
 *    color[3]       DC RGB (degree-0); higher SH bands added via format ver.
 * ══════════════════════════════════════════════════════════════ */
typedef struct {
    float center[3];
    float quat[4];
    float log_scale[2];
    float opacity_logit;
    float color[3];
} qtsq_surfel_t;

_Static_assert(sizeof(qtsq_surfel_t) == 52, "qtsq_surfel_t must be 52 bytes");

/* ══════════════════════════════════════════════════════════════
 *  qtsq_surfel_model_t — a captured surfel set
 * ══════════════════════════════════════════════════════════════ */
typedef struct {
    qtsq_surfel_t *surfels;
    uint32_t       count;
    uint32_t       capacity;
    float          aabb_min[3];
    float          aabb_max[3];
} qtsq_surfel_model_t;

void qtsq_surfel_model_init(qtsq_surfel_model_t *m);
void qtsq_surfel_model_free(qtsq_surfel_model_t *m);
int  qtsq_surfel_model_add(qtsq_surfel_model_t *m, const qtsq_surfel_t *s);
int  qtsq_surfel_model_append_array(qtsq_surfel_model_t *m,
                                    const qtsq_surfel_t *arr, uint32_t count);
/* Recompute aabb_min/aabb_max from the current surfels. */
void qtsq_surfel_model_recompute_aabb(qtsq_surfel_model_t *m);

/* ══════════════════════════════════════════════════════════════
 *  Binary packing
 *  Format: ['S','R','F','1'][u32 count][qtsq_surfel_t × count]
 * ══════════════════════════════════════════════════════════════ */
int qtsq_surfel_pack(const qtsq_surfel_t *surfels, uint32_t count,
                     uint8_t **out_data, size_t *out_size);
int qtsq_surfel_unpack(const uint8_t *data, size_t size,
                       qtsq_surfel_t **out_surfels, uint32_t *out_count);

/* ══════════════════════════════════════════════════════════════
 *  Container I/O — routes through the fail-closed secure write path
 *  (sanitize + keystore-encrypt are mandatory; mirrors gs_container_write).
 * ══════════════════════════════════════════════════════════════ */
int qtsq_surfel_container_write(const char *filename,
                                const qtsq_surfel_model_t *model);
int qtsq_surfel_container_write_sealed(const char *filename,
                                       const qtsq_surfel_model_t *model,
                                       const char *author_name,
                                       const char *license,
                                       uint32_t consent_flags);
/* Phase F: also embed a TSDF-extracted 'mesh' stream (mesh may be NULL for
 * the surfels-only form). Variants keep the exact historical semantics. */
int qtsq_surfel_container_write_mesh(const char *filename,
                                     const qtsq_surfel_model_t *model,
                                     const qtsq_mesh_t *mesh);
int qtsq_surfel_container_read(const char *filename,
                               qtsq_surfel_model_t *model);
int qtsq_surfel_container_read_with_mesh(const char *filename,
                                         qtsq_surfel_model_t *model,
                                         qtsq_mesh_t *mesh);

#endif /* QTSQ_SURFEL_H */
