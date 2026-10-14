use kitae_qtsq::{
    audio_info, decode_audio, decode_image, decode_tensor, decode_text, is_qtsq, probe,
    QtsqDataType, VideoDecoder, HEADER_V3_SIZE,
};

fn iters() -> usize {
    std::env::var("KITAE_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const PROBE_FIXTURES: &[(&str, &[u8])] = &[
    ("text", include_bytes!("../testdata/fix_text.qtsq")),
    (
        "locked_text",
        include_bytes!("../testdata/fix_text_locked.qtsq"),
    ),
    ("image", include_bytes!("../testdata/fix_image.qtsq")),
    ("audio", include_bytes!("../testdata/fix_audio.qtsq")),
    ("video", include_bytes!("../testdata/fix_video.qtsq")),
    ("tensor", include_bytes!("../testdata/fix_tensor.qtsq")),
];

const DECODE_FIXTURES: &[(&str, &[u8])] = &[
    ("text", include_bytes!("../testdata/fix_text.qtsq")),
    ("image", include_bytes!("../testdata/fix_image.qtsq")),
    ("audio", include_bytes!("../testdata/fix_audio.qtsq")),
    ("video", include_bytes!("../testdata/fix_video.qtsq")),
    ("tensor", include_bytes!("../testdata/fix_tensor.qtsq")),
];

fn mutate_bytes(rng: &mut XorShift, base: &[u8]) -> Vec<u8> {
    let mut bytes = base.to_vec();
    match rng.below(5) {
        0 => {
            if !bytes.is_empty() {
                let at = rng.below(bytes.len());
                bytes[at] ^= rng.next() as u8;
            }
        }
        1 => {
            if !bytes.is_empty() {
                let at = rng.below(bytes.len());
                bytes.remove(at);
            }
        }
        2 => {
            let at = rng.below(bytes.len() + 1);
            bytes.insert(at, rng.next() as u8);
        }
        3 => bytes.truncate(rng.below(bytes.len() + 1)),
        _ => {
            if !bytes.is_empty() {
                let at = rng.below(bytes.len());
                bytes.push(bytes[at]);
            }
        }
    }
    bytes
}

fn mutate_payload(rng: &mut XorShift, base: &[u8]) -> Vec<u8> {
    let mut bytes = base.to_vec();
    if bytes.len() <= HEADER_V3_SIZE {
        return bytes;
    }
    let at = HEADER_V3_SIZE + rng.below(bytes.len() - HEADER_V3_SIZE);
    match rng.below(4) {
        0 => bytes[at] ^= rng.next() as u8,
        1 => {
            bytes.remove(at);
        }
        2 => bytes.insert(at, rng.next() as u8),
        _ => bytes.push(bytes[at]),
    }
    bytes
}

#[test]
fn probe_mutations_never_panic() {
    let mut rng = XorShift::new(0x50524F42);
    for _ in 0..iters() {
        let base = PROBE_FIXTURES[rng.below(PROBE_FIXTURES.len())].1;
        let bytes = mutate_bytes(&mut rng, base);
        let _ = probe(&bytes);
        let _ = is_qtsq(&bytes);
        let _ = audio_info(&bytes);
    }
}

#[test]
fn decode_mutations_never_panic() {
    let mut rng = XorShift::new(0x4445434D);
    for _ in 0..iters() {
        let base = DECODE_FIXTURES[rng.below(DECODE_FIXTURES.len())].1;
        let info = match probe(base) {
            Some(info) => info,
            None => continue,
        };
        let bytes = mutate_payload(&mut rng, base);
        if probe(&bytes).is_none() {
            continue;
        }
        match info.data_type {
            QtsqDataType::ImageGray | QtsqDataType::ImageRgb | QtsqDataType::ImageRgba => {
                let _ = decode_image(&bytes, None);
            }
            QtsqDataType::AudioMono | QtsqDataType::AudioStereo | QtsqDataType::AudioMulti => {
                let _ = decode_audio(&bytes, None);
            }
            QtsqDataType::VideoFrames => {
                if let Ok(decoder) = VideoDecoder::open(&bytes, None) {
                    let _ = decoder.dimensions();
                    let _ = decoder.frame_count();
                }
            }
            QtsqDataType::Tensor => {
                let _ = decode_tensor(&bytes, None);
            }
            QtsqDataType::TextAscii
            | QtsqDataType::TextUtf8
            | QtsqDataType::Json
            | QtsqDataType::Csv => {
                let _ = decode_text(&bytes, None);
            }
            _ => {}
        }
    }
}
