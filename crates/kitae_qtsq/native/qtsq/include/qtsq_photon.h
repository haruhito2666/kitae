/*
 * Quantum Tensor Sequence — Photon Spatial Audio Engine
 *
 * Master header for the real-time spatial sensing pipeline.
 * Pure C types and API — no platform dependencies.
 *
 * Pipeline:
 *   Platform Layer (Core Audio / AAudio)
 *     → Sonar (FMCW chirp → echo → range)
 *     → TDOA  (GCC-PHAT multi-mic → angle)
 *     → WiFi  (RSSI fingerprint → zone)
 *     → Fusion (Kalman filter → final position)
 *     → Encoder (photon_atom_t → .qtsq)
 *
 * Author: Priyanshu Boruah / QTSQ Project
 */

#ifndef QTSQ_PHOTON_H
#define QTSQ_PHOTON_H

#include <stdint.h>
#include <stddef.h>

/* ═══════════════════════════════════════════════════════════════
 *  Constants
 * ═══════════════════════════════════════════════════════════════ */

#define PHOTON_MAX_MICS          16
#define PHOTON_MAX_SOURCES       32
#define PHOTON_MAX_RF_OBS        64
#define PHOTON_MAX_ZONES         8
#define PHOTON_KALMAN_STATE_DIM  6   /* x, y, z, vx, vy, vz */

#define PHOTON_DEFAULT_SAMPLE_RATE    48000
#define PHOTON_DEFAULT_FFT_SIZE       2048
#define PHOTON_DEFAULT_HOP_SIZE       512
#define PHOTON_DEFAULT_SONAR_LO       18000  /* Hz */
#define PHOTON_DEFAULT_SONAR_HI       22000  /* Hz */
#define PHOTON_DEFAULT_CHIRP_MS       2
#define PHOTON_DEFAULT_SONAR_INTERVAL 100    /* ms */
#define PHOTON_DEFAULT_SPEED_OF_SOUND 343.0f /* m/s at 20°C */

/* ═══════════════════════════════════════════════════════════════
 *  Core Configuration
 * ═══════════════════════════════════════════════════════════════ */

typedef struct {
    uint32_t sample_rate;            /* Audio sample rate (48000) */
    uint32_t fft_size;               /* STFT window size (2048) */
    uint32_t hop_size;               /* STFT hop (512) */
    uint32_t sonar_freq_lo;          /* Chirp start Hz (18000) */
    uint32_t sonar_freq_hi;          /* Chirp end Hz (22000) */
    uint32_t sonar_chirp_ms;         /* Chirp duration in ms (2) */
    uint32_t sonar_interval_ms;      /* Between chirps in ms (100) */
    uint8_t  num_mics;               /* Mic count on device (2–16) */
    float    mic_positions[PHOTON_MAX_MICS][3]; /* (x,y,z) in meters, device-local */
    float    speed_of_sound;         /* Default: 343.0 m/s */
} photon_config_t;

/* ═══════════════════════════════════════════════════════════════
 *  Tracked Spatial Source
 * ═══════════════════════════════════════════════════════════════ */

typedef struct {
    float    x, y, z;                /* Room-space position in meters */
    float    confidence;             /* 0.0 – 1.0 */
    float    freq_lo, freq_hi;       /* Dominant frequency band (Hz) */
    uint32_t source_id;              /* Persistent tracking ID */
    uint64_t last_seen_us;           /* Microsecond timestamp */
} photon_source_t;

/* ═══════════════════════════════════════════════════════════════
 *  Multi-Channel Audio Frame (from platform layer)
 * ═══════════════════════════════════════════════════════════════ */

typedef struct {
    float   *channels[PHOTON_MAX_MICS]; /* Per-mic PCM buffers (float32) */
    uint32_t num_channels;
    uint32_t num_samples;               /* Samples per channel */
    uint64_t timestamp_us;              /* Capture timestamp in µs */
} photon_audio_frame_t;

/* ═══════════════════════════════════════════════════════════════
 *  RF Observation (WiFi / BLE / UWB)
 * ═══════════════════════════════════════════════════════════════ */

typedef enum {
    PHOTON_RF_WIFI = 0,
    PHOTON_RF_BLE  = 1,
    PHOTON_RF_UWB  = 2
} photon_rf_type_t;

typedef struct {
    char     identifier[64];         /* SSID or BLE device UUID */
    float    rssi_dbm;               /* Received signal strength */
    uint64_t timestamp_us;
    uint8_t  type;                   /* photon_rf_type_t */
    float    uwb_distance_m;         /* UWB only: measured distance */
    float    uwb_azimuth_deg;        /* UWB only: measured angle */
} photon_rf_observation_t;

/* ═══════════════════════════════════════════════════════════════
 *  Sonar Engine State
 * ═══════════════════════════════════════════════════════════════ */

