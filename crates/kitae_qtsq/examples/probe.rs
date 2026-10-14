//! Probe every .qtsq file given as an argument through the same decision
//! path the engine's viewer uses, and report what would render.
//!
//! Usage: cargo run -p kitae_qtsq --example probe <file.qtsq> [...]

use kitae_qtsq::{decode_audio, decode_image, decode_text, probe, VideoDecoder};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: probe <file.qtsq> [...]");
        std::process::exit(2);
    }
    for path in &args {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                println!("{path}: READ ERROR {e}");
                continue;
            }
        };
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path);
        let Some(info) = probe(&bytes) else {
            println!("{name}: NOT QTSQ ({} bytes)", bytes.len());
            continue;
        };
        let enc = if info.encrypted { "enc" } else { "plain" };
        match &info.data_type {
            t if t.is_image() => match decode_image(&bytes, None) {
                Ok(img) => println!(
                    "{name}: IMAGE OK {}x{} ({enc}, v{}, strategy {}, dt {})",
                    img.width,
                    img.height,
                    info.strategy.name(),
                    t.name(),
                    info.version
                ),
                Err(e) => println!(
                    "{name}: IMAGE FAIL {e} ({enc}, v{}, strategy {}, dt {})",
                    info.strategy.name(),
                    t.name(),
                    info.version
                ),
            },
            t if t.is_text() => match decode_text(&bytes, None) {
                Ok(text) => println!(
                    "{name}: TEXT OK {} chars ({enc}, v{}, strategy {})",
                    text.chars().count(),
                    info.strategy.name(),
                    info.version
                ),
                Err(e) => println!(
                    "{name}: TEXT FAIL {e} ({enc}, v{}, strategy {})",
                    info.strategy.name(),
                    info.version
                ),
            },
            t if t.is_audio() => match decode_audio(&bytes, None) {
                Ok(pcm) => println!(
                    "{name}: AUDIO OK {} frames @{}Hz x{}ch ({enc}, v{}, strategy {})",
                    pcm.frames(),
                    pcm.sample_rate,
                    pcm.channels,
                    info.strategy.name(),
                    info.version
                ),
                Err(e) => println!(
                    "{name}: AUDIO FAIL {e} ({enc}, v{}, strategy {})",
                    info.strategy.name(),
                    info.version
                ),
            },
            t if t.is_video() => match VideoDecoder::open(&bytes, None) {
                Ok(vd) => println!(
                    "{name}: VIDEO OK {} frames {}x{} ({enc}, v{}, strategy {})",
                    vd.frame_count(),
                    vd.dimensions().0,
                    vd.dimensions().1,
                    info.strategy.name(),
                    info.version
                ),
                Err(e) => println!(
                    "{name}: VIDEO FAIL {e} ({enc}, v{}, strategy {})",
                    info.strategy.name(),
                    info.version
                ),
            },
            t => println!(
                "{name}: {} ({enc}, v{}, strategy {}) — info page only",
                t.name(),
                info.strategy.name(),
                info.version
            ),
        }
    }
}
