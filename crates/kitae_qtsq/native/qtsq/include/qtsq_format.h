/*
 * Quantum Tensor Sequence — File Format Structures & Constants
 * File Extension: .qtsq (Quantum Tensor Sequence)
 * 
 * Author: Haruhito
 * License: MIT
 */

#ifndef QTSQ_FORMAT_H
#define QTSQ_FORMAT_H

#include <stdint.h>
#include <stddef.h>
/* Magic bytes: "QTS\x01" */
#define QTSQ_MAGIC_0 'Q'
#define QTSQ_MAGIC_1 'T'
#define QTSQ_MAGIC_2 'S'
#define QTSQ_MAGIC_3 '\x01'
#define QTSQ_VERSION_1  1
#define QTSQ_VERSION_2  2
#define QTSQ_VERSION_3  3
#define QTSQ_VERSION    3     /* current version */

/* gravitational wave diff (.qtsw): "QTW\x01" */
#define QTW_MAGIC_0 'Q'
#define QTW_MAGIC_1 'T'
#define QTW_MAGIC_2 'W'
#define QTW_MAGIC_3 '\x01'

/* Chandrasekhara limit*/
#define QTSQ_CHANDRASEKHAR_LIMIT 4096

/* Compression Strategy*/
typedef enum {
    QTSQ_STRATEGY_NONE        = 0x00,
    QTSQ_STRATEGY_PROCEDURAL  = 0x01,
    QTSQ_STRATEGY_FRACTAL     = 0x02,
    QTSQ_STRATEGY_FOURIER     = 0x03,
    QTSQ_STRATEGY_DELTA_SEED  = 0x04,
    QTSQ_STRATEGY_DICTIONARY  = 0x05,
    QTSQ_STRATEGY_TEMPORAL    = 0x06,
    QTSQ_STRATEGY_TRIANGLE    = 0x07,
    QTSQ_STRATEGY_PHOTON      = 0x08,  /* HQ Gabor atom audio (Photon Audio) */
    QTSQ_STRATEGY_SPLAT       = 0x09,  /* Pure 2D Gaussian Splat image (no triangles) */
    QTSQ_STRATEGY_TENSOR      = 0x0A,  /* Float delta-quantize-zlib for neural tensors */
    QTSQ_STRATEGY_PIXEL       = 0x0B,  /* Exact pixel encoding with delta filter + zlib */
    QTSQ_STRATEGY_TSUBU       = 0x0C,  /* World-space 4D Gaussian atom (Tsubu Engine) */
    QTSQ_STRATEGY_4DGS        = 0x0D,  /* 4D Gaussian Splatting (3DGS + 4DGS renderer) */
    QTSQ_STRATEGY_SURFEL      = 0x0E,  /* Captured 2D Gaussian surfels (on-device 2DGS) */
    QTSQ_STRATEGY_OPUS        = 0x0F,  /* Opus audio (Creative default, lossy) */
    QTSQ_STRATEGY_SINGULARITY = 0x10,  /* Generic bytes via qtc (Phase 6a, opt-in) */
    QTSQ_STRATEGY_LAYERED     = 0xFF,
} qtsq_strategy_t;

/* data type*/
typedef enum {
    QTSQ_TYPE_RAW          = 0x00,
    QTSQ_TYPE_IMAGE_GRAY   = 0x10,
    QTSQ_TYPE_IMAGE_RGB    = 0x11,
    QTSQ_TYPE_IMAGE_RGBA   = 0x12,
    QTSQ_TYPE_AUDIO_MONO   = 0x20,
    QTSQ_TYPE_AUDIO_STEREO = 0x21,
    QTSQ_TYPE_AUDIO_MULTI  = 0x22,
    QTSQ_TYPE_VIDEO_FRAMES = 0x30,
    QTSQ_TYPE_TEXT_ASCII   = 0x40,
    QTSQ_TYPE_TEXT_UTF8    = 0x41,
    QTSQ_TYPE_TABLE_INT    = 0x50,
    QTSQ_TYPE_TABLE_FLOAT  = 0x51,
    QTSQ_TYPE_JSON         = 0x52,
    QTSQ_TYPE_CSV          = 0x53,
    QTSQ_TYPE_SIGNAL_1D    = 0x60,
    QTSQ_TYPE_SIGNAL_2D    = 0x61,
    QTSQ_TYPE_TENSOR       = 0x70,  /* Neural tensor: weights, biases, embeddings */
    QTSQ_TYPE_ATOM         = 0x80,  /* Tsubu atom payload (world-space 4D Gaussian) */
    QTSQ_TYPE_4DGS         = 0x81,  /* 4D Gaussian Splatting model */
    QTSQ_TYPE_SURFEL       = 0x82,  /* Captured 2D Gaussian surfel set */
    QTSQ_TYPE_MESH         = 0x83,  /* TSDF-extracted triangle mesh (Phase F) */
    QTSQ_TYPE_CONTAINER    = 0xF0,
} qtsq_data_type_t;

