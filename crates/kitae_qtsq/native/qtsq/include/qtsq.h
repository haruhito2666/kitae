/*
 * Quantum Tensor Sequence — Universal Public API
 * File Extension: .qtsq (Quantum Tensor Sequence)
 * Author: Haruhito
 */

#ifndef QTSQ_H
#define QTSQ_H

#include "qtsq_format.h"
#include "qtsq_types.h"
#include "qtsq_turbo.h"
#include "qtsq_triangle_only.h"
#include "qtsq_ionic.h"
#include "qtsq_splat_tsubu.h"
#include "qtsq_4dgs.h"
#include "qtsq_seal.h"
#include "qtsq_surfel.h"

int  qtsq_init(qtsq_context_t *ctx);
void qtsq_free(qtsq_context_t *ctx);

/* ═══ Two-Intent API (Phase 3) ═══ */

/**
 * qtsq_create_creative — Initialize a context for Creative intent.
 * Re-synthesis modes (triangle, gaussian, 3DGS) are ALLOWED.
 * Signature attests the QTSQ *output* (the re-synthesized result).
 */
int qtsq_create_creative(qtsq_context_t *ctx);

/**
 * qtsq_create_provenance — Initialize a context for Provenance intent.
 * Re-synthesis modes are BLOCKED (pixel-only, bit-exact).
 * Signature attests the original *input* bytes.
 */
int qtsq_create_provenance(qtsq_context_t *ctx);

/**
 * qtsq_intent — Returns the intent of a context.
 */
qtsq_intent_t qtsq_intent(const qtsq_context_t *ctx);

/**
 * qtsq_is_resynthesis_allowed — Returns 1 if the context allows re-synthesis modes.
 */
int qtsq_is_resynthesis_allowed(const qtsq_context_t *ctx);

/**
 * qtsq_require_creative — Guard function for re-synthesis modes.
 * Returns QTSQ_OK if intent is Creative or None, QTSQ_ERR_TYPE if Provenance.
 */
int qtsq_require_creative(qtsq_context_t *ctx, const char *mode_name);

/**
 * qtsq_set_original_input — Store original input bytes for Provenance signing.
 * In Provenance intent, the signature covers these bytes (not the output).
 * Caller provides the raw input before compression.
 */
int qtsq_set_original_input(qtsq_context_t *ctx,
                            const uint8_t *data, size_t size);

int qtsq_compress(qtsq_context_t *ctx,
                 const uint8_t *data, size_t size,
                 qtsq_data_type_t type_hint);

int qtsq_compress_image(qtsq_context_t *ctx,
                       const uint8_t *pixels,
                       uint32_t width, uint32_t height,
                       uint8_t channels);

/* Phase 6b: 16-bit/channel camera-raw image capture path. Requires
 * ctx->codec == QTSQ_CODEC_QTC (the only 16-bit image coder in tree).
 * Lossless by default; ctx->quality > 0 = near-lossless (<= 8 LSB error).
 * Implemented in the pixel engine — see qtsq_pixel.h. */
int qtsq_compress_pixel16(qtsq_context_t *ctx,
                          const uint16_t *pixels,
                          uint32_t width, uint32_t height,
                          uint8_t channels, int compression_level);
int qtsq_decompress_pixel16(qtsq_context_t *ctx,
                            uint16_t **out_pixels, size_t *out_size);

int qtsq_compress_gravitational(qtsq_context_t *ctx,
                                const uint8_t *pixels,
                                uint32_t width, uint32_t height,
                                uint8_t channels,
                                size_t source_size);

int qtsq_compress_gravitational_hd(qtsq_context_t *ctx,
                                    const uint8_t *pixels,
                                    uint32_t width, uint32_t height,
                                    uint8_t channels,
                                    size_t source_size);

/* Mathematical upscaler for lossy inputs (JPEG/WebP).
 * Uses dense triangle mesh + splats to reconstruct clean edges
 * from compression artifacts. Output is LARGER but HIGHER QUALITY. */
int qtsq_compress_gravitational_upscale(qtsq_context_t *ctx,
                                         const uint8_t *pixels,
                                         uint32_t width, uint32_t height,
                                         uint8_t channels,
                                         size_t source_size);

