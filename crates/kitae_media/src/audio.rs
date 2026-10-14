//! F1: full audio decode for plain `<audio>` (and `<video>` audio tracks)
//! — symphonia probes the container, decodes every packet, and hands back
//! interleaved f32 PCM. The engine converts to CAF so the existing native
//! audio bar plays it with AVFoundation (the QTSQ hand-off, now fed from
//! the web). Opus is NOT decodable here: symphonia 0.5 never shipped an
//! opus codec, so opus-in-webm audio is a recorded deferral (parity §F).

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::{Hint, Probe};

/// Decoded audio: interleaved f32 samples (`len == frames * channels`).
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl DecodedAudio {
    pub fn duration(&self) -> f32 {
        if self.rate == 0 || self.channels == 0 {
            0.0
        } else {
            self.samples.len() as f32 / self.channels as f32 / self.rate as f32
        }
    }
}

/// Decode any container/codec symphonia supports (mp3, wav, flac,
/// ogg-vorbis, m4a-aac, mkv/webm-vorbis, caf, aiff…) into f32 PCM.
/// `ext` is an optional filename hint (`mp3`) for ambiguous containers.
pub fn decode_audio(bytes: &[u8], ext: &str) -> Result<DecodedAudio, String> {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    let mut hint = Hint::new();
    let ext = ext.trim_start_matches('.');
    if !ext.is_empty() {
        hint.with_extension(ext);
    }

    // Probe via the UFCS spelling — see note below.
    let mut probe = symphonia::default::get_probe();
    let probed = Probe::format(
        &mut probe,
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )
    .map_err(|e| e.to_string())?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("no audio track")?
        .clone();
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;

    let mut samples: Vec<f32> = Vec::new();
    let mut rate = track.codec_params.sample_rate.unwrap_or(0);
    let mut channels = track
        .codec_params
        .channels
        .map(|c| c.count() as u16)
        .unwrap_or(0);
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // End of stream (or a malformed tail) = done.
            Err(SymError::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                if sample_buf.is_none() {
                    let spec = *decoded.spec();
                    rate = spec.rate;
                    channels = spec.channels.count() as u16;
                    sample_buf = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
                }
                if let Some(buf) = &mut sample_buf {
                    buf.copy_interleaved_ref(decoded);
                    samples.extend_from_slice(buf.samples());
                }
            }
            Err(SymError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    if rate == 0 || channels == 0 || samples.is_empty() {
        return Err("no audio frames decoded".into());
    }
    // Trim trailing partial frame.
    let per_frame = channels as usize;
    samples.truncate(samples.len() / per_frame * per_frame);
    Ok(DecodedAudio {
        rate,
        channels,
        samples,
    })
}
