/*
 * QTSQ 4D Atom (Tsubu) Splat Engine
 * Part of QTSQ — World-space 4D Gaussian splats for simulation + rendering
 * Author: Haruhito (Tsubu Engine extension)
 *
 * Each Tsubu atom is a persistent 3D world-space particle with:
 *   - Full physics state (position, velocity, mass, temperature, bonds)
 *   - Anisotropic 3D Gaussian shape (scale + quaternion rotation)
 *   - 4D Gaussian fields (temporal center, temporal spread, space-time coupling)
 *
 * The same primitive drives both simulation (PBD/SPH solver) and
 * rendering (EWA splat rasterization). It is a strict superset of
 * splat2d_t and splat4d_t.
 */

#ifndef QTSQ_SPLAT_TSUBU_H
#define QTSQ_SPLAT_TSUBU_H

#include "qtsq_format.h"
#include <stdint.h>
#include <stddef.h>
#include <math.h>

/* ══════════════════════════════════════════════════════════════
 *  Conditional Slice — evaluate atom at time t
 * ══════════════════════════════════════════════════════════════ */

/* Check if an atom is alive at time t */
static inline int tsubu_alive(const splat_tsubu_t *a, float t) {
    if (a->t_scale <= 0.0f) return 1; /* static atom, always alive */
    float dt = t - a->t_center;
    /* Alive within ±3 sigma */
    return (fabsf(dt) <= 3.0f * a->t_scale);
}

/* Evaluate 3D position at time t */
static inline void tsubu_position(const splat_tsubu_t *a, float t,
                                   float *out_x, float *out_y, float *out_z) {
    float dt = t - a->t_center;
    *out_x = a->x + a->wx * dt;
    *out_y = a->y + a->wy * dt;
    *out_z = a->z + a->wz * dt;
}

/* Evaluate opacity at time t (Gaussian temporal window) */
static inline float tsubu_alpha_at(const splat_tsubu_t *a, float t) {
    if (a->t_scale <= 0.0f) return 1.0f;
    float dt = t - a->t_center;
    float z = dt / a->t_scale;
    return expf(-0.5f * z * z);
}

/* ══════════════════════════════════════════════════════════════
 *  Pack / Unpack — Binary Format for Tsubu Streams
 *
 *  Each atom stream payload:
 *    [u32 atom_count]
 *    [splat_tsubu_t × atom_count]  (108 bytes per atom)
 *
 *  Pack: serializes to planar-friendly binary for compression
 *  Unpack: deserializes back to splat_tsubu_t array
 * ══════════════════════════════════════════════════════════════ */

/* Pack atom array into compact binary format.
 * Returns malloc'd buffer, caller owns it. */
int tsubu_pack(const splat_tsubu_t *atoms, uint32_t count,
               uint8_t **out_data, size_t *out_size);

/* Unpack binary format back to atom array.
 * Allocates the atom array, caller must free it. */
int tsubu_unpack(const uint8_t *data, size_t size,
                 splat_tsubu_t **out_atoms, uint32_t *out_count);

/* ══════════════════════════════════════════════════════════════
 *  Container helpers — write/read full .tsubu multi-stream files
 * ══════════════════════════════════════════════════════════════ */

/* Build a Tsubu container from LOD atom arrays.
 * Writes the full multi-stream .tsubu (.qtsq v3) file.
 *
 * Parameters:
 *   filename       — output .tsubu file path
 *   asset_id       — 16-byte UUID (can be NULL for random)
 *   lod_atoms      — array of atom arrays, one per LOD level
 *   lod_counts     — number of atoms per LOD
 *   num_lods       — number of LOD levels
 *   bonds          — bond array (can be NULL)
 *   bond_count     — number of bonds
 *   materials      — material array (can be NULL)
 *   material_count — number of materials */
int tsubu_container_write(const char *filename,
                           const uint8_t *asset_id,
                           const splat_tsubu_t *const *lod_atoms,
                           const uint32_t *lod_counts,
                           uint32_t num_lods,
                           const void *bonds,
                           uint32_t bond_count,
                           const void *materials,
                           uint16_t material_count);

/* Read a Tsubu container and extract LOD 0 atoms.
 * For full multi-LOD extraction, use the generic qtsq_container API.
 *
 * Parameters:
 *   filename       — input .tsubu file path
 *   out_atoms      — receives malloc'd atom array (caller frees)
 *   out_count      — receives atom count
 *   out_asset_id   — receives 16-byte UUID (caller provides buffer) */
int tsubu_container_read(const char *filename,
                          splat_tsubu_t **out_atoms,
                          uint32_t *out_count,
                          uint8_t *out_asset_id);

#endif /* QTSQ_SPLAT_TSUBU_H */
