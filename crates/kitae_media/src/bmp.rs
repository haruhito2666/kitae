//! BMP (Windows/OS-2 bitmap) decode: `BITMAPCOREHEADER` (12), the OS/2 v2/v3
//! variants (16/52/56/64), `BITMAPINFOHEADER` (40) and the V4/V5 headers
//! (108/124) at 1/2/4/8/16/24/32 bpp, with `BI_RGB`, `BI_BITFIELDS`,
//! `BI_ALPHABITFIELDS`, `BI_RLE8` and `BI_RLE4`.
//!
//! Fail-closed rules — a refusal is a [`MediaError`], never a half-drawn
//! raster (the caller keeps its header-dims placeholder):
//! * The 64 MP bomb guard runs on the *header* before any allocation, and
//!   a declared pixel array that does not fit the file is rejected before
//!   the canvas is allocated, so a 40000x40000 header costs nothing.
//! * Truncated pixel data (or an RLE stream that runs out before `end of
//!   bitmap`) is a `Decode` error, not a partly-black image.
//! * `BI_JPEG`/`BI_PNG` embedded streams, unknown header sizes and unknown
//!   compressions are refused rather than guessed at.
//!
//! Alpha: a 32bpp `BI_RGB` file declares no alpha channel, so the 4th byte
//! is ignored and the image is opaque — the Skia/Chrome reading, and the
//! safe one (honouring a stray `0x00` plane is how real files disappear).
//! Real alpha comes from the declared masks (`V4`/`V5`/`BI_ALPHABITFIELDS`),
//! or from the 4th byte when a container says it is one — the ICO
//! convention, see [`Alpha::FourthByte`].

use crate::{check_size, MediaError, RgbaImage};

const BI_RGB: u32 = 0;
const BI_RLE8: u32 = 1;
const BI_RLE4: u32 = 2;
const BI_BITFIELDS: u32 = 3;
const BI_JPEG: u32 = 4;
const BI_PNG: u32 = 5;
const BI_ALPHABITFIELDS: u32 = 6;

/// Whether a 32bpp image's 4th byte is a real alpha channel.
pub(crate) enum Alpha {
    /// A `BI_RGB` file: no declared alpha, the image is opaque.
    Ignore,
    /// An ICO entry: 32bpp means BGRA with real transparency.
    FourthByte,
}

/// What a DIB header declares, for the pixel reader and for container
/// readers (an ICO entry needs the header length and bit depth to find the
/// pixel data).
#[derive(Debug, Clone, Copy)]
pub(crate) struct DibFacts {
    /// Size of the DIB header itself (12/16/40/52/56/64/108/124).
    pub header_len: usize,
    pub width: u32,
    /// `|biHeight|`: doubled in an ICO entry (bitmap plus AND mask).
    pub height: u32,
    /// Negative `biHeight`: rows are stored top-down (RLE is not).
    pub top_down: bool,
    pub bpp: u16,
    pub comp: u32,
    /// Palette entries declared or implied (0 above 8 bpp).
    pub ncolors: u32,
}

fn u16le(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(off)?, *b.get(off + 1)?]))
}

fn u32le(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(off)?,
        *b.get(off + 1)?,
        *b.get(off + 2)?,
        *b.get(off + 3)?,
    ]))
}

/// A DIB header size we know how to read. Anything else is a `None`
/// (unknown header) rather than a misparse.
fn known_header_len(len: u32) -> bool {
    matches!(len, 12 | 16 | 40 | 52 | 56 | 64 | 108 | 124)
}