typedef struct {
    float   *chirp_ref;              /* Generated reference chirp waveform */
    uint32_t chirp_len;              /* Length in samples */
    float   *correlation_buf;        /* Scratch buffer for matched filter */
    uint32_t correlation_len;
    photon_config_t config;
} photon_sonar_state_t;

/* ═══════════════════════════════════════════════════════════════
 *  TDOA Engine State
 * ═══════════════════════════════════════════════════════════════ */

typedef struct {
    float   *fft_scratch_re;         /* FFT scratch buffers */
    float   *fft_scratch_im;
    float   *gcc_scratch;            /* GCC-PHAT output buffer */
    uint32_t fft_size;
    photon_config_t config;
} photon_tdoa_state_t;

/* ═══════════════════════════════════════════════════════════════
 *  WiFi / BLE State
 * ═══════════════════════════════════════════════════════════════ */

#define PHOTON_WIFI_MAX_APS  64
#define PHOTON_WIFI_HISTORY  16      /* RSSI history depth per AP */

typedef struct {
    char     id[64];
    float    rssi_history[PHOTON_WIFI_HISTORY];
    uint32_t history_count;
    float    rssi_avg;
    float    rssi_variance;
    uint64_t last_seen_us;
} photon_wifi_ap_t;

typedef struct {
    photon_wifi_ap_t aps[PHOTON_WIFI_MAX_APS];
    uint32_t         num_aps;
    float            zone_activity[PHOTON_MAX_ZONES];
    uint32_t         num_zones;
} photon_wifi_state_t;

/* ═══════════════════════════════════════════════════════════════
 *  Fusion Engine State (Kalman Filter)
 * ═══════════════════════════════════════════════════════════════ */

typedef struct {
    float    state[PHOTON_KALMAN_STATE_DIM];  /* [x, y, z, vx, vy, vz] */
    float    covariance[PHOTON_KALMAN_STATE_DIM * PHOTON_KALMAN_STATE_DIM];
    uint32_t source_id;
    uint32_t frames_seen;
    uint32_t frames_lost;
    float    confidence;
    float    freq_lo, freq_hi;
    uint64_t last_update_us;
    int      active;
} photon_kalman_track_t;

typedef struct {
    photon_kalman_track_t tracks[PHOTON_MAX_SOURCES];
    uint32_t              num_tracks;
    uint32_t              next_id;
    photon_config_t       config;

    /* Sensor weights for fusion */
    float w_sonar;    /* Default: 0.90 */
    float w_tdoa;     /* Default: 0.60 */
    float w_wifi;     /* Default: 0.20 */
    float w_ble;      /* Default: 0.30 */
    float w_uwb;      /* Default: 0.85 */
} photon_fusion_state_t;

/* ═══════════════════════════════════════════════════════════════
 *  API — Configuration
 * ═══════════════════════════════════════════════════════════════ */

void photon_config_default(photon_config_t *cfg);

/* ═══════════════════════════════════════════════════════════════
 *  API — Sonar Engine
 * ═══════════════════════════════════════════════════════════════ */

int  photon_sonar_init(photon_sonar_state_t *state, const photon_config_t *cfg);
void photon_sonar_generate_chirp(photon_sonar_state_t *state);

/*
 * Detect echo peaks in mic_data using matched filter against the stored chirp.
 * Returns the number of detected sources (up to max_sources).
 */
uint32_t photon_sonar_detect_echoes(photon_sonar_state_t *state,
                                     const float *mic_data, uint32_t num_samples,
                                     photon_source_t *sources, uint32_t max_sources);

/*
 * Subtract the emitted chirp from an audio recording (in-place).
 * Preserves the musical content while removing sonar contamination.
 */
void photon_sonar_subtract_chirp(const photon_sonar_state_t *state,
                                  float *audio, uint32_t num_samples);

/*
 * Get the raw chirp waveform for emission through the speaker.
 */
const float* photon_sonar_get_chirp(const photon_sonar_state_t *state,
                                     uint32_t *out_len);

void photon_sonar_free(photon_sonar_state_t *state);

/* ═══════════════════════════════════════════════════════════════
 *  API — TDOA Engine (GCC-PHAT Acoustic Localization)
 * ═══════════════════════════════════════════════════════════════ */

int  photon_tdoa_init(photon_tdoa_state_t *state, const photon_config_t *cfg);

/*
 * Compute GCC-PHAT time delay between two mic signals.
 * Returns the delay in seconds (positive = mic_b leads mic_a).
 */
float photon_tdoa_gcc_phat(photon_tdoa_state_t *state,
                            const float *mic_a, const float *mic_b,
                            uint32_t num_samples);

/*
 * Locate sound sources using multi-mic TDOA.
 * Computes GCC-PHAT for all mic pairs, clusters by direction,
 * assigns frequency bands.
 * Returns the number of located sources.
 */
