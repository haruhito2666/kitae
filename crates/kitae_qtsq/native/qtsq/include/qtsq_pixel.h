/*
 * Quantum Tensor Sequence — Pixel Encoding Engine
 * Primary safe-mode for exact pixel fidelity with zero metadata.
 * Author: Haruhito
 */

#ifndef QTSQ_PIXEL_H
#define QTSQ_PIXEL_H

#include "qtsq.h"

/* Magic identifier for pixel block */
#define QTSQ_PIX_MAGIC_0 'P'
#define QTSQ_PIX_MAGIC_1 'I'
#define QTSQ_PIX_MAGIC_2 'X'
#define QTSQ_PIX_MAGIC_3 '1'

/* Compression levels */
#define QTSQ_PIXEL_COMPRESS_NONE 0
#define QTSQ_PIXEL_COMPRESS_ZLIB 1
#define QTSQ_PIXEL_COMPRESS_QTC  2   /* Phase 6a: qtc image lens (opt-in) */
#define QTSQ_PIXEL_COMPRESS_QTC_LOSSY 3 /* Phase 6b: qtc lossy (DCT/mesh/accretion) */
#define QTSQ_PIXEL_COMPRESS_QTC16  4 /* Phase 6b: qtc 16-bit image lens (camera raw) */

#pragma pack(push, 1)
typedef struct {
    uint8_t magic[4];
    uint32_t width;
    uint32_t height;
    uint8_t channels;
    uint8_t compression;
    uint32_t uncompressed_size;
} qtsq_pixel_header_t;
#pragma pack(pop)

/* Encode exact pixels. Returns QTSQ_OK on success.
 * With ctx->codec == QTSQ_CODEC_QTC and ctx->quality > 0 the qtc lossy
 * photo coder is used instead (QTSQ_PIXEL_COMPRESS_QTC_LOSSY) and the
 * output is no longer bit-exact. */
int qtsq_compress_pixel(qtsq_context_t *ctx,
                        const uint8_t *pixels,
                        uint32_t width, uint32_t height,
                        uint8_t channels, int compression_level);

/* Encode 16-bit/channel camera-raw pixels (Phase 6b). Requires
 * ctx->codec == QTSQ_CODEC_QTC — the only 16-bit image coder in tree.
 * Lossless by default; ctx->quality > 0 selects near-lossless mode
 * (guaranteed max error <= 8 LSB per sample). */
int qtsq_compress_pixel16(qtsq_context_t *ctx,
                          const uint16_t *pixels,
                          uint32_t width, uint32_t height,
                          uint8_t channels, int compression_level);

/* Pass-through raw image bytes (e.g. JPEG/PNG) directly for exact original file size */
int qtsq_compress_pixel_passthrough(qtsq_context_t *ctx, 
                                    const uint8_t *file_data, 
                                    size_t file_size);

/* Decode pixel payload from context singularity. 
 * Allocates out_pixels (caller must free). */
int qtsq_decompress_pixel(qtsq_context_t *ctx,
                          uint8_t **out_pixels, size_t *out_size);

/* Decode a 16-bit pixel payload (QTSQ_PIXEL_COMPRESS_QTC16).
 * Allocates out_pixels (caller must free); out_size is in bytes. */
int qtsq_decompress_pixel16(qtsq_context_t *ctx,
                            uint16_t **out_pixels, size_t *out_size);

#endif /* QTSQ_PIXEL_H */
