//! F2/F3: container demuxers for web `<video>` — MP4 (BMFF) and EBML/WebM.
//! Hand-rolled on purpose: symphonia's demuxers are audio-track focused, and
//! the engine only needs the VIDEO track's compressed samples, codec id and
//! extradata. The embedder (VideoToolbox) does the pixel decode — C stays in
//! its CPP_ROLE.md lane (media codecs), the demux itself stays pure Rust.

/// Video codec identity carried out of the demuxer. The embedder maps these
/// onto its decoder (VideoToolbox handles all but AV1 on Apple silicon).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Vp8,
    Vp9,
    Av1,
}

impl VideoCodec {
    /// MP4 sample-entry fourcc for this codec.
    pub fn from_mp4_fourcc(f: &[u8]) -> Option<VideoCodec> {
        match f {
            b"avc1" | b"avc3" => Some(VideoCodec::H264),
            b"hvc1" | b"hev1" => Some(VideoCodec::Hevc),
            b"vp08" => Some(VideoCodec::Vp8),
            b"vp09" => Some(VideoCodec::Vp9),
            b"av01" => Some(VideoCodec::Av1),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
            VideoCodec::Vp8 => "vp8",
            VideoCodec::Vp9 => "vp9",
            VideoCodec::Av1 => "av1",
        }
    }
}

/// One compressed video access unit, in decode order.
#[derive(Debug, Clone)]
pub struct VideoSample {
    pub data: Vec<u8>,
    pub pts_ms: f64,
    pub keyframe: bool,
}

/// The demuxed video track.
#[derive(Debug, Clone)]
pub struct VideoTrackInfo {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Codec initialization data (avcC/hvcC/vpcC/av1C for MP4, CodecPrivate
    /// for WebM) — the decoder's format description needs it.
    pub extradata: Vec<u8>,
    pub fps: f64,
}

#[derive(Debug, Clone)]
pub struct DemuxedVideo {
    pub track: VideoTrackInfo,
    pub samples: Vec<VideoSample>,
    pub duration_ms: f64,
}

/// Sniff: MP4 starts with a `ftyp` box, WebM with the EBML magic.
pub fn is_mp4(b: &[u8]) -> bool {
    b.len() >= 12 && &b[4..8] == b"ftyp"
}

pub fn is_webm(b: &[u8]) -> bool {
    b.len() >= 4 && b[0] == 0x1A && b[1] == 0x45 && b[2] == 0xDF && b[3] == 0xA3
}

/// Demux the first supported video track out of an MP4 or WebM stream.
pub fn demux(bytes: &[u8]) -> Result<DemuxedVideo, String> {
    if is_mp4(bytes) {
        demux_mp4(bytes)
    } else if is_webm(bytes) {
        demux_webm(bytes)
    } else {
        Err("not an MP4 or WebM stream".into())
    }
}

// ---------------------------------------------------------------------------
// MP4 / BMFF
// ---------------------------------------------------------------------------

fn be_u32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
}

fn be_u64(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes(b[off..off + 8].try_into().unwrap())
}

/// Iterate `(fourcc, payload_start, payload_end)` boxes in `data`. Handles
/// size==1 (64-bit largesize) and size==0 (runs to end of input).
fn box_list(data: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 8 <= data.len() {
        let size32 = be_u32(data, i) as usize;
        let mut typ = [0u8; 4];
        typ.copy_from_slice(&data[i + 4..i + 8]);
        let mut hdr = 8usize;
        let size = if size32 == 1 {
            if i + 16 > data.len() {
                break;
            }
            hdr = 16;
            be_u64(data, i + 8) as usize
        } else if size32 == 0 {
            data.len() - i
        } else {
            size32
        };
        if size < hdr || i + size > data.len() {
            break;
        }
        out.push((typ, i + hdr, i + size));
        i += size;
    }
    out
}

