//! APNG (animated PNG): frame detection and playback.
//!
//! An APNG is a PNG that carries `acTL`/`fcTL`/`fdAT` chunks. The `png` crate
//! decodes one `IDAT` datastream, so playback here walks the frame controls,
//! re-assembles each frame as a standalone PNG datastream (an `IHDR` at the
//! frame's own size, the frame's compressed stream as a single `IDAT`) and
//! composites the results with the spec's `dispose_op`/`blend_op` rules —
//! the same "every frame is a full-canvas RGBA8 buffer" contract as
//! [`crate::decode_frames`], so embedders blit without compositing.
//!
//! Fail-closed rules:
//! * [`apng_info`] is detection, so any doubt is a `None` (a plain PNG, a
//!   truncated chunk walk, an `acTL` after `IDAT`, an `fcTL` count that
//!   disagrees with `acTL`) and the payload keeps being a still.
//! * [`decode_apng_frames`] is a decode, so those same defects are a
//!   [`MediaError`], as are frames whose rectangle leaves the canvas, a
//!   frame with no data, and a 64 MP canvas (the `MAX_FRAMES` cap plus
//!   the per-frame pixel guard bound the total buffer, as for GIF).

use crate::{check_size, decode_png, Frame, MediaError, MAX_FRAMES, MIN_FRAME_DELAY_MS};

const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Ceiling on the chunks of one kind the walk will collect. A writer that
/// splits its image data into thousands of chunks is unheard of (that is
/// 4096 × 8 KB of pixels), so a file that does is refused rather than walked
/// — the walk is a header parse and its tables must stay bounded.
const MAX_CHUNKS: usize = 4096;

/// What an APNG's control chunks declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApngInfo {
    /// Canvas size (the `IHDR`).
    pub width: u32,
    pub height: u32,
    /// Frames in the animation (`acTL` num_frames, cross-checked against
    /// the `fcTL` chunks present).
    pub frame_count: u32,
    /// Plays: 0 = loop forever.
    pub num_plays: u32,
}

/// One `fcTL` record: where the frame goes, how long it lasts, and how it
/// composites.
#[derive(Debug, Clone, Copy)]
struct Ctl {
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    delay_num: u16,
    delay_den: u16,
    dispose: u8,
    blend: u8,
}

impl Ctl {
    /// Presentation delay in ms, floored at [`MIN_FRAME_DELAY_MS`]. A zero
    /// denominator means 100 (the spec's fallback).
    fn delay_ms(&self) -> u32 {
        let den = if self.delay_den == 0 {
            100
        } else {
            self.delay_den as u32
        };
        (self.delay_num as u32 * 1000 / den).max(MIN_FRAME_DELAY_MS)
    }
}

/// Everything the chunk walk found, in file order.
struct Walk<'a> {
    ihdr: Option<&'a [u8]>,
    plte: Option<&'a [u8]>,
    trns: Option<&'a [u8]>,
    /// The `IDAT` chunks: the default image, and frame 0 when a `fcTL`
    /// precedes them.
    idat: Vec<&'a [u8]>,
    ctl: Vec<Ctl>,
    /// `fdAT` payloads with the 4-byte sequence number stripped.
    fdat: Vec<&'a [u8]>,
    actl: Option<(u32, u32)>,
    /// Whether a `fcTL` preceded the first `IDAT` — i.e. whether the
    /// default image is frame 0 of the animation or a still backdrop the
    /// frames draw over.
    animates_idat: bool,
}

fn u32be(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(off)?,
        *b.get(off + 1)?,
        *b.get(off + 2)?,
        *b.get(off + 3)?,
    ]))
}

/// One PNG chunk: its type, payload, and where the next one starts.
/// `None` when the declared length runs past the end of `bytes` (a
/// truncated file — the caller fails closed).
fn chunk_at(bytes: &[u8], pos: usize) -> Option<([u8; 4], &[u8], usize)> {
    let len = u32be(bytes, pos)? as usize;
    let kind: [u8; 4] = bytes.get(pos + 4..pos + 8)?.try_into().ok()?;
    // 4 length + 4 type + payload + 4 CRC.
    let next = pos.checked_add(12)?.checked_add(len)?;
    if next > bytes.len() {
        return None;
    }
    Some((kind, &bytes[pos + 8..pos + 8 + len], next))
}

