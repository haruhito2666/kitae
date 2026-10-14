/*
 * Quantum Tensor Sequence — Turbo I/O Engine
 * 7-Layer High-Performance I/O for .qtsq files
 *
 * Layer 1: Memory-Mapped I/O (mmap)
 * Layer 2: Chunked/Blocked Format
 * Layer 3: SIMD Vectorization
 * Layer 4: Multi-threaded Compression
 * Layer 5: Zero-Copy Encryption
 * Layer 6: Async I/O (overlap compress + write)
 * Layer 7: Adaptive Buffer Sizes
 *
 * Author: Haruhito
 */

#ifndef QTSQ_TURBO_H
#define QTSQ_TURBO_H

#include "qtsq_format.h"
#include <stdint.h>
#include <stddef.h>

/* ═══════════════════════════════════════════════
 *  Platform Detection
 * ═══════════════════════════════════════════════ */

/* SIMD capabilities */
#if defined(__AVX2__)
  #define QTSQ_SIMD_AVX2   1
  #define QTSQ_SIMD_WIDTH  32
#elif defined(__SSE2__) || defined(_M_X64)
  #define QTSQ_SIMD_SSE2   1
  #define QTSQ_SIMD_WIDTH  16
#elif defined(__ARM_NEON) || defined(__aarch64__)
  #define QTSQ_SIMD_NEON   1
  #define QTSQ_SIMD_WIDTH  16
#else
  #define QTSQ_SIMD_NONE   1
  #define QTSQ_SIMD_WIDTH  1
#endif

/* Threading */
#if defined(_WIN32)
  #define QTSQ_THREADS_WIN  1
  /* windows/shim/pthread.h is a Win32-backed pthread implementation
     (CreateThread / CRITICAL_SECTION / CONDITION_VARIABLE), so the POSIX
     thread-pool code paths run on Windows too — feature parity. */
  #define QTSQ_THREADS_POSIX 1
#else
  #define QTSQ_THREADS_POSIX 1
#endif

/* Memory mapping */
#if defined(_WIN32)
  #define QTSQ_MMAP_WIN 1
#else
  #define QTSQ_MMAP_POSIX 1
#endif

/* ═══════════════════════════════════════════════
 *  Constants
 * ═══════════════════════════════════════════════ */

#define QTSQ_BLOCK_SIZE_DEFAULT  (64 * 1024)   /* 64 KB default block */
#define QTSQ_BLOCK_SIZE_MIN      (4  * 1024)   /* 4 KB minimum */
#define QTSQ_BLOCK_SIZE_MAX      (1  * 1024 * 1024) /* 1 MB maximum */
#define QTSQ_MAX_THREADS         16
#define QTSQ_TURBO_MAGIC         0x54425251    /* "QRBT" — turbo marker */

/* File size tiers for adaptive buffering */
#define QTSQ_TIER_SMALL   (1   * 1024 * 1024)  /* < 1 MB */
#define QTSQ_TIER_MEDIUM  (100 * 1024 * 1024)  /* 1-100 MB */
#define QTSQ_TIER_LARGE   (1024ULL * 1024 * 1024) /* 100 MB - 1 GB */
/* > 1 GB = HUGE */

/* ═══════════════════════════════════════════════
 *  Layer 2: Block Index
 * ═══════════════════════════════════════════════ */

/* Each block's metadata in the file index */
typedef struct {
    uint32_t    original_size;       /* uncompressed size of this block */
    uint32_t    compressed_size;     /* compressed size in file */
    uint64_t    file_offset;         /* byte offset within the file */
    uint32_t    checksum;            /* CRC32 of uncompressed data */
    uint8_t     strategy;            /* compression strategy used */
    uint8_t     flags;               /* per-block flags (encrypted, etc.) */
    uint8_t     reserved[2];
} __attribute__((packed)) qtsq_block_entry_t;

_Static_assert(sizeof(qtsq_block_entry_t) == 24, "block entry must be 24 bytes");

