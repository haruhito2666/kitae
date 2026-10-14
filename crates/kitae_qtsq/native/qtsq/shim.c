#include "qtsq.h"
#include "qtsq_vault.h"
#include "qtsq_sanitize.h"
#include "qtsq_codec.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>
#include <zlib.h>

/* stb_image is vendored (header-only) and compiled into this shim: container
 * payloads — QTSQStudio's ZERO_META saves wrap the *original* file bytes
 * (e.g. a JPEG) — come back from qtsq_decompress as encoded image bytes, and
 * pixels only exist once those are decoded. */
#define STB_IMAGE_IMPLEMENTATION
#define STBI_NO_STDIO
#include "stb_image.h"

/* Kitae C shim — the glue between kitae_qtsq's Rust FFI and libqtsq.
 *
 * qtsq_context_t is a plain (non-opaque) struct, but Rust only needs the
 * opaque handle: every operation below takes the context by pointer and
 * returns only C error codes or out-params.
 *
 * Bytes enter the context through qtsq_read (the only public "load from
 * buffer/file" entry point), so decode_from_buffer spools the payload to a
 * temporary file. Decoded buffers are malloc'd by libqtsq; the shim returns
 * them with an accompanying free() so Rust can release them correctly.
 *
 * The C API is single-threaded per context: every shim call takes the
 * context by &mut from Rust's side, so no internal locking is needed. */

static int write_all(const char *path, const unsigned char *data, size_t size) {
    FILE *f = fopen(path, "wb");
    if (!f) return -100;
    size_t n = fwrite(data, 1, size, f);
    fclose(f);
    return n == size ? 0 : -101;
}

/* Every decode path: read bytes → context, then decrypt with the given
 * password (NULL = not encrypted / no password) before decompressing. */
static int open_from_buffer(qtsq_context_t *ctx, const unsigned char *data, size_t size,
                            const char *path_hint) {
    memset(ctx, 0, sizeof(qtsq_context_t));
    int rc = qtsq_init(ctx);
    if (rc != QTSQ_OK) return rc;
    rc = write_all(path_hint, data, size);
    if (rc != 0) { qtsq_free(ctx); return rc; }
    rc = qtsq_read(ctx, path_hint);
    if (rc != QTSQ_OK) { qtsq_free(ctx); return rc; }
    return QTSQ_OK;
}

/* ── legacy-writer compat (runs after read/decrypt, before decompress) ──
 *
 * Two pre-GEN-1 writer quirks that the current library cannot parse:
 *
 * 1. v2-era Dictionary text stored the singularity as
 *    [u32 LE raw_size][zlib stream] *without* QTSQ_FLAG_ZLIB. The horizon
 *    decoder only inflates when the flag is set, and otherwise reads the
 *    first 8 bytes as dictionary counts → QTSQ_ERR_FORMAT. The zlib magic
 *    at offset 4 is unambiguous here: a genuine packed payload starts with
 *    a u64 dict_size whose byte 4 must be 0 for any sane dictionary, and
 *    SQTR raw-text files carry the qtc magic there instead. Fix: inflate
 *    once (validates the stream, gives the real size), then rebuild the
 *    singularity as [u64 LE raw_size][same zlib stream] with the flag
 *    raised — the modern layout, no recompression.
 *
 * 2. v2-era STRATEGY_NONE images whose header.original_size disagrees with
 *    the singularity (stale metadata from the old writer) and whose
 *    checksum was hashed under the retired read-time convention (see the
 *    GEN-1 FIX comment in qtsq_radiation.c). The NONE path copies only
 *    original_size bytes and verification then rejects the file. Fix:
 *    trust the singularity as the raster extent and drop CHECKSUMMED.
 *    Both fixes leave every file the current library can already parse
 *    untouched. */