/// Parse a DIB header (the bytes right after a 14-byte `BITMAPFILEHEADER`,
/// or a whole ICO entry). `None` for a truncated/unknown/multi-plane header
/// or a bit depth outside 1..=32.
pub(crate) fn dib_facts(dib: &[u8]) -> Option<DibFacts> {
    let header_len = u32le(dib, 0)? as usize;
    if !known_header_len(header_len as u32) || dib.len() < header_len {
        return None;
    }
    // V2 and later share the 40-byte field layout (size 0, width 4, height
    // 8, planes 12, bit count 14, compression 16, …, clrUsed 32); the two
    // OS/2 headers predate it with 16-bit width/height and no palette count
    // (v2 adds a u32 compression at offset 10).
    let (width, height_raw, planes, bpp, comp) = if header_len >= 40 {
        (
            u32le(dib, 4)? as i32,
            u32le(dib, 8)? as i32,
            u16le(dib, 12)?,
            u16le(dib, 14)?,
            u32le(dib, 16)?,
        )
    } else {
        (
            u16le(dib, 4)? as i32,
            u16le(dib, 6)? as i32,
            u16le(dib, 8)?,
            u16le(dib, 10)?,
            if header_len >= 16 { u32le(dib, 10)? } else { 0 },
        )
    };
    if planes != 1 || width <= 0 || height_raw == 0 {
        return None;
    }
    if !matches!(bpp, 1 | 2 | 4 | 8 | 16 | 24 | 32) {
        return None;
    }
    // Palette size: declared (`biClrUsed`), else the full range for the
    // depth. A declared count above the depth's range is a bad header.
    let max_colors = match bpp {
        1 => 2,
        2 => 4,
        4 => 16,
        8 => 256,
        _ => 0,
    };
    let declared = if header_len >= 40 { u32le(dib, 32)? } else { 0 };
    let ncolors = if max_colors == 0 {
        0
    } else if declared == 0 {
        max_colors
    } else if declared <= max_colors {
        declared
    } else {
        return None;
    };
    Some(DibFacts {
        header_len,
        width: width as u32,
        height: height_raw.unsigned_abs(),
        top_down: height_raw < 0,
        bpp,
        comp,
        ncolors,
    })
}

/// Bytes per pixel row, padded to a 4-byte boundary. A 1bpp `AND` mask uses
/// the same rule with `bpp = 1`.
pub(crate) fn row_stride(width: u32, bpp: u16) -> usize {
    (((width as u64 * bpp as u64 + 31) / 32) * 4) as usize
}

/// Intrinsic dimensions from the headers alone (no pixel read), so even a
/// truncated file sizes the placeholder. `None` when the file is not a BMP
/// or the DIB header is unusable.
pub fn bmp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 14 || &bytes[0..2] != b"BM" {
        return None;
    }
    let f = dib_facts(bytes.get(14..)?)?;
    Some((f.width, f.height))
}

/// Decode a standalone BMP file (14-byte `BITMAPFILEHEADER` + DIB + pixels)
/// to RGBA8. A 32bpp image is opaque (see the module docs).
pub fn decode_bmp(bytes: &[u8]) -> Result<RgbaImage, MediaError> {
    if bytes.len() < 14 || &bytes[0..2] != b"BM" {
        return Err(MediaError::Decode("bmp: no BM signature".into()));
    }
    let off = u32le(bytes, 10).ok_or_else(|| MediaError::Decode("bmp: short header".into()))?;
    let off = off as usize;
    if !(14..=bytes.len()).contains(&off) {
        return Err(MediaError::Decode(format!("bmp: pixel offset {off}")));
    }
    decode_dib(&bytes[14..], off - 14, Alpha::Ignore)
}

/// Decode a DIB that carries no file header — an ICO entry, whose container
/// has already worked out where the pixel data starts. `pixel_off` is that
/// offset, relative to the start of `dib`.
pub(crate) fn decode_dib(
    dib: &[u8],
    pixel_off: usize,
    alpha: Alpha,
) -> Result<RgbaImage, MediaError> {
    let f = dib_facts(dib).ok_or_else(|| MediaError::Decode("bmp: bad DIB header".into()))?;
    check_size(f.width, f.height)?;
    if !matches!(
        f.comp,
        BI_RGB | BI_RLE8 | BI_RLE4 | BI_BITFIELDS | BI_ALPHABITFIELDS
    ) {
        return Err(MediaError::Decode(match f.comp {
            BI_JPEG | BI_PNG => format!("bmp: embedded stream (compression {})", f.comp),
            c => format!("bmp: compression {c}"),
        }));
    }
    if pixel_off < f.header_len || pixel_off > dib.len() {
        return Err(MediaError::Decode(format!("bmp: pixel offset {pixel_off}")));
    }
    let pal_stride = palette_stride(pixel_off - f.header_len, f.ncolors);
    let palette = read_palette(dib, &f, pal_stride);
    let image = match f.comp {
        BI_RLE8 | BI_RLE4 => {
            if !matches!(f.bpp, 4 | 8) {
                return Err(MediaError::Decode(format!("bmp: rle at {} bpp", f.bpp)));
            }
            if f.top_down {
                return Err(MediaError::Decode("bmp: rle cannot be top-down".into()));
            }
            read_rle(dib, &f, &palette, pixel_off)?
        }
        _ => {
            let ch = channels(&f, dib, alpha)?;
            read_uncompressed(dib, &f, pixel_off, &palette, ch.as_ref())?
        }
    };
    Ok(image)
}