uint32_t photon_tdoa_locate_sources(photon_tdoa_state_t *state,
                                     const photon_audio_frame_t *frame,
                                     photon_source_t *sources, uint32_t max_sources);

void photon_tdoa_free(photon_tdoa_state_t *state);

/* ═══════════════════════════════════════════════════════════════
 *  API — WiFi / BLE RSSI Engine
 * ═══════════════════════════════════════════════════════════════ */

int  photon_wifi_init(photon_wifi_state_t *state);

/*
 * Feed new RF observations into the tracker.
 * Updates RSSI histories, computes motion zones.
 */
void photon_wifi_update(photon_wifi_state_t *state,
                         const photon_rf_observation_t *obs, uint32_t num_obs);

/*
 * Get the computed zone activity levels.
 * Zones with high variance in RSSI = movement detected.
 */
void photon_wifi_get_zones(const photon_wifi_state_t *state,
                            float *zone_activity, uint32_t *num_zones);

void photon_wifi_free(photon_wifi_state_t *state);

/* ═══════════════════════════════════════════════════════════════
 *  API — Sensor Fusion (Kalman Filter)
 * ═══════════════════════════════════════════════════════════════ */

int  photon_fusion_init(photon_fusion_state_t *state, const photon_config_t *cfg);

/*
 * Feed all sensor measurements into the fusion engine.
 * Runs Kalman predict + update for each tracked source.
 * Handles source appearance, disappearance, and ID persistence.
 */
void photon_fusion_update(photon_fusion_state_t *state,
                           const photon_source_t *sonar_sources, uint32_t num_sonar,
                           const photon_source_t *tdoa_sources, uint32_t num_tdoa,
                           const float *wifi_zones, uint32_t num_zones,
                           const photon_rf_observation_t *ble_obs, uint32_t num_ble);

/*
 * Get the current fused position estimates.
 * Returns the number of active tracked sources.
 */
uint32_t photon_fusion_get_sources(const photon_fusion_state_t *state,
                                    photon_source_t *out, uint32_t max_sources);

void photon_fusion_free(photon_fusion_state_t *state);

/* ═══════════════════════════════════════════════════════════════
 *  Utility
 * ═══════════════════════════════════════════════════════════════ */

/*
 * Compute Shannon entropy of a data block (0.0 – 8.0 bits/byte).
 * Used internally for adaptive sensing resolution.
 */
float photon_shannon_entropy(const uint8_t *data, size_t len);

/*
 * Print a summary of tracked sources to stdout.
 */
void photon_print_sources(const photon_source_t *sources, uint32_t count);

/* ═══════════════════════════════════════════════════════════════
 *  API — Platform Layer (macOS Core Audio / CoreBluetooth)
 *
 *  These functions are implemented in qtsq_photon_coreaudio.m
 *  and are only available on Apple platforms.
 * ═══════════════════════════════════════════════════════════════ */

#ifdef __APPLE__

/*
 * Start the Core Audio engine.
 * Initializes AVAudioEngine for multi-mic capture and speaker output.
 * Starts BLE scanning in the background.
 * Returns 0 on success, -1 on error.
 */
int  photon_coreaudio_start(const photon_config_t *cfg);

/*
 * Get the latest captured multi-mic audio frame.
 * The caller receives freshly allocated channel buffers in out_frame.
 * Call photon_coreaudio_free_frame() when done.
 * Returns 0 on success, -1 if no new frame is available.
 */
int  photon_coreaudio_get_frame(photon_audio_frame_t *out_frame);

/*
 * Free the channel buffers allocated by photon_coreaudio_get_frame().
 */
void photon_coreaudio_free_frame(photon_audio_frame_t *frame);

/*
 * Emit a chirp waveform through the device speaker.
 * Used for ultrasonic sonar ranging.
 */
void photon_coreaudio_emit_chirp(const float *chirp, uint32_t num_samples);

/*
 * Scan visible WiFi access points and report RSSI.
 * Returns the number of observations written (up to max_obs).
 */
int  photon_coreaudio_scan_wifi(photon_rf_observation_t *out_obs, uint32_t max_obs);

/*
 * Get the latest BLE device observations (RSSI).
 * Returns the number of observations written (up to max_obs).
 */
int  photon_coreaudio_scan_ble(photon_rf_observation_t *out_obs, uint32_t max_obs);

/*
 * Stop all audio capture, speaker output, and BLE scanning.
 */
void photon_coreaudio_stop(void);

/*
 * Query engine state.
 */
int      photon_coreaudio_is_running(void);
uint32_t photon_coreaudio_get_mic_count(void);
double   photon_coreaudio_get_sample_rate(void);

#endif /* __APPLE__ */

#endif /* QTSQ_PHOTON_H */

