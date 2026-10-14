/*
 * Quantum Tensor Sequence — Ionic Data Tunnel (IDT)
 *
 * A novel I/O acceleration engine inspired by Ionic Cooling Engine physics.
 * Decomposes data into independently-decodable "ions" (micro-particles),
 * computes an information gradient field, and uses predictive cascade
 * prefetch through a threaded tunnel pipeline to achieve near-zero
 * latency reads after avalanche onset.
 *
 * Physics Mapping:
 *   Ionization        → Entropy-adaptive decomposition (256–4096 B)
 *   Electric field     → Information gradient Φ(x) = H + S + D
 *   Tunnel            → 3-stage threaded ring-buffer pipeline
 *   Momentum transfer  → 4-hop Markov cascade prefetch (1→4→16→64→256)
 *   Avalanche          → Prefetch buffer saturation → zero-latency reads
 *
 * Author: Priyanshu Boruah / QTSQ Project
 */

#ifndef QTSQ_IONIC_H
#define QTSQ_IONIC_H

#include "qtsq_format.h"
#include <stdint.h>
#include <stddef.h>
#include <pthread.h>

/* ═══════════════════════════════════════════════════════════════
 *  Constants
 * ═══════════════════════════════════════════════════════════════ */

#define QTSQ_ION_MAGIC          0x464E4F49  /* "IONF" */
#define QTSQ_ION_SIZE_MIN       256         /* minimum particle size */
#define QTSQ_ION_SIZE_MAX       4096        /* maximum particle size */
#define QTSQ_ION_SIZE_DEFAULT   1024        /* default particle size */
#define QTSQ_CASCADE_DEPTH      4           /* prediction hops: 1→4→16→64→256 */
#define QTSQ_CASCADE_K          4           /* top-K predictions per particle */
#define QTSQ_TUNNEL_SLOTS       64          /* ring buffer slots */
#define QTSQ_FLAG_IONIC         0x00010000  /* 32-bit extended flag (stored in reserved) */

/* ═══════════════════════════════════════════════════════════════
 *  Ion Particle — fundamental data unit
 *
 *  Each ion is independently decodable. Size is entropy-adaptive:
 *    High entropy (edges/textures) → small ions (256B, many "charges")
 *    Low entropy (flat regions)    → large ions (4096B, fewer particles)
 * ═══════════════════════════════════════════════════════════════ */

typedef struct {
    uint32_t   id;              /* particle index (0..N-1) */
    uint32_t   offset;          /* byte offset within singularity payload */
    uint16_t   size;            /* particle size in bytes (256–4096) */
    uint16_t   entropy;         /* quantized Shannon entropy (0–65535) */
    uint32_t   hilbert_pos;     /* Hilbert curve position (spatial ordering) */
    uint16_t   predict[4];      /* top-4 predicted next particle IDs */
} __attribute__((packed)) qtsq_ion_t;

/* Static assert: 4+4+2+2+4+8 = 24 bytes */

/* ═══════════════════════════════════════════════════════════════
 *  Field Map — Information Potential Gradient
 *
 *  Φ(pᵢ) = w₁·H(pᵢ) + w₂·S(pᵢ) + w₃·D(pᵢ)
 *    H = Shannon entropy (information density)
 *    S = spatial centrality (Hilbert curve distance from center)
 *    D = dependency count (downstream particles that need this one)
 * ═══════════════════════════════════════════════════════════════ */

typedef struct {
    uint32_t   magic;           /* QTSQ_ION_MAGIC = "IONF" */
    uint32_t   num_ions;        /* total number of particles */
    uint16_t   base_ion_size;   /* base particle size (before adaptation) */
    uint16_t   cascade_depth;   /* prediction hops (4) */
    float      w_entropy;       /* weight for entropy term */
    float      w_spatial;       /* weight for spatial centrality */
    float      w_dependency;    /* weight for dependency count */
    uint32_t   avalanche_onset; /* estimated particle index where avalanche begins */
    uint32_t   checksum;        /* CRC32 of ion index + field map */
} __attribute__((packed)) qtsq_field_header_t;