static int qtsq_compat_normalize(qtsq_context_t *ctx) {
    if (!ctx || !ctx->singularity) return QTSQ_OK;

    /* Legacy Dictionary text: singularity = [u32 LE inflated_size][zlib
     * stream] with 32-bit dict counts inside. Modern readers expect
     * [u64 raw_size][zlib] with u64 counts — and these old writers DO set
     * the ZLIB flag, so the flag can't discriminate; sniff the layout
     * instead (a modern u64 raw_size with 78 da at bytes 4..6 would exceed
     * the decoder's own 1e13 size cap, so the sniff is unambiguous). */
    if (ctx->header.strategy == QTSQ_STRATEGY_DICTIONARY &&
        ctx->singularity_size >= 8 &&
        !(ctx->singularity[0] == 'S' && ctx->singularity[1] == 'Q' &&
          ctx->singularity[2] == 'T' && ctx->singularity[3] == 'R') &&
        ctx->singularity[4] == 0x78 && ctx->singularity[5] == 0xda) {
        uint32_t bound;
        memcpy(&bound, ctx->singularity, 4);
        if (bound == 0 || bound > (1u << 30)) return QTSQ_OK;
        unsigned char *payload = (unsigned char *)malloc(bound);
        if (!payload) return QTSQ_ERR_ALLOC;
        uLongf plen = bound;
        int zrc = uncompress(payload, &plen, ctx->singularity + 4,
                             (uLong)(ctx->singularity_size - 4));
        if (zrc != Z_OK || plen < 16) { free(payload); return QTSQ_OK; }

        /* Legacy payload: [u32 dict_size][u32 num_tokens][dict][varint
         * tokens]; the modern decoder reads u64 counts. Convert when the
         * first 16 bytes don't already parse as the u64 layout. */
        uint32_t d32, t32;
        memcpy(&d32, payload, 4);
        memcpy(&t32, payload + 4, 4);
        uint64_t d64, t64;
        memcpy(&d64, payload, 8);
        memcpy(&t64, payload + 8, 8);
        if (d64 == 0 || d64 > 2000000000ull || t64 > 1000000000000ull) {
            if (d32 == 0 || d32 > 2000000000u || t32 > 1000000000000u) {
                free(payload);
                return QTSQ_OK;
            }
            unsigned char *np = (unsigned char *)malloc(plen + 8);
            if (!np) { free(payload); return QTSQ_ERR_ALLOC; }
            uint64_t dd = d32, tt = t32;
            memcpy(np, &dd, 8);
            memcpy(np + 8, &tt, 8);
            memcpy(np + 16, payload + 8, plen - 8);
            free(payload);
            payload = np;
            plen += 8;
        }

        /* Re-compress the normalized payload and wrap it as
         * [u64 LE raw_size][zlib stream] — the modern ZLIB-flag layout. */
        uLongf clen = compressBound(plen);
        unsigned char *zbuf = (unsigned char *)malloc(clen);
        unsigned char *nb = (unsigned char *)malloc(8 + clen);
        if (!zbuf || !nb) { free(zbuf); free(nb); free(payload); return QTSQ_ERR_ALLOC; }
        if (compress2(zbuf, &clen, payload, plen, Z_BEST_SPEED) != Z_OK) {
            free(payload); free(zbuf); free(nb);
            return QTSQ_OK;
        }
        uint64_t declared = plen;
        memcpy(nb, &declared, 8);
        memcpy(nb + 8, zbuf, clen);
        free(payload);
        free(zbuf);
        free(ctx->singularity);
        ctx->singularity = nb;
        ctx->singularity_size = 8 + clen;
        ctx->header.flags |= QTSQ_FLAG_ZLIB;
        /* The legacy checksum convention (pre-GEN-1 read-time hashing) can't
         * be verified against the decoded text; drop the flag rather than
         * fail a file whose payload we have fully validated above. */
        ctx->header.flags &= ~(uint32_t)QTSQ_FLAG_CHECKSUMMED;
        return QTSQ_OK;
    }

    if (ctx->header.strategy == QTSQ_STRATEGY_NONE &&
        ctx->header.original_size != ctx->singularity_size &&
        (ctx->header.data_type == QTSQ_TYPE_IMAGE_GRAY ||
         ctx->header.data_type == QTSQ_TYPE_IMAGE_RGB ||
         ctx->header.data_type == QTSQ_TYPE_IMAGE_RGBA) &&
        ctx->schema.dimensions[0] > 0 && ctx->schema.dimensions[1] > 0) {
        uint64_t need = (uint64_t)ctx->schema.dimensions[0] *
                        ctx->schema.dimensions[1] * ctx->header.num_channels;
        uint64_t row = (uint64_t)ctx->schema.dimensions[0] * ctx->header.num_channels;
        /* Only when the singularity is plausibly the raw raster — within one
         * row of the schema size in either direction — does trusting it make
         * sense; otherwise the mismatch means something we can't repair. */
        if (need > 0 &&
            ctx->singularity_size + row >= need &&
            ctx->singularity_size <= need + row) {
            ctx->header.original_size = ctx->singularity_size;
            ctx->header.flags &= ~(uint32_t)QTSQ_FLAG_CHECKSUMMED;
        }
    }
    return QTSQ_OK;
}

