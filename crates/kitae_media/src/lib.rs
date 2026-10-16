//! kitae_media — image decode for the Kitae engine (Phase K).
//!
//! Pure-Rust decoders behind one seam (`decode_image`), so a future
//! libjpeg-turbo / HarfBuzz-style C module can slot in without touching
//! callers (per `CPP_ROLE.md` the C side would only ever be a link-in
//! library):
//!
//! * PNG: the `png` crate → RGBA8 (interlaced and 16-bit accepted, ICC/gamma
//!   ignored — documented reader-browser nicety).
//! * JPEG: `zune-jpeg` (the plan's pure-Rust escape hatch) → RGBA8. CMYK
//!   JPEGs are a documented fallback (header dims + placeholder).
//! * GIF: `gif` crate → still (composited first frame) via `decode_image`,
//!   full animation via `decode_frames` (disposal compositing, loop count).
//!   The embedder drives timing (N8: no engine timeline).
//! * WebP: `image-webp` → still RGBA8 via `decode_image` (lossy VP8,
//!   lossless VP8L, with alpha). Animated WebP presents its first frame —
//!   documented still-only scope (Q9); animation stays a GIF/APNG path.
//! * BMP: hand-rolled decoder in `bmp` → `decode_bmp`. Windows
//!   `BITMAPINFOHEADER`/V4/V5 and the OS/2 headers, 1/2/4/8/16/24/32 bpp,
//!   `BI_RGB`/`BI_BITFIELDS`/`BI_ALPHABITFIELDS` plus `BI_RLE8`/`BI_RLE4`.
//!   No new dependency: a BMP is a header plus rows.
//! * ICO: `ico` walks the size directory and decodes the largest entry — a
//!   PNG entry through `decode_png`, a DIB entry through `bmp` (doubled
//!   height, 4-byte palette entries, 32bpp alpha, and the 1bpp `AND` mask
//!   from whichever half of the pixel array holds it). `.cur` files are not
//!   images and are not sniffed.
//! * APNG: `apng` detects the animation (`apng_info`/`is_animated_png`)
//!   and plays it (`decode_apng_frames`), reusing the `png` crate by
//!   reassembling each `fcTL`/`fdAT` frame as a standalone datastream. It
//!   sniffs as `image/png`; `decode_image` returns the `IDAT` image. The
//!   frame-sequence contract is the GIF one (`Frame`, `MAX_FRAMES`,
//!   `MIN_FRAME_DELAY_MS`), so an embedder drives either the same way — the
//!   engine still routes animation by content type, and picking up APNG is a
//!   one-line change where it checks for GIF.
//! * SVG: `resvg`/`usvg` → RGBA8 via `decode_svg` (Q10). SVG has no fixed
//!   intrinsic bitmap, so callers rasterize at the layout box size and the
//!   SVG's own coordinate system scales to fit, centered. `usvg` drops
//!   `<script>` and SMIL at parse, so the render is script-free by
//!   construction; `decode_image` keeps SVG `Unsupported` (no box size to
//!   rasterize against) — the engine routes it to `decode_svg`.
//! * Web fonts (Q11): `decode_woff` / `decode_woff2` reverse the WOFF
//!   containers into sfnt bytes in memory so Core Text can register an
//!   `@font-face` without staging to disk. A decode failure is
//!   `MediaError::Font` and the engine falls back to the next `src:` entry.
//!
//! No JS, no WASM, no threads: decode is a pure function of bytes.

mod apng;
mod bmp;
mod ico;
mod woff;

pub use apng::{apng_info, decode_apng_frames, is_animated_png, ApngInfo};
pub use bmp::decode_bmp;
pub use ico::decode_ico;
pub use woff::{decode_woff, decode_woff2, sfnt_find_table, sfnt_whole_checksum};

// F-bucket media (parity plan §F): audio decode, video demux, captions.
pub mod audio;
pub mod video;
pub mod vtt;

use kitae_style::ObjectFit;

/// An RGBA8 raster: row-major, top-left origin, 4 bytes per pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

impl RgbaImage {
    /// A solid-color raster (placeholders, tests).
    pub fn solid(width: u32, height: u32, rgba: [u8; 4]) -> RgbaImage {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            pixels.extend_from_slice(&rgba);
        }
        RgbaImage {
            width,
            height,
            pixels,
        }
    }

    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = ((y * self.width + x) * 4) as usize;
        Some([
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ])
    }
}

/// Why pixels (or fonts) are unavailable. Callers fall back — images to
/// header dims (see `dimensions`) plus the layout placeholder, fonts to the
/// next `src:` entry — never a blank page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
    /// Not a decodable still (unknown magic, GIF, SVG, video, …).
    Unsupported(String),
    /// Recognized but undecodable (truncated, CMYK JPEG, …).
    Decode(String),
    /// Header claims more than the bomb guard allows (64 MP).
    TooLarge { width: u32, height: u32 },
    /// Web font container rejected (Q11): bad structure or failed
    /// reconstruction. The engine falls back to the next `src:` entry.
    Font(String),
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaError::Unsupported(s) => write!(f, "unsupported image: {s}"),
            MediaError::Decode(s) => write!(f, "cannot decode image: {s}"),
            MediaError::TooLarge { width, height } => {
                write!(f, "image too large: {width}x{height}")
            }
            MediaError::Font(s) => write!(f, "bad web font: {s}"),
        }
    }
}

impl std::error::Error for MediaError {}

/// Decompression-bomb guard: refuse headers above 64 megapixels.
pub const MAX_PIXELS: u64 = 64 * 1024 * 1024;

pub(crate) fn check_size(width: u32, height: u32) -> Result<(), MediaError> {
    if width == 0 || height == 0 {
        return Err(MediaError::Decode("zero dimension".into()));
    }
    if width as u64 * height as u64 > MAX_PIXELS {
        return Err(MediaError::TooLarge { width, height });
    }
    Ok(())
}

/// Magic-byte sniff. Returns a MIME-ish tag, not a full content type.
///
/// BMP and ICO both have loose magics (`BM` is two bytes, and an ICO header
/// is four), so each is confirmed by a structural check rather than taken on
/// faith: a `BM` file must carry a DIB header size this crate reads, and an
/// ICO must declare `type = 1` (icons; `type = 2` is a cursor, which is not
/// an image and stays `None`).
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes[0..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        Some("image/png")
    } else if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        Some("image/jpeg")
    } else if bytes.len() >= 6 && (bytes[0..6] == *b"GIF87a" || bytes[0..6] == *b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes[0..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
        Some("image/webp")
    } else if bytes.len() >= 18 && bytes[0..2] == *b"BM" && bmp::bmp_dimensions(bytes).is_some() {
        Some("image/bmp")
    } else if bytes.len() >= 6
        && bytes[0..4] == [0x00, 0x00, 0x01, 0x00]
        && ico::ico_dimensions(bytes).is_some()
    {
        Some("image/x-icon")
    } else if is_svg_text(bytes) {
        Some("image/svg+xml")
    } else {
        None
    }
}

