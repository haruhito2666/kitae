/*
 * Quantum Tensor Sequence — Post-Quantum Security Vault
 * Uses ML-KEM-768 for Key Encapsulation, ML-DSA-65 for Signatures
 * Uses AES-256-CTR + SHA3-256 MAC for symmetric authenticated encryption.
 * Author: Haruhito
 */

#ifndef QTSQ_VAULT_H
#define QTSQ_VAULT_H

#include "qtsq.h"

/* ═══ Key Management ═══ */

/* ML-KEM-768 keypair */
typedef struct {
    uint8_t public_key[1184];
    uint8_t secret_key[2400];
} qtsq_keypair_t;

/* ML-DSA-65 keypair */
typedef struct {
    uint8_t sign_pk[1952];
    uint8_t sign_sk[4032];
} qtsq_sign_keypair_t;

/* Generate a new ML-KEM-768 keypair for encryption/decryption */
int qtsq_keygen(qtsq_keypair_t *kp);

/* Generate a new ML-DSA-65 keypair for signing/verifying */
int qtsq_sign_keygen(qtsq_sign_keypair_t *skp);


/* ═══ Encryption API ═══ */

/* Encrypt the singularity using a password (HKDF-SHA3 -> AES256) */
int qtsq_vault_encrypt_password(qtsq_context_t *ctx, const char *password);

/* Decrypt the singularity using a password */
int qtsq_vault_decrypt_password(qtsq_context_t *ctx, const char *password);

/* Encrypt the singularity for a specific recipient using their ML-KEM public key */
int qtsq_vault_encrypt_pq(qtsq_context_t *ctx, const uint8_t recipient_pk[1184]);

/* Decrypt the singularity using your ML-KEM secret key */
int qtsq_vault_decrypt_pq(qtsq_context_t *ctx, const uint8_t my_sk[2400]);


/* ═══ Digital Signature API ═══ */

/* Sign the entire context (header + singularity) using ML-DSA-65 */
int qtsq_vault_sign(qtsq_context_t *ctx, const uint8_t sign_sk[4032]);

/* Verify the signature of the context using the sender's ML-DSA-65 public key */
int qtsq_vault_verify(const qtsq_context_t *ctx, const uint8_t sign_pk[1952]);


/* ═══ Query API ═══ */

/* Returns 1 if the context is encrypted */
int qtsq_vault_is_encrypted(const qtsq_context_t *ctx);

/* Returns 1 if the context is signed */
int qtsq_vault_is_signed(const qtsq_context_t *ctx);

/* 
 * Returns the security level:
 * 0 = Unencrypted, Unsigned
 * 1 = Password Encrypted
 * 2 = PQ Encrypted (ML-KEM)
 * 3 = PQ Encrypted and Signed
 */
int qtsq_vault_get_security_level(const qtsq_context_t *ctx);

#endif /* QTSQ_VAULT_H */