/* ── device-key (passwordless) decrypt ──
 *
 * QTSQStudio's "Save Without a Password" encrypts the context to the
 * account's ML-KEM-768 device keypair (tools/StudioSwift/DeviceKeyStore.swift),
 * stored 0600 at <Application Support>/QTSQ/device_kem.sk. Kitae tries the
 * same key so those files open here without a prompt, exactly like they do
 * in the Studio. Files locked to another device or password-encrypted fail
 * closed with QTSQ_ERR_ENCRYPTED. */
#define QTSQ_DEVICE_SK_SIZE 2400

/* Key store directory: $KITAE_QTSQ_KEY_DIR overrides the default per-user
 * location (tests use the override to stay off the real device key). */
static int kitae_qtsq_key_dir(char *out, size_t cap) {
    const char *env = getenv("KITAE_QTSQ_KEY_DIR");
    if (env && env[0]) { snprintf(out, cap, "%s", env); return 0; }
    const char *home = getenv("HOME");
    if (!home || !home[0]) return -1;
    snprintf(out, cap, "%s/Library/Application Support/QTSQ", home);
    return 0;
}

static int kitae_qtsq_try_device_key(qtsq_context_t *ctx) {
    char dir[512], path[600];
    if (kitae_qtsq_key_dir(dir, sizeof(dir)) != 0) return -1;
    snprintf(path, sizeof(path), "%s/device_kem.sk", dir);
    FILE *f = fopen(path, "rb");
    if (!f) return -1;
    unsigned char sk[QTSQ_DEVICE_SK_SIZE];
    size_t n = fread(sk, 1, sizeof(sk), f);
    fclose(f);
    if (n != QTSQ_DEVICE_SK_SIZE) return -1;
    return qtsq_vault_decrypt_pq(ctx, sk);
}

/* ── header access (schema is populated by qtsq_read from the horizon block
 * when present; pixel-strategy files carry horizon_size == 0 and the schema
 * is filled in only during decompress) ── */

unsigned int kitae_qtsq_header_strategy(const qtsq_context_t *ctx) { return ctx->header.strategy; }
unsigned int kitae_qtsq_header_data_type(const qtsq_context_t *ctx) { return ctx->header.data_type; }
unsigned int kitae_qtsq_header_channels(const qtsq_context_t *ctx) { return ctx->header.num_channels; }
unsigned long long kitae_qtsq_header_original_size(const qtsq_context_t *ctx) { return ctx->header.original_size; }
unsigned long long kitae_qtsq_header_horizon_size(const qtsq_context_t *ctx) { return ctx->header.horizon_size; }
int kitae_qtsq_is_encrypted(const qtsq_context_t *ctx) { return qtsq_is_encrypted(ctx); }
int kitae_qtsq_header_version(const qtsq_context_t *ctx) { return ctx->header.version; }

/* ── signature verify ──
 *
 * Vault signatures cover ctx[0..112] (the packed v3 header) plus the
 * singularity, hashed by qtsq_vault_sign *before* its own epilogue raises
 * PQ_SIGNED/ZERO_META in header.flags and before qtsq_write stamps
 * security_block_size — qtsq_vault_verify hashes the *current* header, so
 * verifying a reloaded file only succeeds after reconstructing that
 * signing-time state (probes 9–13, /tmp/qtsq_probe13.c). Verification must
 * run on the still-encrypted context: decrypting rewrites the singularity
 * and would change the hash input. Probing state via raw header offsets is
 * safe for v3 (112-byte packed header, offsets fixed by qtsq_format.h). */
