/*
 * Quantum Tensor Sequence — C2PA / Content Credentials (Phase 10)
 *
 * Builds a C2PA-style claim store:
 *   JSON-LD assertions (c2pa.created, claim_generator, author_id, intent,
 *   consent_flags, work_hash)  →  CBOR (RFC 8949) claim  →  JUMBF superbox
 *   container  →  ML-DSA-65 signature (the author's existing signing key).
 *
 * The resulting manifest can be:
 *   - shipped alongside a Provenance .qtsq export as a separate `*.c2pa`
 *     file (the standard C2PA distribution form), and/or
 *   - stored inside a Provenance container as a named stream.
 *
 * The claim deliberately reuses the SAME work_hash / author_id / created_at /
 * intent / consent_flags that the QTSQ Authorship Seal token carries, so the
 * two are cross-verifiable (qtsq_c2pa_cross_verify).
 *
 * NOTE on interoperability: C2PA's signature-algorithm registry currently
 * recognizes ES256/ES384/PS256/PS384/Ed25519.  ML-DSA-65 is not yet registered,
 * so Adobe/Photoshop C2PA verifiers will not validate the ML-DSA signature
 * band even though they can parse the claim/JSON-LD metadata.  A future bump
 * can add an EC signature alongside ML-DSA for full external interop; the
 * claim/container structure here is already the real JUMBF+CBOR form.
 *
 * Author: Haruhito
 */
#ifndef QTSQ_C2PA_H
#define QTSQ_C2PA_H

#include <stddef.h>
#include <stdint.h>
#include "qtsq.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Max sizes matching the seal token conventions. */
#define QTSQ_C2PA_MAGIC     0x32504143   /* "C2PA" little-endian readable */
#define QTSQ_C2PA_VERSION   1
#define QTSQ_C2PA_LABEL     "c2pa"

typedef struct {
    char     claim_generator[64];   /* e.g. "QTSQ 2.0.2" */
    char     author_id[128];        /* matches the authorship token's author_id */
    char     license[64];            /* e.g. "MIT" (same as seal token) */
    char     created_iso[64];       /* ISO-8601 creation timestamp */
    uint64_t created_at;             /* unix seconds — matches seal token */
    uint32_t intent;                 /* qtsq_intent_t */
    uint32_t consent_flags;          /* QTSQ_FLAG_DO_NOT_TRAIN | NO_AI */
    uint8_t  work_hash[32];          /* SHA3-256 — matches seal token work_hash */
} qtsq_c2pa_metadata_t;

/*
 * qtsq_c2pa_build_manifest — encode the claim to CBOR, wrap it in a JUMBF
 * superbox, and sign the claim digest with ML-DSA-65.  Returns a malloc'd
 * manifest buffer (caller frees).
 */
int qtsq_c2pa_build_manifest(const qtsq_c2pa_metadata_t *meta,
                             const uint8_t sign_sk[4032],
                             const uint8_t sign_pk[1952],
                             uint8_t **out_manifest, size_t *out_size);

/*
 * qtsq_c2pa_parse_manifest — validate structure and extract the metadata.
 * If claim_json_out is non-NULL and *claim_json_out is NULL, allocates a
 * JSON representation of the claim's JSON-LD assertions (caller frees).
 */
int qtsq_c2pa_parse_manifest(const uint8_t *manifest, size_t size,
                             qtsq_c2pa_metadata_t *meta_out,
                             char **claim_json_out);

/*
 * qtsq_c2pa_verify_manifest — verify the ML-DSA-65 signature over the claim.
 * Returns QTSQ_OK on valid, QTSQ_ERR_CHECKSUM on mismatch, QTSQ_ERR_FORMAT on
 * malformed input.
 */
int qtsq_c2pa_verify_manifest(const uint8_t *manifest, size_t size,
                              const uint8_t sign_pk[1952]);

/*
 * qtsq_c2pa_cross_verify — confirm the manifest's work_hash / author_id /
 * created_at / intent agree with the value carried by a QTSQ Authorship Seal
 * token.  Returns QTSQ_OK if consistent, QTSQ_ERR_CHECKSUM otherwise.
 */
int qtsq_c2pa_cross_verify(const uint8_t *manifest, size_t size,
                           const uint8_t work_hash[32],
                           const char *author_id,
                           uint64_t created_at);

/*
 * File I/O for the standalone `*.c2pa` manifest (sidecar of a Provenance
 * export).
 */
int qtsq_c2pa_write_manifest(const uint8_t *manifest, size_t size,
                             const char *filename);
int qtsq_c2pa_load_manifest(const char *filename,
                            uint8_t **out_manifest, size_t *out_size);

/*
 * Attach to a Provenance container: store the manifest bytes as a named
 * stream (so it rides inside the encrypted .qtsq envelope).
 */
int qtsq_c2pa_add_manifest_stream(qtsq_context_t *container,
                                  const uint8_t *manifest, size_t size,
                                  const char *stream_name);
int qtsq_c2pa_get_manifest_stream(qtsq_context_t *container,
                                  const char *stream_name,
                                  uint8_t **out_manifest, size_t *out_size);

#ifdef __cplusplus
}
#endif

#endif /* QTSQ_C2PA_H */