/// SVG text sniff: an optional BOM/whitespace/XML declaration, then markup
/// whose root element is `<svg` within the first kilobyte (comments or a
/// DOCTYPE may sit between the declaration and the root). The `<svg` scan is
/// bounded so huge binaries never pay for it, and `usvg` re-validates on
/// parse, so a loose match fails closed to a decode error.
fn is_svg_text(bytes: &[u8]) -> bool {
    let mut s = bytes;
    if s.starts_with(&[0xEF, 0xBB, 0xBF]) {
        s = &s[3..];
    }
    while matches!(s.first(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        s = &s[1..];
    }
    if !s.starts_with(b"<?xml")
        && !s.starts_with(b"<!--")
        && !s.starts_with(b"<!DOCTYPE")
        && !s.starts_with(b"<svg")
    {
        return false;
    }
    let head = &s[..s.len().min(1024)];
    head.windows(4).any(|w| w == *b"<svg")
}

/// Encode an RGBA8 raster as a PNG (8-bit RGBA, no interlace). The
/// canvas `toDataURL` path needs an encoder next to the decoders: a page
/// can hand the result straight back to the engine as an image source.
pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, MediaError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, img.width, img.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| MediaError::Decode(format!("png header: {e}")))?;
        writer
            .write_image_data(&img.pixels)
            .map_err(|e| MediaError::Decode(format!("png encode: {e}")))?;
    }
    Ok(out)
}

/// Decode PNG bytes → RGBA8 (any color type / bit depth the `png` crate
/// accepts, expanded to 8-bit RGBA; interlace handled by the crate).
pub fn decode_png(bytes: &[u8]) -> Result<RgbaImage, MediaError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Expand everything toward 8-bit with alpha (palette/gray/RGB → RGBA8).
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|e| MediaError::Decode(format!("png header: {e}")))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| MediaError::Decode("png size overflow".into()))?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| MediaError::Decode(format!("png decode: {e}")))?;
    check_size(info.width, info.height)?;
    let n = info.width as usize * info.height as usize;
    let pixels = match info.color_type {
        png::ColorType::Rgba => {
            if buf.len() < n * 4 {
                return Err(MediaError::Decode(format!(
                    "png short frame: {} < {}",
                    buf.len(),
                    n * 4
                )));
            }
            buf[..n * 4].to_vec()
        }
        png::ColorType::GrayscaleAlpha => {
            if buf.len() < n * 2 {
                return Err(MediaError::Decode(format!(
                    "png short frame: {} < {}",
                    buf.len(),
                    n * 2
                )));
            }
            let mut out = Vec::with_capacity(n * 4);
            for px in buf[..n * 2].chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        png::ColorType::Rgb => {
            if buf.len() < n * 3 {
                return Err(MediaError::Decode(format!(
                    "png short frame: {} < {}",
                    buf.len(),
                    n * 3
                )));
            }
            let mut out = Vec::with_capacity(n * 4);
            for px in buf[..n * 3].chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            out
        }
        png::ColorType::Grayscale => {
            if buf.len() < n {
                return Err(MediaError::Decode(format!(
                    "png short frame: {} < {n}",
                    buf.len()
                )));
            }
            let mut out = Vec::with_capacity(n * 4);
            for &g in &buf[..n] {
                out.extend_from_slice(&[g, g, g, 0xFF]);
            }
            out
        }
        other => {
            return Err(MediaError::Decode(format!("png color type {other:?}")));
        }
    };
    Ok(RgbaImage {
        width: info.width,
        height: info.height,
        pixels,
    })
}

/// Number of image components from the first JPEG SOF marker (`None` when
/// no SOF is found). Used to route CMYK (4 components) to the documented
/// fallback before attempting a decode.
pub fn jpeg_components(bytes: &[u8]) -> Option<u8> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let mut m = i + 1;
        while m < bytes.len() && bytes[m] == 0xFF {
            m += 1;
        }
        if m >= bytes.len() {
            break;
        }
        let marker = bytes[m];
        if marker == 0xD8 || marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i = m + 1;
            continue;
        }
        if m + 3 >= bytes.len() {
            break;
        }
        let len = u16::from_be_bytes([bytes[m + 1], bytes[m + 2]]) as usize;
        if len < 2 || m + 1 + len > bytes.len() {
            break;
        }
        if matches!(
            marker,
            0xC0 | 0xC1
                | 0xC2
                | 0xC3
                | 0xC5
                | 0xC6
                | 0xC7
                | 0xC9
                | 0xCA
                | 0xCB
                | 0xCD
                | 0xCE
                | 0xCF
        ) && len >= 8
            && bytes[m + 3] == 8
        {
            return Some(bytes[m + 8]);
        }
        i = m + 1 + len;
    }
    None
}

/// Decode JPEG bytes → RGBA8 (baseline, progressive, 4:2:0/4:4:4, grayscale).
/// CMYK (4 components) is rejected so callers take the header-dims fallback.
pub fn decode_jpeg(bytes: &[u8]) -> Result<RgbaImage, MediaError> {
    if jpeg_components(bytes) == Some(4) {
        return Err(MediaError::Decode("cmyk jpeg: header-dims fallback".into()));
    }
    let mut dec = zune_jpeg::JpegDecoder::new(bytes);
    let pixels = dec
        .decode()
        .map_err(|e| MediaError::Decode(format!("jpeg decode: {e:?}")))?;
    let info = dec
        .info()
        .ok_or_else(|| MediaError::Decode("jpeg info missing".into()))?;
    let (width, height) = (info.width as u32, info.height as u32);
    check_size(width, height)?;
    // zune-jpeg emits RGB or L depending on components; normalize to RGBA.
    let rgba = match pixels.len() {
        n if n == width as usize * height as usize * 4 => pixels,
        n if n == width as usize * height as usize * 3 => {
            let mut out = Vec::with_capacity(width as usize * height as usize * 4);
            for px in pixels.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            out
        }
        n if n == width as usize * height as usize => {
            let mut out = Vec::with_capacity(width as usize * height as usize * 4);
            for &g in &pixels {
                out.extend_from_slice(&[g, g, g, 0xFF]);
            }
            out
        }
        n => {
            return Err(MediaError::Decode(format!(
                "jpeg pixel count {n} fits no RGB/RGBA/gray plane"
            )));
        }
    };
    Ok(RgbaImage {
        width,
        height,
        pixels: rgba,
    })
}

/// Decode any supported still → RGBA8. `mime` (e.g. a content type) wins when
/// present; otherwise the magic bytes decide. GIF stills composite the first
/// frame; SVG → `Unsupported` here — rasterization is size-dependent, so the
/// engine's relayout routes it to `decode_svg` — and unknown magic →
/// `Unsupported` (layout keeps its placeholder path).
///
/// ICO picks its largest entry; BMP decodes 1..32 bpp with `BI_RGB`,
/// `BI_BITFIELDS` and RLE. An APNG (which sniffs as `image/png`) yields its
/// `IDAT` default image here — the animation lives in
/// [`decode_apng_frames`], which the embedder asks for by frame count
/// ([`apng_info`]) as it does for GIF.
pub fn decode_image(bytes: &[u8], mime: Option<&str>) -> Result<RgbaImage, MediaError> {
    let sniffed = mime
        .map(|m| m.to_ascii_lowercase())
        .filter(|m| m.starts_with("image/"))
        .or_else(|| sniff(bytes).map(str::to_string));
    match sniffed.as_deref() {
        Some("image/png") => decode_png(bytes),
        Some("image/jpeg") | Some("image/jpg") => decode_jpeg(bytes),
        Some("image/gif") => decode_gif_still(bytes),
        Some("image/webp") => decode_webp(bytes),
        Some("image/bmp") | Some("image/x-ms-bmp") | Some("image/x-bmp") => decode_bmp(bytes),
        Some("image/x-icon") | Some("image/vnd.microsoft.icon") => decode_ico(bytes),
        Some(other) => Err(MediaError::Unsupported(other.to_string())),
        None => Err(MediaError::Unsupported("unknown magic".into())),
    }
}

