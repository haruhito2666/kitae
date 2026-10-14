//! kitae_qtsq — native QTSQ (.qtsq) support for the Kitae engine.
//!
//! Two layers:
//! 1. **Pure-Rust header probe** (`probe`) — reads only the documented v3
//!    header layout from `qtsq_format.h`; never decodes, never links C.
//!    Pinned to `QTSQ_VERSION == 3` and fails closed on anything else.
//! 2. **C decode bridge** (`decode_image`, `decode_text`, `decode_audio`,
//!    `VideoDecoder`) — hand-written FFI onto a small shim (`native/qtsq/shim.c`)
//!    that wraps the vendored `libqtsq.a`. Used only where the engine actually
//!    needs pixels/PCM/frames.
//!
//! Governing rule (docs/CPP_ROLE.md): the engine decodes; the embedder plays.

use std::ffi::{c_char, c_int, c_uchar, c_uint, c_void};
use std::fmt;

// ─────────────────────────────────────────────────────────────────────────────
// Format constants (mirror include/qtsq_format.h, QTSQ_VERSION == 3)
// ─────────────────────────────────────────────────────────────────────────────

pub const QTSQ_MAGIC: [u8; 4] = *b"QTS\x01";
/// Highest header version this crate parses (v1 and v2 are read too).
pub const QTSQ_VERSION_SUPPORTED: u8 = 3;

/// v1 packed header is 80 bytes (u16 flags, u32 block sizes).
pub const HEADER_V1_SIZE: usize = 80;
/// v2 packed header is 96 bytes (u16 flags, u64 block sizes).
pub const HEADER_V2_SIZE: usize = 96;
/// v3 packed header is 112 bytes (u32 flags, adds the security block).
pub const HEADER_V3_SIZE: usize = 112;
/// The horizon block, when present, is one aligned `qtsq_schema_t` (176 bytes).
pub const SCHEMA_BLOCK_SIZE: usize = 176;

/// Compression strategies (subset Kitae cares about; others map to `Unknown`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QtsqStrategy {
    None,
    Procedural,
    Fractal,
    Fourier,
    DeltaSeed,
    Dictionary,
    Temporal,
    Triangle,
    Photon,
    Splat,
    Tensor,
    Pixel,
    Tsubu,
    FourDgs,
    Surfel,
    Opus,
    Singularity,
    Layered,
    Unknown(u8),
}

impl QtsqStrategy {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0x00 => Self::None,
            0x01 => Self::Procedural,
            0x02 => Self::Fractal,
            0x03 => Self::Fourier,
            0x04 => Self::DeltaSeed,
            0x05 => Self::Dictionary,
            0x06 => Self::Temporal,
            0x07 => Self::Triangle,
            0x08 => Self::Photon,
            0x09 => Self::Splat,
            0x0A => Self::Tensor,
            0x0B => Self::Pixel,
            0x0C => Self::Tsubu,
            0x0D => Self::FourDgs,
            0x0E => Self::Surfel,
            0x0F => Self::Opus,
            0x10 => Self::Singularity,
            0xFF => Self::Layered,
            other => Self::Unknown(other),
        }
    }

    pub fn raw(self) -> u8 {
        match self {
            Self::None => 0x00,
            Self::Procedural => 0x01,
            Self::Fractal => 0x02,
            Self::Fourier => 0x03,
            Self::DeltaSeed => 0x04,
            Self::Dictionary => 0x05,
            Self::Temporal => 0x06,
            Self::Triangle => 0x07,
            Self::Photon => 0x08,
            Self::Splat => 0x09,
            Self::Tensor => 0x0A,
            Self::Pixel => 0x0B,
            Self::Tsubu => 0x0C,
            Self::FourDgs => 0x0D,
            Self::Surfel => 0x0E,
            Self::Opus => 0x0F,
            Self::Singularity => 0x10,
            Self::Layered => 0xFF,
            Self::Unknown(v) => v,
        }
    }

    /// Human-readable name (mirrors qtsq_strategy_name; stable for info pages).
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Procedural => "Procedural",
            Self::Fractal => "Fractal",
            Self::Fourier => "Fourier",
            Self::DeltaSeed => "Delta-Seed",
            Self::Dictionary => "Dictionary",
            Self::Temporal => "Temporal",
            Self::Triangle => "Triangle",
            Self::Photon => "Photon Audio",
            Self::Splat => "Splat",
            Self::Tensor => "Tensor",
            Self::Pixel => "Pixel",
            Self::Tsubu => "Tsubu",
            Self::FourDgs => "4DGS",
            Self::Surfel => "Surfel",
            Self::Opus => "Opus",
            Self::Singularity => "Singularity",
            Self::Layered => "Layered",
            Self::Unknown(_) => "Unknown",
        }
    }
}

/// Payload data types (subset; mirrors qtsq_data_type_t).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QtsqDataType {
    Raw,
    ImageGray,
    ImageRgb,
    ImageRgba,
    AudioMono,
    AudioStereo,
    AudioMulti,
    VideoFrames,
    TextAscii,
    TextUtf8,
    TableInt,
    TableFloat,
    Json,
    Csv,
    Signal1D,
    Signal2D,
    Tensor,
    Atom,
    FourDgs,
    Surfel,
    Mesh,
    Container,
    Unknown(u8),
}

impl QtsqDataType {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0x00 => Self::Raw,
            0x10 => Self::ImageGray,
            0x11 => Self::ImageRgb,
            0x12 => Self::ImageRgba,
            0x20 => Self::AudioMono,
            0x21 => Self::AudioStereo,
            0x22 => Self::AudioMulti,
            0x30 => Self::VideoFrames,
            0x40 => Self::TextAscii,
            0x41 => Self::TextUtf8,
            0x50 => Self::TableInt,
            0x51 => Self::TableFloat,
            0x52 => Self::Json,
            0x53 => Self::Csv,
            0x60 => Self::Signal1D,
            0x61 => Self::Signal2D,
            0x70 => Self::Tensor,
            0x80 => Self::Atom,
            0x81 => Self::FourDgs,
            0x82 => Self::Surfel,
            0x83 => Self::Mesh,
            0xF0 => Self::Container,
            other => Self::Unknown(other),
        }
    }

    pub fn raw(self) -> u8 {
        match self {
            Self::Raw => 0x00,
            Self::ImageGray => 0x10,
            Self::ImageRgb => 0x11,
            Self::ImageRgba => 0x12,
            Self::AudioMono => 0x20,
            Self::AudioStereo => 0x21,
            Self::AudioMulti => 0x22,
            Self::VideoFrames => 0x30,
            Self::TextAscii => 0x40,
            Self::TextUtf8 => 0x41,
            Self::TableInt => 0x50,
            Self::TableFloat => 0x51,
            Self::Json => 0x52,
            Self::Csv => 0x53,
            Self::Signal1D => 0x60,
            Self::Signal2D => 0x61,
            Self::Tensor => 0x70,
            Self::Atom => 0x80,
            Self::FourDgs => 0x81,
            Self::Surfel => 0x82,
            Self::Mesh => 0x83,
            Self::Container => 0xF0,
            Self::Unknown(v) => v,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Raw => "Raw",
            Self::ImageGray => "Image (gray)",
            Self::ImageRgb => "Image (RGB)",
            Self::ImageRgba => "Image (RGBA)",
            Self::AudioMono => "Audio (mono)",
            Self::AudioStereo => "Audio (stereo)",
            Self::AudioMulti => "Audio (multi-channel)",
            Self::VideoFrames => "Video",
            Self::TextAscii => "Text (ASCII)",
            Self::TextUtf8 => "Text (UTF-8)",
            Self::TableInt => "Table (int)",
            Self::TableFloat => "Table (float)",
            Self::Json => "JSON",
            Self::Csv => "CSV",
            Self::Signal1D => "Signal (1D)",
            Self::Signal2D => "Signal (2D)",
            Self::Tensor => "Tensor",
            Self::Atom => "Tsubu atoms",
            Self::FourDgs => "4DGS model",
            Self::Surfel => "Surfel set",
            Self::Mesh => "Mesh",
            Self::Container => "Container",
            Self::Unknown(_) => "Unknown",
        }
    }

    pub fn is_image(self) -> bool {
        matches!(self, Self::ImageGray | Self::ImageRgb | Self::ImageRgba)
    }
    pub fn is_audio(self) -> bool {
        matches!(self, Self::AudioMono | Self::AudioStereo | Self::AudioMulti)
    }
    pub fn is_text(self) -> bool {
        matches!(
            self,
            Self::TextAscii | Self::TextUtf8 | Self::Json | Self::Csv
        )
    }
    pub fn is_video(self) -> bool {
        self == Self::VideoFrames
    }
    pub fn is_tensor(self) -> bool {
        self == Self::Tensor
    }
}

