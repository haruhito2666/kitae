//! Real-world container fixtures: BMP, ICO and APNG files written by other
//! tools, so the hand-rolled readers are checked against bytes this crate did
//! not produce. The source image is 48×32: a bright left half (`#ff3cc8`)
//! and a dark right half (`#001e78`) — split by luminance so a 1-bit
//! quantizer separates them too — with every 7th diagonal pixel transparent
//! in the icon fixtures.
//!
//! Fixtures and how they were made (Pillow 12, ffmpeg):
//! ```text
//! rgb24.bmp       rgb.save("rgb24.bmp")                       # 24bpp BI_RGB
//! indexed8.bmp    rgb.convert("P", ADAPTIVE, colors=16).save(…)  # 8bpp
//! mono1.bmp      rgb.convert("1").save(…)                     # 1bpp
//! png_entries.ico  rgba.save(…, sizes=[(16,16),(32,32),(48,32)])   # PNG entries
//! dib_entries.ico  rgba.save(…, bitmap_format="bmp", sizes=…)       # DIB entries
//! mixed_entries.ico  hand-assembled: 16×16 PNG entry + 32×32 DIB entry
//! anim.png       ffmpeg -framerate 5 -i f%02d.png -plays 0 -f apng anim.png
//! ```
//! `rgb24`/`indexed8`/`mono1` come from an opaque RGB source; the icons from
//! an RGBA one, so the ICO fixtures carry the transparent diagonal.

use kitae_media::{
    apng_info, decode_apng_frames, decode_bmp, decode_ico, decode_image, dimensions,
    is_animated_png, sniff, MAX_FRAMES, MIN_FRAME_DELAY_MS,
};

const LEFT: [u8; 3] = [0xFF, 0x3C, 0xC8];
const RIGHT: [u8; 3] = [0x00, 0x1E, 0x78];

