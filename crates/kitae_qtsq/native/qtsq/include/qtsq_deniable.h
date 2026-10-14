/*
 * Quantum Tensor Sequence — Deniable Dual-Image Encryption
 *
 * One .qtsq file, two passwords. Password A decrypts the real image.
 * Password B decrypts the decoy image. An adversary who coerces you
 * into revealing a password gets the decoy. They cannot prove a second
 * image exists because the ciphertext is statistically indistinguishable
 * from single-image encryption.
 *
 * This is inspired by TrueCrypt's hidden volumes but applied to images
 * at the geometric primitive layer — a first for any image format.
 *
 * Author: Haruhito
 */

#ifndef QTSQ_DENIABLE_H
#define QTSQ_DENIABLE_H

#include <stdint.h>
#include <stddef.h>
#include "qtsq.h"

/**
 * qtsq_deniable_encrypt — Encrypt two payloads into one indistinguishable file.
 *
 * The resulting ctx->singularity is indistinguishable from a single-encrypted
 * payload. Decrypting with real_password yields real_data; decrypting with
 * decoy_password yields decoy_data.
 *
 * @param ctx             Output context (singularity will be set)
 * @param real_data       The real/sensitive singularity bytes
 * @param real_size       Size of real_data
 * @param decoy_data      The innocent/decoy singularity bytes
 * @param decoy_size      Size of decoy_data (will be padded to match real_size)
 * @param real_password   Password that unlocks the real image
 * @param decoy_password  Password that unlocks the decoy image
 * @return                QTSQ_OK on success
 */
int qtsq_deniable_encrypt(qtsq_context_t *ctx,
                           const uint8_t *real_data,  size_t real_size,
                           const uint8_t *decoy_data, size_t decoy_size,
                           const char *real_password,
                           const char *decoy_password);

/**
 * qtsq_deniable_decrypt — Decrypt using the provided password.
 *
 * Internally tries to decrypt as real or decoy — returns whichever
 * MAC validates. Returns QTSQ_ERR_DECRYPT if neither password matches.
 *
 * @param ctx             Context with deniable ciphertext in singularity
 * @param password        Either the real or decoy password
 * @param out_data        Receives malloc'd decrypted bytes (caller frees)
 * @param out_size        Receives size of decrypted data
 * @return                QTSQ_OK on success
 */
int qtsq_deniable_decrypt(qtsq_context_t *ctx,
                           const char *password,
                           uint8_t **out_data, size_t *out_size);

#endif /* QTSQ_DENIABLE_H */