/// Header flag bits Kitae surfaces (subset of qtsq_format.h).
pub mod flags {
    pub const LOSSY: u32 = 0x0002;
    pub const ENCRYPTED: u32 = 0x0010;
    pub const PQ_ENCRYPTED: u32 = 0x10000;
    pub const PQ_SIGNED: u32 = 0x20000;
    pub const ZERO_META: u32 = 0x80000;
    pub const GEOCRYPT: u32 = 0x100000;
    pub const SECRET_SHARE: u32 = 0x200000;
    pub const REGION_ENCRYPT: u32 = 0x400000;
    pub const DENIABLE: u32 = 0x1000000;
    pub const STREAM_ENCRYPTED: u32 = 0x20000000;
    pub const CREATIVE: u32 = 0x8000000;
    pub const PROVENANCE: u32 = 0x10000000;
    pub const DO_NOT_TRAIN: u32 = 0x40000000;
    pub const NO_AI: u32 = 0x80000000;
    pub const ENCRYPTION_MASK: u32 = ENCRYPTED
        | PQ_ENCRYPTED
        | STREAM_ENCRYPTED
        | GEOCRYPT
        | SECRET_SHARE
        | REGION_ENCRYPT
        | DENIABLE;
}

// ─────────────────────────────────────────────────────────────────────────────
// Probe — pure Rust, header only, v3 fail-closed
// ─────────────────────────────────────────────────────────────────────────────

/// What the probe could read from the header. `width`/`height`/rates come
/// from the horizon (schema) block when the file carries one — pixel-strategy
/// files keep `horizon_size == 0` and only know dimensions after decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QtsqInfo {
    pub version: u8,
    pub strategy: QtsqStrategy,
    pub data_type: QtsqDataType,
    pub channels: u8,
    pub encrypted: bool,
    pub lossy: bool,
    /// Header claims an embedded ML-DSA-65 signature (v3 PQ_SIGNED flag).
    /// Whether the signature actually verifies is [`verify_signature`]'s job.
    pub signed: bool,
    pub original_size: u64,
    /// Present only when the file carries a horizon (schema) block.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sample_rate: Option<u32>,
    pub frame_rate: Option<u32>,
}

/// Parse the header of any QTSQ file version (1, 2, or 3). Returns `None`
/// for non-QTSQ bytes, other versions, or truncated headers. Never guesses:
/// all fields come from fixed offsets matching qtsq_format.h.
///
/// Layouts (packed, little-endian):
/// ```text
/// v1 (80 B)                    v2 (96 B)                    v3 (112 B)
/// 0   magic[4]                 0   magic[4]                 0   magic[4]
/// 4   version u8               4   version u8               4   version u8
/// 5   strategy u8              5   strategy u8              5   strategy u8
/// 6   flags u16                6   flags u16                6   flags u32
/// 8   data_type u8             8   data_type u8             10  data_type u8
/// 9   num_channels u8          9   num_channels u8          11  num_channels u8
/// 10  sample_depth u16         10  sample_depth u16         12  sample_depth u16
/// 12  reserved u32             16  original_size u64        14  original_size u64
/// 16  original_size u64        24  timestamp u64            22  timestamp u64
/// 24  timestamp u64            32  checksum[32]             30  checksum[32]
/// 32  checksum[32]             64  horizon_size u64         62  horizon_size u64
/// 64  horizon_size u32         72  accretion_size u64       70  accretion_size u64
/// 68  accretion_size u32       80  singularity_size u64     78  singularity_size u64
/// 72  singularity_size u32     88  radiation_size u64       86  radiation_size u64
/// 76  radiation_size u32                                    94  security_block u64
///                                                           102 intent u8
/// ```
/// The horizon block (when `horizon_size > 0`) follows the header and is one
/// *aligned* `qtsq_schema_t`: `name[64]`, `data_type u8` + 3 pad,
/// `dimensions[8] u32` @ +68, `num_dims u8` @ +100 + 3 pad, `sample_rate u32`
/// @ +104, `frame_rate u32` @ +108, `encoding[64]` @ +112 → 176 bytes.
pub fn probe(bytes: &[u8]) -> Option<QtsqInfo> {
    if bytes.len() < 5 || bytes[..4] != QTSQ_MAGIC {
        return None;
    }
    let version = bytes[4];
    if bytes.len() < header_size(version)? {
        return None;
    }
    // Per-version field offsets (see layout table above).
    let (flags_off, dt_off, orig_off, horizon_off, hdr) = match version {
        1 => (6usize, 8usize, 16usize, 64usize, HEADER_V1_SIZE),
        2 => (6, 8, 16, 64, HEADER_V2_SIZE),
        3 => (6, 10, 14, 62, HEADER_V3_SIZE),
        _ => return None,
    };
    let strategy = bytes[5];
    let data_type = bytes[dt_off];
    let channels = bytes[dt_off + 1];
    let original_size = u64::from_le_bytes(bytes[orig_off..orig_off + 8].try_into().unwrap());
    // v1 packs the block sizes as u32; v2/v3 widen them to u64.
    let horizon_size: u64 = match version {
        1 => u32::from_le_bytes(bytes[64..68].try_into().unwrap()).into(),
        _ => u64::from_le_bytes(bytes[horizon_off..horizon_off + 8].try_into().unwrap()),
    };
    // v1/v2 pack flags as u16; v3 widens to u32. Read per version.
    let flags: u32 = match version {
        1 | 2 => u32::from(u16::from_le_bytes([bytes[6], bytes[7]])),
        _ => u32::from_le_bytes(bytes[flags_off..flags_off + 4].try_into().unwrap()),
    };

    let mut width = None;
    let mut height = None;
    let mut sample_rate = None;
    let mut frame_rate = None;
    if horizon_size > 0 && bytes.len() >= hdr + SCHEMA_BLOCK_SIZE {
        let s = hdr;
        // dimensions[8] u32 @ schema+68 (4-byte aligned layout)
        let d0 = u32::from_le_bytes([bytes[s + 68], bytes[s + 69], bytes[s + 70], bytes[s + 71]]);
        let d1 = u32::from_le_bytes([bytes[s + 72], bytes[s + 73], bytes[s + 74], bytes[s + 75]]);
        let sr = u32::from_le_bytes([
            bytes[s + 104],
            bytes[s + 105],
            bytes[s + 106],
            bytes[s + 107],
        ]);
        let fr = u32::from_le_bytes([
            bytes[s + 108],
            bytes[s + 109],
            bytes[s + 110],
            bytes[s + 111],
        ]);
        width = Some(d0);
        height = Some(d1);
        sample_rate = Some(sr);
        frame_rate = Some(fr);
    }

    Some(QtsqInfo {
        version,
        strategy: QtsqStrategy::from_raw(strategy),
        data_type: QtsqDataType::from_raw(data_type),
        channels,
        encrypted: flags & flags::ENCRYPTION_MASK != 0,
        lossy: flags & flags::LOSSY != 0,
        signed: version == 3 && flags & flags::PQ_SIGNED != 0,
        original_size,
        width,
        height,
        sample_rate,
        frame_rate,
    })
}

/// Packed header size for a version byte, or `None` for unknown versions.
fn header_size(version: u8) -> Option<usize> {
    match version {
        1 => Some(HEADER_V1_SIZE),
        2 => Some(HEADER_V2_SIZE),
        3 => Some(HEADER_V3_SIZE),
        _ => None,
    }
}

/// Quick magic check for the net-layer sniffing gate.
pub fn is_qtsq(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[..4] == QTSQ_MAGIC && header_size(bytes[4]).is_some()
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QtsqError {
    NotQtsq,
    UnsupportedVersion(u8),
    UnsupportedStrategy(QtsqStrategy),
    Encrypted,
    /// Non-negative C error code from the shim/libqtsq (QTSQ_ERR_*).
    CApi(i32),
    BadAudio,
    BadVideo,
    /// The decompressed payload is an encoded image (JPEG/PNG/…) whose
    /// container bytes cannot be decoded as pixels.
    BadImage,
    BadTensor,
}

impl fmt::Display for QtsqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotQtsq => write!(f, "not a QTSQ payload"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported QTSQ version {v} (only 3)"),
            Self::UnsupportedStrategy(s) => {
                write!(
                    f,
                    "unsupported QTSQ strategy {} ({:#04x})",
                    s.name(),
                    s.raw()
                )
            }
            Self::Encrypted => write!(f, "QTSQ payload is encrypted"),
            Self::CApi(code) => write!(f, "QTSQ C API error {code}"),
            Self::BadAudio => write!(f, "malformed QTSQ audio payload"),
            Self::BadVideo => write!(f, "malformed QTSQ video payload"),
            Self::BadImage => write!(f, "malformed QTSQ image payload"),
            Self::BadTensor => write!(f, "malformed QTSQ tensor payload"),
        }
    }
}

