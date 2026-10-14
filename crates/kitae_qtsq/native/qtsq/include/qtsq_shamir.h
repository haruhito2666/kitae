/*
 * Quantum Tensor Sequence — Visual Secret Sharing (Shamir over GF-256)
 *
 * Splits a QTSQ singularity into N shares where any K shares reconstruct
 * the original. Fewer than K shares reveal zero information (information-
 * theoretic security — no computational assumptions needed).
 *
 * This operates on the singularity bytes AFTER compression but BEFORE
 * encryption, so the share files themselves can be unencrypted (each share
 * is information-theoretically worthless alone) or additionally encrypted.
 *
 * Each share is a valid qtsq_context_t that:
 *   - Has QTSQ_FLAG_SECRET_SHARE set in its flags
 *   - Stores its share index (1..N) in schema.dimensions[4]
 *   - Stores K (threshold) in schema.dimensions[5]
 *   - Stores N (total shares) in schema.dimensions[6]
 *   - Contains exactly (singularity_size) bytes of share data
 *
 * GF(256) implementation uses the AES irreducible polynomial:
 *   x^8 + x^4 + x^3 + x + 1  (0x11B)
 *
 * Author: Haruhito
 */

#ifndef QTSQ_SHAMIR_H
#define QTSQ_SHAMIR_H

#include <stdint.h>
#include <stddef.h>
#include "qtsq.h"

/* ── Split a QTSQ context into N shares ─────────────────────────── */

/**
 * qtsq_shamir_split — Split ctx's singularity into N independent shares.
 *
 * @param ctx      Source context (singularity must be populated)
 * @param k        Threshold: minimum shares needed to reconstruct (2..N)
 * @param n        Total number of shares to generate (k..255)
 * @param shares   Output array of N contexts (caller allocates, caller frees
 *                 each share's singularity via qtsq_free)
 * @return         QTSQ_OK on success
 */
int qtsq_shamir_split(const qtsq_context_t *ctx,
                       uint8_t k, uint8_t n,
                       qtsq_context_t *shares);

/* ── Reconstruct from K shares ───────────────────────────────────── */

/**
 * qtsq_shamir_reconstruct — Reconstruct original singularity from K shares.
 *
 * @param shares        Array of K contexts (each with SECRET_SHARE flag)
 * @param k             Number of shares provided
 * @param out           Output context to receive reconstructed singularity
 * @return              QTSQ_OK on success, QTSQ_ERR_FORMAT if shares are
 *                      incompatible or threshold not met
 */
int qtsq_shamir_reconstruct(const qtsq_context_t *shares, uint8_t k,
                              qtsq_context_t *out);

/* ── GF(256) Arithmetic (exposed for testing) ───────────────────── */

uint8_t qtsq_gf256_mul(uint8_t a, uint8_t b);
uint8_t qtsq_gf256_inv(uint8_t a);

#endif /* QTSQ_SHAMIR_H */