fn demux_mp4(b: &[u8]) -> Result<DemuxedVideo, String> {
    let top = box_list(b);
    let Some((_, ms, me)) = top.iter().find(|(t, _, _)| t == b"moov") else {
        return Err("mp4: no moov box".into());
    };
    let moov = &b[*ms..*me];

    // Movie timescale/duration from mvhd (version 0 and 1 layouts).
    let mut movie_ts = 0u32;
    let mut movie_dur = 0u64;
    for (t, s, e) in box_list(moov) {
        if t != *b"mvhd" {
            continue;
        }
        let p = &moov[s..e];
        if p.is_empty() {
            continue;
        }
        let ver = p[0];
        let toff = 4 + if ver == 1 { 16 } else { 8 };
        let dlen = if ver == 1 { 8 } else { 4 };
        if p.len() >= toff + 4 + dlen {
            movie_ts = be_u32(p, toff);
            movie_dur = if ver == 1 {
                be_u64(p, toff + 4)
            } else {
                be_u32(p, toff + 4) as u64
            };
        }
    }

    for (t, s, e) in box_list(moov) {
        if t != *b"trak" {
            continue;
        }
        match demux_trak(b, &moov[s..e], movie_ts, movie_dur) {
            Ok(Some(d)) => return Ok(d),
            Ok(None) => continue,
            Err(_) => continue, // try the next trak
        }
    }
    Err("mp4: no supported video track".into())
}