#define QTSQ_SIGN_TIME_FLAGS_MASK 0xA0000u /* PQ_SIGNED | ZERO_META */

static void kitae_qtsq_patch_signing_header(unsigned char *h, unsigned int *saved_flags,
                                            uint64_t *saved_sb) {
    memcpy(saved_flags, h + 6, 4);
    memcpy(saved_sb, h + 94, 8);
    unsigned int flags = *saved_flags & ~QTSQ_SIGN_TIME_FLAGS_MASK;
    uint64_t sb = 0;
    memcpy(h + 6, &flags, 4);
    memcpy(h + 94, &sb, 8);
}

static void kitae_qtsq_restore_signing_header(unsigned char *h, unsigned int saved_flags,
                                              uint64_t saved_sb) {
    memcpy(h + 6, &saved_flags, 4);
    memcpy(h + 94, &saved_sb, 8);
}

int kitae_qtsq_is_signed(const qtsq_context_t *ctx) { return qtsq_vault_is_signed(ctx); }

int kitae_qtsq_sig_size(const qtsq_context_t *ctx) {
    if (!ctx || !ctx->has_security) return 0;
    return (int)ctx->security.sig_size;
}

int kitae_qtsq_sig_bytes(const qtsq_context_t *ctx, unsigned char *out, int cap) {
    if (!ctx || !ctx->has_security || ctx->security.signature_type == 0) return -1;
    if (ctx->security.sig_size > (uint16_t)cap) return -2;
    memcpy(out, ctx->security.signature, ctx->security.sig_size);
    return (int)ctx->security.sig_size;
}

/* Self-contained verify: spool → qtsq_read → reconstruct signing-time header
 * state → verify → restore. Independent of decode_open's decrypt path by
 * design (the hash input includes the encrypted singularity bytes). */
int kitae_qtsq_verify_buffer(const unsigned char *data, size_t size,
                             const unsigned char sign_pk[1952]) {
    if (!data || !sign_pk) return QTSQ_ERR_NULL;
    qtsq_context_t ctx;
    memset(&ctx, 0, sizeof(ctx));
    int rc = qtsq_init(&ctx);
    if (rc != QTSQ_OK) return rc;
    char path[80];
    snprintf(path, sizeof(path), "/tmp/kitae_qtsq_verify_%d_%p.bin", (int)getpid(),
             (void *)&ctx);
    rc = write_all(path, data, size);
    if (rc != 0) { qtsq_free(&ctx); return rc; }
    rc = qtsq_read(&ctx, path);
    unlink(path);
    if (rc != QTSQ_OK) { qtsq_free(&ctx); return rc; }
    if (!qtsq_vault_is_signed(&ctx)) { qtsq_free(&ctx); return -40; }
    unsigned char *h = (unsigned char *)&ctx.header;
    unsigned int saved_flags;
    uint64_t saved_sb;
    kitae_qtsq_patch_signing_header(h, &saved_flags, &saved_sb);
    rc = qtsq_vault_verify(&ctx, sign_pk);
    kitae_qtsq_restore_signing_header(h, saved_flags, saved_sb);
    qtsq_free(&ctx);
    return rc;
}

/* ── signing side (unit tests + Save-Page-as-QTSQ) ── */

int kitae_qtsq_sign_keygen(unsigned char *out_pk, unsigned char *out_sk) {
    if (!out_pk || !out_sk) return QTSQ_ERR_NULL;
    qtsq_sign_keypair_t kp;
    int rc = qtsq_sign_keygen(&kp);
    if (rc != QTSQ_OK) return rc;
    memcpy(out_pk, kp.sign_pk, 1952);
    memcpy(out_sk, kp.sign_sk, 4032);
    return QTSQ_OK;
}

/* This machine's device sign public key, when one exists (written by the
 * Save-Page feature; see kitae_sign_key_store). -1 = none. */