/* Flags */
#define QTSQ_FLAG_LAZY_DECOMP   0x0001
#define QTSQ_FLAG_LOSSY         0x0002
#define QTSQ_FLAG_CHECKSUMMED   0x0004
#define QTSQ_FLAG_SEEKABLE      0x0008
#define QTSQ_FLAG_ENCRYPTED     0x0010
#define QTSQ_FLAG_MULTI_STREAM  0x0020
#define QTSQ_FLAG_AUTO_DETECTED 0x0040
#define QTSQ_FLAG_HAS_WORMHOLES 0x0080
#define QTSQ_FLAG_HAS_ECC       0x0100
#define QTSQ_FLAG_REDSHIFTABLE  0x0200
#define QTSQ_FLAG_HD            0x0400  /* High-quality triangle mesh (QTSQ HD) */
#define QTSQ_FLAG_3D            0x0800  /* 3D Depth encoded (Parallax/Volumetric) */
#define QTSQ_FLAG_UPSCALE       0x1000  /* Upscale mathematical enhancement for lossy frames */
#define QTSQ_FLAG_SPATIAL       0x2000  /* Spatial audio via atoms or object positioning */
#define QTSQ_FLAG_ZLIB          0x4000  /* Singularity payload is compressed
                                         * (zlib, or qtc since Phase 6a — the
                                         * decoder sniffs the stream magic) */
#define QTSQ_FLAG_SPLAT_ONLY    0x8000  /* Pure 2D Gaussian Splat image (no triangles) */

/* V3 Security Flags (uses uint32_t flags in V3 header) */
#define QTSQ_FLAG_PQ_ENCRYPTED  0x10000 /* Post-quantum ML-KEM encrypted */
#define QTSQ_FLAG_PQ_SIGNED     0x20000 /* Post-quantum ML-DSA signed */
#define QTSQ_FLAG_ANTI_STEGO    0x40000 /* Anti-steganography LSB randomization applied */
#define QTSQ_FLAG_ZERO_META     0x80000 /* Zero-metadata guarantee */

/* Novel Geometric Security Flags */
#define QTSQ_FLAG_GEOCRYPT      0x100000 /* Geometric-level encryption (positions/colors/permutation) */
#define QTSQ_FLAG_SECRET_SHARE  0x200000 /* Shamir visual secret share — one of N shares */
#define QTSQ_FLAG_REGION_CRYPT  0x400000 /* Per-region selective encryption */
#define QTSQ_FLAG_FINGERPRINTED 0x800000 /* Forensic fingerprint embedded for leak tracing */
#define QTSQ_FLAG_DENIABLE      0x1000000 /* Deniable dual-image encryption */

/* Tsubu Engine extension flag */
#define QTSQ_FLAG_TSUBU_ATOMS   0x2000000 /* This stream carries Tsubu 4D atoms */

/* Phase 1: Local-only guarantee flag */
#define QTSQ_FLAG_LOCAL_ONLY    0x4000000 /* No network code paths used */

/* Phase 3: Two-Intent flags */
#define QTSQ_FLAG_CREATIVE      0x8000000  /* Creative intent: re-synthesis allowed */
#define QTSQ_FLAG_PROVENANCE    0x10000000 /* Provenance intent: bit-exact, no re-synthesis */

/* Phase 7: Per-stream encryption flag */
#define QTSQ_FLAG_STREAM_ENCRYPTED 0x20000000 /* Container has per-stream encryption */

/* Phase 9: Consent flags */
#define QTSQ_FLAG_DO_NOT_TRAIN  0x40000000 /* Do not use for AI training */
#define QTSQ_FLAG_NO_AI         0x80000000 /* No AI processing of any kind */