/// Parse one `trak` box; `Ok(None)` = not a video track.
fn demux_trak(
    b: &[u8],
    tk: &[u8],
    movie_ts: u32,
    movie_dur: u64,
) -> Result<Option<DemuxedVideo>, String> {
    let mut width = 0u32;
    let mut height = 0u32;
    let mut media_ts = 0u32;
    let mut media_dur = 0u64;
    let mut is_video = false;
    let mut codec: Option<VideoCodec> = None;
    let mut extradata: Vec<u8> = Vec::new();
    let mut stts: Vec<(u32, u32)> = Vec::new();
    let mut stss: Vec<u32> = Vec::new();
    let mut stsc: Vec<(u32, u32)> = Vec::new();
    let mut uniform_size = 0u32;
    let mut sizes: Vec<u32> = Vec::new();
    let mut chunk_offs: Vec<u64> = Vec::new();

    for (t, s, e) in box_list(tk) {
        let p = &tk[s..e];
        match &t[..] {
            // tkhd ends with 16.16 fixed width/height in every version.
            b"tkhd" if p.len() >= 8 => {
                width = be_u32(p, p.len() - 8) >> 16;
                height = be_u32(p, p.len() - 4) >> 16;
            }
            b"mdia" => {
                for (t2, s2, e2) in box_list(p) {
                    let q = &p[s2..e2];
                    match &t2[..] {
                        b"mdhd" if q.len() >= 20 => {
                            let ver = q[0];
                            let toff = 4 + if ver == 1 { 16 } else { 8 };
                            if q.len() >= toff + 4 {
                                media_ts = be_u32(q, toff);
                                let doff = toff + 4;
                                if q.len() >= doff + 8 {
                                    media_dur = if ver == 1 {
                                        be_u64(q, doff)
                                    } else {
                                        be_u32(q, doff) as u64
                                    };
                                }
                            }
                        }
                        b"hdlr" if q.len() >= 12 => {
                            is_video |= &q[8..12] == b"vide";
                        }
                        b"minf" => {
                            for (t3, s3, e3) in box_list(q) {
                                if t3 != *b"stbl" {
                                    continue;
                                }
                                for (t4, s4, e4) in box_list(&q[s3..e3]) {
                                    let sp = &q[s3..e3][s4..e4];
                                    match &t4[..] {
                                        b"stsd" => {
                                            if sp.len() < 8 {
                                                continue;
                                            }
                                            for (et, es, ee) in box_list(&sp[8..]) {
                                                let ep = &sp[8..][es..ee];
                                                // `et` IS the fourcc — `ep`
                                                // starts after the 8-byte box
                                                // header, inside the entry.
                                                if let Some(c) = VideoCodec::from_mp4_fourcc(&et) {
                                                    codec = Some(c);
                                                    // Visual sample entry: 78
                                                    // bytes of fixed body after
                                                    // the fourcc, then child
                                                    // boxes (avcC …) — 86 from
                                                    // the box start, 78 from ep.
                                                    let kids = 78;
                                                    if ep.len() > kids {
                                                        for (kt, ks, ke) in box_list(&ep[kids..]) {
                                                            let kp = &ep[kids..][ks..ke];
                                                            if matches!(
                                                                &kt[..],
                                                                b"avcC"
                                                                    | b"hvcC"
                                                                    | b"vpcC"
                                                                    | b"av1C"
                                                            ) {
                                                                extradata = kp.to_vec();
                                                            }
                                                        }
                                                    }
                                                    break;
                                                }
                                            }
                                        }
                                        b"stts" => {
                                            if sp.len() >= 8 {
                                                let n = be_u32(sp, 4) as usize;
                                                for i in 0..n {
                                                    if 8 + i * 8 + 8 > sp.len() {
                                                        break;
                                                    }
                                                    stts.push((
                                                        be_u32(sp, 8 + i * 8),
                                                        be_u32(sp, 12 + i * 8),
                                                    ));
                                                }
                                            }
                                        }
                                        b"stss" => {
                                            if sp.len() >= 8 {
                                                let n = be_u32(sp, 4) as usize;
                                                for i in 0..n {
                                                    if 8 + i * 4 + 4 > sp.len() {
                                                        break;
                                                    }
                                                    stss.push(be_u32(sp, 8 + i * 4));
                                                }
                                            }
                                        }
                                        b"stsc" => {
                                            if sp.len() >= 8 {
                                                let n = be_u32(sp, 4) as usize;
                                                for i in 0..n {
                                                    if 8 + i * 12 + 12 > sp.len() {
                                                        break;
                                                    }
                                                    stsc.push((
                                                        be_u32(sp, 8 + i * 12),
                                                        be_u32(sp, 12 + i * 12),
                                                    ));
                                                }
                                            }
                                        }
                                        b"stsz" => {
                                            if sp.len() >= 12 {
                                                uniform_size = be_u32(sp, 4);
                                                let n = be_u32(sp, 8) as usize;
                                                if uniform_size == 0 {
                                                    for i in 0..n {
                                                        if 12 + i * 4 + 4 > sp.len() {
                                                            break;
                                                        }
                                                        sizes.push(be_u32(sp, 12 + i * 4));
                                                    }
                                                }
                                            }
                                        }
                                        b"stco" => {
                                            if sp.len() >= 8 {
                                                let n = be_u32(sp, 4) as usize;
                                                for i in 0..n {
                                                    if 8 + i * 4 + 4 > sp.len() {
                                                        break;
                                                    }
                                                    chunk_offs.push(be_u32(sp, 8 + i * 4) as u64);
                                                }
                                            }
                                        }
                                        b"co64" => {
                                            if sp.len() >= 8 {
                                                let n = be_u32(sp, 4) as usize;
                                                for i in 0..n {
                                                    if 8 + i * 8 + 8 > sp.len() {
                                                        break;
                                                    }
                                                    chunk_offs.push(be_u64(sp, 8 + i * 8));
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    if !is_video {
        return Ok(None);
    }
    let codec = codec.ok_or("mp4: video track without a supported sample entry")?;
    let scale = if media_ts > 0 { media_ts as f64 } else { 1.0 };

    // Per-sample decode durations from stts.
    let mut durs: Vec<u32> = Vec::new();
    for &(cnt, delta) in &stts {
        for _ in 0..cnt {
            durs.push(delta);
        }
    }

    // Expand stsc into a per-chunk sample count (chunks are 1-based).
    let mut per_chunk: Vec<u32> = Vec::with_capacity(chunk_offs.len());
    for (i, &(first, spc)) in stsc.iter().enumerate() {
        let first = first as usize;
        if first == 0 {
            continue;
        }
        let next = stsc
            .get(i + 1)
            .map(|&(f, _)| f as usize)
            .unwrap_or(chunk_offs.len() + 1);
        for _ in first..next.min(chunk_offs.len() + 1) {
            per_chunk.push(spc);
        }
    }

    let all_key = stss.is_empty();
    let mut samples = Vec::new();
    let mut ts: u64 = 0;
    let mut si: usize = 0;
    for (ci, &off) in chunk_offs.iter().enumerate() {
        let spc = per_chunk.get(ci).copied().unwrap_or(0) as usize;
        let mut pos = off;
        for _ in 0..spc {
            let size = if uniform_size != 0 {
                uniform_size as usize
            } else {
                *sizes.get(si).unwrap_or(&0) as usize
            };
            if size == 0 {
                break;
            }
            let end = pos + size as u64;
            if end > b.len() as u64 {
                break;
            }
            let dur = *durs.get(si).unwrap_or(&0) as u64;
            // pts from accumulated stts deltas; B-frame reordering via ctts
            // is not applied (fixtures are baseline/VP8/9 — no B frames).
            samples.push(VideoSample {
                data: b[pos as usize..end as usize].to_vec(),
                pts_ms: ts as f64 * 1000.0 / scale,
                keyframe: all_key || stss.contains(&(si as u32 + 1)),
            });
            ts += dur;
            pos = end;
            si += 1;
        }
    }

    let duration_ms = if media_dur > 0 {
        media_dur as f64 * 1000.0 / scale
    } else if movie_dur > 0 && movie_ts > 0 {
        movie_dur as f64 * 1000.0 / movie_ts as f64
    } else {
        ts as f64 * 1000.0 / scale
    };
    let fps = if duration_ms > 0.0 {
        samples.len() as f64 * 1000.0 / duration_ms
    } else {
        0.0
    };
    Ok(Some(DemuxedVideo {
        track: VideoTrackInfo {
            codec,
            width,
            height,
            extradata,
            fps,
        },
        samples,
        duration_ms,
    }))
}

// ---------------------------------------------------------------------------
// EBML / WebM
// ---------------------------------------------------------------------------

/// Read an EBML element ID (vint with the marker bit KEPT — ids are
/// compared with the marker set, exactly as serialized).
fn ebml_id(b: &[u8]) -> Option<(u32, usize)> {
    if b.is_empty() {
        return None;
    }
    let len = b[0].leading_zeros() as usize + 1;
    if len > 4 || b.len() < len {
        return None;
    }
    let mut id = b[0] as u32;
    for k in 1..len {
        id = (id << 8) | b[k] as u32;
    }
    Some((id, len))
}

/// Read a size vint (marker bit stripped). All-ones = unknown size,
/// reported as u64::MAX.
fn ebml_size(b: &[u8]) -> Option<(u64, usize)> {
    let (v, len) = ebml_vint_raw(b)?;
    if v == (1u64 << (7 * len as u32)) - 1 {
        return Some((u64::MAX, len));
    }
    Some((v, len))
}

/// Raw size-vint value with the marker stripped, no unknown-size mapping
/// (lacing deltas need the un-mapped value for signed interpretation).
fn ebml_vint_raw(b: &[u8]) -> Option<(u64, usize)> {
    if b.is_empty() {
        return None;
    }
    let len = b[0].leading_zeros() as usize + 1;
    if len > 8 || b.len() < len {
        return None;
    }
    // A length-8 vint spends all 8 first-byte bits on the marker.
    let mut v = if len >= 8 {
        0
    } else {
        (b[0] & (0xFFu8 >> len)) as u64
    };
    for k in 1..len {
        v = (v << 8) | b[k] as u64;
    }
    Some((v, len))
}

fn ebml_uint(p: &[u8]) -> Option<u64> {
    if p.is_empty() || p.len() > 8 {
        return None;
    }
    let mut v = 0u64;
    for &b in p {
        v = (v << 8) | b as u64;
    }
    Some(v)
}

fn ebml_float(p: &[u8]) -> f64 {
    match p.len() {
        4 => f32::from_be_bytes(p[..4].try_into().unwrap()) as f64,
        8 => f64::from_be_bytes(p[..8].try_into().unwrap()),
        _ => 0.0,
    }
}

struct WebmScan {
    scale_ns: f64,
    duration_raw: f64,
    cluster_tc: f64,
    samples: Vec<VideoSample>,
    // Current TrackEntry scratch (entries may appear in any order).
    cur_no: u64,
    tmp_type: u64,
    tmp_codec: String,
    tmp_private: Vec<u8>,
    tmp_w: u32,
    tmp_h: u32,
    // The promoted video track (TrackType == 1).
    video_no: u64,
    codec_id: String,
    codec_private: Vec<u8>,
    width: u32,
    height: u32,
}

const SEGMENT: u32 = 0x18538067;
const INFO: u32 = 0x1549A966;
const TIMECODE_SCALE: u32 = 0x2AD7B1;
const DURATION: u32 = 0x4489;
const TRACKS: u32 = 0x1654AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const CLUSTER: u32 = 0x1F43B675;
const CLUSTER_TC: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;

fn demux_webm(b: &[u8]) -> Result<DemuxedVideo, String> {
    let mut st = WebmScan {
        scale_ns: 1_000_000.0,
        duration_raw: 0.0,
        cluster_tc: 0.0,
        samples: Vec::new(),
        cur_no: 0,
        tmp_type: 0,
        tmp_codec: String::new(),
        tmp_private: Vec::new(),
        tmp_w: 0,
        tmp_h: 0,
        video_no: 0,
        codec_id: String::new(),
        codec_private: Vec::new(),
        width: 0,
        height: 0,
    };
    webm_walk(b, &mut st, 0);

    let codec = match st.codec_id.as_str() {
        "V_VP9" => VideoCodec::Vp9,
        "V_VP8" => VideoCodec::Vp8,
        "V_AV1" => VideoCodec::Av1,
        "V_MPEG4/ISO/AVC" => VideoCodec::H264,
        "" => return Err("webm: no track entry found".into()),
        other => return Err(format!("webm: unsupported codec {other}")),
    };
    if st.samples.is_empty() {
        return Err("webm: no video blocks".into());
    }
    let duration_ms = if st.duration_raw > 0.0 {
        st.duration_raw * st.scale_ns / 1_000_000.0
    } else {
        st.samples.last().map(|s| s.pts_ms).unwrap_or(0.0)
    };
    let fps = if duration_ms > 0.0 {
        st.samples.len() as f64 * 1000.0 / duration_ms
    } else {
        0.0
    };
    Ok(DemuxedVideo {
        track: VideoTrackInfo {
            codec,
            width: st.width,
            height: st.height,
            extradata: st.codec_private,
            fps,
        },
        samples: st.samples,
        duration_ms,
    })
}

/// Walk EBML elements recursively, harvesting the track metadata and video
/// blocks. Depth-guarded; malformed tails stop the walk.
fn webm_walk(data: &[u8], st: &mut WebmScan, depth: usize) {
    if depth > 12 {
        return;
    }
    let mut i = 0usize;
    while i < data.len() {
        let Some((id, idlen)) = ebml_id(&data[i..]) else {
            break;
        };
        i += idlen;
        let Some((size, szlen)) = ebml_size(&data[i..]) else {
            break;
        };
        i += szlen;
        let unknown = size == u64::MAX;
        let end = if unknown || i as u64 + size > data.len() as u64 {
            data.len()
        } else {
            i + size as usize
        };
        if i > end {
            break;
        }
        let payload = &data[i..end];
        match id {
            SEGMENT | INFO | TRACKS | VIDEO | CLUSTER | BLOCK_GROUP => {
                if id == CLUSTER {
                    st.cluster_tc = 0.0; // blocks are relative to their cluster
                }
                webm_walk(payload, st, depth + 1);
            }
            TIMECODE_SCALE => {
                if let Some(v) = ebml_uint(payload) {
                    if v > 0 {
                        st.scale_ns = v as f64;
                    }
                }
            }
            DURATION => st.duration_raw = ebml_float(payload),
            CLUSTER_TC => {
                if let Some(v) = ebml_uint(payload) {
                    st.cluster_tc = v as f64;
                }
            }
            TRACK_ENTRY => {
                // Each entry owns the scratch; promote it into the video
                // track slots when TrackType == 1, restore afterwards.
                let saved = (
                    st.cur_no,
                    st.tmp_type,
                    std::mem::take(&mut st.tmp_codec),
                    std::mem::take(&mut st.tmp_private),
                    st.tmp_w,
                    st.tmp_h,
                );
                st.cur_no = 0;
                st.tmp_type = 0;
                st.tmp_w = 0;
                st.tmp_h = 0;
                webm_walk(payload, st, depth + 1);
                if st.tmp_type == 1 {
                    st.video_no = st.cur_no;
                    st.codec_id = st.tmp_codec.clone();
                    st.codec_private = st.tmp_private.clone();
                    st.width = st.tmp_w;
                    st.height = st.tmp_h;
                }
                st.cur_no = saved.0;
                st.tmp_type = saved.1;
                st.tmp_codec = saved.2;
                st.tmp_private = saved.3;
                st.tmp_w = saved.4;
                st.tmp_h = saved.5;
            }
            TRACK_NUMBER => {
                if let Some(v) = ebml_uint(payload) {
                    st.cur_no = v;
                }
            }
            TRACK_TYPE => {
                if let Some(v) = ebml_uint(payload) {
                    st.tmp_type = v;
                }
            }
            CODEC_ID => st.tmp_codec = String::from_utf8_lossy(payload).into_owned(),
            CODEC_PRIVATE => st.tmp_private = payload.to_vec(),
            PIXEL_WIDTH => {
                if let Some(v) = ebml_uint(payload) {
                    st.tmp_w = v as u32;
                }
            }
            PIXEL_HEIGHT => {
                if let Some(v) = ebml_uint(payload) {
                    st.tmp_h = v as u32;
                }
            }
            SIMPLE_BLOCK => webm_block(payload, st, true),
            BLOCK => webm_block(payload, st, false),
            _ => {}
        }
        if unknown {
            break;
        }
        i = end;
    }
}

/// Decode a SimpleBlock/Block payload: track vint, timecode i16, flags u8,
/// then frames (with optional lacing).
fn webm_block(p: &[u8], st: &mut WebmScan, is_simple: bool) {
    // Track number is a plain vint (marker stripped) — not an EBML id.
    let Some((tno, tl)) = ebml_vint_raw(p) else {
        return;
    };
    // Only the video track's blocks — files with audio tracks interleave
    // clusters (accept everything when no TrackEntry was seen, streaming).
    if st.video_no != 0 && tno != st.video_no {
        return;
    }
    let mut i = tl;
    if p.len() < i + 3 {
        return;
    }
    let tc = i16::from_be_bytes([p[i], p[i + 1]]) as i64;
    let flags = p[i + 2];
    i += 3;
    // 0x80 = keyframe on SimpleBlock; Block inside a BlockGroup doesn't
    // carry it (reference-count semantics) — approximate as a keyframe.
    let kf = if is_simple { flags & 0x80 != 0 } else { true };
    let pts_ms = (st.cluster_tc + tc as f64) * st.scale_ns / 1_000_000.0;
    match (flags >> 1) & 0x03 {
        0 => st.samples.push(VideoSample {
            data: p[i..].to_vec(),
            pts_ms,
            keyframe: kf,
        }),
        1 => {
            // Xiph lacing: frame count-1, then n-1 burst-coded lengths.
            if i >= p.len() {
                return;
            }
            let n = p[i] as usize;
            i += 1;
            let mut lens = Vec::with_capacity(n + 1);
            for _ in 0..n {
                let mut l = 0usize;
                loop {
                    if i >= p.len() {
                        return;
                    }
                    let byte = p[i];
                    i += 1;
                    l += byte as usize;
                    if byte != 255 {
                        break;
                    }
                }
                lens.push(l);
            }
            push_laced(p, i, &lens, pts_ms, kf, st);
        }
        2 => {
            // Fixed-size lacing: frame count, then equal splits.
            if i >= p.len() {
                return;
            }
            let n = p[i] as usize;
            i += 1;
            let total = p.len() - i;
            if n == 0 || total % n != 0 {
                return;
            }
            let l = total / n;
            for k in 0..n {
                let s = i + k * l;
                st.samples.push(VideoSample {
                    data: p[s..s + l].to_vec(),
                    pts_ms,
                    keyframe: kf,
                });
            }
        }
        _ => {
            // EBML lacing: frame count-1, first size vint, then signed
            // size-delta vints.
            if i >= p.len() {
                return;
            }
            let n = p[i] as usize + 1;
            i += 1;
            let Some((s0, sl)) = ebml_vint_raw(&p[i..]) else {
                return;
            };
            i += sl;
            let mut lens = vec![s0 as usize];
            for _ in 1..n {
                let Some((raw, dl)) = ebml_vint_raw(&p[i..]) else {
                    return;
                };
                i += dl;
                let bits = 7 * dl as u32;
                let delta = if raw & (1u64 << (bits - 1)) != 0 {
                    raw as i64 - (1i64 << bits)
                } else {
                    raw as i64
                };
                lens.push((*lens.last().unwrap() as i64 + delta).max(0) as usize);
            }
            push_laced(p, i, &lens, pts_ms, kf, st);
        }
    }
}

fn push_laced(p: &[u8], mut i: usize, lens: &[usize], pts_ms: f64, kf: bool, st: &mut WebmScan) {
    for &l in lens {
        if l == 0 || i + l > p.len() {
            return;
        }
        st.samples.push(VideoSample {
            data: p[i..i + l].to_vec(),
            pts_ms,
            keyframe: kf,
        });
        i += l;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let root = env!("CARGO_MANIFEST_DIR");
        std::fs::read(format!("{root}/tests/fixtures/{name}"))
            .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    #[test]
    fn mp4_h264_demux() {
        let b = fixture("clip.mp4");
        assert!(is_mp4(&b));
        let d = demux(&b).unwrap();
        assert_eq!(d.track.codec, VideoCodec::H264);
        assert!(d.track.width >= 320 && d.track.height >= 240);
        assert!(!d.samples.is_empty());
        assert!(d.samples[0].keyframe);
        assert!(!d.track.extradata.is_empty());
        assert_eq!(d.track.extradata[0], 1); // avcC configurationVersion
        assert!((d.track.fps - 10.0).abs() < 2.0);
        assert!(d.duration_ms > 2000.0 && d.duration_ms < 5000.0);
    }

    #[test]
    fn webm_vp9_demux() {
        let b = fixture("clip.webm");
        assert!(is_webm(&b));
        let d = demux(&b).unwrap();
        assert_eq!(d.track.codec, VideoCodec::Vp9);
        assert_eq!(d.track.width, 320);
        assert_eq!(d.track.height, 240);
        assert!(!d.samples.is_empty());
        assert!(d.samples[0].keyframe);
        assert!(!d.samples[0].data.is_empty());
        assert!(d.duration_ms > 2000.0);
    }

    #[test]
    fn rejects_garbage() {
        assert!(demux(b"not a video at all").is_err());
        assert!(!is_webm(b"\x1a\x45\xdf\xa9garbage"));
    }
}