/// Animation guards (Q8): a frame count above this is refused outright and
/// `MAX_PIXELS` bounds each frame, so total buffered pixels stay bounded.
pub const MAX_FRAMES: usize = 512;
/// Floor for per-frame presentation delays (ms): frames asking for less
/// (0-delay "fast loop" GIFs) play at this rate instead of busy-spinning.
pub const MIN_FRAME_DELAY_MS: u32 = 20;

/// One composited animation frame: RGBA8 pixels + presentation delay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// `width * height * 4` bytes, full-canvas composited (ready to blit).
    pub rgba: Vec<u8>,
    /// Presentation delay in milliseconds (floored at [`MIN_FRAME_DELAY_MS`]).
    pub delay_ms: u32,
}

/// Decode a GIF into its full frame sequence. Frames are composited onto the
/// running canvas per the GIF spec's disposal methods (`Background` clears
/// the frame region to transparent, `Previous` rolls back to the snapshot
/// taken before the frame, `Keep`/`Any` leave the canvas as-is). `loop_count`
/// is the file's own repeat declaration (`None` = play once, `Some(n)` = n
/// extra repetitions, `Some(u16::MAX)` = forever). Every frame is
/// full-canvas RGBA8, so embedders blit without compositing.
pub fn decode_frames(bytes: &[u8]) -> Result<(Vec<Frame>, Option<u16>), MediaError> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options
        .read_info(std::io::Cursor::new(bytes))
        .map_err(|e| MediaError::Decode(format!("gif header: {e}")))?;
    let (width, height) = (decoder.width() as u32, decoder.height() as u32);
    check_size(width, height)?;

    let mut frames: Vec<Frame> = Vec::new();
    // The running canvas + a snapshot for `Previous` disposal.
    let mut canvas = vec![0u8; width as usize * height as usize * 4];
    // Snapshot for `Previous` disposal; set before each composite.
    let mut prev: Vec<u8>;

    while let Some(frame) = decoder
        .read_next_frame()
        .map_err(|e| MediaError::Decode(format!("gif frame: {e}")))?
    {
        if frames.len() >= MAX_FRAMES {
            break;
        }
        // Per-frame draw region (canvas coords).
        let (fx, fy, fw, fh) = (
            frame.left as u32,
            frame.top as u32,
            frame.width as u32,
            frame.height as u32,
        );
        if fx + fw > width || fy + fh > height {
            return Err(MediaError::Decode("gif frame exceeds canvas".into()));
        }
        // Snapshot before mutating, for RestoreToPrevious.
        prev = canvas.clone();

        // Composite: draw over the canvas (GIF alpha is 1-bit — a pixel is
        // either opaque or transparent; no partial blending needed).
        let buffer: &[u8] = frame.buffer.as_ref();
        for y in 0..fh as usize {
            let dst_row = (fy as usize + y) * width as usize * 4;
            let src_row = y * fw as usize * 4;
            for x in 0..fw as usize {
                let s = src_row + x * 4;
                if buffer[s + 3] == 0 {
                    continue;
                }
                let d = dst_row + (fx as usize + x) * 4;
                canvas[d..d + 4].copy_from_slice(&buffer[s..s + 4]);
            }
        }

        let delay_ms = ((frame.delay as u32) * 10).max(MIN_FRAME_DELAY_MS);
        frames.push(Frame {
            rgba: canvas.clone(),
            delay_ms,
        });

        // Disposal for the NEXT frame's starting canvas.
        match frame.dispose {
            gif::DisposalMethod::Background => {
                // Browsers clear the drawn region to transparent (the crate
                // reader documents the same convention).
                for y in 0..fh as usize {
                    let row = (fy as usize + y) * width as usize * 4 + fx as usize * 4;
                    canvas[row..row + fw as usize * 4].fill(0);
                }
            }
            gif::DisposalMethod::Previous => {
                canvas.copy_from_slice(&prev);
            }
            _ => {} // Keep / Any: canvas stays composited.
        }
    }

    if frames.is_empty() {
        return Err(MediaError::Decode("gif has no frames".into()));
    }
    let loop_count = match decoder.repeat() {
        // No NETSCAPE extension (default) = play once.
        gif::Repeat::Finite(0) => None,
        gif::Repeat::Finite(n) => Some(n),
        gif::Repeat::Infinite => Some(u16::MAX),
    };
    Ok((frames, loop_count))
}

/// Decode a GIF's first frame as a still image (the `decode_image` path).
/// Runs the full compositor so even a first frame that depends on nothing
/// renders exactly as frame one of the animation.
pub fn decode_gif_still(bytes: &[u8]) -> Result<RgbaImage, MediaError> {
    let (frames, _) = decode_frames(bytes)?;
    let f = &frames[0];
    let (w, h) = gif_canvas_dims(bytes).ok_or_else(|| MediaError::Decode("gif dims".into()))?;
    Ok(RgbaImage {
        width: w,
        height: h,
        pixels: f.rgba.clone(),
    })
}

/// Canvas dimensions from the logical screen descriptor (cheap header read).
fn gif_canvas_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 {
        return None;
    }
    let w = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
    let h = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
    (w > 0 && h > 0).then_some((w, h))
}

/// Decode WebP bytes → RGBA8 (Q9). Covers lossy VP8, lossless VP8L and the
/// extended format, with or without alpha. Animated WebP yields its first
/// frame — the documented still-only scope (GIF is the animation path).
pub fn decode_webp(bytes: &[u8]) -> Result<RgbaImage, MediaError> {
    let mut decoder = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|e| MediaError::Decode(format!("webp header: {e}")))?;
    let (width, height) = decoder.dimensions();
    check_size(width, height)?;

    // The decoder emits RGBA when the image carries alpha, RGB otherwise;
    // normalize both into RGBA8 (opaque alpha).
    let bpp = if decoder.has_alpha() { 4 } else { 3 };
    let size = decoder
        .output_buffer_size()
        .ok_or_else(|| MediaError::Decode("webp size overflow".into()))?;
    let mut raw = vec![0u8; size];
    decoder
        .read_image(&mut raw)
        .map_err(|e| MediaError::Decode(format!("webp decode: {e}")))?;

    let n = width as usize * height as usize;
    let mut pixels = Vec::with_capacity(n * 4);
    if bpp == 4 {
        pixels = raw;
    } else {
        for px in raw.chunks_exact(3) {
            pixels.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
        }
    }
    Ok(RgbaImage {
        width,
        height,
        pixels,
    })
}

