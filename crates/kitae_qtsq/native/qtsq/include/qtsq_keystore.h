/*
 * Quantum Tensor Sequence — Device Keystore
 * Persistent device keypair management with OS keystore integration.
 *
 * First run: generates ML-KEM-768 + ML-DSA-65 keypairs, stores in OS keystore.
 * Subsequent runs: loads existing keys from OS keystore.
 * Encrypt-to-self by default: every write auto-encrypts to the device key.
 *
 * Platform support:
 *   macOS   — Keychain Services (hardware-backed, GENUINE)
 *   Android — Hardware-backed Android Keystore via JNI (Phase 2d COMPLETE)
 *             Keys are encrypted with an AndroidKeyStore AES-256 key
 *             before being written to app-private file storage.
 *             JNI bridge must be registered at app startup.
 *   Linux   — File-based (~/.qtsq/keys/) with chmod 600
 *   Other   — Generic file-based (~/.qtsq/keys/)
 *
 * Author: Haruhito
 */

#ifndef QTSQ_KEYSTORE_H
#define QTSQ_KEYSTORE_H

#include "qtsq.h"
#include "qtsq_vault.h"

/* ═══ Device Keystore Lifecycle ═══ */

/**
 * qtsq_keystore_init — Initialize the device keystore.
 * On first run: generates keypairs, stores in OS keystore.
 * On subsequent runs: loads existing keypairs from OS keystore.
 *
 * @return QTSQ_OK on success
 */
int qtsq_keystore_init(void);

/**
 * qtsq_keystore_destroy — Zero and release in-memory keys.
 * Keys in the OS keystore are NOT deleted.
 */
void qtsq_keystore_destroy(void);

/**
 * qtsq_keystore_is_initialized — Returns 1 if keystore has been loaded.
 */
int qtsq_keystore_is_initialized(void);


/* ═══ Device Key Access ═══ */

/**
 * qtsq_keystore_get_enc_pk — Get device ML-KEM-768 public key (1184 bytes).
 * Use this for encrypt-to-self and for sharing with others.
 */
const uint8_t* qtsq_keystore_get_enc_pk(void);

/**
 * qtsq_keystore_get_enc_sk — Get device ML-KEM-768 secret key (2400 bytes).
 * Use this for decrypting files encrypted to this device.
 */
const uint8_t* qtsq_keystore_get_enc_sk(void);

/**
 * qtsq_keystore_get_sign_pk — Get device ML-DSA-65 signing public key (1952 bytes).
 */
const uint8_t* qtsq_keystore_get_sign_pk(void);

/**
 * qtsq_keystore_get_sign_sk — Get device ML-DSA-65 signing secret key (4032 bytes).
 */
const uint8_t* qtsq_keystore_get_sign_sk(void);


/* ═══ Encrypt-to-Self (Default Write Path) ═══ */

/**
 * qtsq_keystore_encrypt — Encrypt ctx to the device's own public key.
 * This is the zero-friction default: no password prompt, no recipient needed.
 *
 * @return QTSQ_OK on success
 */
int qtsq_keystore_encrypt(qtsq_context_t *ctx);

/**
 * qtsq_keystore_decrypt — Decrypt ctx using the device's secret key.
 *
 * @return QTSQ_OK on success
 */
int qtsq_keystore_decrypt(qtsq_context_t *ctx);

/**
 * qtsq_keystore_sign — Sign ctx using the device's signing key.
 *
 * @return QTSQ_OK on success
 */
int qtsq_keystore_sign(qtsq_context_t *ctx);


/* ═══ Key Export / Import (QTSQ Address) ═══ */

/**
 * qtsq_keystore_export_address — Export device public key as base64 string.
 * Format: "qtsq_kem768:<base64_of_1184_bytes>"
 * Caller must free() the returned string.
 */
char* qtsq_keystore_export_address(void);

/**
 * qtsq_keystore_import_address — Parse a QTSQ address into a public key.
 *
 * @param address  Base64 string (with or without "qtsq_kem768:" prefix)
 * @param out_pk   Output buffer (1184 bytes)
 * @return QTSQ_OK on success
 */
int qtsq_keystore_import_address(const char *address, uint8_t out_pk[1184]);


/* ═══ Shamir Recovery ═══ */

/**
 * qtsq_keystore_backup — Split device secret key into N Shamir shares.
 * Each share is encrypted with the password and written to a .qtsq file.
 *
 * @param k           Threshold (minimum shares to recover)
 * @param n           Total shares
 * @param share_paths Array of N file paths for the share files
 * @param password    Password used to encrypt each share
 * @return QTSQ_OK on success
 */
int qtsq_keystore_backup(uint8_t k, uint8_t n, const char **share_paths,
                         const char *password);

/**
 * qtsq_keystore_recover — Recover device key from K Shamir share files.
 * Writes recovered key to OS keystore.
 *
 * @param k           Number of shares provided
 * @param share_paths Array of K file paths to share files
 * @param password    Password used to decrypt each share
 * @return QTSQ_OK on success
 */
int qtsq_keystore_recover(uint8_t k, const char **share_paths,
                          const char *password);


/* ═══ Android JNI Bridge (Phase 2d) ═══
 *
 * On Android, the C keystore needs a JNI bridge to access the
 * hardware-backed AndroidKeyStore. The app registers encrypt/decrypt
 * callbacks at startup; the keystore uses them to encrypt key files
 * before writing to disk.
 *
 * When the bridge is not registered (e.g. during unit tests), the
 * keystore falls back to plaintext file storage (same as Linux).
 *
 * The Java side (KeystoreHelper.java) creates an AndroidKeyStore
 * AES-256 key with PURPOSE_ENCRYPT|PURPOSE_DECRYPT and uses the
 * Cipher API to encrypt/decrypt the key file bytes. The key never
 * leaves the Android Keystore hardware. */

#ifdef __ANDROID__
typedef int (*qtsq_jni_encrypt_fn)(const uint8_t *plaintext, size_t len,
                                    uint8_t *ciphertext, size_t cap, size_t *out_len);
typedef int (*qtsq_jni_decrypt_fn)(const uint8_t *ciphertext, size_t len,
                                    uint8_t *plaintext, size_t cap, size_t *out_len);

/* Register JNI encrypt/decrypt callbacks. Call once at app startup
 * from Java (e.g. in Activity.onCreate). */
void qtsq_keystore_set_jni_bridge(qtsq_jni_encrypt_fn enc_fn,
                                   qtsq_jni_decrypt_fn dec_fn);

/* Note: qtsq_jni_encrypt_impl / qtsq_jni_decrypt_impl were declared here
 * as "defined in qtsq_keystore_jni.c" but that file does not exist in the
 * tree. The declarations were removed to avoid link errors. If Android
 * JNI support is added, provide the implementations in a new .c file. */
#endif

#endif /* QTSQ_KEYSTORE_H */