/// Palette entries are 3 (`RGBTRIPLE`) or 4 (`RGBQUAD`) bytes — both
/// layouts occur, ICO entries being the 4-byte ones. Anything else means the
/// palette is not where `pixel_off` says it is, so use 3.
fn palette_stride(avail: usize, ncolors: u32) -> usize {
    if ncolors == 0 {
        return 0;
    }
    match avail / ncolors as usize {
        3 => 3,
        4 => 4,
        _ => 3,
    }
}

/// Palette as RGBA quadruples, always opaque: a 4-byte palette entry's last
/// byte is padding in the BMP layout and in the ICO layout alike (Windows
/// writes zeros there), and an indexed image's transparency comes from the
/// ICO `AND` mask. Entries past the end of the payload read as black; the
/// count is clamped to the depth's range, so this cannot run away.
fn read_palette(dib: &[u8], f: &DibFacts, stride: usize) -> Vec<[u8; 4]> {
    let mut out = vec![[0u8, 0, 0, 0xFF]; f.ncolors as usize];
    for (i, slot) in out.iter_mut().enumerate() {
        let at = f.header_len + i * stride;
        let Some(px) = dib.get(at..at + 3) else { break };
        slot[0] = px[2]; // BGR to RGBA
        slot[1] = px[1];
        slot[2] = px[0];
    }
    out
}

/// A color channel as `(shift, max_value)` for `v >> shift` extraction.
type Channel = (u32, u32);

struct Channels {
    r: Channel,
    g: Channel,
    b: Channel,
    a: Option<Channel>,
}

fn split_mask(mask: u32) -> Option<Channel> {
    if mask == 0 {
        return None;
    }
    let shift = mask.trailing_zeros();
    let width = (mask >> shift).trailing_ones();
    let max = if width >= 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    };
    Some((shift, max))
}

/// Widen a masked channel value to 8 bits (rounded).
fn scale8(v: u32, max: u32) -> u8 {
    if max == 0 {
        return 0;
    }
    ((v as u64 * 255 + max as u64 / 2) / max as u64) as u8
}

/// Channel masks: the header's declared set when it has one (V4/V5 always,
/// `BI_BITFIELDS`/`BI_ALPHABITFIELDS` carry three or four), else the depth's
/// default layout. Declared masks always sit at DIB offset 40.
fn channels(f: &DibFacts, dib: &[u8], alpha: Alpha) -> Result<Option<Channels>, MediaError> {
    if !matches!(f.bpp, 16 | 24 | 32) {
        // 8 bpp and below index a palette; no channel masks exist.
        return Ok(None);
    }
    let mut ch = match f.bpp {
        16 => Channels {
            r: (11, 31),
            g: (6, 31),
            b: (0, 31),
            a: None,
        },
        _ => Channels {
            r: (16, 255),
            g: (8, 255),
            b: (0, 255),
            a: None,
        },
    };
    if f.header_len >= 108 || matches!(f.comp, BI_BITFIELDS | BI_ALPHABITFIELDS) {
        let m = |i: usize| u32le(dib, 40 + 4 * i);
        ch.r = m(0).and_then(split_mask).unwrap_or(ch.r);
        ch.g = m(1).and_then(split_mask).unwrap_or(ch.g);
        ch.b = m(2).and_then(split_mask).unwrap_or(ch.b);
        // The 4th mask is present in V4/V5 and in BI_ALPHABITFIELDS.
        if f.header_len >= 108 || f.comp == BI_ALPHABITFIELDS {
            ch.a = m(3).and_then(split_mask);
        }
    }
    if f.bpp == 32 && matches!(alpha, Alpha::FourthByte) && ch.a.is_none() {
        ch.a = Some((24, 255));
    }
    Ok(Some(ch))
}