/* Intent enum — determines what operations are allowed */
typedef enum {
    QTSQ_INTENT_NONE       = 0,  /* Not set (legacy/compat) */
    QTSQ_INTENT_CREATIVE   = 1,  /* Re-synthesis allowed: triangle, gaussian, 3DGS */
    QTSQ_INTENT_PROVENANCE = 2,  /* Bit-exact: pixel-only, no re-synthesis */
} qtsq_intent_t;

/* Codec preference (Phase 6a; default flipped in Phase 7) — selects the
 * entropy coder below all privacy layers. qtsq_init() sets qtc as the
 * default; zlib remains as an explicit opt-out and as the universal read
 * path. qtc streams are self-describing ('SGC\x01' magic), so decoders
 * dispatch on the payload, never on this field — it only affects encoding,
 * and every file written by an older (zlib) version still decompresses. */
#define QTSQ_CODEC_ZLIB 0   /* legacy zlib — explicit opt-out after Phase 7 */
#define QTSQ_CODEC_QTC  1   /* default: qtc (compressor/), smaller captures */

/* Sanitize report bitfield — tracks what privacy actions were applied */
#define QTSQ_SANITIZE_IMAGE_LSB   0x01 /* Image LSB stego destroyed */
#define QTSQ_SANITIZE_AUDIO_LSB   0x02 /* Audio LSB stego destroyed */
#define QTSQ_SANITIZE_TEXT_NORM   0x04 /* Text BOM/whitespace normalized */
#define QTSQ_SANITIZE_JSON_MINIFY 0x08 /* JSON structural whitespace removed */
#define QTSQ_SANITIZE_NAMES_HIDE  0x10 /* Names moved inside encryption envelope */
#define QTSQ_SANITIZE_ENCRYPTED   0x20 /* Singularity encrypted */
#define QTSQ_SANITIZE_ZERO_META   0x40 /* Zero metadata confirmed */
#define QTSQ_SANITIZE_SIZE_PADDED 0x80 /* Size-padded to fixed bucket */

/* error codes */
#define QTSQ_OK                  0
#define QTSQ_ERR_NULL           -1
#define QTSQ_ERR_ALLOC          -2
#define QTSQ_ERR_IO             -3
#define QTSQ_ERR_FORMAT         -4
#define QTSQ_ERR_CHECKSUM       -5
#define QTSQ_ERR_STRATEGY       -6
#define QTSQ_ERR_TYPE           -7
#define QTSQ_ERR_ENCRYPTED      -8
#define QTSQ_ERR_DECRYPT        -9
#define QTSQ_ERR_WORMHOLE       -10
#define QTSQ_ERR_WORMHOLE_HASH  -11
#define QTSQ_ERR_CORRUPT        -12
#define QTSQ_ERR_RECOVERY_FAIL  -13
#define QTSQ_ERR_TOO_SMALL      -14
#define QTSQ_ERR_NOT_ENCRYPTED  -15  /* Phase 1: qtsq_write refuses plaintext (.qtsq must be encrypted) */
#define QTSQ_ERR_NOT_SANITIZED  -16  /* Phase 1: qtsq_write refuses unsanitized context */
#define QTSQ_ERR_TOO_LARGE      -17  /* SEC: input or file-declared size exceeds safety cap */

/* file header v2 — 96 bytes packed
 * v1 was 80 bytes with uint32_t sizes (4 GB limit)
 * v2 uses uint64_t sizes (18.4 EB limit) */
typedef struct {
    uint8_t      magic[4];
    uint8_t      version;
    uint8_t      strategy;
    uint16_t     flags;
    uint8_t      data_type;
    uint8_t      num_channels;
    uint16_t     sample_depth;
    uint32_t     reserved;
    uint64_t     original_size;
    uint64_t     timestamp;
    uint8_t      checksum[32];
    uint64_t     horizon_size;       /* v2: was uint32_t */
    uint64_t     accretion_size;     /* v2: was uint32_t */
    uint64_t     singularity_size;   /* v2: was uint32_t */
    uint64_t     radiation_size;     /* v2: was uint32_t */
} __attribute__((packed)) qtsq_header_v2_t;

_Static_assert(sizeof(qtsq_header_v2_t) == 96, "v2 header must be 96 bytes");