/// Rasterize SVG bytes → RGBA8 at the requested box (Q10). An SVG has no
/// single intrinsic bitmap, so the caller (the engine's relayout, which
/// knows the layout box) asks for the size; the SVG's own coordinate system
/// (`width`/`height`/`viewBox`) is uniformly scaled to *fit* inside and
/// centered, leaving transparent margins — the raster comes back box-sized
/// with the SVG's aspect preserved. Parsing drops `<script>` and SMIL
/// (`usvg`), so the render is script-free by construction; no JS, no WASM.
/// `<image href>` resolves data URIs only: usvg never does network requests,
/// and the default resolver's *local file* reads are explicitly disabled —
/// an SVG document must not be able to pull arbitrary files off disk.
pub fn decode_svg(bytes: &[u8], target_w: u32, target_h: u32) -> Result<RgbaImage, MediaError> {
    check_size(target_w, target_h)?;
    let opts = resvg::usvg::Options {
        // Data-URI `<image href>` stays; file-path hrefs resolve to nothing.
        image_href_resolver: resvg::usvg::ImageHrefResolver {
            resolve_string: Box::new(|_, _| None),
            ..Default::default()
        },
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_data(bytes, &opts)
        .map_err(|e| MediaError::Decode(format!("svg parse: {e}")))?;
    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    if sw <= 0.0 || sh <= 0.0 {
        return Err(MediaError::Decode("svg zero intrinsic size".into()));
    }
    let (tw, th) = (target_w as f32, target_h as f32);
    // Fit-to-box (aspect preserved) + centering: p' = scale·p + offset.
    let scale = (tw / sw).min(th / sh);
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale)
        .post_translate((tw - sw * scale) * 0.5, (th - sh * scale) * 0.5);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(target_w, target_h)
        .ok_or_else(|| MediaError::Decode("svg pixmap alloc".into()))?;
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    // tiny-skia stores premultiplied alpha; the engine's raster is straight.
    let mut pixels = Vec::with_capacity(pixmap.pixels().len() * 4);
    for c in pixmap.pixels() {
        let c = c.demultiply();
        pixels.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Ok(RgbaImage {
        width: target_w,
        height: target_h,
        pixels,
    })
}

/// Intrinsic SVG size from the `usvg` tree: the `width`/`height` attributes
/// (relative ones resolve through the default viewport), falling back to the
/// `viewBox`, then 100×100. A full parse, but no rasterization.
fn svg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let (w, h) = (size.width().round() as u32, size.height().round() as u32);
    (w > 0 && h > 0).then_some((w, h))
}

/// Fast intrinsic dimensions without a full decode: PNG info, JPEG headers,
/// GIF screen descriptor, WebP VP8/VP8L chunk headers, BMP DIB header, ICO
/// directory, SVG XML size — each with a manual header-math fallback so
/// truncated prefixes still yield geometry (SVG re-validates via the `usvg`
/// parse instead). This replaces the old header-math helper (same signature,
/// one call site in `kitae_layout`).
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match sniff(bytes)? {
        "image/png" => png_dimensions(bytes).or_else(|| png_dimensions_header(bytes)),
        "image/jpeg" => jpeg_dimensions(bytes).or_else(|| jpeg_dimensions_header(bytes)),
        "image/gif" => {
            if bytes.len() < 10 {
                return None;
            }
            let w = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
            let h = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
            (w > 0 && h > 0).then_some((w, h))
        }
        "image/webp" => webp_dimensions(bytes),
        "image/bmp" => bmp::bmp_dimensions(bytes),
        "image/x-icon" => ico::ico_dimensions(bytes),
        "image/svg+xml" => svg_dimensions(bytes),
        _ => None,
    }
}

/// WebP dimensions from the chunk headers (cheap RIFF walk, no decode).
/// Handles simple lossy (VP8), simple lossless (VP8L) and extended (VP8X)
/// files. `None` on anything malformed.
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // RIFF header: "RIFF" u32le "WEBP", then 8-byte chunk records.
    if bytes.len() < 12 || bytes[0..4] != *b"RIFF" || bytes[8..12] != *b"WEBP" {
        return None;
    }
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let tag = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = pos + 8;
        match tag {
            b"VP8 " => {
                // Lossy bitstream: frame tag (3 bytes, keyframe check) then
                // start code 0x9D 0x01 0x2A, then 14-bit width/height.
                if body + 10 > bytes.len() {
                    return None;
                }
                let b0 = bytes[body];
                if b0 & 1 != 0 {
                    return None; // interframe: no dimensions here
                }
                if bytes[body + 3..body + 6] != [0x9D, 0x01, 0x2A] {
                    return None;
                }
                let w = u16::from_le_bytes([bytes[body + 6], bytes[body + 7]]) & 0x3FFF;
                let h = u16::from_le_bytes([bytes[body + 8], bytes[body + 9]]) & 0x3FFF;
                return (w > 0 && h > 0).then_some((w as u32, h as u32));
            }
            b"VP8L" => {
                // Lossless bitstream: signature 0x2F then packed 14-bit
                // (height-1, width-1) in 4 bytes.
                if body + 5 > bytes.len() || bytes[body] != 0x2F {
                    return None;
                }
                let bits = u32::from_le_bytes([
                    bytes[body + 1],
                    bytes[body + 2],
                    bytes[body + 3],
                    bytes[body + 4],
                ]);
                let w = (bits & 0x3FFF) + 1;
                let h = ((bits >> 14) & 0x3FFF) + 1;
                return Some((w, h));
            }
            b"VP8X" => {
                // Extended: flags(1) reserved(3) canvas width-1 (3le) height-1 (3le).
                if body + 10 > bytes.len() {
                    return None;
                }
                let rd = |i: usize| -> u32 {
                    bytes[body + i] as u32
                        | (bytes[body + i + 1] as u32) << 8
                        | (bytes[body + i + 2] as u32) << 16
                };
                let w = rd(4) + 1;
                let h = rd(7) + 1;
                return (w > 0 && h > 0).then_some((w, h));
            }
            _ => {}
        }
        // Chunks pad to even sizes.
        pos = body + size + (size & 1);
    }
    None
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let reader = decoder.read_info().ok()?;
    let info = reader.info();
    (info.width > 0 && info.height > 0).then_some((info.width, info.height))
}

/// Manual IHDR parse for truncated prefixes (no IDAT needed).
fn png_dimensions_header(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 24
        && bytes[0..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        && bytes[12..16] == [0x49, 0x48, 0x44, 0x52]
    {
        let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        if w > 0 && h > 0 && w <= 65535 && h <= 65535 {
            return Some((w, h));
        }
    }
    None
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut dec = zune_jpeg::JpegDecoder::new(bytes);
    dec.decode_headers().ok()?;
    let i = dec.info()?;
    (i.width > 0 && i.height > 0).then_some((i.width as u32, i.height as u32))
}

/// Manual SOF scan for truncated prefixes.
fn jpeg_dimensions_header(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let mut m = i + 1;
        while m < bytes.len() && bytes[m] == 0xFF {
            m += 1;
        }
        if m >= bytes.len() {
            break;
        }
        let marker = bytes[m];
        if marker == 0xD8 || marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i = m + 1;
            continue;
        }
        if m + 3 >= bytes.len() {
            break;
        }
        let len = u16::from_be_bytes([bytes[m + 1], bytes[m + 2]]) as usize;
        if len < 2 || m + 1 + len > bytes.len() {
            break;
        }
        if matches!(
            marker,
            0xC0 | 0xC1
                | 0xC2
                | 0xC3
                | 0xC5
                | 0xC6
                | 0xC7
                | 0xC9
                | 0xCA
                | 0xCB
                | 0xCD
                | 0xCE
                | 0xCF
        ) && len >= 7
        {
            let h = u16::from_be_bytes([bytes[m + 4], bytes[m + 5]]) as u32;
            let w = u16::from_be_bytes([bytes[m + 6], bytes[m + 7]]) as u32;
            if w > 0 && h > 0 {
                return Some((w, h));
            }
            return None;
        }
        i = m + 1 + len;
    }
    None
}