/* Lossless HD mode — maximum fidelity with 6M vertices.
 * Output caps: <5MB input → ≤10MB output, ≥5MB input → ≤30MB output. */
int qtsq_compress_gravitational_lossless(qtsq_context_t *ctx,
                                          const uint8_t *pixels,
                                          uint32_t width, uint32_t height,
                                          uint8_t channels,
                                          size_t source_size);

/* Pure 2D Gaussian Splat compression (no triangle mesh). */
int qtsq_compress_splat_compact(qtsq_context_t *ctx,
                                 const uint8_t *pixels,
                                 uint32_t width, uint32_t height,
                                 uint8_t channels, size_t source_size);
int qtsq_compress_splat_hd(qtsq_context_t *ctx,
                             const uint8_t *pixels,
                             uint32_t width, uint32_t height,
                             uint8_t channels, size_t source_size);
int qtsq_compress_splat_lossless(qtsq_context_t *ctx,
                                   const uint8_t *pixels,
                                   uint32_t width, uint32_t height,
                                   uint8_t channels, size_t source_size);
int qtsq_decompress_splat_only(qtsq_context_t *ctx,
                                 uint8_t **out_data, size_t *out_size);

int qtsq_decompress_gravitational(qtsq_context_t *ctx,
                                  uint8_t **out_data, size_t *out_size);

int qtsq_optimize_mesh(qtsq_context_t *ctx, const uint8_t *rgb_target, 
                       uint32_t width, uint32_t height);

/* ══════════════════════════════════════════════════════════════
 *  Gravitational AI Direct Render API 
 *  Bypasses redundant Delaunay triangulation during optimizations
 * ══════════════════════════════════════════════════════════════ */
typedef struct {
    float x, y;
    uint8_t r, g, b;
} tri_vertex_t;

typedef struct {
    uint32_t v0, v1, v2;
} tri_face_t;

int qtsq_render_mesh(const tri_vertex_t *verts, uint32_t nv,
                     const tri_face_t *faces, uint32_t nf,
                     uint32_t orig_w, uint32_t orig_h, uint8_t ch,
                     uint8_t *output);

/* AI Hooks for MeshNet V3 */
int qtsq_extract_vertices(const qtsq_context_t *ctx, float *flat_verts, uint32_t max_verts);
int qtsq_apply_vertices(qtsq_context_t *ctx, const float *new_verts, uint32_t num_verts);

/* Mesh Editor: Splat extract/apply — works with splat2d_t arrays directly */
int qtsq_extract_splats(const qtsq_context_t *ctx, void *out_splats, uint32_t max_splats,
                         uint32_t *out_width, uint32_t *out_height);
int qtsq_apply_splats(qtsq_context_t *ctx, const void *splats, uint32_t count,
                       uint32_t width, uint32_t height);

/* Phase F: Surfel extract/apply — works with qtsq_surfel_t arrays directly.
 * The context must be a surfel stream (data_type == QTSQ_TYPE_SURFEL) whose
 * singularity is the zlib-compressed ['S','R','F','1'][u32 count][surfels]
 * blob written by qtsq_apply_surfels / qtsq_surfel_container_write. A
 * [u32 uncompressed_size] prefix (splat convention) is tolerated on
 * extract; trailing seal markers after the zlib stream are ignored. */
int qtsq_extract_surfels(const qtsq_context_t *ctx, qtsq_surfel_t *out_surfels,
                         uint32_t max_surfels, uint32_t *out_count);
int qtsq_apply_surfels(qtsq_context_t *ctx, const qtsq_surfel_t *surfels,
                       uint32_t count);

int qtsq_compress_gravitational_audio(qtsq_context_t *ctx,
                                       const int16_t *pcm,
                                       uint32_t total_samples,
                                       uint8_t channels,
                                       uint32_t sample_rate);

int qtsq_decompress_gravitational_audio(const qtsq_context_t *ctx,
                                          uint8_t **out_data, size_t *out_size);

int qtsq_compress_audio(qtsq_context_t *ctx,
                       const int16_t *samples,
                       size_t num_samples,
                       uint32_t sample_rate,
                       uint8_t channels);