int kitae_qtsq_device_sign_pk(unsigned char *out_pk) {
    char dir[512], path[600];
    if (kitae_qtsq_key_dir(dir, sizeof(dir)) != 0) return -1;
    snprintf(path, sizeof(path), "%s/device_sign.pk", dir);
    FILE *f = fopen(path, "rb");
    if (!f) return -1;
    size_t n = fread(out_pk, 1, 1952, f);
    fclose(f);
    return n == 1952 ? 0 : -1;
}

/* ── Save-Page-as-QTSQ key store ──
 *
 * Archives are locked to this machine (ML-KEM-768 recipient = device_kem.pk)
 * and signed with a device ML-DSA-65 keypair (device_sign.pk/sk), so a saved
 * page reopens passwordless here and verifies as Valid. Generation is
 * on-first-save, matching the Studio DeviceKeyStore pattern: write to a
 * unique temp sibling, fsync, rename into place (atomic; a crash mid-write
 * leaves the previous keys — or none — intact), 0600 throughout. */

static int kitae_qtsq_write_secret(const char *path, const unsigned char *data, size_t len) {
    char tmp[640];
    snprintf(tmp, sizeof(tmp), "%s.tmp%d", path, (int)getpid());
    FILE *f = fopen(tmp, "wb");
    if (!f) return -1;
    size_t n = fwrite(data, 1, len, f);
    int io = fflush(f);
    if (fsync(fileno(f)) != 0) io = -1;
    fclose(f);
    if (n != len || io != 0) { unlink(tmp); return -1; }
    if (chmod(tmp, 0600) != 0) { unlink(tmp); return -1; }
    if (rename(tmp, path) != 0) { unlink(tmp); return -1; }
    return 0;
}

static int kitae_qtsq_read_exact(const char *path, unsigned char *out, size_t len) {
    FILE *f = fopen(path, "rb");
    if (!f) return -1;
    size_t n = fread(out, 1, len, f);
    fclose(f);
    return n == len ? 0 : -1;
}

/* Ensure both device pairs exist — each independently, so an existing KEM
 * pair (Studio-written or a previous save) is never regenerated: old
 * device-encrypted files would stop decrypting. On return, hands back the
 * three keys a save needs: the ML-KEM-768 recipient public key, and the
 * ML-DSA-65 signing public + secret keys. */
int kitae_qtsq_ensure_device_keys(unsigned char *out_kem_pk,
                                  unsigned char *out_sign_pk,
                                  unsigned char *out_sign_sk) {
    if (!out_kem_pk || !out_sign_pk || !out_sign_sk) return QTSQ_ERR_NULL;
    char dir[512];
    if (kitae_qtsq_key_dir(dir, sizeof(dir)) != 0) return -1;
    char pk_path[600], sk_path[600], spk_path[600], ssk_path[600];
    snprintf(pk_path, sizeof(pk_path), "%s/device_kem.pk", dir);
    snprintf(sk_path, sizeof(sk_path), "%s/device_kem.sk", dir);
    snprintf(spk_path, sizeof(spk_path), "%s/device_sign.pk", dir);
    snprintf(ssk_path, sizeof(ssk_path), "%s/device_sign.sk", dir);

    unsigned char kem_pk[1184], kem_sk[2400], sign_pk[1952], sign_sk[4032];
    if (!(kitae_qtsq_read_exact(pk_path, kem_pk, sizeof(kem_pk)) == 0 &&
          kitae_qtsq_read_exact(sk_path, kem_sk, sizeof(kem_sk)) == 0)) {
        qtsq_keypair_t kem;
        if (qtsq_keygen(&kem) != QTSQ_OK) return -2;
        memcpy(kem_pk, kem.public_key, 1184);
        memcpy(kem_sk, kem.secret_key, 2400);
        if (kitae_qtsq_write_secret(pk_path, kem_pk, 1184) != 0) return -3;
        if (kitae_qtsq_write_secret(sk_path, kem_sk, 2400) != 0) return -3;
    }
    if (!(kitae_qtsq_read_exact(spk_path, sign_pk, sizeof(sign_pk)) == 0 &&
          kitae_qtsq_read_exact(ssk_path, sign_sk, sizeof(sign_sk)) == 0)) {
        qtsq_sign_keypair_t signer;
        if (qtsq_sign_keygen(&signer) != QTSQ_OK) return -2;
        memcpy(sign_pk, signer.sign_pk, 1952);
        memcpy(sign_sk, signer.sign_sk, 4032);
        if (kitae_qtsq_write_secret(spk_path, sign_pk, 1952) != 0) return -3;
        if (kitae_qtsq_write_secret(ssk_path, sign_sk, 4032) != 0) return -3;
    }
    memcpy(out_kem_pk, kem_pk, 1184);
    memcpy(out_sign_pk, sign_pk, 1952);
    memcpy(out_sign_sk, sign_sk, 4032);
    return 0;
}