fn overflow() -> MediaError {
    MediaError::Decode("bmp: geometry overflow".into())
}

fn read_uncompressed(
    dib: &[u8],
    f: &DibFacts,
    pixel_off: usize,
    palette: &[[u8; 4]],
    ch: Option<&Channels>,
) -> Result<RgbaImage, MediaError> {
    let w = f.width as usize;
    let h = f.height as usize;
    let stride = row_stride(f.width, f.bpp);
    // Refuse a pixel array the file cannot hold *before* allocating it.
    let need = pixel_off
        .checked_add(stride.checked_mul(h).ok_or_else(overflow)?)
        .ok_or_else(overflow)?;
    if need > dib.len() {
        return Err(MediaError::Decode(format!(
            "bmp: {} pixel bytes declared, {} present",
            need,
            dib.len()
        )));
    }
    let mut pixels = vec![0u8; w * h * 4];
    for y in 0..h {
        let row = &dib[pixel_off + y * stride..][..stride];
        let dy = if f.top_down { y } else { h - 1 - y };
        let dst = &mut pixels[dy * w * 4..(dy + 1) * w * 4];
        for x in 0..w {
            dst[x * 4..x * 4 + 4].copy_from_slice(&pixel(f, palette, ch, row, x));
        }
    }
    Ok(RgbaImage {
        width: f.width,
        height: f.height,
        pixels,
    })
}

/// One pixel out of a padded row: an index into the palette, or a channel
/// unpack. Indices past a short palette clamp to its last entry.
fn pixel(
    f: &DibFacts,
    palette: &[[u8; 4]],
    ch: Option<&Channels>,
    row: &[u8],
    x: usize,
) -> [u8; 4] {
    if f.bpp <= 8 {
        let per = 8 / f.bpp as usize;
        let byte = row[x / per];
        let shift = 8 - (x % per + 1) * f.bpp as usize;
        let idx = ((byte >> shift) as usize) & ((1usize << f.bpp) - 1);
        return palette.get(idx).copied().unwrap_or([0, 0, 0, 0xFF]);
    }
    // Masks are `Some` for every depth that is not palette-indexed.
    let Some(ch) = ch else {
        return [0, 0, 0, 0xFF];
    };
    match f.bpp {
        16 => unpack(u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]) as u32, ch),
        24 => unpack(
            row[x * 3] as u32 | (row[x * 3 + 1] as u32) << 8 | (row[x * 3 + 2] as u32) << 16,
            ch,
        ),
        _ => unpack(
            u32::from_le_bytes([row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]]),
            ch,
        ),
    }
}

fn unpack(v: u32, ch: &Channels) -> [u8; 4] {
    let get = |c: Channel| scale8((v >> c.0) & c.1, c.1);
    [
        get(ch.r),
        get(ch.g),
        get(ch.b),
        ch.a.map(get).unwrap_or(0xFF),
    ]
}