/* ═══════════════════════════════════════════════════════════════
 *  Tunnel Pipeline — 3-stage threaded ring buffer
 *
 *  Stage 1 (Reader):   reads particles from file/mmap into ring buffer
 *  Stage 2 (Decoder):  decompresses particles in-place
 *  Stage 3 (Predictor): predicts next particles, enqueues prefetch
 *
 *  Each stage runs on a separate thread. Connected by lock-free
 *  ring buffer with atomic head/tail pointers.
 * ═══════════════════════════════════════════════════════════════ */

/* Ring buffer slot state */
typedef enum {
    ION_SLOT_EMPTY    = 0,      /* available for reading */
    ION_SLOT_READING  = 1,      /* stage 1: being read from disk */
    ION_SLOT_LOADED   = 2,      /* stage 1 done, waiting for decode */
    ION_SLOT_DECODING = 3,      /* stage 2: being decompressed */
    ION_SLOT_READY    = 4,      /* stage 2 done, ready to consume */
    ION_SLOT_CONSUMED = 5       /* consumer has taken the data */
} qtsq_slot_state_t;

/* Single ring buffer slot */
typedef struct {
    uint8_t           *data;          /* raw/decoded particle data */
    uint32_t           data_size;     /* current data size */
    uint32_t           ion_id;        /* which particle is in this slot */
    volatile int       state;         /* qtsq_slot_state_t (atomic) */
} qtsq_tunnel_slot_t;

/* Tunnel configuration */
typedef struct {
    uint16_t   base_particle_size;    /* base size before entropy adaptation */
    uint8_t    cascade_depth;         /* prediction hops (4) */
    uint8_t    cascade_k;             /* predictions per particle (4) */
    uint8_t    num_slots;             /* ring buffer slots (64) */
    uint8_t    num_threads;           /* pipeline threads (3) */
    float      w_entropy;             /* field weight: entropy */
    float      w_spatial;             /* field weight: spatial */
    float      w_dependency;          /* field weight: dependency */
} qtsq_ionic_config_t;

/* Tunnel statistics */
typedef struct {
    uint64_t   total_reads;           /* total particles read */
    uint64_t   cache_hits;            /* particles found in prefetch buffer */
    uint64_t   cache_misses;          /* particles not in buffer (cold reads) */
    uint64_t   cascade_predictions;   /* total predictions generated */
    uint64_t   correct_predictions;   /* predictions that were actually used */
    uint32_t   avalanche_onset;       /* particle index where avalanche started */
    double     speedup_factor;        /* measured speedup vs sequential */
    double     hit_rate;              /* cache_hits / total_reads */
} qtsq_ionic_stats_t;

/* Main tunnel state */
typedef struct {
    /* Ion index */
    qtsq_ion_t         *ions;              /* particle descriptors */
    uint32_t            num_ions;          /* total particles */
    qtsq_field_header_t field;             /* field map header */

    /* Ring buffer */
    qtsq_tunnel_slot_t *slots;             /* ring buffer slots */
    uint32_t            num_slots;
    volatile uint32_t   read_head;         /* next particle to read from disk */
    volatile uint32_t   decode_head;       /* next particle to decode */
    volatile uint32_t   consume_head;      /* next particle to give to consumer */

    /* Prefetch queue (cascade predictions) */
    uint32_t           *prefetch_queue;    /* circular queue of ion IDs to prefetch */
    uint32_t            pq_capacity;
    volatile uint32_t   pq_head;
    volatile uint32_t   pq_tail;

    /* Decoded particle cache (LRU) */
    uint8_t           **cache_data;        /* cached decoded particles */
    uint32_t           *cache_ids;         /* which ion ID in each cache slot */
    uint32_t            cache_capacity;
    uint32_t            cache_count;

    /* Source data */
    const uint8_t      *source;            /* mmap'd or loaded singularity data */
    size_t              source_size;

    /* Threads */
    pthread_t           reader_thread;
    pthread_t           decoder_thread;
    pthread_t           predictor_thread;
    volatile int        running;           /* 0 = shutdown signal */
    pthread_mutex_t     mutex;
    pthread_cond_t      cond_read;         /* wake reader when slots available */
    pthread_cond_t      cond_decode;       /* wake decoder when data loaded */
    pthread_cond_t      cond_consume;      /* wake consumer when data decoded */

    /* Stats */
    qtsq_ionic_stats_t  stats;

    /* Configuration */
    qtsq_ionic_config_t config;
} qtsq_tunnel_t;

