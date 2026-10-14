/* Public API for the Phase 4b frequency-DCT seal mark.
 * Defined in qtsq_seal_freqdct.c as a separate TU to keep seal.c focused.
 */
#ifndef QTSQ_SEAL_FREQDCT_H
#define QTSQ_SEAL_FREQDCT_H

#include <stdint.h>

/* Embed 32 bytes of `share` into `pixels` (DCT domain mid-frequency marks).
 * Returns number of bytes embedded (>=0), or negative error code. */
int qtsq_seal_embed_freq_dct(uint8_t *pixels, uint32_t w, uint32_t h,
                              uint8_t ch, const uint8_t share[32]);

/* Extract 32 bytes from a pixel buffer that was previously marked with
 * qtsq_seal_embed_freq_dct. Returns bytes recovered or negative error. */
int qtsq_seal_extract_freq_dct(const uint8_t *pixels, uint32_t w, uint32_t h,
                                uint8_t ch, uint8_t out_share[32]);

#endif /* QTSQ_SEAL_FREQDCT_H */
