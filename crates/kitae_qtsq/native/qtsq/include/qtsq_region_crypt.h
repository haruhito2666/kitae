/*
 * Quantum Tensor Sequence — Selective Region Encryption
 *
 * Encrypts individual triangles or splats that fall within specified
 * rectangular regions using per-region keys. The same .qtsq file
 * shows different content to different key holders:
 *   - No key:   background only (redacted)
 *   - Key A:    region A revealed
 *   - All keys: full image
 *
 * This makes redaction CRYPTOGRAPHICALLY REVERSIBLE by authorized
 * parties — unlike Photoshop blur/black bars which destroy the data.
 *
 * Operates at the packed binary level (after splat_only_pack /
 * triangle pack, before ZLib compression) so the record offsets are
 * stable and per-record region classification is embedded in the
 * schema metadata.
 *
 * Author: Haruhito
 */

#ifndef QTSQ_REGION_CRYPT_H
#define QTSQ_REGION_CRYPT_H

#include <stdint.h>
#include <stddef.h>
#include "qtsq.h"
#include "qtsq_splat2d.h"

/* Maximum number of encrypted regions per file
 * Limited by 64-byte encoding field: (64 - 5) / 9 = 6 */
#define QTSQ_MAX_REGIONS 6

/* ── Region Definition ───────────────────────────────────────────── */

typedef struct {
    uint32_t x, y;      /* Top-left corner (pixels) */
    uint32_t w, h;      /* Width and height (pixels) */
    uint8_t  key_id;    /* 0-15: which key encrypts this region */
} qtsq_region_t;

/* ── Region Metadata stored in file ─────────────────────────────── */
/* Packed into schema.encoding (64 bytes) as:
 *  "REGC" + num_regions(1) + [key_id(1) + x(2) + y(2) + w(2) + h(2)] * N */

/* ── Encrypt specific regions of a splat-encoded image ──────────── */

/**
 * qtsq_region_encrypt_splats — Encrypt splats that fall within specified regions.
 *
 * Splats whose center (x,y) falls within a region are encrypted with that
 * region's key. Splats outside all regions are left unencrypted (publicly
 * visible). Overlapping regions: first matching region wins.
 *
 * Must be called AFTER splat fitting, BEFORE splat_only_pack().
 *
 * @param splats        Splat array (modified in-place)
 * @param count         Number of splats
 * @param regions       Array of regions to encrypt
 * @param num_regions   Number of regions (max QTSQ_MAX_REGIONS)
 * @param keys          2D array: keys[key_id][32] — 32-byte key per region
 * @param ctx           Context to update flags and store region metadata
 * @return              QTSQ_OK on success
 */
int qtsq_region_encrypt_splats(splat2d_t *splats, uint32_t count,
                                const qtsq_region_t *regions, uint8_t num_regions,
                                const uint8_t keys[QTSQ_MAX_REGIONS][32],
                                qtsq_context_t *ctx);

/**
 * qtsq_region_decrypt_splats — Decrypt regions for which keys are provided.
 *
 * Splats in undecrypted regions render with scrambled colors/positions
 * (appear as colored noise). Pass NULL keys for regions you don't have
 * access to — they will remain encrypted.
 *
 * Must be called AFTER splat_only_unpack(), BEFORE rendering.
 *
 * @param splats        Splat array (modified in-place)
 * @param count         Number of splats
 * @param keys          keys[key_id] = 32-byte key, or NULL if not available
 * @param ctx           Context containing region metadata from encoding
 * @return              QTSQ_OK on success (even if some regions undecrypted)
 */
int qtsq_region_decrypt_splats(splat2d_t *splats, uint32_t count,
                                const uint8_t *keys[QTSQ_MAX_REGIONS],
                                const qtsq_context_t *ctx);

/* ── Region Metadata Helpers ─────────────────────────────────────── */

/** Store region definitions into ctx->schema.encoding */
int qtsq_region_store_metadata(qtsq_context_t *ctx,
                                const qtsq_region_t *regions, uint8_t num_regions);

/** Read region definitions from ctx->schema.encoding */
int qtsq_region_load_metadata(const qtsq_context_t *ctx,
                               qtsq_region_t *regions, uint8_t *num_regions);

#endif /* QTSQ_REGION_CRYPT_H */