/* Test helper: generate a KEM pair in the current key dir without touching
 * the sign keys (lets engine tests set up device_kem.pk for encryption
 * while device_sign.pk stays absent for Unverified-verdict assertions). */
int kitae_qtsq_kem_keygen(void) {
    char dir[512];
    if (kitae_qtsq_key_dir(dir, sizeof(dir)) != 0) return -1;
    qtsq_keypair_t kem;
    if (qtsq_keygen(&kem) != QTSQ_OK) return -2;
    char pk_path[600], sk_path[600];
    snprintf(pk_path, sizeof(pk_path), "%s/device_kem.pk", dir);
    snprintf(sk_path, sizeof(sk_path), "%s/device_kem.sk", dir);
    if (kitae_qtsq_write_secret(pk_path, kem.public_key, 1184) != 0) return -3;
    if (kitae_qtsq_write_secret(sk_path, kem.secret_key, 2400) != 0) return -3;
    return 0;
}

/* One-shot text encode: compress + encrypt + sign (qtsq_secure_text) and
 * return the .qtsq bytes. `password` XOR `recipient_pk` picks the vault
 * mode (password, or ML-KEM for passwordless device saves); `sign_sk` may
 * be NULL to skip signing. */
int kitae_qtsq_encode_text_secure(const unsigned char *text, size_t len,
                                  const char *password,
                                  const unsigned char *recipient_pk,
                                  const unsigned char *sign_sk,
                                  unsigned char **out_data, size_t *out_size) {
    if (!text || !out_data || !out_size) return QTSQ_ERR_NULL;
    if (!password && !recipient_pk) return QTSQ_ERR_NULL;
    qtsq_context_t ctx;
    memset(&ctx, 0, sizeof(ctx));
    int rc = qtsq_init(&ctx);
    if (rc != QTSQ_OK) return rc;
    rc = qtsq_create_creative(&ctx);
    if (rc != QTSQ_OK) { qtsq_free(&ctx); return rc; }
    rc = qtsq_secure_text(&ctx, (const char *)text, len, password, recipient_pk, sign_sk);
    if (rc != QTSQ_OK) { qtsq_free(&ctx); return rc; }
    char path[80];
    snprintf(path, sizeof(path), "/tmp/kitae_qtsq_enc_%d_%p.bin", (int)getpid(), (void *)&ctx);
    rc = qtsq_write(&ctx, path);
    qtsq_free(&ctx);
    if (rc != QTSQ_OK) return rc;
    FILE *f = fopen(path, "rb");
    if (!f) return QTSQ_ERR_IO;
    fseek(f, 0, SEEK_END);
    long flen = ftell(f);
    fseek(f, 0, SEEK_SET);
    unsigned char *buf = (unsigned char *)malloc(flen > 0 ? (size_t)flen : 1);
    if (!buf) { fclose(f); unlink(path); return QTSQ_ERR_ALLOC; }
    size_t n = fread(buf, 1, (size_t)flen, f);
    fclose(f);
    unlink(path);
    if ((long)n != flen) { free(buf); return QTSQ_ERR_IO; }
    *out_data = buf;
    *out_size = (size_t)flen;
    return QTSQ_OK;
}

