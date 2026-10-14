/*
 * Quantum Tensor Sequence — Forensic Fingerprinting
 *
 * Embeds a unique, invisible fingerprint into the geometric parameters
 * (splat positions and colors) of each distributed copy. The perturbations
 * are sub-pixel and visually imperceptible (PSNR > 55 dB).
 *
 * Novel advantage over pixel-domain watermarking:
 *   - Traditional watermarks (DWT, DCT) break under screenshot, crop, resize
 *   - QTSQ fingerprints live in geometric primitive space — they survive
 *     any raster-domain transformation because the geometry is re-synthesized
 *   - An attacker would need to reverse-engineer the splat decomposition to
 *     remove the fingerprint, which requires knowing the encoding parameters
 *
 * Author: Haruhito
 */

#ifndef QTSQ_FINGERPRINT_H
#define QTSQ_FINGERPRINT_H

#include <stdint.h>
#include <stddef.h>
#include "qtsq.h"
#include "qtsq_splat2d.h"

/**
 * qtsq_fingerprint_embed — Embed a unique fingerprint for a specific recipient.
 *
 * Perturbs each splat's (x,y) position by ±1 pixel and (r,g,b) by ±1 value
 * using a keyed PRNG seeded from (master_key || recipient_id || splat_index).
 * The perturbation is imperceptible but uniquely identifies the recipient.
 *
 * Must be called AFTER splat fitting/refinement, BEFORE geocrypt and packing.
 *
 * @param splats        Splat array (modified in-place)
 * @param count         Number of splats
 * @param recipient_id  32-byte unique identifier for this recipient
 * @param master_key    32-byte secret key known only to the distributor
 * @param ctx           Context to update flags
 * @return              QTSQ_OK on success
 */
int qtsq_fingerprint_embed(splat2d_t *splats, uint32_t count,
                            const uint8_t recipient_id[32],
                            const uint8_t master_key[32],
                            qtsq_context_t *ctx);

/**
 * qtsq_fingerprint_extract — Identify which recipient leaked an image.
 *
 * Renders the suspect image to pixels, renders the original to pixels,
 * computes per-splat residuals, and correlates against each candidate's
 * expected PRNG sequence. Returns the index of the best-matching candidate.
 *
 * @param original_splats   Unfingerprinted original splat array
 * @param count             Number of splats
 * @param width             Image width
 * @param height            Image height
 * @param suspect_pixels    Suspect image (decoded to raw RGB pixels)
 * @param channels          Number of channels (typically 3)
 * @param master_key        32-byte distributor master key
 * @param candidate_ids     Array of candidate recipient IDs to test
 * @param num_candidates    Number of candidates
 * @param match_index       Output: index of best-matching candidate (-1 if none)
 * @param match_score       Output: correlation score (0.0–1.0, >0.7 = confident)
 * @return                  QTSQ_OK on success
 */
int qtsq_fingerprint_extract(const splat2d_t *original_splats, uint32_t count,
                              uint32_t width, uint32_t height,
                              const uint8_t *suspect_pixels, uint8_t channels,
                              const uint8_t master_key[32],
                              const uint8_t candidate_ids[][32],
                              uint32_t num_candidates,
                              int *match_index, float *match_score);

#endif /* QTSQ_FINGERPRINT_H */
