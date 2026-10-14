/*
 * Quantum Tensor Sequence — Codec Shim
 * Bridges QTSQ's engines to the qtc compression library (compressor/).
 *
 * Phase 6a: integration. Phase 7: qtc is the default codec; zlib remains an
 * explicit opt-out and the read path for all existing files — qtc streams
 * are recognized on read by their magic, and zlib streams (0x78) by theirs.
 */

#ifndef QTSQ_CODEC_H
#define QTSQ_CODEC_H

#include <stddef.h>
#include <stdint.h>

/* qtc stream magic: 'S','G','C',0x01 (first 4 bytes of every qtc stream).
 * zlib streams always begin with 0x78, so the two are unambiguous. */
#define QTSQ_QTC_MAGIC_0 'S'
#define QTSQ_QTC_MAGIC_1 'G'
#define QTSQ_QTC_MAGIC_2 'C'
#define QTSQ_QTC_MAGIC_3 0x01

/* qtc container header layout (16 bytes):
 *   [4B magic][1B lens][1B flags][2B reserved][8B original_size LE] */
#define QTSQ_QTC_HEADER_SIZE 16

/* True when buf[0..len) begins with the qtc stream magic. */
int qtsq_codec_is_qtc(const uint8_t *buf, size_t len);

/* Original (uncompressed) size declared in a qtc stream header.
 * Returns 0 when len < QTSQ_QTC_HEADER_SIZE or the magic does not match.
 * Used to pre-validate before allocating, so a crafted stream cannot
 * declare a huge output. */
uint64_t qtsq_codec_declared_size(const uint8_t *buf, size_t len);

/* Compress with qtc (router picks the lens; lossless). On success sets
 * *out to a malloc'd qtc stream of *out_len bytes (caller frees).
 * Returns QTSQ_OK or a QTSQ_ERR_* code. */
int qtsq_codec_compress(const uint8_t *in, size_t in_len,
                        uint8_t **out, size_t *out_len);

/* Decompress a qtc stream with a hard output cap (DoS bound). Refuses to
 * allocate more than max_out bytes. Returns QTSQ_OK or a QTSQ_ERR_* code. */
int qtsq_codec_decompress(const uint8_t *in, size_t in_len,
                          uint64_t max_out,
                          uint8_t **out, size_t *out_len);

#endif /* QTSQ_CODEC_H */
