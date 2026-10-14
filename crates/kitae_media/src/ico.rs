//! ICO (Windows icon) decode: the container that holds one entry per icon
//! size, each either a PNG datastream (Vista+) or a bare DIB (the classic
//! format) with its height doubled to hold the bitmap and the 1bpp `AND`
//! transparency mask.
//!
//! Fail-closed rules: a directory that does not parse (bad magic, zero or
//! over-long entry count, an entry whose range leaves the file) is a
//! [`MediaError`], and each entry's own decoder refuses what it cannot
//! render — a placeholder, never a garbled icon. The largest entry wins,
//! which is what browsers pick for `<img>`; a PNG entry wins an area tie
//! because its alpha is exact.
//!
//! `.cur` files share the magic but are cursors, not images: they are not
//! sniffed and `decode_ico` rejects them.

use crate::bmp::{self, Alpha};
use crate::{decode_png, MediaError, RgbaImage};

const BI_RLE8: u32 = 1;
const BI_RLE4: u32 = 2;

const MAGIC_ICON: [u8; 4] = [0x00, 0x00, 0x01, 0x00];
/// Directory entries we will even look at. Real icons carry a handful; the
/// cap stops a hostile 65535-entry header from walking a long table.
const MAX_ENTRIES: usize = 512;

const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// One 16-byte directory record.
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// Directory size (0 means 256).
    width: u32,
    height: u32,
    size: u32,
    offset: u32,
}

/// Parse the directory. `None` for anything that is not a well-formed ICO
/// header whose entries all fit inside `bytes`.
fn entries(bytes: &[u8]) -> Option<Vec<Entry>> {
    if bytes.len() < 6 || bytes[0..4] != MAGIC_ICON {
        return None;
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    if count == 0 || count > MAX_ENTRIES || bytes.len() < 6 + count * 16 {
        return None;
    }
    let rd = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let d = 6 + i * 16;
        let (w, h) = (bytes[d], bytes[d + 1]);
        let e = Entry {
            width: if w == 0 { 256 } else { w as u32 },
            height: if h == 0 { 256 } else { h as u32 },
            size: rd(d + 8)?,
            offset: rd(d + 12)?,
        };
        // u64 math: a hostile size/offset pair must not wrap.
        let end = e.offset as u64 + e.size as u64;
        if e.size == 0 || end > bytes.len() as u64 {
            return None;
        }
        out.push(e);
    }
    Some(out)
}

fn payload<'a>(bytes: &'a [u8], e: &Entry) -> &'a [u8] {
    &bytes[e.offset as usize..(e.offset + e.size) as usize]
}

fn is_png(p: &[u8]) -> bool {
    p.len() >= 8 && p[..8] == PNG_MAGIC
}

/// The entry a renderer would use: biggest area, PNG first on a tie.
fn best<'a>(bytes: &[u8], entries: &'a [Entry]) -> Option<&'a Entry> {
    entries
        .iter()
        .max_by_key(|e| (e.width * e.height, is_png(payload(bytes, e)) as u32))
}

/// Intrinsic dimensions (the largest entry's) straight from the directory —
/// no entry payload is parsed, so a truncated icon still sizes the box.
pub(crate) fn ico_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let entries = entries(bytes)?;
    let e = best(bytes, &entries)?;
    Some((e.width, e.height))
}

/// Decode the largest entry of an ICO to RGBA8. PNG entries go through the
/// PNG decoder; DIB entries through the BMP reader with the ICO conventions
/// (4-byte palette entries, real 32bpp alpha, trailing `AND` mask).
pub fn decode_ico(bytes: &[u8]) -> Result<RgbaImage, MediaError> {
    let entries = entries(bytes).ok_or_else(|| MediaError::Decode("ico: bad directory".into()))?;
    let e = best(bytes, &entries).ok_or_else(|| MediaError::Decode("ico: no entries".into()))?;
    let data = payload(bytes, e);
    if is_png(data) {
        return decode_png(data);
    }
    decode_ico_dib(data, e.height)
}