/* schema (valid after read for horizon-carrying files, after decompress for pixel files) */
unsigned int kitae_qtsq_schema_width(const qtsq_context_t *ctx) { return ctx->schema.dimensions[0]; }
unsigned int kitae_qtsq_schema_height(const qtsq_context_t *ctx) { return ctx->schema.dimensions[1]; }
unsigned int kitae_qtsq_schema_sample_rate(const qtsq_context_t *ctx) { return ctx->schema.sample_rate; }
unsigned int kitae_qtsq_schema_frame_rate(const qtsq_context_t *ctx) { return ctx->schema.frame_rate; }

/* ── lifecycle ── */

int kitae_qtsq_context_new(qtsq_context_t **out) {
    qtsq_context_t *ctx = (qtsq_context_t *)malloc(sizeof(qtsq_context_t));
    if (!ctx) return QTSQ_ERR_ALLOC;
    memset(ctx, 0, sizeof(qtsq_context_t));
    int rc = qtsq_init(ctx);
    if (rc != QTSQ_OK) { free(ctx); return rc; }
    *out = ctx;
    return QTSQ_OK;
}

void kitae_qtsq_context_free(qtsq_context_t *ctx) {
    if (!ctx) return;
    qtsq_free(ctx);
    free(ctx);
}

/* ── image/text decode: qtsq_decompress → raw bytes ── */

int kitae_qtsq_decode_open(qtsq_context_t **out,
                           const unsigned char *data, size_t size,
                           const char *password) {
    qtsq_context_t *ctx = NULL;
    int rc = kitae_qtsq_context_new(&ctx);
    if (rc != QTSQ_OK) return rc;
    /* Unique spool path per context (pointer value): decode contexts are
     * independent and may be created concurrently in one process. */
    char path[80];
    snprintf(path, sizeof(path), "/tmp/kitae_qtsq_%d_%p.bin", (int)getpid(), (void *)ctx);
    rc = write_all(path, data, size);
    if (rc != 0) { kitae_qtsq_context_free(ctx); return rc; }
    rc = qtsq_read(ctx, path);
    unlink(path);
    if (rc != QTSQ_OK) { kitae_qtsq_context_free(ctx); return rc; }
    if (qtsq_is_encrypted(ctx)) {
        if (!password) {
            /* No password given: try this machine's device key (the same
             * ML-KEM secret QTSQStudio uses for passwordless saves). */
            if (kitae_qtsq_try_device_key(ctx) != QTSQ_OK) {
                kitae_qtsq_context_free(ctx);
                return QTSQ_ERR_ENCRYPTED;
            }
        } else {
            /* Vault files (qtsq_vault_encrypt_password/QTSQStudio saves) carry
             * the security block; the legacy qtsq_decrypt path cannot open
             * them, so decrypt through the vault API. Wrong passwords fail
             * with QTSQ_ERR_DECRYPT (-9) — distinct from -8, so the viewer
             * can tell "locked" from "wrong password". */
            rc = qtsq_vault_decrypt_password(ctx, password);
            if (rc != QTSQ_OK) { kitae_qtsq_context_free(ctx); return rc; }
        }
    }
    /* secure_text files: pad_size stashed the true size in the (now
     * decrypted) trailer and unpad_size restored original_size to the
     * pre-pad singularity size — which still includes the embedded-names
     * block extract just stripped. For SQTR raw-text dictionary payloads
     * the qtc stream header declares the real text length, so trust it;
     * the checksum verifier then runs over the right extent. Plain
     * compress_text files already carry the correct size here, making
     * this a no-op for them. */
    if (ctx->header.strategy == QTSQ_STRATEGY_DICTIONARY &&
        ctx->singularity && ctx->singularity_size >= 4 + QTSQ_QTC_HEADER_SIZE &&
        ctx->singularity[0] == 'S' && ctx->singularity[1] == 'Q' &&
        ctx->singularity[2] == 'T' && ctx->singularity[3] == 'R') {
        uint64_t decl = qtsq_codec_declared_size(ctx->singularity + 4,
                                                 ctx->singularity_size - 4);
        if (decl > 0 && decl <= 10000000000000ull) {
            ctx->header.original_size = (size_t)decl;
        }
    }
    rc = qtsq_compat_normalize(ctx);
    if (rc != QTSQ_OK) { kitae_qtsq_context_free(ctx); return rc; }
    *out = ctx;
    return QTSQ_OK;
}