/// Paint-time geometry for `object-fit` (Phase K4): which source sub-rect of
/// the intrinsic raster lands in which sub-rect of the layout box. `fill`
/// stretches; the rest preserve aspect. Coordinates are pixels (source) and
/// CSS px relative to the box origin (destination).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitRect {
    pub src_x: f32,
    pub src_y: f32,
    pub src_w: f32,
    pub src_h: f32,
    pub dst_x: f32,
    pub dst_y: f32,
    pub dst_w: f32,
    pub dst_h: f32,
}

pub fn object_fit_rect(box_w: f32, box_h: f32, img_w: f32, img_h: f32, fit: ObjectFit) -> FitRect {
    let full = FitRect {
        src_x: 0.0,
        src_y: 0.0,
        src_w: img_w,
        src_h: img_h,
        dst_x: 0.0,
        dst_y: 0.0,
        dst_w: box_w,
        dst_h: box_h,
    };
    if box_w <= 0.0 || box_h <= 0.0 || img_w <= 0.0 || img_h <= 0.0 {
        return full;
    }
    match fit {
        ObjectFit::Fill => full,
        ObjectFit::None => {
            // Natural size, centered; no scaling, no crop beyond the box.
            let (dw, dh) = (img_w.min(box_w), img_h.min(box_h));
            FitRect {
                src_x: ((img_w - dw) * 0.5).max(0.0),
                src_y: ((img_h - dh) * 0.5).max(0.0),
                src_w: dw,
                src_h: dh,
                dst_x: ((box_w - dw) * 0.5).max(0.0),
                dst_y: ((box_h - dh) * 0.5).max(0.0),
                dst_w: dw,
                dst_h: dh,
            }
        }
        ObjectFit::Contain | ObjectFit::ScaleDown => {
            let scale = (box_w / img_w).min(box_h / img_h);
            let scale = match fit {
                ObjectFit::ScaleDown => scale.min(1.0),
                _ => scale,
            };
            let (dw, dh) = (img_w * scale, img_h * scale);
            FitRect {
                dst_x: (box_w - dw) * 0.5,
                dst_y: (box_h - dh) * 0.5,
                dst_w: dw,
                dst_h: dh,
                ..full
            }
        }
        ObjectFit::Cover => {
            let scale = (box_w / img_w).max(box_h / img_h);
            let (sw, sh) = (box_w / scale, box_h / scale);
            FitRect {
                src_x: (img_w - sw) * 0.5,
                src_y: (img_h - sh) * 0.5,
                src_w: sw,
                src_h: sh,
                ..full
            }
            .with_dst(box_w, box_h)
        }
    }
}