impl std::error::Error for QtsqError {}

// ─────────────────────────────────────────────────────────────────────────────
// C shim FFI (native/qtsq/shim.c) — linked only into binaries, not into
// the probe (which is pure Rust and works everywhere).
// ─────────────────────────────────────────────────────────────────────────────

#[repr(C)]
struct QtsqContextOpaque {
    _private: [u8; 0],
}
type QtsqContext = QtsqContextOpaque;

#[repr(C)]
struct QtsqVideoDecoderOpaque {
    _private: [u8; 0],
}
type QtsqVideoDecoder = QtsqVideoDecoderOpaque;

unsafe extern "C" {
    fn kitae_qtsq_context_free(ctx: *mut QtsqContext);
    fn kitae_qtsq_decode_open(
        out: *mut *mut QtsqContext,
        data: *const u8,
        size: usize,
        password: *const c_char,
    ) -> c_int;
    fn kitae_qtsq_decompress_bytes(
        ctx: *mut QtsqContext,
        out_data: *mut *mut u8,
        out_size: *mut usize,
    ) -> c_int;
    fn kitae_qtsq_payload_is_encoded_image(data: *const u8, size: usize) -> c_int;
    fn kitae_qtsq_decode_encoded_image(
        data: *const u8,
        size: usize,
        out_pixels: *mut *mut u8,
        out_w: *mut c_uint,
        out_h: *mut c_uint,
        out_channels: *mut c_uint,
    ) -> c_int;
    fn kitae_qtsq_free_buffer(p: *mut c_void);
    fn kitae_qtsq_decode_audio(
        ctx: *mut QtsqContext,
        out_pcm: *mut *mut i16,
        out_samples: *mut usize,
        out_rate: *mut c_uint,
        out_channels: *mut c_uchar,
    ) -> c_int;
    fn kitae_qtsq_decode_tensor(
        ctx: *mut QtsqContext,
        out_data: *mut *mut f32,
        out_count: *mut usize,
        out_num_dims: *mut c_uint,
        out_dims: *mut c_uint,
    ) -> c_int;
    fn kitae_qtsq_video_decoder_open(
        ctx: *mut QtsqContext,
        out: *mut *mut QtsqVideoDecoder,
    ) -> c_int;
    fn kitae_qtsq_video_get_frame(
        dec: *mut QtsqVideoDecoder,
        index: c_uint,
        out_pixels: *mut *mut u8,
        out_w: *mut c_uint,
        out_h: *mut c_uint,
    ) -> c_int;
    fn kitae_qtsq_video_frame_count(dec: *mut QtsqVideoDecoder) -> c_uint;
    fn kitae_qtsq_video_decoder_free(dec: *mut QtsqVideoDecoder);
    fn kitae_qtsq_schema_width(ctx: *const QtsqContext) -> c_uint;
    fn kitae_qtsq_schema_height(ctx: *const QtsqContext) -> c_uint;
    fn kitae_qtsq_schema_sample_rate(ctx: *const QtsqContext) -> c_uint;
    fn kitae_qtsq_schema_frame_rate(ctx: *const QtsqContext) -> c_uint;
    fn kitae_qtsq_is_signed(ctx: *const QtsqContext) -> c_int;
    fn kitae_qtsq_sig_size(ctx: *const QtsqContext) -> c_int;
    fn kitae_qtsq_sig_bytes(ctx: *const QtsqContext, out: *mut u8, cap: c_int) -> c_int;
    fn kitae_qtsq_verify_buffer(data: *const u8, size: usize, sign_pk: *const u8) -> c_int;
    fn kitae_qtsq_sign_keygen(out_pk: *mut u8, out_sk: *mut u8) -> c_int;
    fn kitae_qtsq_encode_text_secure(
        text: *const u8,
        len: usize,
        password: *const c_char,
        recipient_pk: *const u8,
        sign_sk: *const u8,
        out_data: *mut *mut u8,
        out_size: *mut usize,
    ) -> c_int;
    fn kitae_qtsq_device_sign_pk(out_pk: *mut u8) -> c_int;
    fn kitae_qtsq_ensure_device_keys(
        out_kem_pk: *mut u8,
        out_sign_pk: *mut u8,
        out_sign_sk: *mut u8,
    ) -> c_int;
    fn kitae_qtsq_kem_keygen() -> c_int;
}

/// RAII context handle for the C bridge. Not `Send`/`Sync`: the C API is
/// thread-unsafe per context, matching the engine's one-thread-per-tab model.
struct QtsqContextHolder {
    ctx: *mut QtsqContext,
}

// Safety: the shim's API is single-threaded per context and all entry points
// take `&mut`; we keep the holder private and never share it across threads.
unsafe impl Send for QtsqContextHolder {}

impl QtsqContextHolder {
    fn open(bytes: &[u8], password: Option<&str>) -> Result<Self, QtsqError> {
        if !is_qtsq(bytes) {
            return Err(QtsqError::NotQtsq);
        }
        let mut ctx: *mut QtsqContext = std::ptr::null_mut();
        let pw_c = password.map(|p| {
            // NUL-terminated copy handed to C; freed before returning.
            let mut buf: Vec<c_char> = p.bytes().map(|b| b as c_char).collect();
            buf.push(0);
            buf
        });
        let pw = pw_c
            .as_ref()
            .map(|b| b.as_ptr())
            .unwrap_or(std::ptr::null());
        let rc = unsafe { kitae_qtsq_decode_open(&mut ctx, bytes.as_ptr(), bytes.len(), pw) };
        if rc != 0 {
            if rc == -8 {
                return Err(QtsqError::Encrypted);
            }
            return Err(QtsqError::CApi(rc));
        }
        Ok(Self { ctx })
    }

    fn width(&self) -> u32 {
        unsafe { kitae_qtsq_schema_width(self.ctx) }
    }
    fn height(&self) -> u32 {
        unsafe { kitae_qtsq_schema_height(self.ctx) }
    }
    fn sample_rate(&self) -> u32 {
        unsafe { kitae_qtsq_schema_sample_rate(self.ctx) }
    }
    fn frame_rate(&self) -> u32 {
        unsafe { kitae_qtsq_schema_frame_rate(self.ctx) }
    }
}

impl Drop for QtsqContextHolder {
    fn drop(&mut self) {
        unsafe { kitae_qtsq_context_free(self.ctx) };
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Decoded payload types
// ─────────────────────────────────────────────────────────────────────────────

/// Decoded RGBA8 image (engine's canonical raster format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    /// width * height * 4 bytes, row-major, straight (non-premultiplied) alpha.
    pub pixels: Vec<u8>,
}

/// Decoded audio as interleaved f32 PCM in [-1.0, 1.0].
#[derive(Debug, Clone, PartialEq)]
pub struct PCMAudio {
    pub sample_rate: u32,
    pub channels: u8,
    /// Interleaved samples (frames * channels).
    pub samples: Vec<f32>,
}

impl PCMAudio {
    /// Frame count = interleaved samples / channels.
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }
}

/// Audio metadata that can be read without decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QtsqAudioInfo {
    pub sample_rate: u32,
    pub channels: u8,
    /// Total interleaved samples when known from the header (original_size / 2
    /// for int16 PCM capture payloads); `None` for compressed/unknown layouts.
    pub samples_hint: Option<u64>,
}

