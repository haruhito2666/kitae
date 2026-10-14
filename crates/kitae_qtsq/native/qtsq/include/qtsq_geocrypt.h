/*
 * Quantum Tensor Sequence — Geometric Cryptography Engine
 *
 * Encrypts geometric primitives (splat positions, colors, covariances,
 * triangle vertex positions) directly, BEFORE they are packed into the
 * singularity payload. This means the image NEVER exists as pixels
 * inside the encrypted container — only as scrambled geometry.
 *
 * Three independent operations (can be combined):
 *   GEOCRYPT_PERMUTE  — Fisher-Yates keyed shuffle of primitive order
 *   GEOCRYPT_POSITION — XOR (x,y) positions with PRNG stream
 *   GEOCRYPT_COLOR    — XOR (r,g,b,alpha) with PRNG stream
 *
 * The key is derived from the vault key via HKDF-SHA3-256 with a
 * "geocrypt" domain separator, so it is always unique from the AES key.
 *
 * Author: Haruhito
 */

#ifndef QTSQ_GEOCRYPT_H
#define QTSQ_GEOCRYPT_H

#include <stdint.h>
#include <stddef.h>
#include "qtsq_splat2d.h"

/* ── Mode Flags (combinable with |) ─────────────────────────────── */
#define QTSQ_GEOCRYPT_PERMUTE   0x01  /* Keyed permutation of primitive order     */
#define QTSQ_GEOCRYPT_POSITION  0x02  /* XOR (x,y) positions with keyed PRNG      */
#define QTSQ_GEOCRYPT_COLOR     0x04  /* XOR (r,g,b,alpha) with keyed PRNG        */
#define QTSQ_GEOCRYPT_FULL      0x07  /* All three — maximum geometric obfuscation */

/* ── Splat Encryption / Decryption ─────────────────────────────── */

/**
 * qtsq_geocrypt_encrypt_splats — Encrypt splat array in-place.
 *
 * @param splats  Array of splat2d_t structs (modified in-place)
 * @param count   Number of splats
 * @param key     32-byte symmetric key (derived from vault key)
 * @param mode    Combination of QTSQ_GEOCRYPT_* flags
 * @return        QTSQ_OK on success
 */
int qtsq_geocrypt_encrypt_splats(splat2d_t *splats, uint32_t count,
                                  const uint8_t key[32], uint8_t mode);

/**
 * qtsq_geocrypt_decrypt_splats — Decrypt splat array in-place.
 * Must be called with the same key and mode used to encrypt.
 */
int qtsq_geocrypt_decrypt_splats(splat2d_t *splats, uint32_t count,
                                  const uint8_t key[32], uint8_t mode);

/* ── Vertex Buffer (TRM3) Encryption / Decryption ───────────────── */

/**
 * qtsq_geocrypt_encrypt_vertices — Encrypt raw TRM3 vertex buffer.
 *
 * Operates on the packed binary vertex data (after splat2d_pack_gsp2/gsp3
 * or triangle packing). Permutes vertex record order and XORs
 * position/color bytes according to the selected mode.
 *
 * @param data    Packed binary buffer (modified in-place)
 * @param size    Size of buffer in bytes
 * @param key     32-byte symmetric key
 * @param mode    Combination of QTSQ_GEOCRYPT_* flags
 * @param stride  Bytes per record (e.g., sizeof(splat2d_t) = 12)
 * @return        QTSQ_OK on success
 */
int qtsq_geocrypt_encrypt_vertices(uint8_t *data, size_t size,
                                    const uint8_t key[32],
                                    uint8_t mode, uint32_t stride);

int qtsq_geocrypt_decrypt_vertices(uint8_t *data, size_t size,
                                    const uint8_t key[32],
                                    uint8_t mode, uint32_t stride);

/* ── Key Derivation Helper ──────────────────────────────────────── */

/**
 * qtsq_geocrypt_derive_key — Derive a geocrypt-specific subkey from a master key.
 * Uses HKDF-SHA3-256 with domain separator "QTSQ-GeoCrypt-v1".
 */
void qtsq_geocrypt_derive_key(const uint8_t master_key[32],
                               uint8_t out_key[32]);

#endif /* QTSQ_GEOCRYPT_H */