/* Phase 6b: float32 audio capture path (microphone / DSP). num_samples is
 * TOTAL samples (nframes * channels), interleaved. The default codec is qtc
 * (Phase 7): the qtc AUDIO_FLOAT lens is used — bit-exact lossless for every
 * IEEE-754 pattern (denormals/inf/NaN/-0); ctx->quality > 0 selects the
 * lossy int16-MDCT2 adapter. Setting ctx->codec = QTSQ_CODEC_ZLIB opts out
 * to raw floats under zlib (lossless). */
int qtsq_compress_audio_float(qtsq_context_t *ctx,
                              const float *samples,
                              size_t num_samples,
                              uint32_t sample_rate,
                              uint8_t channels);
int qtsq_decompress_audio_float(const qtsq_context_t *ctx,
                                float **out_pcm, size_t *out_samples,
                                uint32_t *out_sample_rate, uint8_t *out_channels);

/* Photon Audio — HQ Spatial Gabor atom codec */
int qtsq_compress_audio_hq(qtsq_context_t *ctx,
                            const float *samples,     /* 32-bit float */
                            size_t num_samples,
                            uint32_t sample_rate,
                            uint8_t channels);

int qtsq_decompress_audio_hq(const qtsq_context_t *ctx,
                              float **out_pcm,            /* outputs 32-bit float interleaved */
                              size_t *out_total_samples,
                              photon_atom_t **out_atoms,  /* outputs spatial atoms array */
                              size_t *out_atom_count);

/* Generic audio decompress — auto-detects codec, outputs int16 PCM */
int qtsq_decompress_audio(const qtsq_context_t *ctx,
                          int16_t **out_pcm, size_t *out_samples,
                          uint32_t *out_sample_rate, uint8_t *out_channels);

/* Tensor precision levels */
typedef enum {
    QTSQ_TENSOR_F32    = 0,  /* 32-bit float (default, delta-quantized) */
    QTSQ_TENSOR_F16    = 1,  /* 16-bit IEEE 754 half-precision */
    QTSQ_TENSOR_F8     = 2,  /* 8-bit E4M3 (NVIDIA FP8) */
    QTSQ_TENSOR_F4     = 3,  /* 4-bit symmetric linear quantization */
    QTSQ_TENSOR_RAW    = 4,  /* 32-bit lossless (no delta encoding) */
} qtsq_tensor_precision_t;

/* Tensor compression — optimized for float32 arrays (AI weights, embeddings) */
int qtsq_compress_tensor(qtsq_context_t *ctx,
                          const float *data, size_t count,
                          const uint32_t *dimensions, uint8_t num_dims);

/* Quantized tensor — specify precision for model size reduction */
int qtsq_compress_tensor_quantized(qtsq_context_t *ctx,
                                    const float *data, size_t count,
                                    const uint32_t *dimensions, uint8_t num_dims,
                                    qtsq_tensor_precision_t precision);

int qtsq_decompress_tensor(const qtsq_context_t *ctx,
                            float **out_data, size_t *out_count);

/* High-level AI brain state API — packs everything into a container */
int qtsq_brain_save(const char *filename,
                     const float *weights, size_t weight_count,
                     const float *biases, size_t bias_count,
                     const float *memory_vectors, size_t num_memories,
                     size_t vector_dim,
                     const char *metadata_json);

int qtsq_brain_load(const char *filename,
                     float **out_weights, size_t *out_weight_count,
                     float **out_biases, size_t *out_bias_count,
                     float **out_memory_vectors, size_t *out_num_memories,
                     size_t *out_vector_dim,
                     char **out_metadata_json);

int qtsq_compress_text(qtsq_context_t *ctx,
                      const char *text, size_t length);

int qtsq_compress_csv(qtsq_context_t *ctx,
                     const char *csv_data, size_t length);

int qtsq_compress_json(qtsq_context_t *ctx,
                       const char *json, size_t length);

int qtsq_compress_signal(qtsq_context_t *ctx,
                        const double *signal, size_t num_samples);

int qtsq_compress_video(qtsq_context_t *ctx,
                        const uint8_t *frames,
                        uint32_t width, uint32_t height,
                        uint32_t num_frames, uint8_t channels);