impl FitRect {
    fn with_dst(mut self, w: f32, h: f32) -> FitRect {
        self.dst_x = 0.0;
        self.dst_y = 0.0;
        self.dst_w = w;
        self.dst_h = h;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `pixels` (RGBA8) as a PNG with the given color type settings.
    fn encode_png(
        width: u32,
        height: u32,
        color: png::ColorType,
        bit_depth: png::BitDepth,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, width, height);
            enc.set_color(color);
            enc.set_depth(bit_depth);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(data).unwrap();
        }
        out
    }

    fn gray_fixture() -> Vec<u8> {
        // 4x4 grayscale ramp 0..240.
        let data: Vec<u8> = (0..16u8).map(|i| i * 16).collect();
        encode_png(4, 4, png::ColorType::Grayscale, png::BitDepth::Eight, &data)
    }

    #[test]
    fn png_gray_decodes_to_rgba() {
        let img = decode_png(&gray_fixture()).unwrap();
        assert_eq!((img.width, img.height), (4, 4));
        assert_eq!(img.pixel(0, 0), Some([0, 0, 0, 255]));
        assert_eq!(img.pixel(3, 3), Some([240, 240, 240, 255]));
        assert_eq!(dimensions(&gray_fixture()), Some((4, 4)));
    }

    #[test]
    fn png_rgb_and_rgba_round_trip() {
        let rgb: Vec<u8> = (0..48).collect();
        let img = decode_png(&encode_png(
            4,
            4,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &rgb,
        ))
        .unwrap();
        assert_eq!((img.width, img.height), (4, 4));
        assert_eq!(img.pixel(0, 0), Some([0, 1, 2, 255]));
        let rgba: Vec<u8> = (0..64).map(|i| i as u8).collect();
        let img = decode_png(&encode_png(
            4,
            4,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &rgba,
        ))
        .unwrap();
        assert_eq!(img.pixel(1, 0), Some([4, 5, 6, 7]));
    }

    #[test]
    fn png_palette_and_interlaced_accepted() {
        // 2-color palette, 2x2.
        let mut pal = vec![255u8, 0, 0, 0, 0, 255];
        pal.resize(256 * 3, 0);
        let data = vec![0u8, 1, 1, 0];
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 2, 2);
            enc.set_color(png::ColorType::Indexed);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_palette(pal);
            enc.write_header().unwrap().write_image_data(&data).unwrap();
        }
        let img = decode_png(&out).unwrap();
        assert_eq!(img.pixel(0, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(1, 0), Some([0, 0, 255, 255]));

        let adam7 = adam7_png_8x8_rgb();
        let img = decode_png(&adam7).unwrap();
        assert_eq!((img.width, img.height), (8, 8));
        // Spot-check the deterministic gradient: R=x*32, G=y*32, B=(x+y)*16.
        assert_eq!(img.pixel(0, 0), Some([0, 0, 0, 255]));
        assert_eq!(img.pixel(7, 0), Some([224, 0, 112, 255]));
        assert_eq!(img.pixel(0, 7), Some([0, 224, 112, 255]));
        assert_eq!(img.pixel(7, 7), Some([224, 224, 224, 255]));
        assert_eq!(img.pixel(3, 5), Some([96, 160, 128, 255]));
        assert_eq!(dimensions(&adam7), Some((8, 8)));
    }

    /// Hand-rolled Adam7-interlaced 8x8 RGB8 PNG writer (the `png` crate's
    /// encoder only writes non-interlaced): unfiltered pass scanlines,
    /// flate2 compression, correct CRCs.
    fn adam7_png_8x8_rgb() -> Vec<u8> {
        use flate2::{write::ZlibEncoder, Compression};
        use std::io::Write;
        const PASSES: [(u32, u32, u32, u32); 7] = [
            (0, 0, 8, 8),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ];
        let mut raw = Vec::new();
        for &(x0, y0, dx, dy) in &PASSES {
            let mut y = y0;
            while y < 8 {
                raw.push(0u8); // filter: None
                let mut x = x0;
                while x < 8 {
                    raw.extend_from_slice(&[(x * 32) as u8, (y * 32) as u8, ((x + y) * 16) as u8]);
                    x += dx;
                }
                y += dy;
            }
        }
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&raw).unwrap();
        let idat = enc.finish().unwrap();

        fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(tag);
            out.extend_from_slice(data);
            let mut h = crc32fast::Hasher::new();
            h.update(tag);
            h.update(data);
            out.extend_from_slice(&h.finalize().to_be_bytes());
        }
        let mut out = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&8u32.to_be_bytes());
        ihdr.extend_from_slice(&8u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 1]); // 8-bit RGB, no compression/filter change, Adam7
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &idat);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn rejects_garbage_and_bombs() {
        assert!(matches!(
            decode_image(b"nope", None),
            Err(MediaError::Unsupported(_))
        ));
        assert!(matches!(
            decode_image(b"\x89PNG\r\n\x1a\njunk", Some("image/png")),
            Err(MediaError::Decode(_))
        ));
        // GIF is known, but a header-only file has no frames to decode.
        let gif = [b'G', b'I', b'F', b'8', b'9', b'a', 10, 0, 20, 0, 0, 0];
        assert!(matches!(
            decode_image(&gif, None),
            Err(MediaError::Decode(_))
        ));
        assert_eq!(dimensions(&gif), Some((10, 20)));
        assert!(matches!(
            check_size(100_000, 100_000),
            Err(MediaError::TooLarge { .. })
        ));
    }

    #[test]
    fn jpeg_fixtures_decode() {
        for (name, w, h) in [
            ("testdata/baseline.jpg", 16, 16),
            ("testdata/progressive.jpg", 16, 16),
            ("testdata/444.jpg", 16, 16),
            ("testdata/420.jpg", 16, 16),
            ("testdata/gray.jpg", 8, 8),
        ] {
            let bytes = std::fs::read(env!("CARGO_MANIFEST_DIR").to_string() + "/" + name).unwrap();
            let img =
                decode_image(&bytes, Some("image/jpeg")).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((img.width, img.height), (w, h), "{name}");
            assert_eq!(dimensions(&bytes), Some((w, h)), "{name} dims");
            assert_eq!(img.pixels.len(), w as usize * h as usize * 4);
            // Solid (200,50,80)-ish survive quantization approximately.
            if w == 16 {
                let [r, g, b, a] = img.pixel(8, 8).unwrap();
                assert!((r as i32 - 200).abs() < 30, "{name} r={r}");
                assert!((g as i32 - 50).abs() < 30, "{name} g={g}");
                assert!((b as i32 - 80).abs() < 30, "{name} b={b}");
                assert_eq!(a, 255);
            }
        }
    }

    #[test]
    fn jpeg_cmyk_falls_back_to_header_dims() {
        let bytes =
            std::fs::read(env!("CARGO_MANIFEST_DIR").to_string() + "/testdata/cmyk.jpg").unwrap();
        assert_eq!(jpeg_components(&bytes), Some(4));
        assert!(matches!(
            decode_image(&bytes, Some("image/jpeg")),
            Err(MediaError::Decode(_))
        ));
        // …while the header path still yields geometry for the placeholder.
        assert_eq!(dimensions(&bytes), Some((8, 8)));
    }

    // ---- GIF (Q8) ---------------------------------------------------------

    /// Build a multi-frame GIF with exact per-frame control: global palette
    /// (index = color), per-frame disposal/delay/region. Frames are written
    /// in order after `set_repeat` (must precede frame writes). Note the
    /// encoder skips the NETSCAPE extension entirely for `Finite(0)`, so
    /// `None` and `Some(Repeat::Finite(0))` both produce play-once files.
    fn gif_fixture(
        w: u16,
        h: u16,
        repeat: Option<gif::Repeat>,
        frames: &[gif::Frame<'static>],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = gif::Encoder::new(
            &mut out,
            w,
            h,
            &[0xFF, 0, 0, 0, 0xFF, 0, 0, 0, 0xFF], // 1=green, 2=blue
        )
        .unwrap();
        if let Some(r) = repeat {
            enc.set_repeat(r).unwrap();
        }
        for f in frames {
            enc.write_frame(f).unwrap();
        }
        drop(enc);
        out
    }

    fn solid_indexed(w: u16, h: u16, idx: u8, transparent: Option<u8>) -> gif::Frame<'static> {
        gif::Frame::from_indexed_pixels(w, h, vec![idx; w as usize * h as usize], transparent)
    }

    #[test]
    fn gif_still_decodes_first_composited_frame() {
        let bytes = gif_fixture(
            4,
            4,
            None,
            &[
                gif::Frame {
                    delay: 5, // 50ms
                    dispose: gif::DisposalMethod::Keep,
                    ..solid_indexed(4, 4, 1, None)
                },
                gif::Frame {
                    delay: 5,
                    dispose: gif::DisposalMethod::Keep,
                    ..solid_indexed(2, 2, 2, None)
                },
            ],
        );
        let img = decode_image(&bytes, Some("image/gif")).unwrap();
        assert_eq!((img.width, img.height), (4, 4));
        assert_eq!(img.pixels.len(), 4 * 4 * 4);
        // Frame 0 is all-green, fully opaque.
        assert_eq!(&img.pixels[0..8], &[0, 0xFF, 0, 0xFF, 0, 0xFF, 0, 0xFF]);
        // The animation path sees both frames.
        let (frames, loop_count) = decode_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(loop_count, None); // no repeat set → play once
        assert_eq!(frames[0].delay_ms, 50);
    }

    #[test]
    fn gif_animation_composites_and_restores_background() {
        // F1: full green (Keep). F2: top row transparent (keeps F1's green),
        // bottom row blue (composites over) — with Background disposal, so
        // the *next* frame would start from the whole region cleared.
        let bytes = gif_fixture(
            2,
            2,
            None,
            &[
                gif::Frame {
                    delay: 2,
                    dispose: gif::DisposalMethod::Keep,
                    ..solid_indexed(2, 2, 1, None)
                },
                gif::Frame {
                    delay: 3,
                    dispose: gif::DisposalMethod::Background,
                    ..gif::Frame::from_indexed_pixels(
                        2,
                        2,
                        vec![0u8, 0, 2, 2], // top transparent, bottom blue
                        Some(0),
                    )
                },
            ],
        );
        let (frames, loop_count) = decode_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(loop_count, None); // no NETSCAPE ext → play once
        assert_eq!(frames[0].delay_ms, 20);
        assert_eq!(frames[1].delay_ms, 30);
        let px = |f: usize, x: usize, y: usize| -> [u8; 4] {
            let i = (y * 2 + x) * 4;
            frames[f].rgba[i..i + 4].try_into().unwrap()
        };
        // Frame 1: top row stays green (transparent pixels skip compositing),
        // bottom row is blue (composited over the green canvas).
        assert_eq!(px(1, 0, 0), [0, 0xFF, 0, 0xFF]);
        assert_eq!(px(1, 1, 0), [0, 0xFF, 0, 0xFF]);
        assert_eq!(px(1, 0, 1), [0, 0, 0xFF, 0xFF]);
        assert_eq!(px(1, 1, 1), [0, 0, 0xFF, 0xFF]);
        // Background disposal cleared the whole drawn rect to transparent:
        // chain a third green frame and it must composite onto a cleared
        // canvas — pure green everywhere (already covered by the dedicated
        // disposal test below).
    }

    #[test]
    fn gif_background_disposal_clears_to_transparent() {
        // F1 full green (Keep). F2 full blue (Background). F3 full green
        // again — composited over F2's cleared canvas, so F3 is pure green.
        let bytes = gif_fixture(
            2,
            1,
            None,
            &[
                gif::Frame {
                    delay: 1,
                    dispose: gif::DisposalMethod::Keep,
                    ..solid_indexed(2, 1, 1, None)
                },
                gif::Frame {
                    delay: 1,
                    dispose: gif::DisposalMethod::Background,
                    ..solid_indexed(2, 1, 2, None)
                },
                gif::Frame {
                    delay: 1,
                    dispose: gif::DisposalMethod::Keep,
                    ..solid_indexed(2, 1, 1, None)
                },
            ],
        );
        let (frames, _) = decode_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 3);
        let px = |f: usize, i: usize| -> [u8; 4] {
            frames[f].rgba[i * 4..i * 4 + 4].try_into().unwrap()
        };
        assert_eq!(px(1, 0), [0, 0, 0xFF, 0xFF]); // blue frame is blue
        assert_eq!(px(2, 0), [0, 0xFF, 0, 0xFF]); // green over cleared canvas
    }

    #[test]
    fn gif_previous_disposal_restores_snapshot() {
        // F1 green full (Keep) → canvas green. F2 blue full (Background,
        // snapshot = green taken before compositing). F3 transparent
        // (Previous) → canvas rolls back to green, F3 still paints
        // transparent-over-green.
        let bytes = gif_fixture(
            1,
            1,
            None,
            &[
                gif::Frame {
                    delay: 1,
                    dispose: gif::DisposalMethod::Keep,
                    ..solid_indexed(1, 1, 1, None)
                },
                gif::Frame {
                    delay: 1,
                    dispose: gif::DisposalMethod::Background,
                    ..solid_indexed(1, 1, 2, None)
                },
                gif::Frame {
                    delay: 1,
                    dispose: gif::DisposalMethod::Previous,
                    ..solid_indexed(1, 1, 1, Some(0))
                },
            ],
        );
        let (frames, _) = decode_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 3);
        let px = |f: usize| -> [u8; 4] { frames[f].rgba[0..4].try_into().unwrap() };
        assert_eq!(px(0), [0, 0xFF, 0, 0xFF]);
        assert_eq!(px(1), [0, 0, 0xFF, 0xFF]);
        // F3's transparent pixel skips compositing, so the restored-green
        // canvas shows through.
        assert_eq!(px(2), [0, 0xFF, 0, 0xFF]);
    }

    #[test]
    fn gif_infinite_repeat_maps_to_max() {
        // NETSCAPE 0 = infinite: decoder yields Repeat::Infinite.
        let infinite = gif_fixture(
            1,
            1,
            Some(gif::Repeat::Infinite),
            &[gif::Frame {
                delay: 1,
                dispose: gif::DisposalMethod::Any,
                ..solid_indexed(1, 1, 1, None)
            }],
        );
        let (_, loop_count) = decode_frames(&infinite).unwrap();
        assert_eq!(loop_count, Some(u16::MAX));
        // No NETSCAPE extension at all → Finite(0) → play once (None).
        let once = gif_fixture(
            1,
            1,
            None,
            &[gif::Frame {
                delay: 1,
                dispose: gif::DisposalMethod::Any,
                ..solid_indexed(1, 1, 1, None)
            }],
        );
        let (_, loop_count) = decode_frames(&once).unwrap();
        assert_eq!(loop_count, None);
    }

    #[test]
    fn gif_huge_canvas_rejected_before_decode() {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&u16::MAX.to_le_bytes()); // w
        bytes.extend_from_slice(&u16::MAX.to_le_bytes()); // h
        bytes.extend_from_slice(&[0x00]);
        assert!(decode_frames(&bytes).is_err());
    }

    #[test]
    fn gif_frame_count_capped() {
        // 520 tiny frames: decode stops at MAX_FRAMES instead of buffering
        // an unbounded sequence (Q8 memory rule).
        let frames: Vec<gif::Frame<'static>> = (0..520)
            .map(|_| gif::Frame {
                delay: 1,
                dispose: gif::DisposalMethod::Keep,
                ..solid_indexed(1, 1, 1, None)
            })
            .collect();
        let bytes = gif_fixture(1, 1, None, &frames);
        let (frames, _) = decode_frames(&bytes).unwrap();
        assert_eq!(frames.len(), MAX_FRAMES);
    }

    /// Encode `pixels` (RGB8, 3 bytes/px) as a lossless WebP (VP8L).
    fn webp_fixture_rgb(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        image_webp::WebPEncoder::new(&mut out)
            .encode(pixels, width, height, image_webp::ColorType::Rgb8)
            .unwrap();
        out
    }

    /// Encode `pixels` (RGBA8) as a lossless WebP with alpha (VP8L).
    fn webp_fixture_rgba(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        image_webp::WebPEncoder::new(&mut out)
            .encode(pixels, width, height, image_webp::ColorType::Rgba8)
            .unwrap();
        out
    }

    #[test]
    fn webp_lossless_round_trips_pixels() {
        // 4x2 RGB: first row red ramp, second row blue ramp.
        let mut px = Vec::new();
        for x in 0..4u8 {
            px.extend_from_slice(&[x * 60, 0, 0]);
        }
        for x in 0..4u8 {
            px.extend_from_slice(&[0, 0, x * 60]);
        }
        let bytes = webp_fixture_rgb(4, 2, &px);

        assert_eq!(sniff(&bytes), Some("image/webp"));
        assert_eq!(dimensions(&bytes), Some((4, 2)));

        let img = decode_image(&bytes, None).unwrap();
        assert_eq!((img.width, img.height), (4, 2));
        assert_eq!(img.pixel(0, 0), Some([0, 0, 0, 255]));
        assert_eq!(img.pixel(3, 0), Some([180, 0, 0, 255]));
        assert_eq!(img.pixel(1, 1), Some([0, 0, 60, 255]));
        // MIME hint and magic sniffing agree.
        let img = decode_image(&bytes, Some("image/webp")).unwrap();
        assert_eq!(img.pixel(2, 0), Some([120, 0, 0, 255]));
    }

    #[test]
    fn webp_alpha_preserved() {
        // 2x2 RGBA with a transparent quadrant.
        let px: Vec<u8> = vec![
            255, 0, 0, 255, 0, 255, 0, 128, //
            0, 0, 255, 0, 10, 20, 30, 255,
        ];
        let bytes = webp_fixture_rgba(2, 2, &px);
        let img = decode_webp(&bytes).unwrap();
        assert_eq!(img.pixel(0, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(1, 0), Some([0, 255, 0, 128]));
        assert_eq!(img.pixel(0, 1), Some([0, 0, 255, 0]));
        assert_eq!(img.pixel(1, 1), Some([10, 20, 30, 255]));
    }

    #[test]
    fn webp_truncated_rejected() {
        let bytes = webp_fixture_rgb(4, 2, &[0; 24]);
        // Cut the file mid-chunk: header parses, decode must not.
        let cut = &bytes[..bytes.len() - 4];
        assert!(matches!(decode_webp(cut), Err(MediaError::Decode(_))));
        // And a corrupt RIFF tag is unsupported, not a decode error.
        let mut bad = bytes.clone();
        bad[8..12].copy_from_slice(b"JUNK");
        assert!(matches!(
            decode_image(&bad, None),
            Err(MediaError::Unsupported(_))
        ));
    }

    #[test]
    fn webp_dimensions_header_math() {
        // A full encode then verify the cheap header walker against the
        // decoder's own answer, plus truncated-prefix tolerance.
        let bytes = webp_fixture_rgb(7, 3, &[0; 7 * 3 * 3]);
        assert_eq!(webp_dimensions(&bytes), Some((7, 3)));
        assert_eq!(dimensions(&bytes), Some((7, 3)));
        // Truncate into the chunk body: RIFF walk still finds VP8L dims.
        let cut = &bytes[..bytes.len() - 2];
        assert_eq!(dimensions(cut), Some((7, 3)));
    }

    // ---- SVG (Q10) ---------------------------------------------------------

    const SVG_NS: &str = "http://www.w3.org/2000/svg";

    #[test]
    fn svg_viewbox_only_sizing() {
        // No width/height attributes: intrinsic size comes from the viewBox.
        let bytes = format!(
            r##"<svg xmlns="{}" viewBox="0 0 100 50"><rect width="100" height="50" fill="#0a1df5"/></svg>"##,
            SVG_NS
        );
        assert_eq!(sniff(bytes.as_bytes()), Some("image/svg+xml"));
        assert_eq!(dimensions(bytes.as_bytes()), Some((100, 50)));
        let img = decode_svg(bytes.as_bytes(), 100, 50).unwrap();
        assert_eq!((img.width, img.height), (100, 50));
        assert_eq!(img.pixel(50, 25), Some([0x0A, 0x1D, 0xF5, 0xFF]));
    }

    #[test]
    fn svg_explicit_px_sizing_scales_to_request() {
        let bytes = format!(
            r##"<svg xmlns="{}" width="120" height="80"><rect width="120" height="80" fill="#e03220"/></svg>"##,
            SVG_NS
        );
        assert_eq!(dimensions(bytes.as_bytes()), Some((120, 80)));
        // Requested box differs from the intrinsic size: the raster comes
        // back at the request with the content uniformly scaled.
        let img = decode_svg(bytes.as_bytes(), 240, 160).unwrap();
        assert_eq!((img.width, img.height), (240, 160));
        assert_eq!(img.pixel(120, 80), Some([0xE0, 0x32, 0x20, 0xFF]));
    }

    #[test]
    fn svg_fit_centers_aspect_mismatch() {
        // Square SVG into a 2:1 box: scale = 0.4, drawn width 40 centered in
        // 80 (x from 20..60); the margins stay transparent.
        let bytes = format!(
            r##"<svg xmlns="{}" viewBox="0 0 100 100"><rect width="100" height="100" fill="#ff0000"/></svg>"##,
            SVG_NS
        );
        let img = decode_svg(bytes.as_bytes(), 80, 40).unwrap();
        assert_eq!((img.width, img.height), (80, 40));
        assert_eq!(img.pixel(40, 20), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(5, 20), Some([0, 0, 0, 0]));
        assert_eq!(img.pixel(75, 20), Some([0, 0, 0, 0]));
    }

    #[test]
    fn svg_gradient_renders_expected_average() {
        // Vertical black→white gradient across the full canvas: the mean
        // channel value must land near mid-gray (resvg correctness smoke).
        let bytes = format!(
            r##"<svg xmlns="{}" viewBox="0 0 10 10"><defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#000"/><stop offset="1" stop-color="#fff"/></linearGradient></defs><rect width="10" height="10" fill="url(#g)"/></svg>"##,
            SVG_NS
        );
        let img = decode_svg(bytes.as_bytes(), 20, 20).unwrap();
        let sum: u64 = (0..20u32)
            .flat_map(|y| (0..20u32).map(move |x| (x, y)))
            .map(|(x, y)| img.pixel(x, y).unwrap()[0] as u64)
            .sum();
        let mean = (sum / 400) as i64;
        assert!((mean - 127).abs() <= 6, "mean gray {mean}");
        // Gradient direction: top dark, bottom light.
        assert!(img.pixel(10, 1).unwrap()[0] < img.pixel(10, 18).unwrap()[0]);
        assert!(img.pixel(0, 5).unwrap()[3] == 0xFF);
    }

    #[test]
    fn svg_script_element_is_dropped_at_parse() {
        // usvg drops <script>/SMIL at parse (the Q10 purity guarantee): an
        // SVG with a script block renders pixel-identical to one without.
        let body = r##"<rect width="16" height="16" fill="#0a8f3c"/>"##;
        let with_script = format!(
            r#"<svg xmlns="{ns}" viewBox="0 0 16 16"><script type="text/javascript">alert(1)</script>{body}</svg>"#,
            ns = SVG_NS
        );
        let without = format!(
            r#"<svg xmlns="{ns}" viewBox="0 0 16 16">{body}</svg>"#,
            ns = SVG_NS
        );
        let a = decode_svg(with_script.as_bytes(), 32, 32).unwrap();
        let b = decode_svg(without.as_bytes(), 32, 32).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn svg_alpha_corners_and_bad_input() {
        // A circle on a transparent canvas: corners transparent, center opaque.
        let bytes = format!(
            r##"<svg xmlns="{}" viewBox="0 0 8 8"><circle cx="4" cy="4" r="3" fill="#000"/></svg>"##,
            SVG_NS
        );
        let img = decode_svg(bytes.as_bytes(), 8, 8).unwrap();
        assert_eq!(img.pixel(0, 0), Some([0, 0, 0, 0]));
        assert_eq!(img.pixel(4, 4).unwrap()[3], 0xFF);
        // Non-markup text is not SVG.
        assert_eq!(sniff(b"plain text"), None);
        // Markup that opens like SVG sniffs as SVG but fails the parse.
        let junk = b"<svg><not an svg at all";
        assert_eq!(sniff(junk), Some("image/svg+xml"));
        assert!(matches!(decode_svg(junk, 8, 8), Err(MediaError::Decode(_))));
        // A `<svg` mention inside a non-SVG document does not sniff as SVG.
        let html = b"<!DOCTYPE html><html><body>svg later</body></html>";
        assert_eq!(sniff(html), None);
    }

    #[test]
    fn svg_bomb_box_rejected_before_render() {
        let bytes = format!(
            r#"<svg xmlns="{}" viewBox="0 0 4 4"><rect width="4" height="4"/></svg>"#,
            SVG_NS
        );
        assert!(matches!(
            decode_svg(bytes.as_bytes(), 100_000, 100_000),
            Err(MediaError::TooLarge { .. })
        ));
    }

    #[test]
    fn object_fit_geometry() {
        // 200x100 box, 100x100 intrinsic.
        let c = object_fit_rect(200.0, 100.0, 100.0, 100.0, ObjectFit::Contain);
        assert_eq!((c.dst_w, c.dst_h), (100.0, 100.0));
        assert!((c.dst_x - 50.0).abs() < 0.01 && c.dst_y.abs() < 0.01);
        assert_eq!((c.src_w, c.src_h), (100.0, 100.0));
        let c = object_fit_rect(200.0, 100.0, 100.0, 100.0, ObjectFit::Cover);
        assert_eq!((c.dst_w, c.dst_h), (200.0, 100.0));
        assert_eq!((c.src_w, c.src_h), (100.0, 50.0));
        assert!((c.src_y - 25.0).abs() < 0.01);
        let f = object_fit_rect(200.0, 100.0, 100.0, 100.0, ObjectFit::Fill);
        assert_eq!(
            (f.dst_w, f.dst_h, f.src_w, f.src_h),
            (200.0, 100.0, 100.0, 100.0)
        );
        let s = object_fit_rect(200.0, 100.0, 50.0, 50.0, ObjectFit::ScaleDown);
        assert_eq!((s.dst_w, s.dst_h), (50.0, 50.0));
        let n = object_fit_rect(200.0, 100.0, 400.0, 400.0, ObjectFit::None);
        assert_eq!((n.dst_w, n.dst_h), (200.0, 100.0));
        assert_eq!((n.src_w, n.src_h), (200.0, 100.0));
    }
}