/// Run-length decode (`BI_RLE8`/`BI_RLE4`): encoded runs, absolute runs,
/// end-of-line, delta and end-of-bitmap. A run that would write outside the
/// declared rectangle, or a stream that runs out before `end of bitmap`, is
/// a `Decode` error — the canvas never comes back half-painted.
fn read_rle(
    dib: &[u8],
    f: &DibFacts,
    palette: &[[u8; 4]],
    pixel_off: usize,
) -> Result<RgbaImage, MediaError> {
    let w = f.width as usize;
    let h = f.height as usize;
    let four = f.bpp == 4;
    let mut pixels = vec![0u8; w * h * 4];
    let (mut x, mut y, mut pos) = (0usize, 0usize, pixel_off);

    let pair = |p: usize| -> Result<(u8, u8), MediaError> {
        let a = *dib
            .get(p)
            .ok_or_else(|| MediaError::Decode("bmp: rle truncated".into()))?;
        let b = *dib
            .get(p + 1)
            .ok_or_else(|| MediaError::Decode("bmp: rle truncated".into()))?;
        Ok((a, b))
    };
    let put = |pixels: &mut [u8], x: usize, y: usize, idx: u8| -> Result<(), MediaError> {
        if y >= h || x >= w {
            return Err(MediaError::Decode(format!("bmp: rle run at {x},{y}")));
        }
        let c = palette
            .get(idx as usize)
            .copied()
            .unwrap_or([0, 0, 0, 0xFF]);
        let at = (y * w + x) * 4;
        pixels[at..at + 4].copy_from_slice(&c);
        Ok(())
    };

    loop {
        let (count, val) = pair(pos)?;
        pos += 2;
        if count > 0 {
            for i in 0..count as usize {
                put(&mut pixels, x + i, y, val)?;
            }
            x += count as usize;
            continue;
        }
        match val {
            0 => {
                x = 0;
                y += 1;
            }
            1 => break, // end of bitmap
            2 => {
                let (dx, dy) = pair(pos)?;
                pos += 2;
                x += dx as usize;
                y += dy as usize;
            }
            n => {
                // Absolute mode: `n` literal pixels, then word alignment.
                if n as usize > w {
                    return Err(MediaError::Decode(format!("bmp: rle absolute run {n}")));
                }
                // RLE4 absolute runs count pixels but pack two per byte.
                let data_len = if four {
                    (n as usize).div_ceil(2)
                } else {
                    n as usize
                };
                let data = dib
                    .get(pos..pos + data_len)
                    .ok_or_else(|| MediaError::Decode("bmp: rle truncated".into()))?;
                for i in 0..n as usize {
                    let idx = if four {
                        let b = data[i / 2];
                        if i % 2 == 0 {
                            b >> 4
                        } else {
                            b & 0x0F
                        }
                    } else {
                        data[i]
                    };
                    put(&mut pixels, x + i, y, idx)?;
                }
                pos += data_len;
                if (pos - pixel_off) % 2 == 1 {
                    pos += 1;
                }
                x += n as usize;
            }
        }
    }
    Ok(RgbaImage {
        width: f.width,
        height: f.height,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BMP file: 14-byte file header + DIB + 4-byte palette + rows.
    /// `rows` are handed in top-down and written bottom-up, like real
    /// writers.
    fn bmp_file(
        dib_size: usize,
        width: i32,
        height: i32,
        bpp: u16,
        comp: u32,
        palette: &[[u8; 3]],
        rows: &[Vec<u8>],
    ) -> Vec<u8> {
        let off = 14 + dib_size + palette.len() * 4;
        let mut out = Vec::new();
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&0u32.to_le_bytes()); // bfSize, patched below
        out.extend_from_slice(&0u16.to_le_bytes()); // bfReserved1
        out.extend_from_slice(&0u16.to_le_bytes()); // bfReserved2
        out.extend_from_slice(&(off as u32).to_le_bytes()); // bfOffBits
        let mut dib = vec![0u8; dib_size];
        dib[0..4].copy_from_slice(&(dib_size as u32).to_le_bytes());
        dib[4..8].copy_from_slice(&width.to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes()); // planes
        dib[14..16].copy_from_slice(&bpp.to_le_bytes());
        dib[16..20].copy_from_slice(&comp.to_le_bytes());
        dib[32..36].copy_from_slice(&(palette.len() as u32).to_le_bytes());
        if dib_size >= 108 {
            // V4: explicit BGRA masks, so the 4th byte is real alpha.
            for (i, m) in [0x00FF0000u32, 0x0000FF00, 0x000000FF, 0xFF00_0000]
                .iter()
                .enumerate()
            {
                dib[40 + 4 * i..44 + 4 * i].copy_from_slice(&m.to_le_bytes());
            }
        }
        out.extend_from_slice(&dib);
        for c in palette {
            // 4-byte RGBQUAD entries: the ICO palette layout, and what
            // several Windows writers emit in plain files too.
            out.extend_from_slice(&[c[2], c[1], c[0], 0]);
        }
        // Rows arrive top-down; a positive height means storing them
        // bottom-up, a negative one means as-is.
        let rows: Box<dyn Iterator<Item = &Vec<u8>>> = if height < 0 {
            Box::new(rows.iter())
        } else {
            Box::new(rows.iter().rev())
        };
        for row in rows {
            out.extend_from_slice(row);
        }
        let size = out.len() as u32;
        out[2..6].copy_from_slice(&size.to_le_bytes());
        out
    }

    /// Top-down BGR rows, each padded to a 4-byte boundary.
    fn rows_bgr24(width: usize, pixels: &[[u8; 3]]) -> Vec<Vec<u8>> {
        let mut rows = vec![Vec::new(); pixels.len() / width];
        for (i, px) in pixels.iter().enumerate() {
            rows[i / width].extend_from_slice(&[px[2], px[1], px[0]]);
        }
        for r in rows.iter_mut() {
            while r.len() % 4 != 0 {
                r.push(0);
            }
        }
        rows
    }

    #[test]
    fn decodes_24bit_rows_bottom_up() {
        // 2x2: red, green / blue, white.
        let px = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]];
        let bytes = bmp_file(40, 2, 2, 24, BI_RGB, &[], &rows_bgr24(2, &px));
        assert_eq!(bmp_dimensions(&bytes), Some((2, 2)));
        let img = decode_bmp(&bytes).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        // The helper wrote the rows bottom-up, so `rows[0]` is the last
        // pair in the file and therefore the top row of the image.
        assert_eq!(img.pixel(0, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(1, 0), Some([0, 255, 0, 255]));
        assert_eq!(img.pixel(0, 1), Some([0, 0, 255, 255]));
        assert_eq!(img.pixel(1, 1), Some([255, 255, 255, 255]));
    }

    #[test]
    fn top_down_32bit_is_opaque_without_a_declared_mask() {
        // Negative biHeight: rows stored top-down. The 4th byte is ignored
        // for a plain 32bpp BI_RGB file (no declared alpha channel).
        let mut rows = vec![vec![0u8; 8], vec![0u8; 8]];
        rows[0][0..4].copy_from_slice(&[10, 20, 30, 0x00]);
        rows[1][0..4].copy_from_slice(&[40, 50, 60, 0xFF]);
        let bytes = bmp_file(40, 2, -2, 32, BI_RGB, &[], &rows);
        let img = decode_bmp(&bytes).unwrap();
        assert_eq!(img.pixel(0, 0), Some([30, 20, 10, 255]));
        assert_eq!(img.pixel(0, 1), Some([60, 50, 40, 255]));
    }

    #[test]
    fn v4_header_honours_the_declared_alpha_mask() {
        let mut rows = vec![vec![0u8; 8], vec![0u8; 8]];
        rows[0][0..4].copy_from_slice(&[1, 2, 3, 0x80]);
        rows[0][4..8].copy_from_slice(&[4, 5, 6, 0xFF]);
        rows[1][0..4].copy_from_slice(&[7, 8, 9, 0xFF]);
        let bytes = bmp_file(108, 2, -2, 32, BI_RGB, &[], &rows);
        let img = decode_bmp(&bytes).unwrap();
        assert_eq!(img.pixel(0, 0), Some([3, 2, 1, 0x80]));
        assert_eq!(img.pixel(1, 0), Some([6, 5, 4, 255]));
        assert_eq!(img.pixel(0, 1), Some([9, 8, 7, 255]));
    }

    #[test]
    fn paletted_8_4_and_1_bpp() {
        let pal = [[0, 0, 0], [255, 0, 0], [0, 255, 0]];
        // 8bpp: indices 0,1,2,1 (row padded to 4 bytes).
        let bytes = bmp_file(40, 4, 1, 8, BI_RGB, &pal, &[vec![0, 1, 2, 1, 0]]);
        let img = decode_bmp(&bytes).unwrap();
        assert_eq!(img.pixel(0, 0), Some([0, 0, 0, 255]));
        assert_eq!(img.pixel(1, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(2, 0), Some([0, 255, 0, 255]));
        assert_eq!(img.pixel(3, 0), Some([255, 0, 0, 255]));

        // 4bpp: eight pixels in four bytes, high nibble first.
        let bytes = bmp_file(40, 8, 1, 4, BI_RGB, &pal, &[vec![0x01, 0x20, 0, 0]]);
        let img = decode_bmp(&bytes).unwrap();
        assert_eq!(img.pixel(0, 0), Some([0, 0, 0, 255]));
        assert_eq!(img.pixel(1, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(2, 0), Some([0, 255, 0, 255]));

        // 1bpp: a set bit is palette[1], MSB first, padding bits ignored.
        let bytes = bmp_file(
            40,
            8,
            1,
            1,
            BI_RGB,
            &pal[..2],
            &[vec![0b0101_0000, 0, 0, 0]],
        );
        let img = decode_bmp(&bytes).unwrap();
        assert_eq!(img.pixel(0, 0), Some([0, 0, 0, 255]));
        assert_eq!(img.pixel(1, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(2, 0), Some([0, 0, 0, 255]));
        assert_eq!(img.pixel(3, 0), Some([255, 0, 0, 255]));
    }

    #[test]
    fn rle8_encoded_absolute_and_delta_runs() {
        // 4x2 canvas. Row 0: one red (encoded run), then an absolute run of
        // three green — an odd literal count, so a pad byte must be skipped
        // to stay word-aligned. EOL, a delta to (1,1), one blue, EOB.
        let data = vec![
            0x01, 0x01, // encoded: 1 red
            0x00, 0x03, 0x02, 0x02, 0x02, 0x00, // absolute: 3 green + pad
            0x00, 0x00, // end of line
            0x00, 0x02, 0x01, 0x00, // delta x=1 y=1
            0x01, 0x03, // encoded: 1 blue
            0x00, 0x01, // end of bitmap
        ];
        let pal = [[0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255]];
        let bytes = bmp_file(40, 4, 2, 8, BI_RLE8, &pal, &[data]);
        let img = decode_bmp(&bytes).unwrap();
        assert_eq!((img.width, img.height), (4, 2));
        assert_eq!(img.pixel(0, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(1, 0), Some([0, 255, 0, 255]));
        assert_eq!(img.pixel(2, 0), Some([0, 255, 0, 255]));
        assert_eq!(img.pixel(3, 0), Some([0, 255, 0, 255]));
        // Pixels the stream never wrote stay transparent, not black.
        assert_eq!(img.pixel(0, 1), Some([0, 0, 0, 0]));
        assert_eq!(img.pixel(1, 1), Some([0, 0, 255, 255]));
    }

    #[test]
    fn rle4_absolute_run_is_nibble_first_and_word_aligned() {
        // An absolute run of four: the op is (count 0, pixel count 4), the
        // literals follow high-nibble first, then the end-of-bitmap pair.
        let data = vec![0x00, 0x04, 0x01, 0x12, 0x00, 0x01];
        let pal = [[0, 0, 0], [255, 0, 0], [0, 255, 0]];
        let bytes = bmp_file(40, 4, 1, 4, BI_RLE4, &pal, &[data]);
        let img = decode_bmp(&bytes).unwrap();
        // Indices 0, 1, 1, 2.
        assert_eq!(img.pixel(0, 0), Some([0, 0, 0, 255]));
        assert_eq!(img.pixel(1, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(2, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(3, 0), Some([0, 255, 0, 255]));

        // Five pixels take three data bytes, so a pad byte is skipped to
        // reach a word boundary before the end-of-bitmap pair.
        let data = vec![0x00, 0x05, 0x01, 0x11, 0x20, 0x00, 0x00, 0x01];
        let bytes = bmp_file(40, 8, 1, 4, BI_RLE4, &pal, &[data]);
        let img = decode_bmp(&bytes).unwrap();
        // Indices 0, 1, 1, 1, 2, then three untouched (transparent) pixels.
        let want = [
            [0, 0, 0, 255],
            [255, 0, 0, 255],
            [255, 0, 0, 255],
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 0, 0],
            [0, 0, 0, 0],
            [0, 0, 0, 0],
        ];
        for (x, w) in want.iter().enumerate() {
            assert_eq!(img.pixel(x as u32, 0), Some(*w), "x={x}");
        }
    }

    #[test]
    fn rle_out_of_bounds_and_truncation_refused() {
        let pal = [[0, 0, 0], [255, 0, 0]];
        // A five-pixel run in a four-wide image.
        let bytes = bmp_file(40, 4, 1, 8, BI_RLE8, &pal, &[vec![0x05, 0x01, 0x00, 0x01]]);
        assert!(matches!(decode_bmp(&bytes), Err(MediaError::Decode(_))));
        // The stream ends without an end-of-bitmap marker.
        let bytes = bmp_file(40, 4, 1, 8, BI_RLE8, &pal, &[vec![0x02, 0x01]]);
        assert!(matches!(decode_bmp(&bytes), Err(MediaError::Decode(_))));
        // RLE at a depth it does not exist for.
        let bytes = bmp_file(40, 4, 1, 24, BI_RLE8, &[], &[vec![0x00, 0x01]]);
        assert!(matches!(decode_bmp(&bytes), Err(MediaError::Decode(_))));
    }

    #[test]
    fn rejects_bad_headers_and_truncation() {
        assert_eq!(bmp_dimensions(b"not a bitmap"), None);
        assert!(matches!(decode_bmp(b"nope"), Err(MediaError::Decode(_))));
        // "BM" but no DIB header behind it.
        let junk = [b"BM".to_vec(), vec![0u8; 16]].concat();
        assert_eq!(bmp_dimensions(&junk), None);
        // Unknown DIB header size.
        let mut bytes = bmp_file(40, 2, 2, 24, BI_RGB, &[], &rows_bgr24(2, &[[0; 3]; 4]));
        bytes[14..18].copy_from_slice(&72u32.to_le_bytes());
        assert_eq!(bmp_dimensions(&bytes), None);
        // Multi-plane and zero-dimension headers.
        let mut bytes = bmp_file(40, 2, 2, 24, BI_RGB, &[], &rows_bgr24(2, &[[1; 3]; 4]));
        bytes[26..28].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(bmp_dimensions(&bytes), None);
        let mut bytes = bmp_file(40, 0, 2, 24, BI_RGB, &[], &[]);
        bytes[18..22].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(bmp_dimensions(&bytes), None);
        // Embedded compressed stream: refused, not guessed at.
        let bytes = bmp_file(40, 2, 2, 24, BI_JPEG, &[], &[]);
        assert!(matches!(decode_bmp(&bytes), Err(MediaError::Decode(_))));
        // A pixel offset past the end of the file.
        let mut bytes = bmp_file(40, 2, 2, 24, BI_RGB, &[], &rows_bgr24(2, &[[9; 3]; 4]));
        bytes[10..14].copy_from_slice(&9999u32.to_le_bytes());
        assert!(matches!(decode_bmp(&bytes), Err(MediaError::Decode(_))));
        // Truncated pixel data: the decode refuses…
        let full = bmp_file(40, 2, 2, 24, BI_RGB, &[], &rows_bgr24(2, &[[7; 3]; 4]));
        assert!(decode_bmp(&full).is_ok());
        assert!(matches!(
            decode_bmp(&full[..full.len() - 3]),
            Err(MediaError::Decode(_))
        ));
        // …while the header path still sizes the placeholder.
        assert_eq!(bmp_dimensions(&full[..full.len() - 3]), Some((2, 2)));
    }

    #[test]
    fn bomb_header_rejected_before_allocating() {
        // A 20000x20000 header (400 MP) trips the 64 MP guard on the header
        // alone; nothing is allocated for it.
        let bytes = bmp_file(40, 20000, 20000, 24, BI_RGB, &[], &[]);
        assert_eq!(bmp_dimensions(&bytes), Some((20000, 20000)));
        assert!(matches!(
            decode_bmp(&bytes),
            Err(MediaError::TooLarge { .. })
        ));
    }
}