/// Walk the chunk list, collecting the animation control data. Chunk CRCs
/// are not verified here: this is a header parse (detection), and
/// `decode_apng_frames` hands each reassembled frame to the `png` decoder,
/// which does verify them.
fn walk(bytes: &[u8]) -> Result<Walk<'_>, MediaError> {
    let trunc = || MediaError::Decode("apng: truncated chunk".into());
    if bytes.len() < 8 || bytes[..8] != SIGNATURE {
        return Err(MediaError::Decode("apng: no PNG signature".into()));
    }
    let mut w = Walk {
        ihdr: None,
        plte: None,
        trns: None,
        idat: Vec::new(),
        ctl: Vec::new(),
        fdat: Vec::new(),
        actl: None,
        animates_idat: false,
    };
    let mut seen_idat = false;
    let mut pos = 8;
    while let Some((kind, data, next)) = chunk_at(bytes, pos) {
        match &kind {
            b"IHDR" => {
                if data.len() != 13 {
                    return Err(MediaError::Decode("apng: bad IHDR".into()));
                }
                w.ihdr = Some(data);
            }
            b"acTL" => {
                if seen_idat {
                    return Err(MediaError::Decode("apng: acTL after IDAT".into()));
                }
                if w.actl.is_some() {
                    return Err(MediaError::Decode("apng: duplicate acTL".into()));
                }
                let (frames, plays) = (u32be(data, 0), u32be(data, 4));
                let (frames, plays) = (frames.ok_or_else(trunc)?, plays.ok_or_else(trunc)?);
                if frames == 0 {
                    return Err(MediaError::Decode("apng: acTL declares no frames".into()));
                }
                w.actl = Some((frames, plays));
            }
            b"fcTL" => {
                if data.len() < 26 {
                    return Err(MediaError::Decode("apng: bad fcTL".into()));
                }
                if w.ctl.len() == MAX_CHUNKS {
                    return Err(MediaError::Decode("apng: too many fcTL chunks".into()));
                }
                w.ctl.push(Ctl {
                    width: u32be(data, 4).ok_or_else(trunc)?,
                    height: u32be(data, 8).ok_or_else(trunc)?,
                    x: u32be(data, 12).ok_or_else(trunc)?,
                    y: u32be(data, 16).ok_or_else(trunc)?,
                    delay_num: u16::from_be_bytes([data[20], data[21]]),
                    delay_den: u16::from_be_bytes([data[22], data[23]]),
                    dispose: data[24],
                    blend: data[25],
                });
                if w.ctl.len() == 1 && !seen_idat {
                    w.animates_idat = true;
                }
            }
            b"IDAT" => {
                seen_idat = true;
                if w.idat.len() == MAX_CHUNKS {
                    return Err(MediaError::Decode("apng: too many IDAT chunks".into()));
                }
                w.idat.push(data);
            }
            b"fdAT" => {
                // 4-byte sequence number, then the frame's zlib stream.
                if data.len() <= 4 {
                    return Err(MediaError::Decode("apng: empty fdAT".into()));
                }
                if w.fdat.len() == MAX_CHUNKS {
                    return Err(MediaError::Decode("apng: too many fdAT chunks".into()));
                }
                w.fdat.push(&data[4..]);
            }
            b"PLTE" => w.plte = Some(data),
            b"tRNS" => w.trns = Some(data),
            b"IEND" => return Ok(w),
            _ => {}
        }
        pos = next;
    }
    Err(trunc())
}

impl Walk<'_> {
    /// Canvas size from `IHDR` (13 bytes: width, height, …).
    fn canvas(&self) -> Result<(u32, u32), MediaError> {
        let ihdr = self
            .ihdr
            .ok_or_else(|| MediaError::Decode("apng: no IHDR".into()))?;
        let (w, h) = (u32be(ihdr, 0), u32be(ihdr, 4));
        match (w, h) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Ok((w, h)),
            _ => Err(MediaError::Decode("apng: bad canvas size".into())),
        }
    }
}