typedef struct qtsq_video_encoder qtsq_video_encoder_t;

/* Strategy flag: continuous 4DGS velocity-based splat video */
#define QTSQ_VIDEO_CONTINUOUS 0x10000

/* Initialize a video encoder session */
qtsq_video_encoder_t* qtsq_video_encoder_create(qtsq_context_t *ctx,
                                                uint32_t width, uint32_t height,
                                                uint8_t channels, uint32_t fps,
                                                int strategy_flags,
                                                size_t target_payload_per_frame);

/* Append a single uncompressed frame to the video sequence. */
int qtsq_video_encoder_add_frame(qtsq_video_encoder_t *enc, const uint8_t *pixels);

/* Finish encoding and flush all buffers to context. */
int qtsq_video_encoder_finish(qtsq_video_encoder_t *enc);

/* Get the underlying context */
qtsq_context_t* qtsq_video_encoder_get_context(qtsq_video_encoder_t *enc);

/* Cleanup encoder resources */
void qtsq_video_encoder_free(qtsq_video_encoder_t *enc);

typedef struct qtsq_video_decoder qtsq_video_decoder_t;

/* Open a video for decoding */
qtsq_video_decoder_t* qtsq_video_decoder_open(const qtsq_context_t *ctx);

/* Decode a specific frame. The returned pixel buffer is OWNED by the
 * decoder (a rotating internal buffer) — do NOT free it. It stays valid
 * until the next get_frame call or qtsq_video_decoder_free. */
int qtsq_video_decoder_get_frame(qtsq_video_decoder_t *dec, uint32_t frame_index, uint8_t **out_pixels, uint32_t *width, uint32_t *height);

/* Get total number of frames */
uint32_t qtsq_video_decoder_get_frame_count(qtsq_video_decoder_t *dec);

/* Get the underlying context */
const qtsq_context_t* qtsq_video_decoder_get_context(qtsq_video_decoder_t *dec);

/* Cleanup decoder resources */
void qtsq_video_decoder_free(qtsq_video_decoder_t *dec);

int qtsq_compress_raw(qtsq_context_t *ctx,
                     const uint8_t *data, size_t size, uint64_t seed);

int qtsq_write(const qtsq_context_t *ctx, const char *filename);

/* Write an explicitly unencrypted QTSQ payload.
 *
 * This is an opt-in escape hatch for local application caches such as
 * Sofuu's session/conversation records.  It rejects contexts that already
 * carry encryption/security state and does not weaken qtsq_write(), whose
 * normal contract remains fail-closed: sanitized + encrypted only.
 */
int qtsq_write_plaintext(const qtsq_context_t *ctx, const char *filename);

int qtsq_read(qtsq_context_t *ctx, const char *filename);

int qtsq_decompress(qtsq_context_t *ctx,
                   uint8_t **out_data, size_t *out_size);

int qtsq_decompress_redshift(const qtsq_context_t *ctx,
                            double redshift,
                            uint8_t **out_data, size_t *out_size);

int qtsq_radiate(const qtsq_context_t *ctx,
                uint64_t offset, uint32_t size,
                uint8_t **out_data);
int qtsq_radiate_time(const qtsq_context_t *ctx,
                     double start_sec, double duration_sec,
                     int16_t **out_samples, size_t *out_count);
int qtsq_radiate_region(const qtsq_context_t *ctx,
                       uint32_t x, uint32_t y, uint32_t w, uint32_t h,
                       uint8_t **out_pixels);
int qtsq_radiate_frame(const qtsq_context_t *ctx,
                      uint32_t frame_index, uint8_t **out_pixels);

/* Text search fast-path (V3 Horizon files only):
 * Scans the SimHash sketch stream in O(n/64) via POPCNT without decompressing
 * the full corpus.  Callback is invoked for each confirmed token match.
 *
 *   token_index  — position of the matched token in the token stream
 *   match_number — 0-based count of matches found so far
 *   userdata     — opaque pointer forwarded from the call site
 *
 * Return non-zero from the callback to stop the scan early. */
typedef void (*qtsq_text_match_cb)(uint64_t token_index,
                                   uint64_t match_number,
                                   void *userdata);

