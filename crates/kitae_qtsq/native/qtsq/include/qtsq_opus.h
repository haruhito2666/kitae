/*
 * Quantum Tensor Sequence — Opus Audio Codec (Phase 9)
 *
 * A Creative-intent, lossy audio codec backed by the standard Opus codec
 * (RFC 6716).  Opus is open, royalty-free, and beats MP3 — and is the
 * PRODUCT.md default for the "Simple" Creative audio path.
 *
 * This module is CONDITIONALLY compiled: it only exists when the system
 * provides libopus (detected at build time via pkg-config, see Makefile).
 * When libopus is absent the library still builds — qtsq_compress_audio
 * simply falls back to the in-house spectral/Fourier encoder and the OPUS
 * strategy never appears in output.  Guard consumers of these symbols with
 * #ifdef QTSQ_HAVE_OPUS so the library stays portable.
 *
 * Author: Haruhito
 */
#ifndef QTSQ_OPUS_H
#define QTSQ_OPUS_H

#include <stddef.h>
#include <stdint.h>
#include "qtsq.h"

#ifdef __cplusplus
extern "C" {
#endif

/*
 * qtsq_compress_audio_opus — Opus-encode interleaved int16 PCM into the
 * context singularity (strategy = QTSQ_STRATEGY_OPUS).
 *
 *   samples       — interleaved int16 PCM (in = ch0, ch1, ch0, ch1, ...)
 *   num_samples   — total interleaved sample frames (== frames * channels)
 *   sample_rate   — input sample rate in Hz (mapped to nearest Opus rate)
 *   channels      — 1 (mono) or 2 (stereo); more than 2 is down-mixed
 *
 * Returns QTSQ_OK or a negative QTSQ_ERR_* code.
 */
int qtsq_compress_audio_opus(qtsq_context_t *ctx,
                             const int16_t *samples,
                             size_t num_samples,
                             uint32_t sample_rate,
                             uint8_t channels);

/*
 * qtsq_decompress_audio_opus — decode an OPUS-strategy singularity back to
 * interleaved int16 PCM.  Allocates *out_pcm (caller frees).
 */
int qtsq_decompress_audio_opus(const qtsq_context_t *ctx,
                                int16_t **out_pcm,
                                size_t *out_num_samples,
                                uint32_t *out_sample_rate,
                                uint8_t *out_channels);

#ifdef __cplusplus
}
#endif

#endif /* QTSQ_OPUS_H */