/// Decoded neural tensor: shape plus a flat f32 value array. Dimensions come
/// from the payload's embedded header (the C decoder patches them into the
/// schema after decompress), so they survive even on files without a horizon
/// block.
#[derive(Debug, Clone, PartialEq)]
pub struct QtsqTensor {
    /// Shape, outermost first. Empty when the payload carries no dimensions
    /// (flat 1-D data).
    pub dims: Vec<u32>,
    /// Flat values, row-major; len == product(dims) for well-formed files.
    pub values: Vec<f32>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Decode API (C bridge)
// ─────────────────────────────────────────────────────────────────────────────

/// Decode an image payload to RGBA8. Password-encrypted payloads need the
/// password; device-encrypted (passwordless QTSQStudio saves) open with this
/// machine's key, and everything else fails with [`QtsqError::Encrypted`].
pub fn decode_image(bytes: &[u8], password: Option<&str>) -> Result<RgbaImage, QtsqError> {
    let info = probe(bytes).ok_or(QtsqError::NotQtsq)?;
    if !info.data_type.is_image() {
        return Err(QtsqError::UnsupportedStrategy(info.strategy));
    }
    let holder = QtsqContextHolder::open(bytes, password)?;
    let mut out: *mut u8 = std::ptr::null_mut();
    let mut out_size: usize = 0;
    let rc = unsafe { kitae_qtsq_decompress_bytes(holder.ctx, &mut out, &mut out_size) };
    if rc != 0 {
        return Err(QtsqError::CApi(rc));
    }
    if out.is_null() {
        return Err(QtsqError::CApi(-1));
    }
    let raw = unsafe { std::slice::from_raw_parts(out, out_size) };
    // Container payloads: QTSQStudio wraps whole original files (e.g. the
    // untouched JPEG of a photo import), so the payload magic decides —
    // stb_image decodes the bytes; otherwise they are raw channel samples.
    let image = if unsafe { kitae_qtsq_payload_is_encoded_image(raw.as_ptr(), raw.len()) != 0 } {
        decode_container_pixels(raw)
    } else {
        // Every image strategy decompresses to a raster here: Pixel keeps
        // raw channel samples, while Triangle/Splat/Fractal rasterize inside
        // libqtsq — which writes the rendered dimensions back into
        // ctx->schema, i.e. the same holder these accessors read.
        decode_raw_pixels(raw, &holder, info.channels.max(1) as usize)
    };
    unsafe { kitae_qtsq_free_buffer(out as *mut c_void) };
    image
}

/// Decode an encoded-image payload (JPEG/PNG/GIF/BMP magic) to RGBA8 via the
/// shim's stb_image.
fn decode_container_pixels(raw: &[u8]) -> Result<RgbaImage, QtsqError> {
    let mut pixels: *mut u8 = std::ptr::null_mut();
    let (mut w, mut h, mut ch) = (0u32, 0u32, 0u32);
    let rc = unsafe {
        kitae_qtsq_decode_encoded_image(
            raw.as_ptr(),
            raw.len(),
            &mut pixels,
            &mut w,
            &mut h,
            &mut ch,
        )
    };
    if rc != 0 || pixels.is_null() {
        return Err(QtsqError::BadImage);
    }
    // stb_image promised 4 channels (request-compelled); take it as RGBA8.
    let len = w as usize * h as usize * 4;
    let pixels = unsafe { Vec::from_raw_parts(pixels, len, len) };
    Ok(RgbaImage {
        width: w,
        height: h,
        pixels,
    })
}

/// Expand raw channel samples (gray/rgb/rgba) into RGBA8. Dimensions come
/// from the C context's schema, which the libqtsq renderers refresh while
/// decompressing — so they are valid even for files without a horizon block.
fn decode_raw_pixels(
    raw: &[u8],
    holder: &QtsqContextHolder,
    ch: usize,
) -> Result<RgbaImage, QtsqError> {
    let (w, h) = (holder.width(), holder.height());
    if w == 0 || h == 0 || w > 65535 || h > 65535 {
        return Err(QtsqError::BadImage);
    }
    let ch = ch.clamp(1, 4);
    let need = w as usize * h as usize * ch;
    let mut padded: Vec<u8>;
    let raw: &[u8] = if raw.len() < need {
        // Legacy v2 NONE writers sometimes truncate the raster by a few
        // hundred bytes (the shim's compat pass already straightened the
        // header); pad the missing tail black rather than refusing the file.
        if need - raw.len() > w as usize * ch {
            return Err(QtsqError::BadImage);
        }
        padded = raw.to_vec();
        padded.resize(need, 0);
        &padded
    } else {
        raw
    };
    let mut pixels = vec![0u8; w as usize * h as usize * 4];
    // Expand source channels → RGBA (gray/rgb/rgba accepted defensively).
    for i in 0..(w as usize * h as usize) {
        let src = &raw[i * ch..i * ch + ch];
        let dst = &mut pixels[i * 4..i * 4 + 4];
        match ch {
            1 => {
                dst[0] = src[0];
                dst[1] = src[0];
                dst[2] = src[0];
                dst[3] = 255;
            }
            2 => {
                dst[0] = src[0];
                dst[1] = src[0];
                dst[2] = src[0];
                dst[3] = src[1];
            }
            n => {
                dst[0] = src[0];
                dst[1] = src[1];
                dst[2] = src[2];
                dst[3] = if n >= 4 { src[3] } else { 255 };
            }
        }
    }
    Ok(RgbaImage {
        width: w,
        height: h,
        pixels,
    })
}

/// Decode a text payload (ASCII/UTF-8/JSON/CSV) into a Rust String.
pub fn decode_text(bytes: &[u8], password: Option<&str>) -> Result<String, QtsqError> {
    let info = probe(bytes).ok_or(QtsqError::NotQtsq)?;
    if !info.data_type.is_text() {
        return Err(QtsqError::UnsupportedStrategy(info.strategy));
    }
    let holder = QtsqContextHolder::open(bytes, password)?;
    let mut out: *mut u8 = std::ptr::null_mut();
    let mut out_size: usize = 0;
    let rc = unsafe { kitae_qtsq_decompress_bytes(holder.ctx, &mut out, &mut out_size) };
    if rc != 0 {
        return Err(QtsqError::CApi(rc));
    }
    if out.is_null() {
        return Ok(String::new());
    }
    let raw = unsafe { std::slice::from_raw_parts(out, out_size) };
    let s = String::from_utf8_lossy(raw).into_owned();
    unsafe { kitae_qtsq_free_buffer(out as *mut c_void) };
    Ok(s)
}

/// Audio metadata from the header alone (no decode). Sample rate comes from
/// the horizon block when the file carries one.
pub fn audio_info(bytes: &[u8]) -> Option<QtsqAudioInfo> {
    let info = probe(bytes)?;
    if !info.data_type.is_audio() {
        return None;
    }
    Some(QtsqAudioInfo {
        sample_rate: info.sample_rate.unwrap_or(0),
        channels: if info.channels == 0 { 1 } else { info.channels },
        samples_hint: None,
    })
}

/// Decode audio to interleaved f32 PCM in [-1.0, 1.0].
pub fn decode_audio(bytes: &[u8], password: Option<&str>) -> Result<PCMAudio, QtsqError> {
    let info = probe(bytes).ok_or(QtsqError::NotQtsq)?;
    if !info.data_type.is_audio() {
        return Err(QtsqError::UnsupportedStrategy(info.strategy));
    }
    let holder = QtsqContextHolder::open(bytes, password)?;
    // Prefer the int16 path (covers zlib + qtc + opus capture payloads via
    // the codec-sniffing qtsq_decompress_audio).
    let mut pcm: *mut i16 = std::ptr::null_mut();
    let mut samples: usize = 0;
    let mut rate: c_uint = 0;
    let mut ch: c_uchar = 0;
    let rc =
        unsafe { kitae_qtsq_decode_audio(holder.ctx, &mut pcm, &mut samples, &mut rate, &mut ch) };
    if rc != 0 {
        if !pcm.is_null() {
            unsafe { kitae_qtsq_free_buffer(pcm as *mut c_void) };
        }
        return Err(QtsqError::BadAudio);
    }
    rate = rate.max(holder.sample_rate());
    if pcm.is_null() || samples == 0 {
        if !pcm.is_null() {
            unsafe { kitae_qtsq_free_buffer(pcm as *mut c_void) };
        }
        return Ok(PCMAudio {
            sample_rate: rate,
            channels: ch.max(1),
            samples: Vec::new(),
        });
    }
    let raw = unsafe { std::slice::from_raw_parts(pcm, samples) };
    let mut out = vec![0f32; samples];
    for (dst, src) in out.iter_mut().zip(raw.iter()) {
        *dst = (*src as f32) / 32768.0;
    }
    unsafe { kitae_qtsq_free_buffer(pcm as *mut c_void) };
    Ok(PCMAudio {
        sample_rate: rate,
        channels: ch.max(1),
        samples: out,
    })
}

/// Decode a tensor payload (AI weights, embeddings) to its shape + f32 values.
/// Password/device-key handling matches the other decode paths.
pub fn decode_tensor(bytes: &[u8], password: Option<&str>) -> Result<QtsqTensor, QtsqError> {
    let info = probe(bytes).ok_or(QtsqError::NotQtsq)?;
    if !info.data_type.is_tensor() {
        return Err(QtsqError::UnsupportedStrategy(info.strategy));
    }
    let holder = QtsqContextHolder::open(bytes, password)?;
    let mut out: *mut f32 = std::ptr::null_mut();
    let mut count: usize = 0;
    let mut num_dims: c_uint = 0;
    let mut dims = [0u32; 8];
    let rc = unsafe {
        kitae_qtsq_decode_tensor(
            holder.ctx,
            &mut out,
            &mut count,
            &mut num_dims,
            dims.as_mut_ptr(),
        )
    };
    if rc != 0 || out.is_null() || count == 0 {
        if !out.is_null() {
            unsafe { kitae_qtsq_free_buffer(out as *mut c_void) };
        }
        return Err(QtsqError::BadTensor);
    }
    let values = unsafe { std::slice::from_raw_parts(out, count) }.to_vec();
    unsafe { kitae_qtsq_free_buffer(out as *mut c_void) };
    Ok(QtsqTensor {
        dims: dims[..num_dims.min(8) as usize]
            .iter()
            .copied()
            .filter(|d| *d > 0)
            .collect(),
        values,
    })
}

/// Random-access video decoder over a QTSQ video payload.
pub struct VideoDecoder {
    holder: QtsqContextHolder,
    dec: *mut QtsqVideoDecoder,
    width: u32,
    height: u32,
    channels: u32,
    frame_count: u32,
    fps_hint: Option<u32>,
}

// Safety: the decoder is bound to its context; both are only touched through
// &mut self on the single engine thread.
unsafe impl Send for VideoDecoder {}

impl VideoDecoder {
    /// Open a video payload for random-access frame decoding.
    pub fn open(bytes: &[u8], password: Option<&str>) -> Result<Self, QtsqError> {
        let info = probe(bytes).ok_or(QtsqError::NotQtsq)?;
        if !info.data_type.is_video() {
            return Err(QtsqError::UnsupportedStrategy(info.strategy));
        }
        let holder = QtsqContextHolder::open(bytes, password)?;
        let mut dec: *mut QtsqVideoDecoder = std::ptr::null_mut();
        let rc = unsafe { kitae_qtsq_video_decoder_open(holder.ctx, &mut dec) };
        if rc != 0 || dec.is_null() {
            return Err(QtsqError::BadVideo);
        }
        let mut first: *mut u8 = std::ptr::null_mut();
        let mut w: c_uint = 0;
        let mut h: c_uint = 0;
        let probe_rc = unsafe { kitae_qtsq_video_get_frame(dec, 0, &mut first, &mut w, &mut h) };
        if probe_rc != 0 || w == 0 || h == 0 {
            unsafe { kitae_qtsq_video_decoder_free(dec) };
            return Err(QtsqError::BadVideo);
        }
        let frame_count = unsafe { kitae_qtsq_video_frame_count(dec) };
        if frame_count == 0 {
            unsafe { kitae_qtsq_video_decoder_free(dec) };
            return Err(QtsqError::BadVideo);
        }
        let fps_hint = info.frame_rate.filter(|f| *f > 0).or_else(|| {
            let fps = holder.frame_rate();
            (fps > 0).then_some(fps)
        });
        Ok(Self {
            width: w,
            height: h,
            channels: if info.channels == 0 {
                3
            } else {
                info.channels as u32
            },
            frame_count,
            fps_hint: fps_hint.filter(|f| *f > 0),
            holder,
            dec,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }

    /// Frame rate from the horizon block, if present.
    pub fn fps(&self) -> Option<u32> {
        self.fps_hint
    }

    pub fn duration(&self) -> Option<f32> {
        let fps = self.fps_hint.filter(|fps| *fps > 0)?;
        Some(self.frame_count as f32 / fps as f32)
    }

    pub fn frame_index(&self, position_secs: f32) -> Option<u32> {
        if self.frame_count == 0 || !position_secs.is_finite() {
            return None;
        }
        let fps = self.fps_hint.filter(|fps| *fps > 0)? as f32;
        let position = position_secs.max(0.0);
        let raw = if position >= f32::MAX / fps {
            u32::MAX
        } else {
            (position * fps) as u32
        };
        Some(raw.min(self.frame_count - 1))
    }

    /// Decode one frame to RGBA8.
    pub fn frame(&mut self, index: u32) -> Result<RgbaImage, QtsqError> {
        if index >= self.frame_count {
            return Err(QtsqError::BadVideo);
        }
        let mut pixels: *mut u8 = std::ptr::null_mut();
        let mut w: c_uint = 0;
        let mut h: c_uint = 0;
        let rc =
            unsafe { kitae_qtsq_video_get_frame(self.dec, index, &mut pixels, &mut w, &mut h) };
        if rc != 0 || pixels.is_null() {
            return Err(QtsqError::BadVideo);
        }
        let w = w as usize;
        let h = h as usize;
        if w == 0 || h == 0 || w > 65_535 || h > 65_535 {
            return Err(QtsqError::BadVideo);
        }
        let channels = self.channels.clamp(1, 4) as usize;
        let src_stride = w.checked_mul(channels).ok_or(QtsqError::BadVideo)?;
        let src_len = src_stride.checked_mul(h).ok_or(QtsqError::BadVideo)?;
        let out_len = w
            .checked_mul(h)
            .and_then(|n| n.checked_mul(4))
            .ok_or(QtsqError::BadVideo)?;
        let raw = unsafe { std::slice::from_raw_parts(pixels, src_len) };
        let mut out = vec![0u8; out_len];
        for y in 0..h {
            for x in 0..w {
                let s = &raw[y * src_stride + x * channels..];
                let d = &mut out[(y * w + x) * 4..];
                match channels {
                    1 => {
                        d[0] = s[0];
                        d[1] = s[0];
                        d[2] = s[0];
                        d[3] = 255;
                    }
                    2 => {
                        d[0] = s[0];
                        d[1] = s[0];
                        d[2] = s[0];
                        d[3] = s[1];
                    }
                    3 => {
                        d[0] = s[0];
                        d[1] = s[1];
                        d[2] = s[2];
                        d[3] = 255;
                    }
                    _ => {
                        d[0] = s[0];
                        d[1] = s[1];
                        d[2] = s[2];
                        d[3] = s[3];
                    }
                }
            }
        }
        // The pixel buffer is owned by the decoder (rotating internal buffer) —
        // per qtsq.h it must NOT be freed; it stays valid until the next call.
        Ok(RgbaImage {
            width: w as u32,
            height: h as u32,
            pixels: out,
        })
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        // Free the decoder while the context it borrows (`holder`) is alive;
        // fields drop in declaration order after this, so this reference
        // pins the context to outlive the C decoder.
        let _ = &self.holder;
        unsafe { kitae_qtsq_video_decoder_free(self.dec) };
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Signature API (ML-DSA-65, qtsq_vault)
// ─────────────────────────────────────────────────────────────────────────────

/// ML-DSA-65 public key length in bytes.
pub const SIGN_PK_LEN: usize = 1952;
/// ML-DSA-65 secret key length in bytes.
pub const SIGN_SK_LEN: usize = 4032;

/// Outcome of checking a payload's embedded ML-DSA-65 signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureCheck {
    /// No security block / no signature in the payload.
    Unsigned,
    /// Signed and the signature verifies against the given public key.
    Valid,
    /// Signed but verification failed against the given key — the payload
    /// was modified after signing, or the key is not the signer's.
    Invalid,
}

/// Verify a payload's embedded signature against `sign_pk`. The check runs
/// on the still-encrypted payload inside the shim: the signed hash covers
/// header + singularity, and decrypting would rewrite the singularity.
///
/// The shim reconstructs the signing-time header state before hashing (the
/// library stamps `PQ_SIGNED|ZERO_META` into the header flags and a
/// security-block size only *after* signing, and verification hashes the
/// current header — see shim.c for the probe-verified details).
pub fn verify_signature(
    bytes: &[u8],
    sign_pk: &[u8; SIGN_PK_LEN],
) -> Result<SignatureCheck, QtsqError> {
    let info = probe(bytes).ok_or(QtsqError::NotQtsq)?;
    if !info.signed {
        return Ok(SignatureCheck::Unsigned);
    }
    let rc = unsafe { kitae_qtsq_verify_buffer(bytes.as_ptr(), bytes.len(), sign_pk.as_ptr()) };
    match rc {
        0 => Ok(SignatureCheck::Valid),
        // Header claims signed but the context carries no security block:
        // treat as unsigned (belt-and-suspenders — never observed).
        -40 => Ok(SignatureCheck::Unsigned),
        // Environmental failures, not verdicts.
        -1 | -2 | -3 => Err(QtsqError::CApi(rc)),
        // Wrong key, tampered payload, or a checksum rejection while reading.
        _ => Ok(SignatureCheck::Invalid),
    }
}

/// Generate a fresh ML-DSA-65 signing keypair.
pub fn sign_keygen() -> Result<([u8; SIGN_PK_LEN], [u8; SIGN_SK_LEN]), QtsqError> {
    let mut pk = [0u8; SIGN_PK_LEN];
    let mut sk = [0u8; SIGN_SK_LEN];
    let rc = unsafe { kitae_qtsq_sign_keygen(pk.as_mut_ptr(), sk.as_mut_ptr()) };
    if rc != 0 {
        return Err(QtsqError::CApi(rc));
    }
    Ok((pk, sk))
}

/// Compress + encrypt + sign a text payload in one shot (`qtsq_secure_text`)
/// and return the .qtsq bytes. `password` and `recipient_pk` pick the vault
/// mode: give a password for password-encrypted output, or a recipient
/// ML-KEM public key for passwordless device-encrypted output. `sign_sk`
/// may be `None` to skip signing.
pub fn encode_text_secure(
    text: &[u8],
    password: Option<&str>,
    recipient_pk: Option<&[u8; 1184]>,
    sign_sk: Option<&[u8; SIGN_SK_LEN]>,
) -> Result<Vec<u8>, QtsqError> {
    if password.is_none() && recipient_pk.is_none() {
        return Err(QtsqError::CApi(-1));
    }
    let mut pw_c: Vec<c_char> = password
        .map(|p| p.bytes().map(|b| b as c_char).collect())
        .unwrap_or_default();
    pw_c.push(0);
    let pw = if password.is_some() {
        pw_c.as_ptr()
    } else {
        std::ptr::null()
    };
    let rp = recipient_pk.map(|p| p.as_ptr()).unwrap_or(std::ptr::null());
    let sk_ptr = sign_sk.map(|s| s.as_ptr()).unwrap_or(std::ptr::null());
    let mut out: *mut u8 = std::ptr::null_mut();
    let mut out_len: usize = 0;
    let rc = unsafe {
        kitae_qtsq_encode_text_secure(
            text.as_ptr(),
            text.len(),
            pw,
            rp,
            sk_ptr,
            &mut out,
            &mut out_len,
        )
    };
    if rc != 0 {
        return Err(QtsqError::CApi(rc));
    }
    // Copy into a Rust-owned Vec; the shim's malloc'd buffer goes back the
    // same way every other C buffer here does.
    let bytes = unsafe { std::slice::from_raw_parts(out, out_len) }.to_vec();
    unsafe { kitae_qtsq_free_buffer(out as *mut c_void) };
    Ok(bytes)
}

/// This machine's device sign public key, when one has been created
/// (Save-Page-as-QTSQ writes `device_sign.pk` into the key dir). Files
/// signed by this device verify against it; foreign signatures report
/// [`SignatureCheck::Invalid`] against it.
pub fn device_sign_pk() -> Option<[u8; SIGN_PK_LEN]> {
    let mut pk = [0u8; SIGN_PK_LEN];
    let rc = unsafe { kitae_qtsq_device_sign_pk(pk.as_mut_ptr()) };
    if rc == 0 {
        Some(pk)
    } else {
        None
    }
}

/// Sizes of the ML-KEM-768 device keypair files (`device_kem.pk`/`.sk`).
pub const DEVICE_KEM_PK_LEN: usize = 1184;
pub const DEVICE_KEM_SK_LEN: usize = 2400;

/// Device key material for Save-Page-as-QTSQ, created on first use and
/// stored 0600 in the QTSQ key dir next to the Studio's `device_kem` files.
/// An existing pair is never regenerated (old device-locked files would stop
/// opening); only missing pairs are generated.
#[derive(Clone, Copy)]
pub struct DeviceKeys {
    /// ML-KEM-768 recipient: archives encrypt to it so this machine reopens
    /// them passwordless (`decode_text(.., None)` → the device key path).
    pub kem_pk: [u8; DEVICE_KEM_PK_LEN],
    /// ML-DSA-65 public key (`device_sign.pk`): what `verify_signature` and
    /// the reopen verdict strip check archives against.
    pub sign_pk: [u8; SIGN_PK_LEN],
    /// ML-DSA-65 secret key (`device_sign.sk`): signs each archive.
    pub sign_sk: [u8; SIGN_SK_LEN],
}

/// Ensure this machine's device keys exist (generating what is missing) and
/// return the keys a Save-Page encode needs. Passive readers
/// ([`device_sign_pk`]) never create keys; only an actual save does.
pub fn device_keys() -> Result<DeviceKeys, QtsqError> {
    let mut keys = DeviceKeys {
        kem_pk: [0u8; DEVICE_KEM_PK_LEN],
        sign_pk: [0u8; SIGN_PK_LEN],
        sign_sk: [0u8; SIGN_SK_LEN],
    };
    let rc = unsafe {
        kitae_qtsq_ensure_device_keys(
            keys.kem_pk.as_mut_ptr(),
            keys.sign_pk.as_mut_ptr(),
            keys.sign_sk.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(QtsqError::CApi(rc));
    }
    Ok(keys)
}

/// Generate a device KEM pair into the current key dir without touching the
/// signing keys. Test setup: engine tests need a `device_kem.sk` for the
/// passwordless decrypt path while `device_sign.pk` must stay absent for
/// `Unverified`-verdict assertions.
pub fn kem_keygen() -> Result<(), QtsqError> {
    let rc = unsafe { kitae_qtsq_kem_keygen() };
    if rc != 0 {
        return Err(QtsqError::CApi(rc));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — run with the vendored static libs linked (all targets). Pure-Rust
// probe tests pass even without the C bridge; decode tests exercise the shim.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("testdata")
                .join(name),
        )
        .expect("fixture file")
    }

    // ── probe (pure Rust) ──

    #[test]
    fn probe_rejects_non_qtsq() {
        assert!(probe(b"").is_none());
        assert!(probe(b"QTS\x02suffix").is_none());
        assert!(!is_qtsq(b"<!DOCTYPE html>"));
        assert!(!is_qtsq(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn probe_rejects_truncated_and_unknown_versions() {
        let full = fixture("fix_text.qtsq");
        assert!(
            probe(&full[..40]).is_none(),
            "truncated header must fail closed"
        );
        // Unknown version byte → fail closed. (v1/v2 are supported since the
        // legacy-compat pass; v3 is the current writer.)
        let mut unknown = full.clone();
        unknown[4] = 9;
        assert!(probe(&unknown).is_none());
    }

    #[test]
    fn probe_audio_fixture() {
        let bytes = fixture("fix_audio.qtsq");
        let info = probe(&bytes).expect("probe audio");
        assert_eq!(info.version, 3);
        assert_eq!(info.data_type, QtsqDataType::AudioMono);
        assert_eq!(info.channels, 1);
        assert_eq!(info.sample_rate, Some(22050));
        assert!(!info.encrypted);
        assert_eq!(audio_info(&bytes).map(|a| a.sample_rate), Some(22050));
    }

    #[test]
    fn probe_video_fixture() {
        let bytes = fixture("fix_video.qtsq");
        let info = probe(&bytes).expect("probe video");
        assert_eq!(info.data_type, QtsqDataType::VideoFrames);
        assert_eq!(info.width, Some(16));
        assert_eq!(info.height, Some(16));
        assert_eq!(info.frame_rate, Some(12));
    }

    #[test]
    fn probe_marks_legacy_encryption_modes_locked() {
        let base = fixture("fix_video.qtsq");
        let original = u32::from_le_bytes(base[6..10].try_into().unwrap());
        for bit in [flags::SECRET_SHARE, flags::REGION_ENCRYPT] {
            let mut bytes = base.clone();
            let marked = original | bit;
            bytes[6..10].copy_from_slice(&marked.to_le_bytes());
            let info = probe(&bytes).expect("probe mutated header");
            assert!(info.encrypted, "flag {bit:#x} must keep the payload locked");
        }
    }

    #[test]
    fn video_timing_clamps_positions_and_reports_duration() {
        let bytes = fixture("fix_video.qtsq");
        let dec = VideoDecoder::open(&bytes, None).expect("open video");
        assert_eq!(dec.duration(), Some(0.5));
        assert_eq!(dec.frame_index(-1.0), Some(0));
        assert_eq!(dec.frame_index(0.25), Some(3));
        assert_eq!(dec.frame_index(0.5), Some(5));
        assert_eq!(dec.frame_index(99.0), Some(5));
        assert_eq!(dec.frame_index(f32::NAN), None);
    }

    #[test]
    fn probe_image_and_text_fixtures() {
        let img = probe(&fixture("fix_image.qtsq")).expect("probe image");
        assert_eq!(img.data_type, QtsqDataType::ImageRgba);
        assert_eq!(img.strategy, QtsqStrategy::Pixel);
        // pixel-strategy files carry no horizon → dims unknown until decode
        assert_eq!(img.width, None);
        let txt = probe(&fixture("fix_text.qtsq")).expect("probe text");
        assert_eq!(txt.data_type, QtsqDataType::TextUtf8);
        assert_eq!(txt.strategy, QtsqStrategy::Dictionary);
    }

    // ── password-locked fixture (feature ①) ──

    #[test]
    fn locked_fixture_probes_encrypted() {
        let bytes = fixture("fix_text_locked.qtsq");
        let info = probe(&bytes).expect("probe locked fixture");
        assert_eq!(info.data_type, QtsqDataType::TextUtf8);
        assert!(
            info.encrypted,
            "locked fixture must carry the encrypted flag"
        );
    }

    #[test]
    fn locked_fixture_needs_password() {
        let bytes = fixture("fix_text_locked.qtsq");
        match decode_text(&bytes, None) {
            Err(QtsqError::Encrypted) => {} // the contract: fail closed
            other => panic!("no password must yield Encrypted, got {other:?}"),
        }
    }

    #[test]
    fn locked_fixture_opens_with_password_and_rejects_wrong_one() {
        let bytes = fixture("fix_text_locked.qtsq");
        let text = decode_text(&bytes, Some("hunter2")).expect("decode with the right password");
        assert!(
            text.contains("Secret words behind the password."),
            "decoded payload: {text:?}"
        );
        // A wrong password is NOT Encrypted (the file opened, decryption
        // failed inside the vault) — it surfaces as a C API error so the
        // viewer can render a "wrong password" note instead of a prompt.
        match decode_text(&bytes, Some("wrong")) {
            Err(e @ QtsqError::CApi(_)) => assert_ne!(
                format!("{e}"),
                "QTSQ C API error -8",
                "-8 is the encrypted marker, not a wrong-password failure"
            ),
            other => panic!("wrong password must fail with CApi(rc), got {other:?}"),
        }
    }

    // ── decode (C bridge) ──

    #[test]
    fn decode_text_round_trip() {
        let bytes = fixture("fix_text.qtsq");
        let text = decode_text(&bytes, None).expect("decode text");
        assert!(text.contains("Hello from QTSQ!"));
        assert!(text.contains("wörld ✓"), "UTF-8 preserved");
    }

    #[test]
    fn decode_image_lossless_pixels() {
        let bytes = fixture("fix_image.qtsq");
        let img = decode_image(&bytes, None).expect("decode image");
        assert_eq!((img.width, img.height), (8, 8));
        // pixel i was written as (i*3+1, i*5+2, i*7+3, 255) — lossless PIXEL
        assert_eq!(&img.pixels[0..4], &[1, 2, 3, 255]);
        let i = 63usize;
        assert_eq!(
            &img.pixels[i * 4..i * 4 + 4],
            &[
                (63 * 3 + 1) as u8,
                (63 * 5 + 2) as u8,
                (63 * 7 + 3) as u8,
                255
            ]
        );
    }

    #[test]
    fn decode_audio_sine() {
        let bytes = fixture("fix_audio.qtsq");
        let audio = decode_audio(&bytes, None).expect("decode audio");
        assert_eq!(audio.sample_rate, 22050);
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.frames(), 22050);
        // 440 Hz sine @ 22050 Hz has period 50.1 samples; sample 0 ≈ 0
        assert!(audio.samples[0].abs() < 0.01);
        let peak = audio.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            peak > 0.3,
            "sine should have significant amplitude, got {peak}"
        );
    }

    #[test]
    fn tensor_decode_fixture() {
        let bytes = fixture("fix_tensor.qtsq");
        let info = probe(&bytes).expect("probe tensor");
        assert_eq!(info.data_type, QtsqDataType::Tensor);
        assert_eq!(info.strategy, QtsqStrategy::Tensor);
        assert!(!info.encrypted);
        // Dimensions come only from the payload's embedded header (the file
        // carries no horizon block), so probe knows nothing yet.
        assert_eq!(info.width, None);

        let tensor = decode_tensor(&bytes, None).expect("decode tensor");
        assert!(
            tensor.values.iter().all(|v| v.is_finite()),
            "decoded values must be finite"
        );
        // original_size / sizeof(f32) predicts the value count.
        assert_eq!(
            (info.original_size as usize) / 4,
            tensor.values.len(),
            "count matches the header's original size"
        );
        let product: u64 = tensor.dims.iter().map(|&d| d as u64).product();
        assert_eq!(
            product,
            tensor.values.len() as u64,
            "shape product matches count"
        );
    }

    #[test]
    fn tensor_decode_rejects_non_tensor_payloads() {
        let bytes = fixture("fix_image.qtsq");
        match decode_tensor(&bytes, None) {
            Err(QtsqError::UnsupportedStrategy(s)) => {
                assert_eq!(s, QtsqStrategy::Pixel);
            }
            other => panic!("image payload must not decode as tensor, got {other:?}"),
        }
    }

    #[test]
    fn video_decoder_frames() {
        let bytes = fixture("fix_video.qtsq");
        let mut dec = VideoDecoder::open(&bytes, None).expect("open video");
        assert_eq!(dec.dimensions(), (16, 16));
        assert_eq!(dec.frame_count(), 6);
        let f0 = dec.frame(0).expect("frame 0");
        assert_eq!((f0.width, f0.height), (16, 16));
        // TEMPORAL is lossy by design (segment batching + interpolation), so
        // absolute pixels drift (measured: f0 blue 248 not 255, f5 red 147
        // not 200). Assert the temporal trend instead: the source ramps red
        // up and blue down, and decode preserves red < blue → red > blue.
        assert!(
            f0.pixels[2] > f0.pixels[0],
            "frame 0 should be blue-dominant, got {} vs {}",
            f0.pixels[2],
            f0.pixels[0]
        );
        assert_eq!(f0.pixels[3], 255, "RGB→RGBA alpha expansion is exact");
        let f5 = dec.frame(5).expect("frame 5");
        assert!(
            f5.pixels[0] > f5.pixels[2],
            "frame 5 should be red-dominant, got {} vs {}",
            f5.pixels[0],
            f5.pixels[2]
        );
        let f2 = dec.frame(2).expect("frame 2");
        assert!(
            f2.pixels[0] > f0.pixels[0] && f2.pixels[2] < f0.pixels[2],
            "red should rise and blue fall across frames"
        );
        assert!(
            dec.frame(6).is_err(),
            "out-of-range frame must error, not panic"
        );
    }

    #[test]
    fn encrypted_file_errors_cleanly() {
        // A byte-flipped copy of the plaintext fixture keeps the header magic
        // and flags but corrupts the singularity → decode must error, not
        // panic. (The password on fix_text_locked.qtsq is a placeholder;
        // no real credentials live in tests.)
        let bytes = fixture("fix_image.qtsq");
        assert!(!probe(&bytes).unwrap().encrypted);
        // corrupting the strategy byte (TENSOR on an image payload) must make
        // the decode fail cleanly — not panic, not return garbage pixels
        let mut bad = bytes.clone();
        bad[5] = 0x0A; // TENSOR strategy on an image payload
        match decode_image(&bad, None) {
            Err(err) => assert!(
                matches!(err, QtsqError::CApi(_) | QtsqError::BadImage),
                "expected CApi/BadImage, got {err:?}"
            ),
            Ok(img) => panic!("corrupt payload must not decode, got {img:?}"),
        }
    }

    #[test]
    fn truncated_file_never_panics() {
        let full = fixture("fix_video.qtsq");
        for len in [0usize, 4, 50, 111, 200, full.len() / 2, full.len() - 1] {
            let cut = &full[..len.min(full.len())];
            let _ = decode_text(cut, None);
            let _ = decode_audio(cut, None);
            let _ = VideoDecoder::open(cut, None);
        }
    }

    /// Fuzz-lite: random single-byte flips on a fixture must never panic.
    #[test]
    fn fuzz_lite_byte_flips() {
        let base = fixture("fix_text.qtsq");
        let mut state = 0x2545F4914F6CDD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200 {
            let mut bytes = base.clone();
            let pos = (next() as usize) % bytes.len();
            bytes[pos] = (next() as u8).wrapping_mul(31);
            let _ = decode_text(&bytes, None); // must not panic
        }
    }

    // ── signature verify (feature ④) ──
    //
    // Signing runs through kitae_qtsq::encode_text_secure (qtsq_secure_text)
    // and verification through verify_signature — the same shim paths the
    // engine and Save-Page-as-QTSQ use. Each test generates its own keypair;
    // no keys, passwords, or fixtures are shared between tests (ML-DSA
    // keygen is ~50µs, so regenerating is cheaper than any shared-state
    // lockup, and the shim serializes nothing).

    #[test]
    fn probe_plain_fixture_is_unsigned() {
        let info = probe(&fixture("fix_text.qtsq")).expect("probe");
        assert!(!info.signed, "plain fixture carries no PQ_SIGNED flag");
    }

    #[test]
    fn signed_payload_roundtrip_verifies() {
        let (pk, sk) = sign_keygen().expect("keygen");
        let bytes = encode_text_secure(
            b"signed payload roundtrip",
            Some("probe-password-1"),
            None,
            Some(&sk),
        )
        .expect("encode");
        let info = probe(&bytes).expect("probe signed payload");
        assert!(info.signed, "PQ_SIGNED flag set after signing");
        assert!(info.encrypted, "secure_text also encrypts");
        match verify_signature(&bytes, &pk) {
            Ok(SignatureCheck::Valid) => {}
            other => panic!("own key must verify Valid, got {other:?}"),
        }
    }

    #[test]
    fn signed_secure_payload_decodes_with_password() {
        let (_pk, sk) = sign_keygen().expect("keygen");
        let bytes = encode_text_secure(
            b"decode after signing roundtrip",
            Some("probe-password-1"),
            None,
            Some(&sk),
        )
        .expect("encode");
        let info = probe(&bytes).expect("probe");
        assert!(
            info.data_type.is_text(),
            "secure_text payload stays text: strategy={:?} data_type={:?}",
            info.strategy,
            info.data_type
        );
        let text = decode_text(&bytes, Some("probe-password-1"))
            .expect("correct password must decode a secure_text payload");
        assert_eq!(text, "decode after signing roundtrip");
    }

    #[test]
    fn signed_payload_fails_foreign_key() {
        let (_pk, sk) = sign_keygen().expect("keygen");
        let (other_pk, _other_sk) = sign_keygen().expect("keygen 2");
        let bytes = encode_text_secure(
            b"foreign key check",
            Some("probe-password-1"),
            None,
            Some(&sk),
        )
        .expect("encode");
        match verify_signature(&bytes, &other_pk) {
            Ok(SignatureCheck::Invalid) => {}
            other => panic!("foreign key must verify Invalid, got {other:?}"),
        }
    }

    #[test]
    fn tampered_signed_payload_fails_verify() {
        let (pk, sk) = sign_keygen().expect("keygen");
        let mut bytes = encode_text_secure(
            b"tamper check payload",
            Some("probe-password-1"),
            None,
            Some(&sk),
        )
        .expect("encode");
        // Flip a byte inside the singularity region (past the 112-byte
        // header; the file roundtrip layout puts the payload right after it).
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        match verify_signature(&bytes, &pk) {
            Ok(SignatureCheck::Invalid) => {}
            // A checksum rejection during qtsq_read also surfaces as Invalid.
            Err(_) => {}
            other => panic!("tampered payload must not verify, got {other:?}"),
        }
    }

    #[test]
    fn unsigned_payload_reports_unsigned() {
        let bytes = fixture("fix_text.qtsq");
        let (pk, _sk) = sign_keygen().expect("keygen");
        match verify_signature(&bytes, &pk) {
            Ok(SignatureCheck::Unsigned) => {}
            other => panic!("plain fixture must be Unsigned, got {other:?}"),
        }
    }

    #[test]
    fn unsigned_bytes_rejected_before_key_check() {
        match verify_signature(b"not a qtsq file at all", &[0u8; SIGN_PK_LEN]) {
            Err(QtsqError::NotQtsq) => {}
            other => panic!("non-QTSQ bytes must yield NotQtsq, got {other:?}"),
        }
    }

    #[test]
    fn device_sign_pk_absent_in_fresh_key_dir() {
        // A key dir with no device_sign.pk must read as None. Uses its own
        // fresh temp dir (under the env lock): the REAL store gains a
        // device_sign.pk on the user's first save, so the default-dir
        // version of this assertion could not survive that.
        let _lock = qtsq_env_lock();
        let (_dir, old) = point_key_dir_at_fresh_temp();
        assert!(
            device_sign_pk().is_none(),
            "no device_sign.pk should exist in a fresh key dir"
        );
        restore_key_dir(old);
    }

    #[test]
    fn encode_requires_a_vault_mode() {
        let (_, sk) = sign_keygen().expect("keygen");
        match encode_text_secure(b"no mode", None, None, Some(&sk)) {
            Err(QtsqError::CApi(-1)) => {}
            other => panic!("no password and no recipient must fail, got {other:?}"),
        }
    }

    // ── device key store (feature ② — Save-Page-as-QTSQ) ──
    //
    // All KITAE_QTSQ_KEY_DIR-touching tests here serialize on one lock: the
    // env var is process-global and Rust tests run in parallel threads.

    fn qtsq_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Redirect the key dir at a fresh temp directory and return its path.
    /// Caller holds [`qtsq_env_lock`] and must restore the old value before
    /// releasing it (edition 2021: plain set_var, no unsafe).
    fn point_key_dir_at_fresh_temp() -> (std::path::PathBuf, Option<String>) {
        let old = std::env::var("KITAE_QTSQ_KEY_DIR").ok();
        let dir = std::env::temp_dir().join(format!(
            "kitae_qtsq_keys_{}_{}_{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create key dir");
        std::env::set_var("KITAE_QTSQ_KEY_DIR", &dir);
        (dir, old)
    }

    fn restore_key_dir(old: Option<String>) {
        match old {
            Some(v) => std::env::set_var("KITAE_QTSQ_KEY_DIR", v),
            None => std::env::remove_var("KITAE_QTSQ_KEY_DIR"),
        }
    }

    #[test]
    fn device_keys_generate_once_and_persist() {
        let _lock = qtsq_env_lock();
        let (dir, old) = point_key_dir_at_fresh_temp();

        let keys = device_keys().expect("first call generates");
        assert!(
            dir.join("device_kem.pk").exists()
                && dir.join("device_kem.sk").exists()
                && dir.join("device_sign.pk").exists()
                && dir.join("device_sign.sk").exists(),
            "all four files written"
        );

        // Second call is the persistence proof: same keys, no regeneration.
        let again = device_keys().expect("second call reads back");
        assert_eq!(keys.kem_pk, again.kem_pk);
        assert_eq!(keys.sign_pk, again.sign_pk);
        assert_eq!(keys.sign_sk, again.sign_sk);

        // And device_sign_pk (the passive verify-side reader) now sees them.
        assert_eq!(device_sign_pk(), Some(keys.sign_pk));

        restore_key_dir(old);
    }

    #[test]
    fn existing_kem_pair_is_never_regenerated() {
        let _lock = qtsq_env_lock();
        let (dir, old) = point_key_dir_at_fresh_temp();
        kem_keygen().expect("seed a KEM pair only");

        let keys = device_keys().expect("ensure fills in the sign pair");
        let seeded_kem_pk = std::fs::read(dir.join("device_kem.pk")).expect("read seeded pk");
        assert_eq!(&keys.kem_pk[..], seeded_kem_pk.as_slice(), "KEM pair kept");
        assert!(
            dir.join("device_sign.pk").exists() && dir.join("device_sign.sk").exists(),
            "sign pair generated beside the seeded KEM pair"
        );

        restore_key_dir(old);
    }

    #[test]
    fn device_archive_roundtrip_is_passwordless_and_signed() {
        let _lock = qtsq_env_lock();
        let (_dir, old) = point_key_dir_at_fresh_temp();

        let keys = device_keys().expect("device keys");
        let bytes = encode_text_secure(
            b"archived page body",
            None,
            Some(&keys.kem_pk),
            Some(&keys.sign_sk),
        )
        .expect("device-encrypted + signed encode");

        let info = probe(&bytes).expect("probe");
        assert!(info.encrypted, "locked to the device key");
        assert!(info.signed, "signed with the device sign key");

        // The reopen contract: no password, device decrypt path.
        let text = decode_text(&bytes, None).expect("device key reopens passwordless");
        assert_eq!(text, "archived page body");

        // And the verdict strip check a reopened archive goes through.
        match verify_signature(&bytes, &keys.sign_pk) {
            Ok(SignatureCheck::Valid) => {}
            other => panic!("own device signature must verify, got {other:?}"),
        }

        restore_key_dir(old);
    }
}