int qtsq_radiate_text_search(const qtsq_context_t *ctx,
                              const char *query, size_t query_len,
                              qtsq_text_match_cb callback,
                              void *userdata);

/* ═══ One-Shot Secure APIs (V3) ═══ */

int qtsq_secure_image(qtsq_context_t *ctx,
                      const uint8_t *pixels, uint32_t w, uint32_t h, uint8_t ch,
                      const char *password,
                      const uint8_t *recipient_pk,
                      const uint8_t *sign_sk);

/* 16-bit/channel camera-raw variant (Phase 6b). Forces ctx->codec to
 * QTSQ_CODEC_QTC (the only 16-bit image coder in tree). Sanitizes the 2
 * LSBs of every sample, then compresses via qtsq_compress_pixel16
 * (lossless when ctx->quality == 0). */
int qtsq_secure_image16(qtsq_context_t *ctx,
                        const uint16_t *pixels, uint32_t w, uint32_t h, uint8_t ch,
                        const char *password,
                        const uint8_t *recipient_pk,
                        const uint8_t *sign_sk);

int qtsq_secure_text(qtsq_context_t *ctx,
                     const char *text, size_t length,
                     const char *password,
                     const uint8_t *recipient_pk,
                     const uint8_t *sign_sk);

int qtsq_secure_json(qtsq_context_t *ctx,
                     const char *json, size_t length,
                     const char *password,
                     const uint8_t *recipient_pk,
                     const uint8_t *sign_sk);

int qtsq_secure_csv(qtsq_context_t *ctx,
                    const char *csv, size_t length,
                    const char *password,
                    const uint8_t *recipient_pk,
                    const uint8_t *sign_sk);

int qtsq_secure_tensor(qtsq_context_t *ctx,
                       const float *data, size_t count,
                       const uint32_t *dims, uint8_t ndims,
                       const char *password,
                       const uint8_t *recipient_pk,
                       const uint8_t *sign_sk);

int qtsq_secure_audio(qtsq_context_t *ctx,
                      const int16_t *samples, size_t num_samples,
                      uint32_t sample_rate, uint8_t channels,
                      const char *password,
                      const uint8_t *recipient_pk,
                      const uint8_t *sign_sk);

int qtsq_add_wormhole(qtsq_context_t *ctx,
                     const char *target_path,
                     uint32_t target_offset, uint32_t target_size);
int qtsq_resolve_wormholes(qtsq_context_t *ctx);

int qtsq_encrypt(qtsq_context_t *ctx, const char *password);
int qtsq_decrypt(qtsq_context_t *ctx, const char *password);
int qtsq_is_encrypted(const qtsq_context_t *ctx);

int qtsq_emit_wave(const qtsq_context_t *old_ctx,
                  const qtsq_context_t *new_ctx,
                  const char *wave_filename);
int qtsq_apply_wave(qtsq_context_t *ctx,
                   const char *wave_filename);

int qtsq_add_recovery(qtsq_context_t *ctx);
int qtsq_health_check(const qtsq_context_t *ctx);
int qtsq_recover(qtsq_context_t *ctx);

void            qtsq_print_info(const qtsq_context_t *ctx);
double          qtsq_compression_ratio(const qtsq_context_t *ctx);
const char*     qtsq_error_string(int error_code);

/* ═══ Phase 9: Differentiators ═══ */

/**
 * qtsq_pad_size — Pad singularity to a fixed size bucket to defeat traffic analysis.
 * Pads with random bytes before writing. Buckets: 1K, 4K, 16K, 64K, 256K, 1M, 4M.
 * Call before qtsq_write(). Sets QTSQ_SANITIZE_SIZE_PADDED in sanitize report.
 * The exact original size is hidden from the cleartext header (replaced with
 * the bucketed size) and stored only inside the encryption envelope.
 */
int qtsq_pad_size(qtsq_context_t *ctx);

/**
 * qtsq_unpad_size — Strip size padding after decrypt. Reads and verifies
 * the 16-byte trailer at the end of the singularity ([8B "QTSQPAD1" magic]
 * [8B original size], which lived inside the encryption envelope) and
 * restores the singularity to its original size. No-op if the context was
 * not padded (the magic check also covers file round-trips, where the
 * in-memory SIZE_PADDED report flag is not restored by qtsq_read).
 */