/// A DIB entry. The classic ICO layout doubles the DIB height: the pixel
/// array is the bitmap's rows plus a 1bpp `AND` mask (a set bit is opaque).
/// Three wrinkles the wild is full of, all handled here:
/// * Which half holds the bitmap depends on the row order — a top-down DIB
///   stores the bitmap first, a bottom-up one stores the mask first.
/// * Some writers (Pillow's ICO writer among them) double the height and
///   never write the second half. That is detected (only `h` rows of
///   bitmap fit) and decoded as a plain opaque bitmap, rather than refused
///   or read as mask.
/// * 32bpp entries carry real alpha instead of a mask, so the mask (if any)
///   is ignored at that depth.
///
/// `dir_h` is the entry's height from the directory, which is what sizes the
/// bitmap (falling back to half the DIB height when it is 0); the width
/// comes from the DIB itself, since that is the array being split.
fn decode_ico_dib(dib: &[u8], dir_h: u32) -> Result<RgbaImage, MediaError> {
    let f = bmp::dib_facts(dib).ok_or_else(|| MediaError::Decode("ico: bad DIB header".into()))?;
    if matches!(f.comp, BI_RLE8 | BI_RLE4) {
        // An RLE stream has no rows to split, and no real icon ships one.
        return Err(MediaError::Decode("ico: rle entry".into()));
    }
    let h = if dir_h > 0 { dir_h } else { f.height / 2 };
    if h == 0 || f.width == 0 {
        return Err(MediaError::Decode("ico: empty entry".into()));
    }
    let stride = bmp::row_stride(f.width, f.bpp);
    let half = stride.checked_mul(h as usize);
    let Some(half) = half else {
        return Err(MediaError::Decode("ico: geometry overflow".into()));
    };
    // Where the pixel array starts, and whether both halves are there: an
    // ICO writer uses 4-byte palette entries, a plain BMP 3-byte ones.
    let mut found: Option<(usize, bool)> = None;
    for pal in [4usize, 3] {
        if pal == 3 && f.ncolors == 0 {
            break;
        }
        let off = f.header_len + pal * f.ncolors as usize;
        let Some(off) = off.checked_add(half) else {
            break; // offset arithmetic overflowed
        };
        if off + half <= dib.len() {
            found = Some((off - half, true));
            break;
        }
        if off <= dib.len() {
            found = Some((off - half, false));
            break;
        }
    }
    let (off, doubled) =
        found.ok_or_else(|| MediaError::Decode("ico: pixel rows outside the entry".into()))?;

    // Decode just the bitmap rows: hand the BMP reader a DIB whose height is
    // the entry's and whose pixel array is only the bitmap half.
    let rows_at = if doubled && !f.top_down {
        off + half
    } else {
        off
    };
    let mut synth = dib[..off].to_vec();
    synth.extend_from_slice(&dib[rows_at..rows_at + half]);
    let height = h as i32 * if f.top_down { -1 } else { 1 };
    if f.header_len >= 40 {
        synth[4..8].copy_from_slice(&(f.width as i32).to_le_bytes());
        synth[8..12].copy_from_slice(&height.to_le_bytes());
    } else {
        synth[4..6].copy_from_slice(&(f.width as u16).to_le_bytes());
        synth[6..8].copy_from_slice(&(height as u16).to_le_bytes());
    }
    let mut image = bmp::decode_dib(&synth, off, Alpha::FourthByte)?;
    if image.height != h {
        return Err(MediaError::Decode(format!(
            "ico: entry height {} is not {h}",
            image.height
        )));
    }

    if doubled && f.bpp < 32 {
        let mask_stride = bmp::row_stride(f.width, 1);
        let mask_at = if f.top_down { off + half } else { off };
        let mask = dib
            .get(mask_at..mask_at + mask_stride * h as usize)
            .ok_or_else(|| MediaError::Decode("ico: truncated AND mask".into()))?;
        let w = f.width as usize;
        for y in 0..h as usize {
            // The mask is stored like the bitmap: bottom-up unless the
            // header says top-down.
            let at = (if f.top_down { y } else { h as usize - 1 - y }) * mask_stride;
            let row = &mask[at..at + mask_stride];
            for x in 0..w {
                if (row[x / 8] >> (7 - x % 8)) & 1 == 0 {
                    image.pixels[(y * w + x) * 4 + 3] = 0;
                }
            }
        }
    }
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ICO with the given entries, in order. Each entry is
    /// `(directory w, directory h, payload)`.
    fn ico(entries: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC_ICON);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        let dir_len = 6 + entries.len() * 16;
        let mut offset = dir_len;
        let mut dir = Vec::new();
        for (w, h, payload) in entries {
            dir.push(if *w == 256 { 0 } else { *w as u8 });
            dir.push(if *h == 256 { 0 } else { *h as u8 });
            dir.extend_from_slice(&[0, 0]); // colors, reserved
            dir.extend_from_slice(&1u16.to_le_bytes()); // planes
            dir.extend_from_slice(&32u16.to_le_bytes()); // bpp
            dir.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            dir.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += payload.len();
        }
        out.extend_from_slice(&dir);
        for (_, _, payload) in entries {
            out.extend_from_slice(payload);
        }
        out
    }

    /// A 2x2 PNG via the crate's own encoder path: built by hand so the
    /// test has no encoder dependency.
    fn png_2x2(pixels: [[u8; 4]; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 2, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&pixels.concat()).unwrap();
        }
        out
    }

    /// A 2x2 DIB entry: 32bpp, doubled height, top-down rows (negative
    /// height, so the file order is bitmap then mask) and a second column
    /// that is transparent through its own alpha.
    fn dib_2x2() -> Vec<u8> {
        let mut dib = dib_header(2, -4, 32, 0);
        for px in [
            [255, 0, 0, 255],
            [0, 255, 0, 0],
            [0, 0, 255, 255],
            [9, 9, 9, 255],
        ] {
            dib.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        // The mask half of a 32bpp entry is ignored — alpha is exact — so
        // this claims the opposite and must not win.
        for _ in 0..4 {
            dib.extend_from_slice(&[0b1010_0000, 0, 0, 0]);
        }
        dib
    }

    /// A 40-byte `BITMAPINFOHEADER` (no palette, `biClrUsed = 0`).
    fn dib_header(width: i32, height: i32, bpp: u16, clr_used: u32) -> Vec<u8> {
        let mut d = vec![0u8; 40];
        d[0..4].copy_from_slice(&40u32.to_le_bytes());
        d[4..8].copy_from_slice(&width.to_le_bytes());
        d[8..12].copy_from_slice(&height.to_le_bytes());
        d[12..14].copy_from_slice(&1u16.to_le_bytes()); // planes
        d[14..16].copy_from_slice(&bpp.to_le_bytes());
        d[32..36].copy_from_slice(&clr_used.to_le_bytes());
        d
    }

    #[test]
    fn decodes_png_entry_and_picks_the_largest() {
        let small = png_2x2([[1, 2, 3, 255]; 4]);
        // A 2x2 DIB entry is the same size, so the PNG must win the tie.
        let bytes = ico(&[(2, 2, dib_2x2()), (2, 2, small.clone())]);
        assert_eq!(ico_dimensions(&bytes), Some((2, 2)));
        let img = decode_ico(&bytes).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.pixel(0, 0), Some([1, 2, 3, 255]));
        // The DIB entry alone decodes through the BMP reader: the doubled
        // height is halved and 32bpp alpha is real.
        let bytes = ico(&[(2, 2, dib_2x2())]);
        let img = decode_ico(&bytes).unwrap();
        assert_eq!(img.pixel(0, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(1, 0), Some([0, 255, 0, 0]), "alpha, not the mask");
        assert_eq!(img.pixel(0, 1), Some([0, 0, 255, 255]));
        assert_eq!(img.pixel(1, 1), Some([9, 9, 9, 255]));
    }

    #[test]
    fn dib_entry_keeps_32bit_alpha_and_drops_the_mask_half() {
        // A bottom-up 1x1 icon: the file stores the mask row (the bottom half
        // of the doubled image) first, then the bitmap row.
        let mut dib = dib_header(1, 2, 32, 0);
        dib.extend_from_slice(&[0xFF, 0, 0, 0]); // mask row, unread at 32bpp
        dib.extend_from_slice(&[7, 8, 9, 0x40]); // BGRA, half-transparent
        let img = decode_ico(&ico(&[(1, 1, dib)])).unwrap();
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.pixels.len(), 4);
        assert_eq!(img.pixel(0, 0), Some([9, 8, 7, 0x40]));
    }

    #[test]
    fn paletted_dib_entry_uses_four_byte_palette() {
        // 4bpp, 4x2 icon, top-down (negative height): the pixel array is
        // bitmap rows then mask rows. A 3-entry 4-byte palette (the ICO
        // layout) must be read as palette data, not as pixel bytes.
        let mut dib = dib_header(4, -4, 4, 3);
        for c in [[0, 0, 0], [255, 0, 0], [0, 0, 255]] {
            dib.extend_from_slice(&[c[2], c[1], c[0], 0]);
        }
        // Bitmap rows: index 1 on top, index 2 below (4bpp, stride 4).
        dib.extend_from_slice(&[0x11, 0x11, 0x00, 0x00]);
        dib.extend_from_slice(&[0x22, 0x22, 0x00, 0x00]);
        // Mask rows, top-down, 1bpp padded to 4: column 1 of the top row is
        // transparent, the rest opaque.
        dib.extend_from_slice(&[0b1011_1011, 0, 0, 0]);
        dib.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        let img = decode_ico(&ico(&[(4, 2, dib)])).unwrap();
        assert_eq!((img.width, img.height), (4, 2));
        assert_eq!(img.pixel(0, 0), Some([255, 0, 0, 255]));
        assert_eq!(img.pixel(0, 1), Some([0, 0, 255, 255]));
        // A masked-out pixel keeps its palette colour and loses its alpha,
        // exactly like a transparent GIF pixel.
        assert_eq!(img.pixel(1, 0).map(|p| p[3]), Some(0));
        assert_eq!(img.pixel(3, 1), Some([0, 0, 255, 255]));
    }

    #[test]
    fn bottom_up_dib_entry_reads_the_mask_half_first() {
        // The common Windows shape: positive height, so the doubled pixel
        // array stores the `AND` mask first and the bitmap second, both
        // bottom-up. 2x2, 4bpp.
        let mut dib = dib_header(2, 4, 4, 2);
        for c in [[0, 0, 0], [255, 0, 0]] {
            dib.extend_from_slice(&[c[2], c[1], c[0], 0]);
        }
        // Mask (bottom-up): the image's bottom row opaque, its top row with
        // the left pixel transparent.
        dib.extend_from_slice(&[0xFF, 0, 0, 0]);
        dib.extend_from_slice(&[0b0111_1111, 0, 0, 0]);
        // Bitmap (bottom-up): the image's bottom row is red, its top row
        // black — written in reverse, as a bottom-up array must be.
        dib.extend_from_slice(&[0x11, 0x11, 0, 0]);
        dib.extend_from_slice(&[0x00, 0x00, 0, 0]);
        let img = decode_ico(&ico(&[(2, 2, dib)])).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.pixel(0, 0), Some([0, 0, 0, 0]), "masked top-left");
        assert_eq!(img.pixel(1, 0), Some([0, 0, 0, 255]), "top row, unmasked");
        assert_eq!(img.pixel(0, 1), Some([255, 0, 0, 255]), "bottom row");
        assert_eq!(img.pixel(1, 1), Some([255, 0, 0, 255]));
    }

    #[test]
    fn entry_that_omits_the_mask_half_still_decodes() {
        // Pillow's ICO writer doubles the height without writing the second
        // half. The bitmap rows are there, so the entry decodes — opaque,
        // since no mask means no transparency information.
        let mut dib = dib_header(2, 4, 32, 0); // declared 2x2, writes 2x2
                                               // Bottom-up: the image's bottom row comes first in the file.
        for px in [
            [7, 8, 9, 255],
            [10, 11, 12, 255],
            [1, 2, 3, 255],
            [4, 5, 6, 255],
        ] {
            dib.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        let img = decode_ico(&ico(&[(2, 2, dib)])).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.pixel(0, 0), Some([1, 2, 3, 255]));
        assert_eq!(img.pixel(1, 1), Some([10, 11, 12, 255]));
    }

    #[test]
    fn rejects_bad_directories() {
        assert_eq!(ico_dimensions(b"not an icon"), None);
        assert!(matches!(
            decode_ico(b"\x00\x00\x01\x00"),
            Err(MediaError::Decode(_))
        ));
        // Cursor magic: not an image.
        let mut cur = ico(&[(2, 2, png_2x2([[9; 4]; 4]))]);
        cur[2] = 2;
        assert_eq!(ico_dimensions(&cur), None);
        assert!(matches!(decode_ico(&cur), Err(MediaError::Decode(_))));
        // Zero entries, and a count past the table.
        let mut none = Vec::new();
        none.extend_from_slice(&MAGIC_ICON);
        none.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(ico_dimensions(&none), None);
        let mut huge = none.clone();
        huge[4..6].copy_from_slice(&9999u16.to_le_bytes());
        assert_eq!(ico_dimensions(&huge), None);
        // An entry whose range leaves the file.
        let mut bytes = ico(&[(2, 2, png_2x2([[1; 4]; 4]))]);
        let n = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        let at = 6 + 8 + (n - 1) * 16; // the last entry's dwBytesInRes
        bytes[at..at + 4].copy_from_slice(&999_999u32.to_le_bytes());
        assert_eq!(ico_dimensions(&bytes), None);
        // …and a directory that parses but whose only entry is a DIB
        // header with nothing behind it.
        let bytes = ico(&[(2, 2, vec![40, 0, 0, 0])]);
        assert!(matches!(decode_ico(&bytes), Err(MediaError::Decode(_))));
    }

    #[test]
    fn max_size_entry_decodes() {
        // 256x256 is the format's maximum entry; the bomb guard runs on the
        // doubled DIB height, well under 64 MP.
        let mut dib = dib_header(256, 512, 32, 0);
        dib.extend_from_slice(&vec![0u8; 256 * 4 * 512]); // bitmap + mask halves
        let bytes = ico(&[(256, 256, dib)]);
        assert_eq!(ico_dimensions(&bytes), Some((256, 256)));
        let img = decode_ico(&bytes).unwrap();
        assert_eq!((img.width, img.height), (256, 256));
        assert_eq!(img.pixels.len(), 256 * 256 * 4);
    }
}
