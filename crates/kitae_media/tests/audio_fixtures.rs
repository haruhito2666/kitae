//! F1 fixture tests: ffmpeg-generated containers decode to interleaved
//! f32 PCM (44.1 kHz mono sine, ~0.5 s).

use kitae_media::audio::decode_audio;

fn fixture(name: &str) -> Vec<u8> {
    let root = env!("CARGO_MANIFEST_DIR");
    std::fs::read(format!("{root}/tests/fixtures/{name}"))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn check(name: &str, ext: &str, channels: u16) {
    let a = decode_audio(&fixture(name), ext).unwrap();
    assert_eq!(a.channels, channels, "{name}: channels");
    assert_eq!(a.rate, 44100, "{name}: rate");
    assert!(
        a.duration() > 0.2 && a.duration() < 1.5,
        "{name}: duration {}",
        a.duration()
    );
}

#[test]
fn mp3() {
    check("tone.mp3", "mp3", 1);
}

#[test]
fn wav() {
    check("tone.wav", "wav", 1);
}

#[test]
fn flac() {
    check("tone.flac", "flac", 1);
}

#[test]
fn ogg_vorbis() {
    // ffmpeg's native vorbis encoder is stereo-only.
    check("tone.ogg", "ogg", 2);
}

#[test]
fn m4a_aac() {
    check("tone.m4a", "m4a", 1);
}

#[test]
fn rejects_garbage() {
    assert!(decode_audio(b"this is not audio", "mp3").is_err());
}