int kitae_qtsq_decompress_bytes(qtsq_context_t *ctx,
                                unsigned char **out_data, size_t *out_size) {
    return qtsq_decompress(ctx, out_data, out_size);
}

/* ── tensor: delta/quantized f32 arrays (AI weights, embeddings) ──
 *
 * qtsq_decompress_tensor also patches ctx->schema.dimensions/num_dims from
 * the payload's embedded header (tensor dims survive the container even for
 * files without a horizon block), so the schema read below is only valid
 * after a successful decode — it returns them together, atomically. */

int kitae_qtsq_decode_tensor(qtsq_context_t *ctx,
                             float **out_data, size_t *out_count,
                             unsigned int *out_num_dims,
                             unsigned int *out_dims) {
    int rc = qtsq_decompress_tensor(ctx, out_data, out_count);
    if (rc != QTSQ_OK) return rc;
    *out_num_dims = ctx->schema.num_dims;
    for (int i = 0; i < 8; i++) out_dims[i] = ctx->schema.dimensions[i];
    return QTSQ_OK;
}

/* ── container payloads ──
 *
 * QTSQStudio compresses arbitrary original files into the container, so the
 * decompressed payload may be an encoded image (JPEG/PNG/GIF/BMP/…) rather
 * than raw pixels. `kitae_qtsq_payload_is_encoded_image` sniffs the magic,
 * `kitae_qtsq_decode_encoded_image` runs it through stb_image. */

int kitae_qtsq_payload_is_encoded_image(const unsigned char *data, size_t size) {
    return stbi_info_from_memory(data, (int)size, NULL, NULL, NULL) ? 1 : 0;
}

int kitae_qtsq_decode_encoded_image(const unsigned char *data, size_t size,
                                    unsigned char **out_pixels,
                                    unsigned int *out_w, unsigned int *out_h,
                                    unsigned int *out_channels) {
    int w = 0, h = 0, ch = 0;
    /* Force 4 channels: RGBA8 is the engine's canonical raster format. */
    unsigned char *pixels = stbi_load_from_memory(data, (int)size, &w, &h, &ch, 4);
    if (!pixels) return QTSQ_ERR_FORMAT;
    *out_pixels = pixels;
    *out_w = (unsigned int)w;
    *out_h = (unsigned int)h;
    *out_channels = 4;
    return QTSQ_OK;
}

/* free a buffer that libqtsq malloc'd */
void kitae_qtsq_free_buffer(void *p) { free(p); }

/* ── audio: auto-detect codec → int16 PCM ── */

int kitae_qtsq_decode_audio(qtsq_context_t *ctx,
                            short **out_pcm, size_t *out_samples,
                            unsigned int *out_rate, unsigned char *out_channels) {
    return qtsq_decompress_audio(ctx, out_pcm, out_samples, out_rate, out_channels);
}

/* float PCM path (lossless float capture / qtc float) */
int kitae_qtsq_decode_audio_float(qtsq_context_t *ctx,
                                  float **out_pcm, size_t *out_samples,
                                  unsigned int *out_rate, unsigned char *out_channels) {
    return qtsq_decompress_audio_float(ctx, out_pcm, out_samples, out_rate, out_channels);
}

/* ── video ── */

int kitae_qtsq_video_decoder_open(qtsq_context_t *ctx, qtsq_video_decoder_t **out) {
    qtsq_video_decoder_t *dec = qtsq_video_decoder_open(ctx);
    if (!dec) return QTSQ_ERR_FORMAT;
    *out = dec;
    return QTSQ_OK;
}

int kitae_qtsq_video_get_frame(qtsq_video_decoder_t *dec, unsigned int index,
                               unsigned char **out_pixels,
                               unsigned int *out_w, unsigned int *out_h) {
    return qtsq_video_decoder_get_frame(dec, index, out_pixels, out_w, out_h);
}

unsigned int kitae_qtsq_video_frame_count(qtsq_video_decoder_t *dec) {
    return qtsq_video_decoder_get_frame_count(dec);
}

void kitae_qtsq_video_decoder_free(qtsq_video_decoder_t *dec) {
    qtsq_video_decoder_free(dec);
}