/* v1 header for backward compatibility (reading old files) */
typedef struct {
    uint8_t      magic[4];
    uint8_t      version;
    uint8_t      strategy;
    uint16_t     flags;
    uint8_t      data_type;
    uint8_t      num_channels;
    uint16_t     sample_depth;
    uint32_t     reserved;
    uint64_t     original_size;
    uint64_t     timestamp;
    uint8_t      checksum[32];
    uint32_t     horizon_size;
    uint32_t     accretion_size;
    uint32_t     singularity_size;
    uint32_t     radiation_size;
} __attribute__((packed)) qtsq_header_v1_t;

_Static_assert(sizeof(qtsq_header_v1_t) == 80, "v1 header must be 80 bytes");

/* v3 header — 112 bytes packed
 * Expands flags to uint32_t, adds security block size */
typedef struct {
    uint8_t      magic[4];
    uint8_t      version;
    uint8_t      strategy;
    uint32_t     flags;              /* 32 bits of flags */
    uint8_t      data_type;
    uint8_t      num_channels;
    uint16_t     sample_depth;
    uint64_t     original_size;
    uint64_t     timestamp;
    uint8_t      checksum[32];
    uint64_t     horizon_size;
    uint64_t     accretion_size;
    uint64_t     singularity_size;
    uint64_t     radiation_size;
    uint64_t     security_block_size; /* Size of the V3 security block */
    uint8_t      intent;             /* qtsq_intent_t: CREATIVE or PROVENANCE */
    uint8_t      reserved_byte;      /* Alignment */
    uint16_t     reserved16;         /* Alignment */
    uint32_t     reserved32;         /* General purpose */
    uint8_t      padding[2];         /* Pad to 112 bytes */
} __attribute__((packed)) qtsq_header_t;

_Static_assert(sizeof(qtsq_header_t) == 112, "v3 header must be 112 bytes");

/* schema */
typedef struct {
    char         name[64];
    uint8_t      data_type;
    uint32_t     dimensions[8];
    uint8_t      num_dims;
    uint32_t     sample_rate;
    uint32_t     frame_rate;
    char         encoding[64];
} qtsq_schema_t;

/* transform */
typedef struct {
    uint8_t      type;
    uint32_t     param_size;
    void        *params;
} qtsq_transform_t;

/* Photon Audio Spatial Atom */
typedef struct {
    float freq_hz;
    float magnitude;
    float phase;
    float x;        /* Spatial X (pan): -1 (left) to 1 (right) */
    float y;        /* Spatial Y (height): -1 (low) to 1 (high) */
    float z;        /* Spatial Z (depth): 0 (far) to 1 (near) */
    float spread;   /* Spatial Spread (width): 0 (point) to 1 (diffuse) */
    uint32_t frame_index;
} photon_atom_t;

/* transform */
typedef struct {
    uint64_t     logical_offset;
    uint32_t     chunk_size;
    uint32_t     singularity_offset;
    uint8_t      priority;
} __attribute__((packed)) qtsq_index_t;

/* wormhole — local only (LAN/Cloud removed for local-only privacy guarantee) */
#define QTSQ_WORMHOLE_LOCAL      0x01
/* #define QTSQ_WORMHOLE_LAN     0x02  — REMOVED: breaks local-only guarantee */
/* #define QTSQ_WORMHOLE_CLOUD   0x03  — REMOVED: breaks local-only guarantee */
#define QTSQ_WORMHOLE_MARKER     0xEE

typedef struct {
    uint8_t      type;
    uint8_t      target_hash[32];
    char         target_path[256];
    uint32_t     target_offset;
    uint32_t     target_size;
} qtsq_wormhole_t;

/* censorship (legacy v1/v2) */
typedef struct {
    uint8_t       iv[16];
    uint8_t       salt[16];
    uint8_t       auth_tag[16];
    uint32_t      iterations;
} qtsq_censorship_t;