/* ═══════════════════════════════════════════════════════════════
 *  Public API
 * ═══════════════════════════════════════════════════════════════ */

/* --- Configuration --- */
void qtsq_ionic_config_default(qtsq_ionic_config_t *cfg);

/* --- Ionization (Write Path) --- */

/*
 * Ionize and write: decomposes singularity into entropy-adaptive particles,
 * computes field gradient and Markov predictions, writes ionic format.
 *
 * This wraps qtsq_compress_gravitational_hd() then ionizes the result.
 */
int qtsq_ionic_compress_hd(qtsq_context_t *ctx,
                            const uint8_t *pixels,
                            uint32_t width, uint32_t height,
                            uint8_t channels, size_t source_size);

/*
 * Ionize an already-compressed context (post-compression, pre-write).
 * Splits ctx->singularity into particles, computes predictions,
 * embeds field map into ctx for write.
 *
 * Returns: QTSQ_OK on success
 */
int qtsq_ionize(qtsq_context_t *ctx,
                const qtsq_ionic_config_t *cfg);

/* --- Tunnel Read Path --- */

/*
 * Open an ionic tunnel for reading.
 * Loads the field map + ion index, starts pipeline threads.
 *
 * Returns: QTSQ_OK, sets up tunnel state
 */
int qtsq_tunnel_open(qtsq_tunnel_t *tunnel,
                     const uint8_t *singularity_data,
                     size_t singularity_size,
                     const qtsq_ion_t *ions,
                     uint32_t num_ions,
                     const qtsq_ionic_config_t *cfg);

/*
 * Read the next decoded particle from the tunnel.
 * If in avalanche zone, returns instantly from prefetch buffer.
 * If cold, blocks until the particle is read + decoded.
 *
 * out_data: pointer to decoded particle data (valid until next call)
 * out_size: size of decoded data
 *
 * Returns: QTSQ_OK, or QTSQ_ERR_IO at end
 */
int qtsq_tunnel_read(qtsq_tunnel_t *tunnel,
                     uint8_t **out_data, size_t *out_size);

/* Note: qtsq_tunnel_read_particle was declared here ("Checks cache first (O(1))")
 * but has no implementation in the tree. The LRU cache fields in qtsq_tunnel_t
 * (cache_data, cache_ids, cache_capacity, cache_count) are also never allocated
 * or consulted. The declaration was removed to avoid confusion. If random-access
 * particle reads with caching are needed, implement the function and the LRU. */

/*
 * Close the tunnel, stop threads, free resources.
 */
void qtsq_tunnel_close(qtsq_tunnel_t *tunnel);

/*
 * Get tunnel statistics (cache hits, avalanche onset, speedup).
 */
void qtsq_tunnel_stats(const qtsq_tunnel_t *tunnel,
                       qtsq_ionic_stats_t *stats);

/* --- Utility --- */

/*
 * Compute Shannon entropy of a data block (0.0 – 8.0 bits/byte).
 */
float qtsq_shannon_entropy(const uint8_t *data, size_t len);

/*
 * Compute 2D Hilbert curve index for spatial ordering.
 * Maps (x, y) in [0, 2^order) to a 1D index.
 */
uint32_t qtsq_hilbert_xy2d(uint32_t order, uint32_t x, uint32_t y);

/*
 * Inverse: Hilbert index to (x, y).
 */
void qtsq_hilbert_d2xy(uint32_t order, uint32_t d,
                       uint32_t *x, uint32_t *y);

/*
 * Print ionic tunnel diagnostics.
 */
void qtsq_ionic_print_stats(const qtsq_ionic_stats_t *stats);

#endif /* QTSQ_IONIC_H */
