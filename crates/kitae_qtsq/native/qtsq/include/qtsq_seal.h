/*
 * Quantum Tensor Sequence — Authorship Seal
 *
 * Proves who made a work and that it hasn't been silently altered.
 * Uses ML-DSA-65 (quantum-proof) signatures, multi-domain embedding
 * (header + frequency + geometry), and Shamir K-of-N for robustness.
 *
 * Author: Haruhito
 */

#ifndef QTSQ_SEAL_H
#define QTSQ_SEAL_H

#include "qtsq_format.h"
#include "qtsq_vault.h"
#include "qtsq_shamir.h"

/* ═══ Consent Flags ═══ */
#define QTSQ_CONSENT_DO_NOT_TRAIN  0x01  /* Do not use for AI training */
#define QTSQ_CONSENT_NO_AI         0x02  /* No AI processing of any kind */

/* ═══ Authorship Token ═══
 * Signed content that proves authorship. Lives inside the encryption
 * envelope (not in EXIF-style headers), so it survives the zero-metadata
 * guarantee. */
typedef struct {
    uint8_t  author_id[32];      /* SHA3-256 of author's signing public key */
    char     author_name[64];    /* Optional display name */
    uint8_t  work_hash[32];      /* SHA3-256 of the work (exact hash) */
    uint8_t  perceptual_hash[16];/* Robust hash for anchor matching */
    uint64_t created_at;         /* Unix timestamp */
    uint8_t  intent;             /* QTSQ_INTENT_CREATIVE | QTSQ_INTENT_PROVENANCE */
    uint32_t consent_flags;      /* QTSQ_CONSENT_* flags */
    char     license[64];        /* Optional SPDX or free text */
    /* The signature covers all fields above (not itself) */
    uint8_t  signature[3309];    /* ML-DSA-65 signature (padded) */
    uint16_t sig_size;           /* Actual signature size (3293 for ML-DSA-65) */
} __attribute__((packed)) qtsq_authorship_token_t;

/* ═══ Seal Verdict ═══ */
typedef enum {
    QTSQ_SEAL_AUTHENTIC,   /* Marks + signature + (optional) anchor all agree */
    QTSQ_SEAL_ALTERED,     /* Marks disagree / signature fails → provably tampered */
    QTSQ_SEAL_STRIPPED,    /* No recoverable seal */
    QTSQ_SEAL_UNVERIFIED,  /* Seal present but signature could not be verified */
} qtsq_seal_verdict_t;

/* ═══ Domain bitmask — which embedding domains survived ═══ */
#define QTSQ_SEAL_DOMAIN_HEADER    0x01  /* Header signature block */
#define QTSQ_SEAL_DOMAIN_FREQUENCY 0x02  /* DCT/DWT frequency mark */
#define QTSQ_SEAL_DOMAIN_GEOMETRY  0x04  /* Triangle vertex / splat mark */

/* ═══ Seal Result ═══ */
typedef struct {
    qtsq_seal_verdict_t     verdict;
    qtsq_authorship_token_t author;        /* Recovered token, if any */
    uint32_t                domains_ok;    /* Bitmask: which marks survived */
    int                     anchored;      /* Matched external record? */
} qtsq_seal_result_t;

/* ═══ Default Shamir parameters ═══ */
#define QTSQ_SEAL_SHAMIR_K  3  /* Threshold */
#define QTSQ_SEAL_SHAMIR_N  5  /* Total shares */

/* ═══ Public API ═══ */

/**
 * qtsq_seal_authorship — Sign and embed an authorship seal.
 *
 * 1. Builds an authorship token from the provided metadata
 * 2. Signs it with ML-DSA-65 (the author's signing key)
 * 3. Splits a verification secret with Shamir K-of-N
 * 4. Embeds shares in multiple domains:
 *    - Share 0: header signature block (in security block)
 *    - Share 1: frequency domain (DCT coefficients of image data)
 *    - Share 2: geometry domain (triangle vertex perturbations)
 *    - Shares 3-4: reserved for future domains
 *
 * @param ctx          Context to seal (must have singularity populated)
 * @param author_name  Display name (can be NULL)
 * @param license      SPDX or free text (can be NULL)
 * @param consent_flags QTSQ_CONSENT_* flags
 * @param sign_sk      Author's ML-DSA-65 signing secret key
 * @param sign_pk      Author's ML-DSA-65 signing public key (for fingerprint)
 * @return QTSQ_OK on success
 */
int qtsq_seal_authorship(qtsq_context_t *ctx,
                         const char *author_name,
                         const char *license,
                         uint32_t consent_flags,
                         const uint8_t sign_sk[4032],
                         const uint8_t sign_pk[1952]);

/**
 * qtsq_verify_authorship — Verify the authorship seal.
 *
 * Checks all embedding domains, reconstructs the verification secret
 * from any K surviving shares, and verifies the ML-DSA-65 signature.
 *
 * @param ctx    Context to verify
 * @param out    Result struct (verdict, recovered author, domains)
 * @return QTSQ_OK on success (check out->verdict for the actual result)
 */
int qtsq_verify_authorship(const qtsq_context_t *ctx,
                           qtsq_seal_result_t *out);

/**
 * qtsq_seal_set_author_id — Derive author_id from signing public key.
 * author_id = SHA3-256(sign_pk)[:32]
 */
int qtsq_seal_set_author_id(qtsq_authorship_token_t *token,
                            const uint8_t sign_pk[1952]);

/**
 * qtsq_anchor_export — Export anchor data for external publishing.
 *
 * This is the ONLY network action in the entire design. It produces
 * a small anchor record that the user can publish to a public
 * timestamp service (RFC-3161 / OpenTimestamps). The anchor contains:
 *   { perceptual_hash, author_id, created_at, signature }
 *
 * This function does NOT publish anything — it only writes the
 * anchor data to a file. The user must explicitly upload it.
 *
 * @param ctx        Sealed context
 * @param filepath   Output file path for the anchor record
 * @return QTSQ_OK on success
 */
int qtsq_anchor_export(const qtsq_context_t *ctx, const char *filepath);

#endif /* QTSQ_SEAL_H */