/* V3 Security Metadata Block */
typedef struct {
    uint8_t  security_version;       /* 1 = initial PQ release */
    uint8_t  encryption_type;        /* 0=none, 1=password, 2=PQ-KEM, 3=deniable */
    uint8_t  signature_type;         /* 0=none, 1=ML-DSA-65 */
    uint8_t  geocrypt_mode;          /* QTSQ_GEOCRYPT_* flags applied during compression */
    
    /* AES-256-GCM parameters */
    uint8_t  nonce[12];              /* GCM nonce (CSPRNG) */
    uint8_t  auth_tag[16];           /* GCM authentication tag */
    uint8_t  salt[32];               /* KDF salt (CSPRNG) */
    uint32_t kdf_iterations;         /* HKDF iterations (password mode) */
    
    /* ML-KEM-768 encapsulated key (present when encryption_type >= 2) */
    uint16_t kem_ct_size;            /* 1088 for ML-KEM-768 */
    uint8_t  kem_ciphertext[1088];   /* Encapsulated shared secret */
    
    /* ML-DSA-65 signature (present when signature_type >= 1) */
    uint16_t sig_size;               /* 3293 for ML-DSA-65 */
    uint8_t  signature[3309];        /* ML-DSA-65 signature (padded to 16-byte boundary) */
    
    /* Signer public key fingerprint (SHA3-256 of sign_pk) */
    uint8_t  signer_fingerprint[32];
} __attribute__((packed)) qtsq_security_block_t;

/* recovery */
typedef struct {
    uint32_t       num_chunks;
    uint32_t       *chunk_checksums;
    uint8_t        *reed_solomon_parity;
    uint32_t       parity_size;
} qtsq_recovery_t;

/* wave header (.qtsw) */
typedef struct {
    uint8_t        magic[4];
    uint8_t        source_hash[32];
    uint8_t        result_hash[32];
    uint64_t       timestamp;
    uint32_t       num_patches;
} __attribute__((packed)) qtsq_wave_header_t;

typedef struct {
    uint32_t       offset;
    uint32_t       old_size;
    uint32_t       new_size;
    uint8_t       *new_data;
} qtsq_patch_t;

/* main context */
typedef struct {
    qtsq_header_t       header;
    qtsq_schema_t       schema;
    qtsq_transform_t   *transforms;
    uint32_t           num_transforms;
    uint8_t           *singularity;
    uint64_t           singularity_size;
    qtsq_index_t       *radiation_index;
    uint32_t           num_radiation;
    qtsq_wormhole_t    *wormholes;
    uint32_t           num_wormholes;
    qtsq_censorship_t   censorship;       /* Legacy v1/v2 */
    qtsq_security_block_t security;       /* V3 security block */
    int                has_security;      /* Has V3 security block */
    int                is_encrypted;      /* Flag for active encryption */
    qtsq_recovery_t     recovery;
    int                has_recovery;
    /* Geometric cryptography (in-memory only — key derived from vault password) */
    uint8_t            geocrypt_key[32];  /* Derived subkey for geocrypt operations */
    uint8_t            geocrypt_mode;     /* QTSQ_GEOCRYPT_* flags (mirrors security.geocrypt_mode) */
    int                has_geocrypt;      /* 1 if geocrypt should be applied */
    int                is_sanitized;      /* Privacy guarantees applied (anti-stego, zero-meta) */
    uint32_t           sanitize_report;   /* Bitfield of QTSQ_SANITIZE_* actions applied */
    qtsq_intent_t      intent;            /* Creative or Provenance intent */
    uint8_t           *original_input;    /* Provenance: hash of original input bytes (for signature) */
    size_t             original_input_size;
    uint8_t            codec;             /* Phase 6a: QTSQ_CODEC_ZLIB (default) or QTSQ_CODEC_QTC */
    float              quality;           /* Phase 6b: 0 = lossless; >0 = lossy qtc (0..1). Only
                                           * meaningful when codec == QTSQ_CODEC_QTC. */
} qtsq_context_t;

/* ── Container (Multi-Stream) ── */
#define QTSQ_MAX_STREAMS 65536

/* v2 stream entry — no size limit per stream */
typedef struct {
    uint8_t   data_type;       /* QTSQ_TYPE_* for this stream */
    uint8_t   strategy;        /* QTSQ_STRATEGY_* used to compress */
    uint16_t  flags;           /* per-stream flags (ZLIB, LOSSY, etc.) */
    uint32_t  reserved;        /* alignment padding */
    uint64_t  data_offset;     /* offset from start of stream data block */
    uint64_t  data_size;       /* compressed size in bytes */
    uint64_t  original_size;   /* original uncompressed size */
} __attribute__((packed)) qtsq_stream_entry_t;