fn fixture(name: &str) -> Vec<u8> {
    let root = env!("CARGO_MANIFEST_DIR");
    std::fs::read(format!("{root}/tests/fixtures/{name}"))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

/// Distances the two flat halves can be judged by, for the fixtures that go
/// through a lossy palette (exact equality would not be fair).
fn assert_halves(img: &kitae_media::RgbaImage, what: &str) {
    let near = |got: [u8; 3], want: [u8; 3]| -> bool {
        got.iter()
            .zip(want)
            .all(|(a, b)| (i32::from(*a) - i32::from(b)).abs() <= 40)
    };
    let y = img.height / 2;
    let left = img.pixel(img.width / 12, y).expect("left pixel");
    let right = img
        .pixel(img.width - img.width / 12, y)
        .expect("right pixel");
    assert!(
        near([left[0], left[1], left[2]], LEFT),
        "{what}: left {left:?}"
    );
    assert!(
        near([right[0], right[1], right[2]], RIGHT),
        "{what}: right {right:?}"
    );
    assert_eq!(
        img.pixels.len(),
        img.width as usize * img.height as usize * 4,
        "{what}"
    );
}

#[test]
fn bmp_rgb24_is_exact() {
    let bytes = fixture("rgb24.bmp");
    assert_eq!(sniff(&bytes), Some("image/bmp"));
    assert_eq!(dimensions(&bytes), Some((48, 32)));
    let img = decode_image(&bytes, None).expect("decode");
    assert_eq!((img.width, img.height), (48, 32));
    // A 24bpp BI_RGB file has no alpha channel: every pixel is opaque.
    assert_eq!(img.pixel(0, 0), Some([255, 60, 200, 255]));
    assert_eq!(img.pixel(47, 31), Some([0, 30, 120, 255]));
    // The MIME hint path agrees, including the alias servers actually send.
    assert_eq!(
        decode_image(&bytes, Some("image/x-ms-bmp")).map(|i| i.pixels),
        Ok(img.pixels.clone())
    );
}

#[test]
fn bmp_paletted_and_mono_decode() {
    let bytes = fixture("indexed8.bmp");
    assert_eq!(sniff(&bytes), Some("image/bmp"));
    assert_eq!(dimensions(&bytes), Some((48, 32)));
    let img = decode_bmp(&bytes).expect("8-bit palette");
    assert_halves(&img, "8-bit palette");
    // Palette indices carry no alpha: an indexed BMP is opaque.
    assert_eq!(img.pixel(2, 16).unwrap()[3], 255);

    // 1-bit keeps only luminance, so it is judged on brightness: the bright
    // half is the set bit, the dark half the clear one.
    let bytes = fixture("mono1.bmp");
    let img = decode_bmp(&bytes).expect("1-bit");
    assert_eq!((img.width, img.height), (48, 32));
    let luma = |p: [u8; 4]| 299 * p[0] as u32 + 587 * p[1] as u32 + 114 * p[2] as u32;
    let y = img.height / 2;
    let left = img.pixel(img.width / 12, y).unwrap();
    let right = img.pixel(img.width - img.width / 12, y).unwrap();
    assert!(luma(left) > 128, "left half is bright: {left:?}");
    assert!(luma(right) <= 128, "right half is dark: {right:?}");
    assert_eq!(left[3], 255);
}

#[test]
fn ico_png_entries_decode_the_largest() {
    let bytes = fixture("png_entries.ico");
    assert_eq!(sniff(&bytes), Some("image/x-icon"));
    // The largest of Pillow's three entries is the full 48×32.
    assert_eq!(dimensions(&bytes), Some((48, 32)));
    let img = decode_ico(&bytes).expect("decode");
    assert_eq!((img.width, img.height), (48, 32));
    // PNG-in-ICO keeps its alpha: (0, 0) is on the transparent diagonal.
    assert_eq!(img.pixel(0, 0), Some([255, 60, 200, 0]));
    assert_eq!(img.pixel(40, 10), Some([0, 30, 120, 255]));
    assert_halves(&img, "png entries");
    // …and the same file through the generic still path.
    let via_image = decode_image(&bytes, Some("image/x-icon")).unwrap();
    assert_eq!(via_image.pixels, img.pixels);
}

#[test]
fn ico_dib_entries_decode_through_the_bmp_reader() {
    let bytes = fixture("dib_entries.ico");
    assert_eq!(sniff(&bytes), Some("image/x-icon"));
    // Pillow's entries are aspect-fit thumbnails (16×11, 24×16, 32×21) and
    // each DIB declares twice its height; the largest entry wins.
    assert_eq!(dimensions(&bytes), Some((32, 21)));
    let img = decode_ico(&bytes).expect("decode");
    assert_eq!((img.width, img.height), (32, 21));
    assert_halves(&img, "dib entries");
    // A 32bpp DIB entry carries real alpha (the ICO convention), so the
    // thumbnail's transparent diagonal survives: some pixels are clear and
    // the solid interior is opaque.
    let clears = img.pixels.chunks_exact(4).filter(|p| p[3] < 0x80).count();
    assert!(clears > 10, "{clears} transparent pixels");
    assert!(
        img.pixels.chunks_exact(4).any(|p| p[3] == 255),
        "opaque interior"
    );
}

#[test]
fn ico_mixed_entries_pick_the_largest_and_ignore_the_rest() {
    let bytes = fixture("mixed_entries.ico");
    // Entries: a 16×16 PNG and a 32×32 DIB. The DIB is larger and wins.
    assert_eq!(dimensions(&bytes), Some((32, 32)));
    let img = decode_ico(&bytes).expect("decode");
    assert_eq!((img.width, img.height), (32, 32));
    assert_halves(&img, "mixed entries");
    // 32bpp alpha again (the DIB side of the mixed container).
    assert!(
        img.pixels.chunks_exact(4).any(|p| p[3] < 0x80),
        "alpha kept"
    );

    // Claiming a single entry must select that one: the 16×16 PNG, whose
    // right half is fully transparent.
    let mut png_only = bytes.clone();
    png_only[4..6].copy_from_slice(&1u16.to_le_bytes());
    assert_eq!(dimensions(&png_only), Some((16, 16)));
    let img = decode_ico(&png_only).expect("png entry");
    assert_eq!((img.width, img.height), (16, 16));
    assert_eq!(img.pixel(0, 0), Some([255, 60, 200, 255]));
    assert_eq!(img.pixel(12, 8), Some([0, 0, 0, 0]));
}

#[test]
fn apng_detection_and_playback() {
    let bytes = fixture("anim.png");
    // An APNG is still a PNG to every other layer: it sniffs, sizes and
    // decodes as a still.
    assert_eq!(sniff(&bytes), Some("image/png"));
    assert_eq!(dimensions(&bytes), Some((16, 12)));
    let still = decode_image(&bytes, None).expect("still");
    assert_eq!((still.width, still.height), (16, 12));

    // …and the animation is detectable on its own axis.
    let info = apng_info(&bytes).expect("apng");
    assert_eq!((info.width, info.height), (16, 12));
    assert_eq!(info.frame_count, 3);
    assert_eq!(info.num_plays, 0, "ffmpeg wrote an infinite loop");
    assert!(is_animated_png(&bytes));

    let (frames, loop_count) = decode_apng_frames(&bytes).expect("frames");
    assert_eq!(frames.len(), 3);
    assert_eq!(loop_count, Some(u16::MAX));
    // 5 fps => a 200 ms delay per frame.
    for f in &frames {
        assert_eq!(f.delay_ms, 200, "delay");
        assert!(f.delay_ms >= MIN_FRAME_DELAY_MS);
        assert_eq!(f.rgba.len(), 16 * 12 * 4);
    }
    // ffmpeg's frames are solid colours differing only in blue, +40 each.
    for (f, blue) in [30u8, 70, 110].into_iter().enumerate() {
        assert_eq!(&frames[f].rgba[0..4], &[10, 20, blue, 255], "frame {f}");
        // …and each one covers the whole canvas, opaquely.
        assert!(frames[f]
            .rgba
            .chunks_exact(4)
            .all(|p| p == [10, 20, blue, 255]));
    }
    assert!(frames.len() <= MAX_FRAMES);
}

#[test]
fn truncated_fixtures_fail_closed() {
    // A BMP keeps its header geometry (the placeholder still sizes) while
    // the decode refuses the missing pixels.
    let bytes = fixture("rgb24.bmp");
    let cut = &bytes[..bytes.len() - 100];
    assert_eq!(dimensions(cut), Some((48, 32)));
    assert!(matches!(
        decode_image(cut, Some("image/bmp")),
        Err(kitae_media::MediaError::Decode(_))
    ));
    // An ICO whose last entry is cut off cannot be trusted at all — not even
    // for dimensions — so it fails closed as unsupported.
    for name in ["png_entries.ico", "dib_entries.ico", "mixed_entries.ico"] {
        let bytes = fixture(name);
        let cut = &bytes[..bytes.len() - bytes.len() / 5];
        assert!(decode_image(cut, None).is_err(), "{name}");
    }
    // A garbage payload claiming a BMP/ICO magic is never pixels.
    assert!(decode_image(b"BM\0\0\0\0\0\0\0\0\0\0\0\0", None).is_err());
    assert!(decode_image(&[0u8, 0, 1, 0, 0, 0], None).is_err());
}
