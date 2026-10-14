/*
 * Quantum Tensor Sequence — Sanitizer & Anti-Steganography
 * Author: Haruhito
 */

#ifndef QTSQ_SANITIZE_H
#define QTSQ_SANITIZE_H

#include "qtsq.h"

/* 
 * Apply LSB randomization to destroy any hidden steganographic payloads.
 * XORs the 2 least significant bits of every pixel with a CSPRNG stream.
 * This is visually imperceptible but permanently corrupts embedded data.
 */
int qtsq_sanitize_image_lsb(uint8_t *pixels, uint32_t width, uint32_t height, uint8_t channels);

/*
 * 16-bit variant of qtsq_sanitize_image_lsb for camera-raw payloads.
 * XORs the 2 least significant bits of every uint16 sample with a CSPRNG
 * stream. Visually imperceptible (<= 2 LSB of a 16-bit range) but
 * permanently corrupts any steganographic payload hidden in the low bits.
 */
int qtsq_sanitize_image_lsb16(uint16_t *pixels, uint32_t width, uint32_t height, uint8_t channels);

/*
 * Sanitize text data by stripping Byte Order Marks (BOM), 
 * normalizing line endings to \n, and collapsing duplicate whitespace 
 * (if strict=1). Returns new malloc'd string and updates length.
 */
char *qtsq_sanitize_text(const char *text, size_t in_len, size_t *out_len, int strict);

/*
 * Sanitize JSON data by removing structural whitespace entirely outside strings.
 * This both compresses the JSON and removes covert communication channels.
 * Returns new malloc'd string and updates length.
 */
char *qtsq_sanitize_json(const char *json, size_t in_len, size_t *out_len);

/*
 * Apply LSB randomization to destroy any hidden steganographic payloads in audio.
 * XORs the 2 least significant bits of every int16 sample with a CSPRNG stream.
 * This is imperceptible (below -90 dBFS) but permanently corrupts embedded data.
 */
int qtsq_sanitize_audio_lsb(int16_t *samples, size_t num_samples);

/*
 * Securely zero memory — prevents compiler from optimizing away the memset.
 * Use to scrub keys, plaintext, and sensitive buffers after use.
 */
void qtsq_secure_zero(void *ptr, size_t len);

/*
 * Extract embedded names from singularity after decryption.
 * When names were hidden inside the encryption envelope, this restores
 * them to ctx->schema.name and ctx->schema.encoding.
 * No-op if no embedded names are found.
 */
void qtsq_extract_embedded_names(qtsq_context_t *ctx);

/*
 * Apply the data-type-agnostic sanitize guarantees to a context before
 * encryption: move display/encoding names inside the singularity envelope
 * and mark zero-metadata. This is the floor of Guarantee #1 (zero metadata)
 * and is invoked automatically by qtsq_vault_encrypt_password when the
 * caller has not already set is_sanitized (e.g. CLI/Python/tests paths
 * that bypass qtsq_secure_*). Data-type-specific LSB scrubbing still
 * requires qtsq_secure_image / qtsq_secure_audio.
 */
int qtsq_sanitize_context(qtsq_context_t *ctx);

#endif /* QTSQ_SANITIZE_H */