_Static_assert(sizeof(qtsq_stream_entry_t) == 32, "stream entry must be 32 bytes");

/* Per-stream flags (stored in uint16_t stream_entry.flags) */
#define QTSQ_STREAM_FLAG_ENCRYPTED 0x0001 /* Stream is per-stream encrypted */

/* Per-stream encryption metadata — prepended to encrypted stream data */
typedef struct {
    uint8_t  nonce[12];            /* AES-256-CTR nonce */
    uint8_t  salt[32];             /* KDF salt */
    uint8_t  auth_tag[16];         /* HMAC-SHA3-256 truncated tag */
    uint16_t kem_ct_size;          /* ML-KEM ciphertext size (0 if password-only) */
    uint8_t  encryption_type;      /* 0=none, 1=password, 2=PQ-KEM */
    uint8_t  reserved;             /* alignment */
    uint8_t  kem_ciphertext[1088]; /* ML-KEM-768 ciphertext */
} __attribute__((packed)) qtsq_stream_encryption_t;

/* ════════════════════════════════════════════════════════════════
 *  4D Atom (Tsubu) — World-space Gaussian splat with full physics
 *
 *  Same primitive drives simulation AND rendering. Persistent
 *  world-space particle, NOT a pixel in an image.
 *
 *  Gaussian fields:
 *    Spatial mean:  μ_xyz  = (x, y, z)
 *    Temporal mean: μ_t    = t_center  (seconds)
 *    Spatial cov:   Σ_xyz  = R(scale) · R(scale)^T  (rotated 3D scale)
 *    Temporal cov:  Σ_t    = t_scale^2
 *    Coupling:      ω      = coupling  (3D vector; drives motion)
 *
 *  Conditional slice at time t:
 *    μ₃(t)  = μ_xyz + ω · (t − μ_t)
 *    Σ₃(t)  = Σ_xyz − (ω · ω^T) · σ_t²
 *    α(t)   = α · exp(−½ ((t − μ_t) / σ_t)²)
 * ════════════════════════════════════════════════════════════════ */
typedef struct {
    /* ── Spatial (3D) ── */
    float    x, y, z;             /* 12  — position (μ_xyz)                    */
    float    vx, vy, vz;          /* 24  — velocity (m/s)                      */
    float    mass;                /* 28  — kg                                  */
    float    radius;              /* 32  — visual splat radius (m)             */
    float    temperature;         /* 36  — Kelvin                              */
    uint16_t material_id;         /* 38  — index into material table           */
    uint16_t state;               /* 40  — solid/liquid/gas/plasma enum        */
    uint32_t bond_offset;         /* 44  — index into bond buffer              */
    uint8_t  bond_count;          /* 45  — number of bonds                     */
    uint8_t  flags;               /* 46  — bitflags: static/sleeping/...       */
    uint8_t  shape;               /* 47  — splat shape (legacy)                */
    uint8_t  _pad0;               /* 48  — alignment                           */

    /* ── Anisotropic 3D Gaussian shape ── */
    float    sx, sy, sz;          /* 60  — 3D scale (per-axis std-dev)         */
    float    qx, qy, qz, qw;      /* 76  — orientation quaternion (x,y,z,w)    */
    float    nx, ny, nz;          /* 88  — surface normal (auxiliary)          */

    /* ── 4D (space + time) Gaussian fields ── */
    float    t_center;            /* 92  — temporal mean μ_t (seconds)         */
    float    t_scale;             /* 96  — temporal std-dev σ_t (0 = static)   */
    float    wx, wy, wz;          /* 108 — space-time coupling ω               */
} __attribute__((packed)) splat_tsubu_t;

_Static_assert(sizeof(splat_tsubu_t) == 108, "splat_tsubu_t must be 108 bytes");

/* Tsubu atom state enum */
#define TSUBU_STATE_SOLID    0
#define TSUBU_STATE_LIQUID   1
#define TSUBU_STATE_GAS      2
#define TSUBU_STATE_PLASMA   3

/* Tsubu atom flags */
#define TSUBU_FLAG_STATIC    0x01
#define TSUBU_FLAG_SLEEPING  0x02
#define TSUBU_FLAG_KINEMATIC 0x04
#define TSUBU_FLAG_GHOST     0x08

/* Primitive type ID for Tsubu atoms */
#define QTSQ_PRIMITIVE_TSUBU 0x80

#endif