/* Block index header (written after the main header) */
typedef struct {
    uint32_t    magic;               /* QTSQ_TURBO_MAGIC */
    uint32_t    block_size;          /* size of each block (before compression) */
    uint32_t    num_blocks;          /* total number of blocks */
    uint32_t    index_checksum;      /* CRC32 of the index itself */
} __attribute__((packed)) qtsq_block_index_header_t;

_Static_assert(sizeof(qtsq_block_index_header_t) == 16, "block index header must be 16 bytes");

/* ═══════════════════════════════════════════════
 *  Layer 1: Memory-Mapped File Handle
 * ═══════════════════════════════════════════════ */

typedef struct {
    void       *data;          /* Pointer to mapped data */
    size_t      size;          /* Total file size */
    int         fd;            /* File descriptor (POSIX) */
    int         is_write;      /* 1 = writable mapping */
#ifdef QTSQ_MMAP_WIN
    void       *map_handle;    /* Windows mapping handle */
#endif
} qtsq_mmap_t;

/* ═══════════════════════════════════════════════
 *  Layer 4: Thread Pool
 * ═══════════════════════════════════════════════ */

typedef struct {
    uint8_t        *input;         /* Block input data */
    size_t          input_size;    /* Input block size */
    uint8_t        *output;        /* Compressed output */
    size_t          output_size;   /* Compressed size (filled by worker) */
    uint32_t        checksum;      /* CRC32 of input (filled by worker) */
    uint8_t         strategy;      /* Strategy used */
    int             status;        /* QTSQ_OK or error */
} qtsq_block_job_t;

/* ═══════════════════════════════════════════════
 *  Layer 7: Turbo Configuration
 * ═══════════════════════════════════════════════ */

typedef struct {
    uint32_t    block_size;      /* Block size (auto-determined or user-set) */
    uint8_t     num_threads;     /* Number of worker threads */
    int         use_mmap;        /* Use mmap (auto-decided based on size) */
    int         use_simd;        /* Use SIMD if available */
    int         encrypt;         /* Encrypt blocks as they are compressed */
    const char *password;        /* Encryption password (NULL = no encrypt) */
} qtsq_turbo_config_t;

/* ═══════════════════════════════════════════════
 *  Public API
 * ═══════════════════════════════════════════════ */

/* --- Configuration --- */
void qtsq_turbo_config_default(qtsq_turbo_config_t *cfg);
void qtsq_turbo_config_auto(qtsq_turbo_config_t *cfg, size_t data_size);

/* --- Layer 1: Memory-Mapped I/O --- */
int  qtsq_mmap_open_read(qtsq_mmap_t *m, const char *filename);
int  qtsq_mmap_open_write(qtsq_mmap_t *m, const char *filename, size_t size);
void qtsq_mmap_close(qtsq_mmap_t *m);

/* --- Layer 2+4: Turbo Write (blocked + parallel) --- */
int  qtsq_turbo_write(const qtsq_context_t *ctx, const char *filename,
                      const qtsq_turbo_config_t *cfg);

/* --- Layer 2: Turbo Read --- */
int  qtsq_turbo_read(qtsq_context_t *ctx, const char *filename);

/* --- Layer 2: Random Access Block Read --- */
int  qtsq_turbo_read_block(const char *filename, uint32_t block_index,
                           uint8_t **out_data, size_t *out_size);

/* --- Layer 3: SIMD Helpers --- */
void qtsq_simd_xor(uint8_t *dst, const uint8_t *a, const uint8_t *b, size_t len);
void qtsq_simd_memset(uint8_t *dst, uint8_t val, size_t len);
uint32_t qtsq_simd_crc32(const uint8_t *data, size_t len);
size_t   qtsq_simd_rle_count(const uint8_t *data, size_t len);

/* --- Layer 5: In-place Encryption --- */
int  qtsq_turbo_encrypt_block(uint8_t *data, size_t size,
                               const uint8_t *key, const uint8_t *iv,
                               uint32_t block_counter);

/* --- Benchmarking / Info --- */
void qtsq_turbo_print_caps(void);  /* Print detected capabilities */

#endif /* QTSQ_TURBO_H */