/// Detect an APNG from its control chunks: canvas geometry, frame count and
/// play count. `None` for a plain PNG and for any structure this reader will
/// not vouch for (a truncated chunk walk, an `acTL` after `IDAT`, an
/// `fcTL`/`acTL` count disagreement) — detection never raises, it just says
/// "not an animation I can vouch for".
pub fn apng_info(bytes: &[u8]) -> Option<ApngInfo> {
    let w = walk(bytes).ok()?;
    let (frame_count, num_plays) = w.actl?;
    if w.ctl.len() as u32 != frame_count {
        return None;
    }
    let (width, height) = w.canvas().ok()?;
    Some(ApngInfo {
        width,
        height,
        frame_count,
        num_plays,
    })
}

/// Whether a PNG payload is an animation with more than one frame — the
/// check a caller makes before committing to the frame-sequence path.
pub fn is_animated_png(bytes: &[u8]) -> bool {
    apng_info(bytes).is_some_and(|i| i.frame_count > 1)
}

/// Decode an APNG into its frame sequence, composited onto the canvas per
/// `blend_op` (0 = replace, 1 = over) and disposed of per `dispose_op`
/// (0 = keep, 1 = clear the frame's rect, 2 = restore the pre-frame
/// canvas). `loop_count` mirrors [`crate::decode_frames`]: `None` plays once,
/// `Some(n)` is n extra repetitions, `Some(u16::MAX)` loops forever (an
/// `acTL` num_plays of 0). A plain PNG (no `acTL`) comes back as its single
/// still frame.
pub fn decode_apng_frames(bytes: &[u8]) -> Result<(Vec<Frame>, Option<u16>), MediaError> {
    let w = walk(bytes)?;
    let Some((num_frames, num_plays)) = w.actl else {
        let img = decode_png(bytes)?;
        return Ok((
            vec![Frame {
                rgba: img.pixels,
                delay_ms: MIN_FRAME_DELAY_MS,
            }],
            None,
        ));
    };
    let (width, height) = w.canvas()?;
    check_size(width, height)?;
    if w.ctl.len() as u32 != num_frames {
        return Err(MediaError::Decode(format!(
            "apng: acTL says {num_frames} frames, {} fcTL chunks",
            w.ctl.len()
        )));
    }
    // Frame data: the default image is frame 0 when a `fcTL` preceded it,
    // and a backdrop the frames composite over when it did not.
    let fdat_base = usize::from(w.animates_idat);
    if w.animates_idat && w.idat.is_empty() {
        return Err(MediaError::Decode("apng: no IDAT for frame 0".into()));
    }
    if (w.fdat.len() as u32) < num_frames.saturating_sub(fdat_base as u32) {
        return Err(MediaError::Decode("apng: frame data missing".into()));
    }
    // The canvas is transparent until something draws on it; when the
    // default image is not a frame it *is* the backdrop, so decode it.
    let mut canvas = vec![0u8; width as usize * height as usize * 4];
    if !w.animates_idat {
        canvas = decode_png(bytes)?.pixels;
    }

    let mut frames: Vec<Frame> = Vec::new();
    let mut prev: Vec<u8> = Vec::new();
    for (i, ctl) in w.ctl.iter().enumerate().take(MAX_FRAMES) {
        if ctl.width == 0 || ctl.height == 0 || ctl.x >= width || ctl.y >= height {
            return Err(MediaError::Decode(format!(
                "apng: frame {i} empty or off-canvas"
            )));
        }
        if ctl.width > width - ctl.x || ctl.height > height - ctl.y {
            return Err(MediaError::Decode(format!(
                "apng: frame {i} leaves the canvas"
            )));
        }
        let data: Vec<u8> = if i < fdat_base {
            w.idat.concat()
        } else {
            w.fdat[i - fdat_base].to_vec()
        };
        let ihdr = w
            .ihdr
            .ok_or_else(|| MediaError::Decode("apng: no IHDR".into()))?;
        let img = decode_png(&frame_stream(
            ihdr, ctl.width, ctl.height, w.plte, w.trns, &data,
        ))?;
        if ctl.dispose == 2 {
            prev.clone_from(&canvas);
        }
        for y in 0..ctl.height as usize {
            for x in 0..ctl.width as usize {
                let at = ((ctl.y as usize + y) * width as usize + ctl.x as usize + x) * 4;
                let src = &img.pixels[(y * ctl.width as usize + x) * 4..][..4];
                if ctl.blend == 0 {
                    canvas[at..at + 4].copy_from_slice(src);
                } else {
                    over(&mut canvas[at..at + 4], src);
                }
            }
        }
        frames.push(Frame {
            rgba: canvas.clone(),
            delay_ms: ctl.delay_ms(),
        });
        match ctl.dispose {
            1 => {
                for y in 0..ctl.height as usize {
                    let at = ((ctl.y as usize + y) * width as usize + ctl.x as usize) * 4;
                    canvas[at..at + ctl.width as usize * 4].fill(0);
                }
            }
            2 => canvas.copy_from_slice(&prev),
            _ => {}
        }
    }
    if frames.is_empty() {
        return Err(MediaError::Decode("apng: no frames".into()));
    }
    let loop_count = match num_plays {
        0 => Some(u16::MAX),
        n => Some((n - 1).min(u16::MAX as u32) as u16),
    };
    Ok((frames, loop_count))
}