int qtsq_unpad_size(qtsq_context_t *ctx);

/**
 * qtsq_sanitize_report — Returns a human-readable string of what privacy
 * actions were applied to this context. Caller must NOT free the returned pointer.
 */
const char* qtsq_sanitize_report(const qtsq_context_t *ctx);

/**
 * qtsq_set_consent — Set consent flags (DO_NOT_TRAIN, NO_AI).
 * These flags are embedded in the file header and the authorship token.
 */
int qtsq_set_consent(qtsq_context_t *ctx, uint32_t consent_flags);

/* ══════ Utility ══════ */
void qtsq_sha256(const uint8_t *data, size_t size, uint8_t *hash);
double qtsq_entropy(const uint8_t *data, size_t size);
uint32_t qtsq_crc32(const uint8_t *data, size_t size);

/**
 * qtsq_uncompress_bounded — zlib-decompress with hard caps on the declared
 * output size, so a crafted file cannot force a huge malloc (DoS).
 *
 * @param src         Compressed input (must be at least 4 bytes).
 * @param src_size    Compressed input size.
 * @param max_ratio   Max legitimate uncompressed/compressed ratio (e.g. 256).
 * @param max_abs     Absolute cap on uncompressed size in bytes.
 * @param out         Receives malloc'd output; caller frees. NULL on failure.
 * @param out_size    Receives actual uncompressed size.
 * @return            QTSQ_OK or QTSQ_ERR_FORMAT/QTSQ_ERR_ALLOC.
 */
int qtsq_uncompress_bounded(const uint8_t *src, size_t src_size,
                            uint64_t max_ratio, uint64_t max_abs,
                            uint8_t **out, size_t *out_size);

/* ══════ Engine-specific decompression ══════ */
int qtsq_decompress_singularity(const qtsq_context_t *ctx,
                                uint8_t **out_data, size_t *out_size);
int qtsq_decompress_horizon(const qtsq_context_t *ctx,
                            uint8_t **out_data, size_t *out_size);
int qtsq_decompress_lensing(const qtsq_context_t *ctx,
                            uint8_t **out_data, size_t *out_size);
int qtsq_decompress_accretion(const qtsq_context_t *ctx,
                              uint8_t **out_data, size_t *out_size);

/* ══════ Container (Multi-Data) ══════ */
int qtsq_container_create(qtsq_context_t *ctx);
int qtsq_container_add_file(qtsq_context_t *ctx, const char *path,
                             const char *stream_name);
int qtsq_container_add_stream(qtsq_context_t *ctx,
                               const qtsq_context_t *sub_ctx,
                               const char *stream_name);
int qtsq_container_add_folder(qtsq_context_t *ctx, const char *folder_path);
int qtsq_container_pack(qtsq_context_t *ctx);
int qtsq_container_get_count(const qtsq_context_t *ctx, uint32_t *out_count);
int qtsq_container_get_stream(const qtsq_context_t *ctx, uint32_t index,
                                qtsq_context_t *out_ctx,
                                char *name_buf, size_t name_buf_size);

/* Phase F: Sealed surfel container write — same as
 * qtsq_surfel_container_write (declared in qtsq_surfel.h) but additionally
 * runs qtsq_seal_authorship on the surfels stream before the keystore-
 * encrypt step, embedding Shamir share #2 into the surfel centers
 * (geometry domain). Declared here because qtsq_surfel.h itself is
 * frozen; the unsealed companion stays in qtsq_surfel.h. */
int qtsq_surfel_container_write_sealed(const char *filename,
                                       const qtsq_surfel_model_t *model,
                                       const char *author_name,
                                       const char *license,
                                       uint32_t consent_flags);

/* Per-stream encryption: seal a specific stream to a different recipient */
int qtsq_container_encrypt_stream(qtsq_context_t *ctx, uint32_t stream_index,
                                   const uint8_t recipient_pk[1184]);

/* Per-stream decryption: decrypt a specific stream using own secret key */
int qtsq_container_decrypt_stream(qtsq_context_t *ctx, uint32_t stream_index,
                                   const uint8_t my_sk[2400]);

#endif /* QTSQ_H */
