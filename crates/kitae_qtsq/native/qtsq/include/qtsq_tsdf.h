/*
 * QTSQ TSDF — CPU volumetric fusion + marching-cubes mesh extraction (Phase F
 * of the live-surfel capture roadmap)
 *
 * At save time the trained 2DGS model is rendered at the captured camera
 * poses into posed depth maps; this module fuses those maps into a truncated
 * signed-distance field on a uniform voxel grid and extracts a watertight
 * triangle mesh with marching cubes. Pure CPU (runs on the phone while
 * saving), only libc + math, no global state, thread-safe.
 *
 * Author: Haruhito
 */

#ifndef QTSQ_TSDF_H
#define QTSQ_TSDF_H

#include <stdint.h>
#include <stddef.h>   /* size_t for the mesh stream API */

/* ══════════════════════════════════════════════════════════════
 *  qtsq_tsdf_camera_t — posed depth camera
 *
 *  Matches the capture module's camera convention: +forward view dir,
 *  +right, +up = image-down, world space; pinhole. A world point P projects
 *  with ndx = dot(P-pos, right)/d, ndy = dot(P-pos, up)/d where
 *  d = dot(P-pos, forward); pixel = (fx*ndx + cx, fy*ndy + cy).
 * ══════════════════════════════════════════════════════════════ */
typedef struct {
    float pos[3], right[3], up[3], forward[3];
    float fx, fy, cx, cy;
    int   width, height;
} qtsq_tsdf_camera_t;

/* ══════════════════════════════════════════════════════════════
 *  qtsq_mesh_t — extracted triangle mesh (world space)
 * ══════════════════════════════════════════════════════════════ */
typedef struct {
    float *vertices;   /* n * 3 floats, world space */
    uint32_t nv;
    uint32_t *tris;    /* t * 3 uint32 indices */
    uint32_t nt;
    float *normals;    /* n * 3 floats, per-vertex from SDF gradient */
} qtsq_mesh_t;

void qtsq_mesh_free(qtsq_mesh_t *m);

/* Fuse 'num_maps' posed depth maps (each depth field d, depth_w*depth_h
 * floats, camera-space depth along forward axis, 0 = no hit) into a TSDF
 * volume and extract a mesh. grid_n = voxels per side (default 96), voxel =
 * voxel size in world metres (default 0.01), trunc = truncation distance in
 * metres (default 4*voxel). SDF = depth(voxel) - measured_depth per camera
 * (positive on the far side of the surface), averaged per voxel; voxels no
 * camera measured keep weight 0 and never produce mesh. The grid is
 * centered on the union of the camera frustum slices between 0.3 m and 5.0 m
 * (see qtsq_tsdf_grid_bounds). Returns 0 on success, -1 on failure (out is
 * left empty). */
int qtsq_tsdf_reconstruct(const float *const *depth_maps,
                          const qtsq_tsdf_camera_t *cams, int num_maps,
                          int depth_w, int depth_h,
                          int grid_n, float voxel, float trunc,
                          qtsq_mesh_t *out);

/* Write a mesh as ASCII PLY (vertex + face elements, normals as vertex
 * properties). Returns 0 on success. */
int qtsq_mesh_write_ply(const char *path, const qtsq_mesh_t *m);

/* ══════════════════════════════════════════════════════════════
 *  Binary mesh stream — .qtsq container payload
 *  Format: ['M','E','S','H'][u32 nv][float32 × nv·3 verts]
 *          [u32 nt][u32 × nt·3 indices][float32 × nv·3 normals]
 *  All fields little-endian host order (plain memcpy, like
 *  qtsq_surfel_pack).
 * ══════════════════════════════════════════════════════════════ */

/* Pack a mesh into a freshly malloc'd stream. m must have nv/nt > 0 and all
 * three arrays allocated. Returns 0 on success, -1 on bad input or
 * allocation failure. */
int qtsq_mesh_pack(const qtsq_mesh_t *m, uint8_t **out_data,
                   size_t *out_size);

/* Unpack a stream into out (arrays malloc'd; qtsq_mesh_free frees them).
 * Returns 0 on success, -1 on bad magic / truncated or oversized size /
 * zero counts / allocation failure (out left untouched). */
int qtsq_mesh_unpack(const uint8_t *data, size_t size, qtsq_mesh_t *out);

/* Convenience: compute grid bounds that cover the union of the cameras'
 * frustum slices between near_d and far_d (defaults 0.3 and 5.0 metres
 * along forward). Fills center[3] and half_extent[3] (world). */
void qtsq_tsdf_grid_bounds(const qtsq_tsdf_camera_t *cams, int num_maps,
                           float near_d, float far_d,
                           float center[3], float half_extent[3]);

#endif /* QTSQ_TSDF_H */