/// Straight-alpha `OVER`: `src` on top of `dst`, both un-premultiplied.
fn over(dst: &mut [u8], src: &[u8]) {
    let sa = src[3] as u32;
    if sa == 0 {
        return;
    }
    if sa == 255 {
        dst.copy_from_slice(src);
        return;
    }
    let ia = 255 - sa;
    for c in 0..3 {
        dst[c] = ((src[c] as u32 * sa + dst[c] as u32 * ia + 127) / 255) as u8;
    }
    dst[3] = (sa + (dst[3] as u32 * ia + 127) / 255).min(255) as u8;
}

/// Reassemble one frame as a standalone PNG datastream: the original
/// `IHDR` at the frame's own size plus the frame's zlib stream as a single
/// `IDAT`. The `png` crate then does the unfiltering (and interlacing), so
/// no PNG decoding is re-implemented here.
fn frame_stream(
    ihdr: &[u8],
    width: u32,
    height: u32,
    plte: Option<&[u8]>,
    trns: Option<&[u8]>,
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(ihdr.len() + data.len() + 64);
    out.extend_from_slice(&SIGNATURE);
    let mut head = ihdr.to_vec();
    head[0..4].copy_from_slice(&width.to_be_bytes());
    head[4..8].copy_from_slice(&height.to_be_bytes());
    write_chunk(&mut out, b"IHDR", &head);
    if let Some(p) = plte {
        write_chunk(&mut out, b"PLTE", p);
    }
    if let Some(t) = trns {
        write_chunk(&mut out, b"tRNS", t);
    }
    write_chunk(&mut out, b"IDAT", data);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

/// Append a chunk with a correct CRC-32 (`flate2::Crc` — the `png` crate
/// verifies these).
fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;

    /// A frame: top-down RGBA pixels, zlib-compressed with a `None` filter
    /// byte per row (the `png` crate's own convention).
    fn frame_idat(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        let mut raw = Vec::new();
        for row in pixels.chunks_exact(width as usize * 4) {
            raw.push(0u8);
            raw.extend_from_slice(row);
        }
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&raw).unwrap();
        enc.finish().unwrap()
    }

    fn ihdr(width: u32, height: u32) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(&width.to_be_bytes());
        h.extend_from_slice(&height.to_be_bytes());
        h.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace
        h
    }

    struct Fc {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        num: u16,
        den: u16,
        dispose: u8,
        blend: u8,
    }

    impl Default for Fc {
        fn default() -> Self {
            Fc {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
                num: 1,
                den: 10,
                dispose: 0,
                blend: 0,
            }
        }
    }

    fn fctl(seq: u32, f: &Fc) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&seq.to_be_bytes());
        d.extend_from_slice(&f.w.to_be_bytes());
        d.extend_from_slice(&f.h.to_be_bytes());
        d.extend_from_slice(&f.x.to_be_bytes());
        d.extend_from_slice(&f.y.to_be_bytes());
        d.extend_from_slice(&f.num.to_be_bytes());
        d.extend_from_slice(&f.den.to_be_bytes());
        d.push(f.dispose);
        d.push(f.blend);
        d
    }

    /// A whole APNG: `frames` are `(fcTL, pixel data)`, plus a `plays`
    /// count. Normally the first `fcTL` precedes the `IDAT`, so the default
    /// image *is* frame 0; with `backdrop` the `IDAT` comes first instead and
    /// every frame is an `fdAT` drawn over it.
    fn apng(
        width: u32,
        height: u32,
        plays: u32,
        frames: &[(Fc, Vec<u8>)],
        backdrop: bool,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&SIGNATURE);
        let mut seq = 0u32;
        let ctl_then_fdat = |out: &mut Vec<u8>, f: &Fc, px: &[u8], seq: &mut u32| {
            write_chunk(out, b"fcTL", &fctl(*seq, f));
            *seq += 1;
            let mut d = seq.to_be_bytes().to_vec();
            *seq += 1;
            d.extend_from_slice(&frame_idat(f.w, f.h, px));
            write_chunk(out, b"fdAT", &d);
        };
        write_chunk(&mut out, b"IHDR", &ihdr(width, height));
        write_chunk(
            &mut out,
            b"acTL",
            &[(frames.len() as u32).to_be_bytes(), plays.to_be_bytes()].concat(),
        );
        if backdrop {
            write_chunk(&mut out, b"IDAT", &frame_idat(width, height, &frames[0].1));
            for (f, px) in frames {
                ctl_then_fdat(&mut out, f, px, &mut seq);
            }
        } else {
            for (i, (f, px)) in frames.iter().enumerate() {
                write_chunk(&mut out, b"fcTL", &fctl(seq, f));
                seq += 1;
                let data = frame_idat(f.w, f.h, px);
                if i == 0 {
                    write_chunk(&mut out, b"IDAT", &data);
                } else {
                    let mut d = seq.to_be_bytes().to_vec();
                    seq += 1;
                    d.extend_from_slice(&data);
                    write_chunk(&mut out, b"fdAT", &d);
                }
            }
        }
        write_chunk(&mut out, b"IEND", &[]);
        out
    }

    fn solid(w: u32, h: u32, c: [u8; 4]) -> Vec<u8> {
        assert_eq!((w * h * 4) as usize, (w * h) as usize * 4);
        std::iter::repeat(c)
            .take((w * h) as usize)
            .flatten()
            .collect()
    }

    #[test]
    fn detects_frames_and_plays() {
        let px = solid(2, 2, [1, 2, 3, 255]);
        let bytes = apng(
            2,
            2,
            0,
            &[
                (Fc::default(), px.clone()),
                (Fc::default(), solid(2, 2, [4, 5, 6, 255])),
            ],
            false,
        );
        // Still sniffed as a plain PNG — the animation is a separate axis.
        assert_eq!(crate::sniff(&bytes), Some("image/png"));
        let info = apng_info(&bytes).unwrap();
        assert_eq!(
            info,
            ApngInfo {
                width: 2,
                height: 2,
                frame_count: 2,
                num_plays: 0,
            }
        );
        assert!(is_animated_png(&bytes));
        // A plain PNG is not an animation.
        let plain = {
            let mut out = Vec::new();
            out.extend_from_slice(&SIGNATURE);
            write_chunk(&mut out, b"IHDR", &ihdr(2, 2));
            write_chunk(&mut out, b"IDAT", &frame_idat(2, 2, &px));
            write_chunk(&mut out, b"IEND", &[]);
            out
        };
        assert_eq!(apng_info(&plain), None);
        assert!(!is_animated_png(&plain));
    }

    #[test]
    fn plays_back_frames_with_delays_and_loop_count() {
        let bytes = apng(
            2,
            2,
            0,
            &[
                (Fc::default(), solid(2, 2, [255, 0, 0, 255])),
                (
                    Fc {
                        num: 1,
                        den: 4,
                        ..Fc::default()
                    },
                    solid(2, 2, [0, 255, 0, 255]),
                ),
            ],
            false,
        );
        let (frames, loop_count) = decode_apng_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(loop_count, Some(u16::MAX)); // plays = 0 → forever
        assert_eq!(frames[0].delay_ms, 100);
        assert_eq!(frames[1].delay_ms, 250);
        assert_eq!(&frames[0].rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&frames[1].rgba[0..4], &[0, 255, 0, 255]);
        // A finite play count maps to extra repetitions, like GIF.
        let bytes = apng(
            2,
            2,
            3,
            &[(Fc::default(), solid(2, 2, [1, 1, 1, 255]))],
            false,
        );
        assert_eq!(decode_apng_frames(&bytes).unwrap().1, Some(2));
    }

    #[test]
    fn sub_rect_frames_composite_with_blend_and_dispose() {
        // 4x4 canvas. Frame 0 paints the left 2x4 red (source blend).
        // Frame 1 is a 2x2 patch at (1,1), blended over with 50% alpha.
        // Frame 2 disposes to background, frame 3 disposes to previous.
        let bytes = apng(
            4,
            4,
            1,
            &[
                (
                    Fc {
                        w: 2,
                        h: 4,
                        ..Fc::default()
                    },
                    solid(2, 4, [255, 0, 0, 255]),
                ),
                (
                    Fc {
                        x: 1,
                        y: 1,
                        w: 2,
                        h: 2,
                        blend: 1,
                        ..Fc::default()
                    },
                    solid(2, 2, [0, 0, 255, 128]),
                ),
                (
                    Fc {
                        x: 0,
                        y: 0,
                        w: 4,
                        h: 4,
                        dispose: 1,
                        blend: 1,
                        ..Fc::default()
                    },
                    solid(4, 4, [0, 255, 0, 255]),
                ),
            ],
            false,
        );
        let (frames, _) = decode_apng_frames(&bytes).unwrap();
        let px = |f: usize, x: usize, y: usize| -> [u8; 4] {
            let i = (y * 4 + x) * 4;
            frames[f].rgba[i..i + 4].try_into().unwrap()
        };
        assert_eq!(px(0, 0, 0), [255, 0, 0, 255]);
        assert_eq!(px(0, 3, 3), [0, 0, 0, 0], "outside the first frame");
        // 50% blue over red ≈ (128, 0, 128) — `OVER` is integer-rounded, so
        // allow one step.
        let [r, g, b, a] = px(1, 1, 1);
        assert_eq!((g, b, a), (0, 128, 255), "got {r},{g},{b},{a}");
        assert!((r as i32 - 128).abs() <= 1, "red {r}");
        assert_eq!(px(1, 0, 1), [255, 0, 0, 255], "untouched red");
        // The last frame disposed to background, so nothing of frame 1's
        // blend survives into it beyond its own full-canvas draw.
        assert_eq!(px(2, 0, 0), [0, 255, 0, 255]);
    }

    #[test]
    fn dispose_previous_restores_the_canvas() {
        let bytes = apng(
            2,
            2,
            1,
            &[
                (Fc::default(), solid(2, 2, [9, 9, 9, 255])),
                (
                    Fc {
                        dispose: 2,
                        ..Fc::default()
                    },
                    solid(2, 2, [1, 2, 3, 255]),
                ),
            ],
            false,
        );
        let (frames, _) = decode_apng_frames(&bytes).unwrap();
        assert_eq!(&frames[0].rgba[0..4], &[9, 9, 9, 255]);
        assert_eq!(&frames[1].rgba[0..4], &[1, 2, 3, 255]);
    }

    #[test]
    fn plain_png_decodes_as_one_still_frame() {
        let px = solid(2, 2, [7, 7, 7, 255]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SIGNATURE);
        write_chunk(&mut bytes, b"IHDR", &ihdr(2, 2));
        write_chunk(&mut bytes, b"IDAT", &frame_idat(2, 2, &px));
        write_chunk(&mut bytes, b"IEND", &[]);
        let (frames, loop_count) = decode_apng_frames(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(loop_count, None);
        assert_eq!(&frames[0].rgba[0..4], &[7, 7, 7, 255]);
        assert_eq!(frames[0].delay_ms, MIN_FRAME_DELAY_MS);
        // And it matches the ordinary still decode.
        assert_eq!(decode_png(&bytes).unwrap().pixels, frames[0].rgba);
    }

    #[test]
    fn malformed_structures_fail_closed() {
        let px = solid(2, 2, [1; 4]);
        // acTL after IDAT: not an APNG we vouch for.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SIGNATURE);
        write_chunk(&mut bytes, b"IHDR", &ihdr(2, 2));
        write_chunk(&mut bytes, b"IDAT", &frame_idat(2, 2, &px));
        write_chunk(&mut bytes, b"acTL", &1u32.to_be_bytes());
        write_chunk(&mut bytes, b"IEND", &[]);
        assert_eq!(apng_info(&bytes), None);
        assert!(matches!(
            decode_apng_frames(&bytes),
            Err(MediaError::Decode(_))
        ));

        // A frame count that disagrees with the fcTL chunks.
        let good = apng(
            2,
            2,
            0,
            &[(Fc::default(), px.clone()), (Fc::default(), px.clone())],
            false,
        );
        assert!(apng_info(&good).is_some());
        let mut lying = good.clone();
        // acTL's num_frames: signature (8) + IHDR chunk (12 + 13) + 8.
        let at = 8 + (12 + 13) + 8;
        lying[at..at + 4].copy_from_slice(&7u32.to_be_bytes());
        assert_eq!(apng_info(&lying), None);
        assert!(matches!(
            decode_apng_frames(&lying),
            Err(MediaError::Decode(_))
        ));

        // A truncated chunk walk.
        assert_eq!(apng_info(&good[..good.len() - 12]), None);
        assert!(matches!(
            decode_apng_frames(b"not a png"),
            Err(MediaError::Decode(_))
        ));
        // acTL declaring zero frames.
        let mut zero = Vec::new();
        zero.extend_from_slice(&SIGNATURE);
        write_chunk(&mut zero, b"IHDR", &ihdr(2, 2));
        write_chunk(&mut zero, b"acTL", &[0u8; 8]);
        write_chunk(&mut zero, b"IEND", &[]);
        assert_eq!(apng_info(&zero), None);
    }

    #[test]
    fn chunk_walk_is_bounded() {
        // A file with more chunks of one kind than the walk will collect is
        // refused, not walked into an unbounded table.
        let px = solid(2, 2, [1; 4]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SIGNATURE);
        write_chunk(&mut bytes, b"IHDR", &ihdr(2, 2));
        write_chunk(
            &mut bytes,
            b"acTL",
            &[1u32.to_be_bytes(), 0u32.to_be_bytes()].concat(),
        );
        write_chunk(&mut bytes, b"fcTL", &fctl(0, &Fc::default()));
        for _ in 0..(MAX_CHUNKS + 2) {
            write_chunk(&mut bytes, b"IDAT", &frame_idat(2, 2, &px));
        }
        write_chunk(&mut bytes, b"IEND", &[]);
        assert_eq!(apng_info(&bytes), None);
        assert!(matches!(
            decode_apng_frames(&bytes),
            Err(MediaError::Decode(_))
        ));
    }

    #[test]
    fn frame_rect_and_geometry_limits_enforced() {
        let px = solid(2, 2, [1; 4]);
        // A frame that runs off the canvas.
        let bytes = apng(
            2,
            2,
            1,
            &[(
                Fc {
                    w: 4,
                    h: 2,
                    ..Fc::default()
                },
                solid(4, 2, [1; 4]),
            )],
            false,
        );
        assert!(matches!(
            decode_apng_frames(&bytes),
            Err(MediaError::Decode(_))
        ));
        // A bomb-sized canvas is refused on the header.
        let mut huge = Vec::new();
        huge.extend_from_slice(&SIGNATURE);
        write_chunk(&mut huge, b"IHDR", &ihdr(20000, 20000));
        write_chunk(
            &mut huge,
            b"acTL",
            &[1u32.to_be_bytes(), 0u32.to_be_bytes()].concat(),
        );
        write_chunk(&mut huge, b"fcTL", &fctl(0, &Fc::default()));
        write_chunk(&mut huge, b"IDAT", &frame_idat(2, 2, &px));
        write_chunk(&mut huge, b"IEND", &[]);
        assert!(matches!(
            decode_apng_frames(&huge),
            Err(MediaError::TooLarge { .. })
        ));
    }
}
