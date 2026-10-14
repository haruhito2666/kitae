//! Web font containers (Q11): WOFF 1.0 and WOFF 2.0 → sfnt bytes.
//!
//! `@font-face` payloads arrive as WOFF/woff2 wrappers, but Core Text only
//! registers raw sfnt (TTF/OTF) data. These decoders reverse the container —
//! inflate per-table zlib (WOFF 1.0) or brotli plus the glyf/loca/hmtx
//! table transforms (WOFF 2.0) — and hand back an in-memory sfnt image, so a
//! web font never touches disk (staged files are gone; see
//! `kitae_core::ensure_web_font`).
//!
//! The WOFF 2.0 path is a faithful port of the reference decoder
//! (`woff2_dec.cc`, MIT, Google) including its reject rules: bad signatures,
//! length mismatches, oversized claims (64 MB bomb guard, 100× compression
//! ratio), `loca` with a nonzero transform length, an empty glyph carrying a
//! bbox, a composite without one, `glyf` without `loca`, transform-flag
//! disagreements between the pair, unknown transforms, and every truncated
//! substream. A font that fails any of these returns `MediaError::Font` and
//! the engine falls back to the next `src:` entry (CSS Fonts behavior).
//!
//! No JS, no WASM, no threads: decode is a pure function of bytes.

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;

use crate::MediaError;

/// Decompressed-sfnt bomb guard (same spirit as [`crate::MAX_PIXELS`]).
const MAX_FONT_BYTES: usize = 64 * 1024 * 1024;

const WOFF1_SIGNATURE: &[u8; 4] = b"wOFF";
const WOFF2_SIGNATURE: u32 = 0x774F_4632; // 'wOF2'

const fn tag4(bytes: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*bytes)
}

pub const TAG_GLYF: u32 = tag4(b"glyf");
pub const TAG_LOCA: u32 = tag4(b"loca");
pub const TAG_HEAD: u32 = tag4(b"head");
pub const TAG_HHEA: u32 = tag4(b"hhea");
pub const TAG_HMTX: u32 = tag4(b"hmtx");
const TAG_TTCF: u32 = tag4(b"ttcf");

/// `kKnownTags[63]` from the reference `table_tags.cc`; index 63 means an
/// explicit u32 tag follows in the directory.
const KNOWN_TAGS: [u32; 63] = [
    tag4(b"cmap"),
    tag4(b"head"),
    tag4(b"hhea"),
    tag4(b"hmtx"),
    tag4(b"maxp"),
    tag4(b"name"),
    tag4(b"OS/2"),
    tag4(b"post"),
    tag4(b"cvt "),
    tag4(b"fpgm"),
    tag4(b"glyf"),
    tag4(b"loca"),
    tag4(b"prep"),
    tag4(b"CFF "),
    tag4(b"VORG"),
    tag4(b"EBDT"),
    tag4(b"EBLC"),
    tag4(b"gasp"),
    tag4(b"hdmx"),
    tag4(b"kern"),
    tag4(b"LTSH"),
    tag4(b"PCLT"),
    tag4(b"VDMX"),
    tag4(b"vhea"),
    tag4(b"vmtx"),
    tag4(b"BASE"),
    tag4(b"GDEF"),
    tag4(b"GPOS"),
    tag4(b"GSUB"),
    tag4(b"EBSC"),
    tag4(b"JSTF"),
    tag4(b"MATH"),
    tag4(b"CBDT"),
    tag4(b"CBLC"),
    tag4(b"COLR"),
    tag4(b"CPAL"),
    tag4(b"SVG "),
    tag4(b"sbix"),
    tag4(b"acnt"),
    tag4(b"avar"),
    tag4(b"bdat"),
    tag4(b"bloc"),
    tag4(b"bsln"),
    tag4(b"cvar"),
    tag4(b"fdsc"),
    tag4(b"feat"),
    tag4(b"fmtx"),
    tag4(b"fvar"),
    tag4(b"gvar"),
    tag4(b"hsty"),
    tag4(b"just"),
    tag4(b"lcar"),
    tag4(b"mort"),
    tag4(b"morx"),
    tag4(b"opbd"),
    tag4(b"prop"),
    tag4(b"trak"),
    tag4(b"Zapf"),
    tag4(b"Silf"),
    tag4(b"Glat"),
    tag4(b"Gloc"),
    tag4(b"Feat"),
    tag4(b"Sill"),
];

// simple glyph flags (TrueType)
const K_GLYF_ON_CURVE: u8 = 1 << 0;
const K_GLYF_X_SHORT: u8 = 1 << 1;
const K_GLYF_Y_SHORT: u8 = 1 << 2;
const K_GLYF_REPEAT: u8 = 1 << 3;
const K_GLYF_THIS_X_IS_SAME: u8 = 1 << 4;
const K_GLYF_THIS_Y_IS_SAME: u8 = 1 << 5;
const K_OVERLAP_SIMPLE: u8 = 1 << 6;

// composite glyph flags
const FLAG_ARG_1_AND_2_ARE_WORDS: u16 = 1 << 0;
const FLAG_WE_HAVE_A_SCALE: u16 = 1 << 3;
const FLAG_MORE_COMPONENTS: u16 = 1 << 5;
const FLAG_WE_HAVE_AN_X_AND_Y_SCALE: u16 = 1 << 6;
const FLAG_WE_HAVE_A_TWO_BY_TWO: u16 = 1 << 7;
const FLAG_WE_HAVE_INSTRUCTIONS: u16 = 1 << 8;

/// kMaxPlausibleCompressionRatio: over 14k test fonts the observed max was
/// ~20; above this the uncompressed_size claim is a lie.
const MAX_COMPRESSION_RATIO: f64 = 100.0;

fn round4(x: usize) -> usize {
    (x + 3) & !3
}

fn pad4(out: &mut Vec<u8>) {
    let pad = (4 - out.len() % 4) % 4;
    out.resize(out.len() + pad, 0);
}

/// `ComputeULongSum`: big-endian u32 words, tail padded with zeros.
fn ulong_sum(buf: &[u8]) -> u32 {
    let mut sum: u32 = 0;
    let aligned = buf.len() & !3;
    for w in buf[..aligned].chunks_exact(4) {
        sum = sum.wrapping_add(u32::from_be_bytes([w[0], w[1], w[2], w[3]]));
    }
    let mut tail: u32 = 0;
    for (i, &b) in buf[aligned..].iter().enumerate() {
        tail |= (b as u32) << (24 - 8 * i);
    }
    sum.wrapping_add(tail)
}

/// Big-endian byte cursor with bounds-checked reads (`Buffer` in the
/// reference).
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    fn skip(&mut self, n: usize) -> bool {
        match self.buf.len().checked_sub(self.pos) {
            Some(rem) if rem >= n => {
                self.pos += n;
                true
            }
            _ => false,
        }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn u8(&mut self) -> Option<u8> {
        let v = self.buf.get(self.pos).copied();
        if v.is_some() {
            self.pos += 1;
        }
        v
    }

    fn u16(&mut self) -> Option<u16> {
        let v = self.buf.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(u16::from_be_bytes([v[0], v[1]]))
    }

    fn s16(&mut self) -> Option<i16> {
        self.u16().map(|v| v as i16)
    }

    fn u32(&mut self) -> Option<u32> {
        let v = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
    }

    fn read(&mut self, n: usize) -> Option<&'a [u8]> {
        let v = self.buf.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(v)
    }

    /// `Read255UShort` (variable_length.cc).
    fn read255ushort(&mut self) -> Option<u32> {
        let b0 = self.u8()?;
        match b0 {
            0..=252 => Some(b0 as u32),
            253 => {
                let hi = self.u8()? as u32;
                let lo = self.u8()? as u32;
                Some((hi << 8) | lo)
            }
            254 => Some(self.u8()? as u32 + 506),
            _ => Some(self.u8()? as u32 + 253),
        }
    }

    /// `ReadBase128`: 7 bits per byte, MSB continuation; rejects the
    /// non-minimal encoding of 0, values above u32, and >5 bytes.
    fn base128(&mut self) -> Option<u32> {
        let mut acc: u32 = 0;
        for i in 0..5 {
            let code = self.u8()?;
            if i == 0 && code == 0x80 {
                return None;
            }
            if acc & 0xFE00_0000 != 0 {
                return None;
            }
            acc = (acc << 7) | (code & 0x7f) as u32;
            if code & 0x80 == 0 {
                return Some(acc);
            }
        }
        None
    }
}

// ───────────────────────────── WOFF 1.0 ─────────────────────────────

/// Decode a WOFF 1.0 container into its raw sfnt (TTF/OTF) bytes.
///
/// Per-table zlib payloads are inflated; tables stored uncompressed are
/// copied. The output is a well-formed sfnt: 12-byte offset table, 16-byte
/// directory entries sorted by tag, each table 4-byte aligned.
pub fn decode_woff(bytes: &[u8]) -> Result<Vec<u8>, MediaError> {
    decode_woff_inner(bytes).map_err(|m| MediaError::Font(format!("woff1: {m}")))
}

fn decode_woff_inner(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() < 44 {
        return Err("truncated header".into());
    }
    if &bytes[0..4] != WOFF1_SIGNATURE {
        return Err("bad signature".into());
    }
    let flavor = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
    if u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize != bytes.len() {
        return Err("length field mismatch".into());
    }
    let num_tables = u16::from_be_bytes(bytes[12..14].try_into().unwrap());
    if num_tables == 0 {
        return Err("no tables".into());
    }
    if bytes.len() < 44 + num_tables as usize * 20 {
        return Err("truncated table directory".into());
    }

    struct W1Table {
        tag: u32,
        checksum: u32,
        data: Vec<u8>,
    }
    let mut tables: Vec<W1Table> = Vec::with_capacity(num_tables as usize);
    for i in 0..num_tables as usize {
        let e = 44 + i * 20;
        let tag = u32::from_be_bytes(bytes[e..e + 4].try_into().unwrap());
        let offset = u32::from_be_bytes(bytes[e + 4..e + 8].try_into().unwrap()) as usize;
        let comp_length = u32::from_be_bytes(bytes[e + 8..e + 12].try_into().unwrap()) as usize;
        let orig_length = u32::from_be_bytes(bytes[e + 12..e + 16].try_into().unwrap()) as usize;
        let checksum = u32::from_be_bytes(bytes[e + 16..e + 20].try_into().unwrap());
        if orig_length > MAX_FONT_BYTES {
            return Err("table too large".into());
        }
        let data = if comp_length < orig_length {
            let src = bytes
                .get(offset..offset + comp_length)
                .ok_or("table data out of bounds")?;
            let mut out = vec![0u8; orig_length];
            flate2::read::ZlibDecoder::new(src)
                .read_exact(&mut out)
                .map_err(|e| format!("zlib inflate failed: {e}"))?;
            out
        } else {
            if comp_length != orig_length {
                return Err("compressed length exceeds original".into());
            }
            bytes
                .get(offset..offset + comp_length)
                .ok_or("table data out of bounds")?
                .to_vec()
        };
        tables.push(W1Table {
            tag,
            checksum,
            data,
        });
    }

    // Assemble the sfnt: offset table + tag-sorted entries + aligned data.
    tables.sort_by_key(|t| t.tag);
    let mut out = Vec::new();
    store_offset_table(&mut out, flavor, num_tables);
    let mut offsets = Vec::with_capacity(tables.len());
    let mut pos = out.len() + tables.len() * 16;
    for t in &tables {
        offsets.push(pos as u32);
        pos = round4(pos + t.data.len());
    }
    for (i, t) in tables.iter().enumerate() {
        out.extend_from_slice(&t.tag.to_be_bytes());
        out.extend_from_slice(&t.checksum.to_be_bytes());
        out.extend_from_slice(&offsets[i].to_be_bytes());
        out.extend_from_slice(&(t.data.len() as u32).to_be_bytes());
    }
    for t in &tables {
        out.extend_from_slice(&t.data);
        pad4(&mut out);
    }
    Ok(out)
}

// ───────────────────────────── WOFF 2.0 ─────────────────────────────

/// One table directory entry (reference `Table`).
struct W2Table {
    tag: u32,
    transformed: bool,
    /// Offset of the (transformed) data inside the decompressed blob.
    src_offset: usize,
    /// Its length (`transform_length` in the reference).
    src_length: usize,
    dst_offset: u32,
    /// Directory `origLength` until reconstruction overwrites it.
    dst_length: u32,
}

/// One font of a collection (reference `TtcFont`).
struct W2Font {
    flavor: u32,
    table_indices: Vec<u16>,
    dst_offset: u32,
    header_checksum: u32,
}

/// Per-font reconstruction state (reference `WOFF2FontInfo`).
#[derive(Default)]
struct FontInfo {
    num_glyphs: u16,
    index_format: u16,
    num_hmetrics: u16,
    x_mins: Vec<i16>,
    /// Output position of each 16-byte directory entry, by tag.
    entry_pos_by_tag: HashMap<u32, u32>,
}

struct Point {
    x: i32,
    y: i32,
    on_curve: bool,
}

/// Decode a WOFF 2.0 container into its raw sfnt (TTF/OTF) bytes.
///
/// Handles single fonts and collections (`ttcf`), the brotli-compressed
/// transform stream, and reconstruction of the `glyf`/`loca`/`hmtx` table
/// transforms (plan Q11c). Malformed input yields `MediaError::Font`.
pub fn decode_woff2(bytes: &[u8]) -> Result<Vec<u8>, MediaError> {
    decode_woff2_inner(bytes).map_err(|m| MediaError::Font(format!("woff2: {m}")))
}

fn decode_woff2_inner(bytes: &[u8]) -> Result<Vec<u8>, String> {
    // ── header ──
    let mut file = Cursor::new(bytes);
    if file.u32().ok_or("truncated header")? != WOFF2_SIGNATURE {
        return Err("bad signature".into());
    }
    let flavor = file.u32().ok_or("truncated header")?;
    let reported_length = file.u32().ok_or("truncated header")?;
    if reported_length as usize != bytes.len() {
        return Err("length field mismatch".into());
    }
    let num_tables = file.u16().ok_or("truncated header")?;
    if num_tables == 0 {
        return Err("no tables".into());
    }
    // reserved u16 + totalSfntSize u32 (we compute it ourselves)
    if !file.skip(6) {
        return Err("truncated header".into());
    }
    let compressed_length = file.u32().ok_or("truncated header")? as usize;
    // major/minor version
    if !file.skip(4) {
        return Err("truncated header".into());
    }
    let meta_offset = file.u32().ok_or("truncated header")? as usize;
    let meta_length = file.u32().ok_or("truncated header")? as usize;
    let _meta_orig_length = file.u32().ok_or("truncated header")?;
    let priv_offset = file.u32().ok_or("truncated header")? as usize;
    let priv_length = file.u32().ok_or("truncated header")? as usize;
    if meta_offset != 0 && (meta_offset >= bytes.len() || bytes.len() - meta_offset < meta_length) {
        return Err("metadata out of bounds".into());
    }
    if priv_offset != 0 && (priv_offset >= bytes.len() || bytes.len() - priv_offset < priv_length) {
        return Err("private data out of bounds".into());
    }

    // ── table directory ──
    let mut tables: Vec<W2Table> = Vec::with_capacity(num_tables as usize);
    let mut src_offset: usize = 0;
    for _ in 0..num_tables {
        let flag_byte = file.u8().ok_or("truncated table directory")?;
        let tag = if flag_byte & 0x3f == 0x3f {
            file.u32().ok_or("truncated table directory")?
        } else {
            KNOWN_TAGS[(flag_byte & 0x3f) as usize]
        };
        let xform_version = (flag_byte >> 6) & 0x03;
        // 0 means transformed for glyf/loca, non-0 for everything else.
        let transformed = if tag == TAG_GLYF || tag == TAG_LOCA {
            xform_version == 0
        } else {
            xform_version != 0
        };
        let orig_length = file.base128().ok_or("bad table length")?;
        let mut transform_length = orig_length as usize;
        if transformed {
            transform_length = file.base128().ok_or("bad table length")? as usize;
            if tag == TAG_LOCA && transform_length != 0 {
                return Err("loca transform_length must be zero".into());
            }
        }
        if transform_length > MAX_FONT_BYTES
            || src_offset.checked_add(transform_length).is_none()
            || src_offset + transform_length > MAX_FONT_BYTES
        {
            return Err("table too large".into());
        }
        tables.push(W2Table {
            tag,
            transformed,
            src_offset,
            src_length: transform_length,
            dst_offset: 0,
            dst_length: orig_length,
        });
        src_offset += transform_length;
    }
    // The last table's end is the uncompressed size.
    let uncompressed_size = src_offset;
    if uncompressed_size == 0 {
        return Err("empty compressed stream".into());
    }
    if (uncompressed_size as f64) / (bytes.len() as f64) > MAX_COMPRESSION_RATIO {
        return Err("implausible compression ratio".into());
    }

    // ── collection directory ──
    let mut header_version: u32 = 0;
    let mut ttc_fonts: Vec<W2Font> = Vec::new();
    if flavor == TAG_TTCF {
        header_version = file.u32().ok_or("truncated collection header")?;
        if header_version != 0x0001_0000 && header_version != 0x0002_0000 {
            return Err("bad collection header version".into());
        }
        let num_fonts = file.read255ushort().ok_or("truncated collection header")?;
        if num_fonts == 0 {
            return Err("empty collection".into());
        }
        for _ in 0..num_fonts {
            let n = file.read255ushort().ok_or("truncated collection header")?;
            if n == 0 {
                return Err("collection font with no tables".into());
            }
            let font_flavor = file.u32().ok_or("truncated collection header")?;
            let mut indices = Vec::with_capacity(n as usize);
            let mut glyf_idx: u16 = 0;
            let mut loca_idx: u16 = 0;
            for _ in 0..n {
                let idx = file.read255ushort().ok_or("truncated collection header")?;
                if idx as usize >= tables.len() {
                    return Err("collection table index out of range".into());
                }
                if tables[idx as usize].tag == TAG_LOCA {
                    loca_idx = idx as u16;
                }
                if tables[idx as usize].tag == TAG_GLYF {
                    glyf_idx = idx as u16;
                }
                indices.push(idx as u16);
            }
            // If we have both glyf and loca make sure they are consecutive;
            // if we have just one we'll reject the font elsewhere.
            if glyf_idx > 0 || loca_idx > 0 {
                if glyf_idx > loca_idx || loca_idx - glyf_idx != 1 {
                    return Err("collection glyf/loca not consecutive".into());
                }
            }
            ttc_fonts.push(W2Font {
                flavor: font_flavor,
                table_indices: indices,
                dst_offset: 0,
                header_checksum: 0,
            });
        }
    }

    // ── data chain: compressed block, then optional metadata/private ──
    let compressed_offset = file.pos;
    let mut src_end = round4(compressed_offset + compressed_length);
    if src_end > bytes.len() {
        return Err("compressed data out of bounds".into());
    }
    if meta_offset != 0 {
        if src_end != meta_offset {
            return Err("metadata offset mismatch".into());
        }
        src_end = round4(meta_offset + meta_length);
    }
    if priv_offset != 0 {
        if src_end != priv_offset {
            return Err("private data offset mismatch".into());
        }
        src_end = round4(priv_offset + priv_length);
    }
    if src_end != round4(bytes.len()) {
        return Err("trailing bytes after font data".into());
    }

    let compressed = bytes[compressed_offset..compressed_offset + compressed_length].to_vec();
    let mut blob = brotli_exact(&compressed, uncompressed_size)?;

    // ── headers (offset tables + zeroed directory entries) ──
    // One info per font, created up front so header writing can record
    // where each 16-byte directory entry landed (the reconstruction patch
    // site).
    let mut infos: Vec<FontInfo> = if header_version != 0 {
        (0..ttc_fonts.len()).map(|_| FontInfo::default()).collect()
    } else {
        vec![FontInfo::default()]
    };
    let mut out: Vec<u8> = Vec::new();
    let header_checksum: u32;
    if header_version != 0 {
        out.extend_from_slice(&TAG_TTCF.to_be_bytes());
        out.extend_from_slice(&header_version.to_be_bytes());
        out.extend_from_slice(&(ttc_fonts.len() as u32).to_be_bytes());
        let offset_table_pos = out.len();
        for _ in 0..ttc_fonts.len() {
            out.extend_from_slice(&0u32.to_be_bytes());
        }
        if header_version == 0x0002_0000 {
            out.extend_from_slice(&[0u8; 12]); // ulDsig{Tag,Length,Offset}
        }
        for (i, font) in ttc_fonts.iter_mut().enumerate() {
            // Re-order the font's tables by tag (map semantics dedupe).
            let mut sorted: BTreeMap<u32, u16> = BTreeMap::new();
            for &idx in &font.table_indices {
                sorted.insert(tables[idx as usize].tag, idx);
            }
            font.table_indices = sorted.values().copied().collect();
            // Offset-table location into the TTC header.
            let at = offset_table_pos + i * 4;
            let font_at = out.len() as u32;
            out[at..at + 4].copy_from_slice(&font_at.to_be_bytes());
            font.dst_offset = out.len() as u32;
            store_offset_table(&mut out, font.flavor, font.table_indices.len() as u16);
            for &idx in &font.table_indices {
                let tag = tables[idx as usize].tag;
                let pos = store_table_entry(&mut out, tag);
                infos[i].entry_pos_by_tag.insert(tag, pos);
            }
            font.header_checksum = ulong_sum(&out[font.dst_offset as usize..]);
        }
        header_checksum = 0;
    } else {
        // Re-order tables in output (OTSpec) order; reconstruction then
        // visits them in the same order, so glyf/hhea precede loca/hmtx.
        tables.sort_by_key(|t| t.tag);
        store_offset_table(&mut out, flavor, num_tables);
        for t in &tables {
            let pos = store_table_entry(&mut out, t.tag);
            infos[0].entry_pos_by_tag.insert(t.tag, pos);
        }
        header_checksum = ulong_sum(&out);
    }

    // Per-font table lists: the collection's tag-sorted indices, or all
    // tables for a single font.
    let font_lists: Vec<Vec<u16>> = if header_version != 0 {
        ttc_fonts.iter().map(|f| f.table_indices.clone()).collect()
    } else {
        vec![(0..tables.len() as u16).collect()]
    };

    // ── reconstruct each font's tables ──
    let mut checksums: HashMap<(u32, u32), u32> = HashMap::new();
    for fi in 0..font_lists.len() {
        let list = font_lists[fi].clone();
        let info = &mut infos[fi];
        let mut font_checksum = if header_version != 0 {
            ttc_fonts[fi].header_checksum
        } else {
            header_checksum
        };

        // 'glyf' without 'loca' doesn't make sense (and vice versa).
        let glyf_index = find_tag(&tables, &list, TAG_GLYF);
        let loca_index = find_tag(&tables, &list, TAG_LOCA);
        if glyf_index.is_some() != loca_index.is_some() {
            return Err("cannot have just one of glyf/loca".into());
        }
        if let (Some(g), Some(l)) = (glyf_index, loca_index) {
            if tables[g as usize].transformed != tables[l as usize].transformed {
                return Err("cannot transform just one of glyf/loca".into());
            }
        }

        let mut loca_checksum: u32 = 0;
        for &ti in &list {
            let ti = ti as usize;
            let (tag, transformed, src_offset, src_length) = {
                let t = &tables[ti];
                (t.tag, t.transformed, t.src_offset, t.src_length)
            };
            let key = (tag, src_offset as u32);
            let reused = checksums.contains_key(&key);
            if fi == 0 && reused {
                return Err("duplicate table in first font".into());
            }
            if src_offset + src_length > blob.len() {
                return Err("table data out of bounds".into());
            }

            if tag == TAG_HHEA {
                // numberOfHMetrics: skip 34, read u16.
                let win = &blob[src_offset..src_offset + src_length];
                if win.len() < 36 {
                    return Err("hhea too short".into());
                }
                info.num_hmetrics = u16::from_be_bytes([win[34], win[35]]);
            }

            let checksum;
            if !reused {
                if !transformed {
                    if tag == TAG_HEAD {
                        if src_length < 12 {
                            return Err("head too short".into());
                        }
                        // checkSumAdjustment = 0 while summing
                        blob[src_offset + 8..src_offset + 12].copy_from_slice(&0u32.to_be_bytes());
                    }
                    tables[ti].dst_offset = out.len() as u32;
                    checksum = ulong_sum(&blob[src_offset..src_offset + src_length]);
                    out.extend_from_slice(&blob[src_offset..src_offset + src_length]);
                } else {
                    match tag {
                        TAG_GLYF => {
                            tables[ti].dst_offset = out.len() as u32;
                            // glyf and loca have distinct tags, so ti != li
                            // and split_at_mut can hand out both at once.
                            let li = loca_index.expect("pair checked above") as usize;
                            let (cs, lc) = if ti < li {
                                let (left, right) = tables.split_at_mut(li);
                                reconstruct_glyf(
                                    &blob,
                                    &mut left[ti],
                                    &mut right[0],
                                    info,
                                    &mut out,
                                )?
                            } else {
                                let (left, right) = tables.split_at_mut(ti);
                                reconstruct_glyf(
                                    &blob,
                                    &mut right[0],
                                    &mut left[li],
                                    info,
                                    &mut out,
                                )?
                            };
                            checksum = cs;
                            loca_checksum = lc;
                        }
                        TAG_LOCA => {
                            // All the work was done by reconstruct_glyf.
                            checksum = loca_checksum;
                        }
                        TAG_HMTX => {
                            tables[ti].dst_offset = out.len() as u32;
                            checksum = reconstruct_hmtx(
                                &blob[src_offset..src_offset + src_length],
                                info,
                                &mut out,
                            )?;
                        }
                        _ => return Err("unknown transform".into()),
                    }
                }
                checksums.insert(key, checksum);
            } else {
                checksum = checksums[&key];
            }
            font_checksum = font_checksum.wrapping_add(checksum);

            // Update the directory entry with the real values.
            let (dst_offset, dst_length) = {
                let t = &tables[ti];
                (t.dst_offset, t.dst_length)
            };
            let pos = info.entry_pos_by_tag[&tag] as usize;
            let mut entry = [0u8; 12];
            entry[0..4].copy_from_slice(&checksum.to_be_bytes());
            entry[4..8].copy_from_slice(&dst_offset.to_be_bytes());
            entry[8..12].copy_from_slice(&dst_length.to_be_bytes());
            out[pos + 4..pos + 16].copy_from_slice(&entry);
            font_checksum = font_checksum.wrapping_add(ulong_sum(&entry));

            pad4(&mut out);
            if dst_offset as usize + dst_length as usize > out.len() {
                return Err("table overflow".into());
            }
        }

        // Update 'head' checkSumAdjustment (it was zeroed before summing).
        if let Some(h) = find_tag(&tables, &list, TAG_HEAD) {
            let head = &tables[h as usize];
            if head.dst_length < 12 {
                return Err("head too short".into());
            }
            let adjustment = 0xB1B0_AFBAu32.wrapping_sub(font_checksum);
            let at = head.dst_offset as usize + 8;
            out[at..at + 4].copy_from_slice(&adjustment.to_be_bytes());
        }
    }

    Ok(out)
}

/// Brotli-decompress into exactly `expected` bytes (`Woff2Uncompress`).
fn brotli_exact(input: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; expected];
    let mut reader = brotli::Decompressor::new(input, 4096);
    reader
        .read_exact(&mut out)
        .map_err(|e| format!("brotli decompress failed: {e}"))?;
    let mut extra = [0u8; 1];
    if reader
        .read(&mut extra)
        .map_err(|e| format!("brotli decompress failed: {e}"))?
        != 0
    {
        return Err("decompressed size mismatch".into());
    }
    Ok(out)
}

fn store_offset_table(out: &mut Vec<u8>, flavor: u32, num_tables: u16) {
    out.extend_from_slice(&flavor.to_be_bytes());
    out.extend_from_slice(&num_tables.to_be_bytes());
    let mut max_pow2: u32 = 0;
    while 1u32 << (max_pow2 + 1) <= num_tables as u32 {
        max_pow2 += 1;
    }
    let search_range = (1u32 << max_pow2) << 4;
    out.extend_from_slice(&(search_range as u16).to_be_bytes());
    out.extend_from_slice(&(max_pow2 as u16).to_be_bytes());
    out.extend_from_slice(
        &(((num_tables as u32) << 4).wrapping_sub(search_range) as u16).to_be_bytes(),
    );
}

/// Write one zeroed 16-byte directory entry; returns its position (the
/// patch site for checksum/offset/length is `pos + 4`).
fn store_table_entry(out: &mut Vec<u8>, tag: u32) -> u32 {
    let pos = out.len() as u32;
    out.extend_from_slice(&tag.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    pos
}

/// `FindTable`: first table with `tag` among a font's directory indices.
fn find_tag(tables: &[W2Table], list: &[u16], tag: u32) -> Option<u16> {
    list.iter()
        .copied()
        .find(|&i| tables[i as usize].tag == tag)
}

/// Reconstruct the transformed `glyf` (and write `loca` after it). Returns
/// `(glyf_checksum, loca_checksum)`.
fn reconstruct_glyf(
    blob: &[u8],
    glyf: &mut W2Table,
    loca: &mut W2Table,
    info: &mut FontInfo,
    out: &mut Vec<u8>,
) -> Result<(u32, u32), String> {
    const K_NUM_SUBSTREAMS: usize = 7;
    let data = &blob[glyf.src_offset..glyf.src_offset + glyf.src_length];
    let mut file = Cursor::new(data);
    let glyf_start = out.len();

    let _version = file.u16().ok_or("glyf transform too short")?;
    let flags = file.u16().ok_or("glyf transform too short")?;
    let has_overlap_bitmap = flags & 1 != 0;
    info.num_glyphs = file.u16().ok_or("glyf transform too short")?;
    info.index_format = file.u16().ok_or("glyf transform too short")?;

    // https://dev.w3.org/webfonts/WOFF2/spec/#conform-mustRejectLoca
    let expected_loca_dst_length =
        (if info.index_format != 0 { 4u32 } else { 2 }) * (info.num_glyphs as u32 + 1);
    if loca.dst_length != expected_loca_dst_length {
        return Err("loca length does not match num_glyphs".into());
    }

    let mut offset: usize = (2 + K_NUM_SUBSTREAMS) * 4;
    if offset > data.len() {
        return Err("glyf transform too short".into());
    }
    let mut subs: [(usize, usize); K_NUM_SUBSTREAMS] = [(0, 0); K_NUM_SUBSTREAMS];
    for sub in subs.iter_mut() {
        let size = file.u32().ok_or("glyf transform too short")? as usize;
        if size > data.len() - offset {
            return Err("glyf substream too large".into());
        }
        *sub = (offset, size);
        offset += size;
    }
    let mut n_contour = Cursor::new(&data[subs[0].0..subs[0].0 + subs[0].1]);
    let mut n_points = Cursor::new(&data[subs[1].0..subs[1].0 + subs[1].1]);
    let mut flags_cur = Cursor::new(&data[subs[2].0..subs[2].0 + subs[2].1]);
    let mut glyph_cur = Cursor::new(&data[subs[3].0..subs[3].0 + subs[3].1]);
    let mut composite = Cursor::new(&data[subs[4].0..subs[4].0 + subs[4].1]);
    let mut bbox = Cursor::new(&data[subs[5].0..subs[5].0 + subs[5].1]);
    let mut instructions = Cursor::new(&data[subs[6].0..subs[6].0 + subs[6].1]);

    let overlap_bitmap: Option<&[u8]> = if has_overlap_bitmap {
        let len = (info.num_glyphs as usize + 7) >> 3;
        if len > data.len() - offset {
            return Err("overlap bitmap too large".into());
        }
        Some(&data[offset..offset + len])
    } else {
        None
    };

    let mut loca_values = vec![0u32; info.num_glyphs as usize + 1];
    let bitmap_length = (((info.num_glyphs as u32 + 31) >> 5) << 2) as usize;
    let bbox_bitmap = bbox.read(bitmap_length).ok_or("bbox bitmap too large")?;
    let mut n_points_vec: Vec<u32> = Vec::new();
    let mut points: Vec<Point> = Vec::new();
    info.x_mins = vec![0i16; info.num_glyphs as usize];
    let mut glyf_checksum: u32 = 0;

    for i in 0..info.num_glyphs as usize {
        let mut glyph: Vec<u8> = Vec::new();
        let have_bbox = bbox_bitmap[i >> 3] & (0x80 >> (i & 7)) != 0;
        let n_contours = n_contour.u16().ok_or("nContours stream exhausted")?;

        if n_contours == 0xFFFF {
            // composite glyph — must have an explicit bbox
            if !have_bbox {
                return Err("composite glyph missing bbox".into());
            }
            let (composite_size, have_instructions) = size_of_composite(&mut composite)?;
            let mut instruction_size: u32 = 0;
            if have_instructions {
                instruction_size = glyph_cur.read255ushort().ok_or("glyph stream exhausted")?;
            }
            glyph.extend_from_slice(&n_contours.to_be_bytes());
            glyph.extend_from_slice(bbox.read(8).ok_or("bbox stream exhausted")?);
            glyph.extend_from_slice(
                composite
                    .read(composite_size)
                    .ok_or("composite stream exhausted")?,
            );
            if have_instructions {
                glyph.extend_from_slice(&(instruction_size as u16).to_be_bytes());
                glyph.extend_from_slice(
                    instructions
                        .read(instruction_size as usize)
                        .ok_or("instruction stream exhausted")?,
                );
            }
        } else if n_contours > 0 {
            // simple glyph
            n_points_vec.clear();
            points.clear();
            let mut total: u32 = 0;
            for _ in 0..n_contours {
                let n = n_points.read255ushort().ok_or("nPoints stream exhausted")?;
                total = total.checked_add(n).ok_or("point count overflow")?;
                n_points_vec.push(n);
            }
            if total as usize > flags_cur.remaining() {
                return Err("flag stream exhausted".into());
            }
            let flags_buf = &flags_cur.buf[flags_cur.pos..flags_cur.pos + total as usize];
            let triplet_buf = &glyph_cur.buf[glyph_cur.pos..];
            let consumed = triplet_decode(flags_buf, triplet_buf, total, &mut points)?;
            flags_cur.skip(total as usize);
            glyph_cur.skip(consumed);
            let instruction_size = glyph_cur.read255ushort().ok_or("glyph stream exhausted")?;
            if total >= (1u32 << 27) || instruction_size >= (1u32 << 30) {
                return Err("glyph too large".into());
            }

            glyph.extend_from_slice(&n_contours.to_be_bytes());
            if have_bbox {
                glyph.extend_from_slice(bbox.read(8).ok_or("bbox stream exhausted")?);
            } else {
                compute_bbox(&points[..total as usize], &mut glyph);
            }
            let mut end_point: i64 = -1;
            for j in 0..n_contours as usize {
                end_point += n_points_vec[j] as i64;
                if end_point >= 65536 {
                    return Err("point index overflow".into());
                }
                glyph.extend_from_slice(&(end_point as u16).to_be_bytes());
            }
            glyph.extend_from_slice(&(instruction_size as u16).to_be_bytes());
            glyph.extend_from_slice(
                instructions
                    .read(instruction_size as usize)
                    .ok_or("instruction stream exhausted")?,
            );
            let has_overlap_bit =
                overlap_bitmap.map_or(false, |b| b[i >> 3] & (0x80 >> (i & 7)) != 0);
            glyph.extend_from_slice(&store_points(&points[..total as usize], has_overlap_bit)?);
        } else if have_bbox {
            // empty glyph — must NOT have a bbox
            return Err("empty glyph has a bbox".into());
        }

        loca_values[i] = (out.len() - glyf_start) as u32;
        glyf_checksum = glyf_checksum.wrapping_add(ulong_sum(&glyph));
        out.extend_from_slice(&glyph);
        pad4(out);
        // We may need x_min to reconstruct 'hmtx'.
        if n_contours > 0 {
            info.x_mins[i] = i16::from_be_bytes([glyph[2], glyph[3]]);
        }
    }

    glyf.dst_length = (out.len() - glyf_start) as u32;
    loca.dst_offset = out.len() as u32;
    // loca[n] equals the length of the glyph data table.
    loca_values[info.num_glyphs as usize] = glyf.dst_length;
    let loca_checksum = store_loca(&loca_values, info.index_format, out);
    loca.dst_length = (out.len() - loca.dst_offset as usize) as u32;
    Ok((glyf_checksum, loca_checksum))
}

/// `SizeOfComposite`: walk the component records to measure them.
fn size_of_composite(cur: &mut Cursor) -> Result<(usize, bool), String> {
    let start = cur.pos;
    let mut have_instructions = false;
    let mut flags: u16 = FLAG_MORE_COMPONENTS;
    while flags & FLAG_MORE_COMPONENTS != 0 {
        flags = cur.u16().ok_or("composite stream exhausted")?;
        have_instructions |= flags & FLAG_WE_HAVE_INSTRUCTIONS != 0;
        let mut arg_size: usize = 2; // glyph index
        if flags & FLAG_ARG_1_AND_2_ARE_WORDS != 0 {
            arg_size += 4;
        } else {
            arg_size += 2;
        }
        if flags & FLAG_WE_HAVE_A_SCALE != 0 {
            arg_size += 2;
        } else if flags & FLAG_WE_HAVE_AN_X_AND_Y_SCALE != 0 {
            arg_size += 4;
        } else if flags & FLAG_WE_HAVE_A_TWO_BY_TWO != 0 {
            arg_size += 8;
        }
        if !cur.skip(arg_size) {
            return Err("composite stream exhausted".into());
        }
    }
    Ok((cur.pos - start, have_instructions))
}

/// `WithSign(flag, base)` = `(flag & 1) ? base : -base`.
fn with_sign(flag: i32, base: i32) -> i32 {
    if flag & 1 != 0 {
        base
    } else {
        -base
    }
}

/// `TripletDecode`: point deltas from the compact triplet encoding.
fn triplet_decode(
    flags_in: &[u8],
    input: &[u8],
    n_points: u32,
    points: &mut Vec<Point>,
) -> Result<usize, String> {
    if n_points as usize > input.len() {
        return Err("triplet data too short".into());
    }
    let mut x: i32 = 0;
    let mut y: i32 = 0;
    let mut t: usize = 0;
    for i in 0..n_points as usize {
        let raw_flag = flags_in[i];
        let on_curve = raw_flag >> 7 == 0;
        let flag = (raw_flag & 0x7f) as i32;
        let n_data_bytes: usize = if flag < 84 {
            1
        } else if flag < 120 {
            2
        } else if flag < 124 {
            3
        } else {
            4
        };
        if t + n_data_bytes > input.len() {
            return Err("triplet data too short".into());
        }
        let b = |k: usize| input[t + k] as i32;
        let (dx, dy);
        if flag < 10 {
            dx = 0;
            dy = with_sign(flag, ((flag & 14) << 7) + b(0));
        } else if flag < 20 {
            dx = with_sign(flag, (((flag - 10) & 14) << 7) + b(0));
            dy = 0;
        } else if flag < 84 {
            let b0 = flag - 20;
            dx = with_sign(flag, 1 + (b0 & 0x30) + (b(0) >> 4));
            dy = with_sign(flag >> 1, 1 + ((b0 & 0x0c) << 2) + (b(0) & 0x0f));
        } else if flag < 120 {
            let b0 = flag - 84;
            dx = with_sign(flag, 1 + ((b0 / 12) << 8) + b(0));
            dy = with_sign(flag >> 1, 1 + (((b0 % 12) >> 2) << 8) + b(1));
        } else if flag < 124 {
            dx = with_sign(flag, (b(0) << 4) + (b(1) >> 4));
            dy = with_sign(flag >> 1, ((b(1) & 0x0f) << 8) + b(2));
        } else {
            dx = with_sign(flag, (b(0) << 8) + b(1));
            dy = with_sign(flag >> 1, (b(2) << 8) + b(3));
        }
        t += n_data_bytes;
        x = x.checked_add(dx).ok_or("coordinate overflow")?;
        y = y.checked_add(dy).ok_or("coordinate overflow")?;
        points.push(Point { x, y, on_curve });
    }
    Ok(t)
}

/// `ComputeBbox`: store the coordinate bounding box after numContours.
fn compute_bbox(points: &[Point], glyph: &mut Vec<u8>) {
    let mut x_min = 0i32;
    let mut y_min = 0i32;
    let mut x_max = 0i32;
    let mut y_max = 0i32;
    if let Some(first) = points.first() {
        x_min = first.x;
        x_max = first.x;
        y_min = first.y;
        y_max = first.y;
    }
    for p in &points[1..] {
        x_min = x_min.min(p.x);
        x_max = x_max.max(p.x);
        y_min = y_min.min(p.y);
        y_max = y_max.max(p.y);
    }
    glyph.extend_from_slice(&(x_min as u16).to_be_bytes());
    glyph.extend_from_slice(&(y_min as u16).to_be_bytes());
    glyph.extend_from_slice(&(x_max as u16).to_be_bytes());
    glyph.extend_from_slice(&(y_max as u16).to_be_bytes());
}

/// `StorePoints`: flags (run-length packed) then all x deltas, then all y
/// deltas — the TrueType simple-glyph point section.
fn store_points(points: &[Point], has_overlap_bit: bool) -> Result<Vec<u8>, String> {
    let mut flags: Vec<u8> = Vec::new();
    let mut x_bytes: usize = 0;
    let mut y_bytes: usize = 0;
    let mut last_flag: Option<u8> = None;
    let mut repeat_count: u32 = 0;
    let mut last_x: i32 = 0;
    let mut last_y: i32 = 0;

    for (i, p) in points.iter().enumerate() {
        let mut flag: u8 = if p.on_curve { K_GLYF_ON_CURVE } else { 0 };
        if has_overlap_bit && i == 0 {
            flag |= K_OVERLAP_SIMPLE;
        }
        let dx = p.x - last_x;
        let dy = p.y - last_y;
        if dx == 0 {
            flag |= K_GLYF_THIS_X_IS_SAME;
        } else if dx > -256 && dx < 256 {
            flag |= K_GLYF_X_SHORT | if dx > 0 { K_GLYF_THIS_X_IS_SAME } else { 0 };
            x_bytes += 1;
        } else {
            x_bytes += 2;
        }
        if dy == 0 {
            flag |= K_GLYF_THIS_Y_IS_SAME;
        } else if dy > -256 && dy < 256 {
            flag |= K_GLYF_Y_SHORT | if dy > 0 { K_GLYF_THIS_Y_IS_SAME } else { 0 };
            y_bytes += 1;
        } else {
            y_bytes += 2;
        }

        if last_flag == Some(flag) && repeat_count != 255 {
            *flags.last_mut().expect("repeat needs a prior flag byte") |= K_GLYF_REPEAT;
            repeat_count += 1;
        } else {
            if repeat_count != 0 {
                flags.push(repeat_count as u8);
                repeat_count = 0;
            }
            flags.push(flag);
        }
        last_x = p.x;
        last_y = p.y;
        last_flag = Some(flag);
    }
    if repeat_count != 0 {
        flags.push(repeat_count as u8);
    }

    // Two independent cursors, as in the reference: x deltas land before
    // the y deltas even though both are computed in one pass over points.
    let mut xs: Vec<u8> = Vec::with_capacity(x_bytes);
    let mut ys: Vec<u8> = Vec::with_capacity(y_bytes);
    let mut last_x: i32 = 0;
    let mut last_y: i32 = 0;
    for p in points {
        let dx = p.x - last_x;
        if dx == 0 {
            // pass
        } else if dx > -256 && dx < 256 {
            xs.push(dx.unsigned_abs() as u8);
        } else {
            xs.extend_from_slice(&(dx as u16).to_be_bytes());
        }
        last_x += dx;
        let dy = p.y - last_y;
        if dy == 0 {
            // pass
        } else if dy > -256 && dy < 256 {
            ys.push(dy.unsigned_abs() as u8);
        } else {
            ys.extend_from_slice(&(dy as u16).to_be_bytes());
        }
        last_y += dy;
    }
    let mut out = flags;
    out.extend_from_slice(&xs);
    out.extend_from_slice(&ys);
    Ok(out)
}

/// `StoreLoca`: build the loca table, append it, return its checksum.
fn store_loca(loca_values: &[u32], index_format: u16, out: &mut Vec<u8>) -> u32 {
    let mut content = Vec::with_capacity(loca_values.len() * if index_format != 0 { 4 } else { 2 });
    for &value in loca_values {
        if index_format != 0 {
            content.extend_from_slice(&value.to_be_bytes());
        } else {
            content.extend_from_slice(&((value >> 1) as u16).to_be_bytes());
        }
    }
    let checksum = ulong_sum(&content);
    out.extend_from_slice(&content);
    checksum
}

/// `ReconstructTransformedHmtx`: rebuild the full hmtx from advance widths
/// plus the lsbs the transform kept (the rest come from glyf xMins).
fn reconstruct_hmtx(transformed: &[u8], info: &FontInfo, out: &mut Vec<u8>) -> Result<u32, String> {
    let num_glyphs = info.num_glyphs;
    let mut input = Cursor::new(transformed);
    let hmtx_flags = input.u8().ok_or("hmtx transform too short")?;
    // Bit 0 clear = proportional lsbs present in the stream; bit 1 clear =
    // monospace lsbs present.
    let has_proportional_lsbs = hmtx_flags & 1 == 0;
    let has_monospace_lsbs = hmtx_flags & 2 == 0;
    // Bits 2-7 are reserved and MUST be zero.
    if hmtx_flags & 0xFC != 0 {
        return Err("illegal hmtx flags; bits 2-7 must be zero".into());
    }
    // You say you transformed but there is little evidence of it.
    if has_proportional_lsbs && has_monospace_lsbs {
        return Err("hmtx transform carries all lsbs".into());
    }
    let num_hmetrics = info.num_hmetrics;
    // num_glyphs 0 is OK if there is no 'glyf' but cannot then xform 'hmtx'.
    if num_hmetrics > num_glyphs {
        return Err("numberOfHMetrics exceeds numGlyphs".into());
    }
    // "...only one entry need be in the array, but that entry is required."
    if num_hmetrics < 1 {
        return Err("numberOfHMetrics is zero".into());
    }

    let mut advance_widths = Vec::with_capacity(num_hmetrics as usize);
    for _ in 0..num_hmetrics {
        advance_widths.push(input.u16().ok_or("hmtx transform too short")?);
    }
    let mut lsbs = Vec::with_capacity(num_glyphs as usize);
    for i in 0..num_hmetrics {
        let lsb = if has_proportional_lsbs {
            input.s16().ok_or("hmtx transform too short")?
        } else {
            info.x_mins[i as usize]
        };
        lsbs.push(lsb);
    }
    for i in num_hmetrics..num_glyphs {
        let lsb = if has_monospace_lsbs {
            input.s16().ok_or("hmtx transform too short")?
        } else {
            info.x_mins[i as usize]
        };
        lsbs.push(lsb);
    }

    let mut table = Vec::with_capacity(2 * num_glyphs as usize + 2 * num_hmetrics as usize);
    for i in 0..num_glyphs {
        if i < num_hmetrics {
            table.extend_from_slice(&advance_widths[i as usize].to_be_bytes());
        }
        table.extend_from_slice(&(lsbs[i as usize] as u16).to_be_bytes());
    }
    let checksum = ulong_sum(&table);
    out.extend_from_slice(&table);
    Ok(checksum)
}

/// Look up a table in an sfnt image by 4-byte tag. Test/verification helper
/// (engine round-trip checks) — `None` on any malformation.
pub fn sfnt_find_table<'a>(sfnt: &'a [u8], tag: &[u8; 4]) -> Option<&'a [u8]> {
    if sfnt.len() < 12 {
        return None;
    }
    let num_tables = u16::from_be_bytes([sfnt[4], sfnt[5]]) as usize;
    for i in 0..num_tables {
        let e = 12 + i * 16;
        let entry = sfnt.get(e..e + 16)?;
        if &entry[0..4] == tag {
            let off = u32::from_be_bytes(entry[8..12].try_into().ok()?) as usize;
            let len = u32::from_be_bytes(entry[12..16].try_into().ok()?) as usize;
            return sfnt.get(off..off + len);
        }
    }
    None
}

/// Whole-font checksum with `checkSumAdjustment` in place; a well-formed
/// sfnt sums to 0xB1B0AFBA.
pub fn sfnt_whole_checksum(sfnt: &[u8]) -> u32 {
    ulong_sum(sfnt)
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Andale Mono subset ("AB"), 3 glyphs, from fontTools.
    const TTF: &[u8] = include_bytes!("../tests/fixtures/tiny.ttf");
    /// The same font as a real fontTools WOFF 2.0 (transformed glyf/loca).
    const WOFF2_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tiny.woff2");

    // ── helpers ──

    /// Parse an sfnt directory: (tag, checksum, offset, length).
    fn sfnt_dir(sfnt: &[u8]) -> Vec<([u8; 4], u32, u32, u32)> {
        let n = u16::from_be_bytes([sfnt[4], sfnt[5]]) as usize;
        (0..n)
            .map(|i| {
                let e = &sfnt[12 + i * 16..12 + i * 16 + 16];
                (
                    [e[0], e[1], e[2], e[3]],
                    u32::from_be_bytes(e[4..8].try_into().unwrap()),
                    u32::from_be_bytes(e[8..12].try_into().unwrap()),
                    u32::from_be_bytes(e[12..16].try_into().unwrap()),
                )
            })
            .collect()
    }

    /// loca table as glyph offsets (indexToLocFormat-aware).
    fn loca_values(sfnt: &[u8]) -> Vec<u32> {
        let loca = sfnt_find_table(sfnt, b"loca").expect("loca present");
        let head = sfnt_find_table(sfnt, b"head").expect("head present");
        if u16::from_be_bytes([head[50], head[51]]) == 0 {
            loca.chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]) as u32 * 2)
                .collect()
        } else {
            loca.chunks_exact(4)
                .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        }
    }

    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn brotli_compress(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut out, 4096, 11, 22);
            w.write_all(data).unwrap();
            w.flush().unwrap();
        }
        out
    }

    fn push_base128(out: &mut Vec<u8>, mut v: u32) {
        let mut tmp = [0u8; 5];
        let mut i = tmp.len();
        loop {
            i -= 1;
            tmp[i] = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                break;
            }
        }
        let last = tmp.len() - 1;
        for (j, &b) in tmp[i..].iter().enumerate() {
            out.push(if i + j == last { b } else { b | 0x80 });
        }
    }

    fn push_255ushort(out: &mut Vec<u8>, v: u32) {
        if v < 253 {
            out.push(v as u8);
        } else if v <= 508 {
            out.push(255);
            out.push((v - 253) as u8);
        } else {
            out.push(253);
            out.extend_from_slice(&(v as u16).to_be_bytes());
        }
    }

    // ── WOFF 1.0 builder ──

    /// Wrap `ttf`'s tables in a WOFF 1.0 container. Tables named in `raw`
    /// are stored uncompressed; the rest are zlib-compressed when that is
    /// smaller (compLength > origLength is illegal).
    fn build_woff1(ttf: &[u8], raw: &[&[u8; 4]]) -> Vec<u8> {
        struct Slot {
            tag: [u8; 4],
            checksum: u32,
            orig_length: u32,
            payload: Vec<u8>,
        }
        let mut slots: Vec<Slot> = Vec::new();
        for (tag, checksum, off, len) in sfnt_dir(ttf) {
            let src = &ttf[off as usize..(off + len) as usize];
            let payload = if raw.contains(&&tag) {
                src.to_vec()
            } else {
                let z = zlib_compress(src);
                if z.len() < src.len() {
                    z
                } else {
                    src.to_vec()
                }
            };
            slots.push(Slot {
                tag,
                checksum,
                orig_length: len,
                payload,
            });
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"wOFF");
        out.extend_from_slice(&ttf[0..4]); // flavor
        let length_pos = out.len();
        out.extend_from_slice(&0u32.to_be_bytes()); // totalLength, patched
        out.extend_from_slice(&(slots.len() as u16).to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // reserved
        out.extend_from_slice(&(ttf.len() as u32).to_be_bytes()); // totalSfntSize
        out.extend_from_slice(&0u16.to_be_bytes()); // major
        out.extend_from_slice(&0u16.to_be_bytes()); // minor
        out.extend_from_slice(&0u32.to_be_bytes()); // metaOffset
        out.extend_from_slice(&0u32.to_be_bytes()); // metaLength
        out.extend_from_slice(&0u32.to_be_bytes()); // metaOrigLength
        out.extend_from_slice(&0u32.to_be_bytes()); // privOffset
        out.extend_from_slice(&0u32.to_be_bytes()); // privLength
        assert_eq!(out.len(), 44);

        let mut entries = Vec::new();
        let mut data = Vec::new();
        for s in &slots {
            let offset = (44 + slots.len() * 20 + data.len()) as u32;
            entries.extend_from_slice(&s.tag);
            entries.extend_from_slice(&offset.to_be_bytes());
            entries.extend_from_slice(&(s.payload.len() as u32).to_be_bytes());
            entries.extend_from_slice(&s.orig_length.to_be_bytes());
            entries.extend_from_slice(&s.checksum.to_be_bytes());
            data.extend_from_slice(&s.payload);
            while data.len() % 4 != 0 {
                data.push(0);
            }
        }
        out.extend_from_slice(&entries);
        out.extend_from_slice(&data);
        let total = out.len() as u32;
        out[length_pos..length_pos + 4].copy_from_slice(&total.to_be_bytes());
        out
    }

    // ── WOFF 2.0 builder ──

    struct W2Spec {
        tag: [u8; 4],
        /// Top two bits of the flag byte (xform_version).
        xform: u8,
        orig_length: u32,
        /// Written iff the decoder will treat the entry as transformed.
        transform_length: Option<u32>,
        /// Transformed bytes if transformed, raw table bytes otherwise.
        data: Vec<u8>,
    }

    /// Assemble a WOFF 2.0 file. `fonts: Some(...)` produces a collection:
    /// (flavor, table indices into `specs`) per font.
    fn build_woff2(flavor: u32, specs: &[W2Spec], fonts: Option<&[(u32, Vec<usize>)]>) -> Vec<u8> {
        let transformed = |s: &W2Spec| {
            let tag = u32::from_be_bytes(s.tag);
            if tag == TAG_GLYF || tag == TAG_LOCA {
                s.xform == 0
            } else {
                s.xform != 0
            }
        };
        let mut blob = Vec::new();
        let mut dir = Vec::new();
        for s in specs {
            dir.push(0x3f | (s.xform << 6));
            dir.extend_from_slice(&s.tag);
            push_base128(&mut dir, s.orig_length);
            if transformed(s) {
                push_base128(
                    &mut dir,
                    s.transform_length.expect("transformed needs length"),
                );
            }
            blob.extend_from_slice(&s.data);
        }

        let compressed = brotli_compress(&blob);
        let mut out = Vec::new();
        out.extend_from_slice(b"wOF2");
        out.extend_from_slice(&flavor.to_be_bytes());
        let length_pos = out.len();
        out.extend_from_slice(&0u32.to_be_bytes()); // totalLength, patched
        out.extend_from_slice(&(specs.len() as u16).to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // reserved
        out.extend_from_slice(&0u32.to_be_bytes()); // totalSfntSize (not validated)
        out.extend_from_slice(&(compressed.len() as u32).to_be_bytes()); // totalCompressedSize
        out.extend_from_slice(&0u16.to_be_bytes()); // major
        out.extend_from_slice(&0u16.to_be_bytes()); // minor
        out.extend_from_slice(&0u32.to_be_bytes()); // metaOffset
        out.extend_from_slice(&0u32.to_be_bytes()); // metaLength
        out.extend_from_slice(&0u32.to_be_bytes()); // metaOrigLength
        out.extend_from_slice(&0u32.to_be_bytes()); // privOffset
        out.extend_from_slice(&0u32.to_be_bytes()); // privLength
        assert_eq!(out.len(), 48);
        out.extend_from_slice(&dir);
        if let Some(fonts) = fonts {
            out.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // header_version
            push_255ushort(&mut out, fonts.len() as u32);
            for (font_flavor, indices) in fonts {
                push_255ushort(&mut out, indices.len() as u32);
                out.extend_from_slice(&font_flavor.to_be_bytes());
                for &i in indices {
                    push_255ushort(&mut out, i as u32);
                }
            }
        }
        out.extend_from_slice(&compressed);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        let total = out.len() as u32;
        out[length_pos..length_pos + 4].copy_from_slice(&total.to_be_bytes());
        out
    }

    // ── a minimal two-glyph font ──

    struct MicroFont {
        head: Vec<u8>,
        hhea: Vec<u8>,
        maxp: Vec<u8>,
        /// Expected reconstructed glyf: empty glyph 0 + a 22-byte triangle.
        glyf: Vec<u8>,
        /// The transformed-glyf stream that reconstructs into `glyf`.
        glyf_transform: Vec<u8>,
    }

    fn micro_font() -> MicroFont {
        // head (54 bytes), indexToLocFormat = 0 (short loca)
        let mut head = vec![0u8; 54];
        head[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        head[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes()); // magic
        head[18..20].copy_from_slice(&1000u16.to_be_bytes()); // unitsPerEm
        head[50..52].copy_from_slice(&0u16.to_be_bytes()); // indexToLocFormat
                                                           // hhea (36 bytes), numberOfHMetrics = 1
        let mut hhea = vec![0u8; 36];
        hhea[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        hhea[34..36].copy_from_slice(&1u16.to_be_bytes());
        // maxp (32 bytes), numGlyphs = 2
        let mut maxp = vec![0u8; 32];
        maxp[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        maxp[4..6].copy_from_slice(&2u16.to_be_bytes());

        // glyph 1: one contour, on-curve points (50,0) (50,700) (450,700).
        // glyph 0 is empty (a zero-width loca run).
        let glyph: &[u8] = &[
            0x00, 0x01, // numberOfContours
            0x00, 0x32, // xMin = 50
            0x00, 0x00, // yMin
            0x01, 0xC2, // xMax = 450
            0x02, 0xBC, // yMax = 700
            0x00, 0x02, // endPtsOfContours[0]
            0x00, 0x00, // instructionLength
            0x33, 0x11, 0x21, // flags
            0x32, // x delta: +50
            0x01, 0x90, // x delta: +400
            0x02, 0xBC, // y delta: +700
        ];
        let mut glyf = glyph.to_vec();
        glyf.resize(24, 0);

        // Transformed-glyf stream: 8-byte header, seven substream sizes,
        // then the substreams. Triplet flags/data encode the deltas
        // (50,0) (0,700) (400,0); the bbox bitmap is empty so bboxes are
        // computed from points.
        let n_contours = [0u8, 0, 0, 1].to_vec();
        let n_points = [3u8].to_vec(); // one contour, 3 points
        let flags = [11u8, 5, 13].to_vec();
        let glyph_stream = [50u8, 188, 144, 0].to_vec(); // triplets + instrLength 0
        let composite: Vec<u8> = Vec::new();
        let bbox = [0u8, 0, 0, 0].to_vec();
        let instructions: Vec<u8> = Vec::new();
        let mut t = Vec::new();
        t.extend_from_slice(&0u16.to_be_bytes()); // transformed-glyf version
        t.extend_from_slice(&0u16.to_be_bytes()); // flags (no overlap bitmap)
        t.extend_from_slice(&2u16.to_be_bytes()); // numGlyphs
        t.extend_from_slice(&0u16.to_be_bytes()); // indexFormat (short)
        for d in [
            &n_contours,
            &n_points,
            &flags,
            &glyph_stream,
            &composite,
            &bbox,
            &instructions,
        ] {
            t.extend_from_slice(&(d.len() as u32).to_be_bytes());
        }
        t.extend_from_slice(&n_contours);
        t.extend_from_slice(&n_points);
        t.extend_from_slice(&flags);
        t.extend_from_slice(&glyph_stream);
        t.extend_from_slice(&composite);
        t.extend_from_slice(&bbox);
        t.extend_from_slice(&instructions);

        MicroFont {
            head,
            hhea,
            maxp,
            glyf,
            glyf_transform: t,
        }
    }

    /// glyf/loca consecutive so the collection's index check accepts it.
    fn micro_specs(f: &MicroFont) -> Vec<W2Spec> {
        vec![
            W2Spec {
                tag: *b"glyf",
                xform: 0,
                orig_length: 24,
                transform_length: Some(f.glyf_transform.len() as u32),
                data: f.glyf_transform.clone(),
            },
            W2Spec {
                tag: *b"loca",
                xform: 0,
                orig_length: 6,
                transform_length: Some(0),
                data: Vec::new(),
            },
            W2Spec {
                tag: *b"head",
                xform: 0,
                orig_length: 54,
                transform_length: None,
                data: f.head.clone(),
            },
            W2Spec {
                tag: *b"hhea",
                xform: 0,
                orig_length: 36,
                transform_length: None,
                data: f.hhea.clone(),
            },
            // hmtx transform: flags 0x03 (no lsbs carried), advance 600.
            W2Spec {
                tag: *b"hmtx",
                xform: 1,
                orig_length: 6,
                transform_length: Some(3),
                data: vec![0x03, 0x02, 0x58],
            },
            W2Spec {
                tag: *b"maxp",
                xform: 0,
                orig_length: 32,
                transform_length: None,
                data: f.maxp.clone(),
            },
        ]
    }

    fn build_woff2_untransformed() -> Vec<u8> {
        let specs: Vec<W2Spec> = sfnt_dir(TTF)
            .into_iter()
            .map(|(tag, _, off, len)| {
                let t = u32::from_be_bytes(tag);
                W2Spec {
                    tag,
                    xform: if t == TAG_GLYF || t == TAG_LOCA { 1 } else { 0 },
                    orig_length: len,
                    transform_length: None,
                    data: TTF[off as usize..(off + len) as usize].to_vec(),
                }
            })
            .collect();
        build_woff2(0x0001_0000, &specs, None)
    }

    // ── variable-length coders ──

    #[test]
    fn variable_length_coders_round_trip() {
        for &v in &[0u32, 1, 127, 252, 253, 506, 508, 761, 1000, 65535] {
            let mut buf = Vec::new();
            push_255ushort(&mut buf, v);
            let mut c = Cursor::new(&buf);
            assert_eq!(c.read255ushort(), Some(v), "255UShort {v}");
        }
        for &v in &[0u32, 1, 127, 128, 16383, 16384, 0x0FFF_FFFF, 0x1FFF_FFFF] {
            let mut buf = Vec::new();
            push_base128(&mut buf, v);
            let mut c = Cursor::new(&buf);
            assert_eq!(c.base128(), Some(v), "Base128 {v:#x}");
        }
        // non-minimal encoding of 0 rejected
        assert_eq!(Cursor::new(&[0x80]).base128(), None);
        // more than 5 bytes rejected
        assert_eq!(
            Cursor::new(&[0x81, 0x80, 0x80, 0x80, 0x80, 0x01]).base128(),
            None
        );
    }

    // ── WOFF 1.0 ──

    #[test]
    fn woff1_round_trips_real_font() {
        let woff = build_woff1(TTF, &[b"head", b"hmtx"]);
        let out = decode_woff(&woff).unwrap();
        assert_eq!(&out[0..4], &TTF[0..4]); // flavor preserved
        for (tag, checksum, off, len) in sfnt_dir(TTF) {
            let src = &TTF[off as usize..(off + len) as usize];
            let got = sfnt_find_table(&out, &tag).expect("table present");
            assert_eq!(got, src, "table {:?}", String::from_utf8_lossy(&tag));
        }
        // per-table checksums carried through untouched
        let dec = sfnt_dir(&out);
        for (tag, checksum, _, _) in sfnt_dir(TTF) {
            let e = dec.iter().find(|(t, _, _, _)| *t == tag).unwrap();
            assert_eq!(
                e.1,
                checksum,
                "checksum {:?}",
                String::from_utf8_lossy(&tag)
            );
        }
        // directory is tag-sorted
        let tags: Vec<[u8; 4]> = dec.into_iter().map(|(t, _, _, _)| t).collect();
        let mut sorted = tags.clone();
        sorted.sort();
        assert_eq!(tags, sorted);
    }

    #[test]
    fn woff1_rejects_garbage() {
        assert!(decode_woff(&[]).is_err());
        assert!(decode_woff(b"wOFF").is_err());
        assert!(
            decode_woff(b"NOPE NOPE NOPE NOPE NOPE NOPE NOPE NOPE NOPE NOPE NOPE NOPE").is_err()
        );

        // reported length mismatch
        let mut bad = build_woff1(TTF, &[]);
        bad[8..12].copy_from_slice(&1234u32.to_be_bytes());
        assert!(decode_woff(&bad).is_err());

        // numTables = 0
        let mut bad = build_woff1(TTF, &[]);
        bad[12..14].copy_from_slice(&0u16.to_be_bytes());
        assert!(decode_woff(&bad).is_err());

        // truncated table directory
        let mut bad = build_woff1(TTF, &[]);
        bad.truncate(50);
        bad[8..12].copy_from_slice(&50u32.to_be_bytes());
        assert!(decode_woff(&bad).is_err());

        // compLength > origLength (first directory entry)
        let mut bad = build_woff1(TTF, &[]);
        bad[44 + 8..44 + 12].copy_from_slice(&0xFFFFu32.to_be_bytes());
        assert!(decode_woff(&bad).is_err());

        // corrupt zlib stream in the last table's payload
        let mut bad = build_woff1(TTF, &[]);
        let n = bad.len();
        for b in &mut bad[n - 40..] {
            *b ^= 0xFF;
        }
        assert!(decode_woff(&bad).is_err());
    }

    // ── WOFF 2.0 ──

    #[test]
    fn woff2_untransformed_round_trips_real_font() {
        let out = decode_woff2(&build_woff2_untransformed()).unwrap();
        for (tag, _, off, len) in sfnt_dir(TTF) {
            let src = &TTF[off as usize..(off + len) as usize];
            let got = sfnt_find_table(&out, &tag).expect("table present");
            if &tag == b"head" {
                // checkSumAdjustment is recomputed for the new layout
                assert_eq!(&got[0..8], &src[0..8]);
                assert_eq!(&got[12..], &src[12..]);
            } else {
                assert_eq!(got, src, "table {:?}", String::from_utf8_lossy(&tag));
            }
        }
        assert_eq!(sfnt_whole_checksum(&out), 0xB1B0_AFBA);
    }

    #[test]
    fn woff2_real_fixture_matches_ttf() {
        let out = decode_woff2(WOFF2_FIXTURE).unwrap();
        // same table set, same (tag-sorted) order
        let orig_tags: Vec<[u8; 4]> = sfnt_dir(TTF).into_iter().map(|(t, _, _, _)| t).collect();
        let dec_tags: Vec<[u8; 4]> = sfnt_dir(&out).into_iter().map(|(t, _, _, _)| t).collect();
        assert_eq!(orig_tags, dec_tags);

        // tables outside the transforms are byte-identical
        for tag in [
            &b"OS/2", &b"cmap", &b"cvt ", &b"fpgm", &b"gasp", &b"hhea", &b"hmtx", &b"maxp",
            &b"name", &b"post", &b"prep",
        ] {
            assert_eq!(
                sfnt_find_table(&out, tag).unwrap(),
                sfnt_find_table(TTF, tag).unwrap(),
                "table {:?}",
                String::from_utf8_lossy(*tag),
            );
        }
        let head_o = sfnt_find_table(TTF, b"head").unwrap();
        let head_d = sfnt_find_table(&out, b"head").unwrap();
        assert_eq!(&head_o[0..8], &head_d[0..8]);
        // The WOFF2 encoder sets head.flags bit 11 (lossless-transform
        // marker, cf. fontTools _setHeadTransformFlag); the decoder copies
        // bytes faithfully, so that field alone may differ.
        assert_eq!(&head_o[12..16], &head_d[12..16]);
        assert_eq!(&head_o[18..], &head_d[18..]);
        assert_eq!(
            u16::from_be_bytes([head_d[16], head_d[17]]) & 0x0800,
            0x0800,
            "encoder sets head.flags bit 11",
        );
        assert_eq!(
            u16::from_be_bytes([head_o[16], head_o[17]]) & !0x0800,
            u16::from_be_bytes([head_d[16], head_d[17]]) & !0x0800,
        );

        // per-glyph content matches modulo the reconstruction padding
        let lo = loca_values(TTF);
        let ld = loca_values(&out);
        assert_eq!(lo.len(), ld.len());
        let g_o = sfnt_find_table(TTF, b"glyf").unwrap();
        let g_d = sfnt_find_table(&out, b"glyf").unwrap();
        for w in 0..lo.len() - 1 {
            let a = &g_o[lo[w] as usize..lo[w + 1] as usize];
            let b = &g_d[ld[w] as usize..ld[w + 1] as usize];
            assert!(b.len() >= a.len(), "glyph {w} shorter than original");
            assert_eq!(&b[..a.len()], a, "glyph {w} content");
            assert!(b[a.len()..].iter().all(|&z| z == 0), "glyph {w} pad");
        }

        // the decoder wrote a checkSumAdjustment for its own layout
        assert_eq!(sfnt_whole_checksum(&out), 0xB1B0_AFBA);
    }

    #[test]
    fn woff2_transformed_synthetic_reconstructs() {
        let f = micro_font();
        let out = decode_woff2(&build_woff2(0x0001_0000, &micro_specs(&f), None)).unwrap();

        // glyf: empty glyph 0, then the 22-byte glyph padded to 24
        let glyf = sfnt_find_table(&out, b"glyf").unwrap();
        assert_eq!(glyf, &f.glyf[..]);

        // loca (short): [0, 0, 24] → word offsets [0, 0, 12]
        let loca = sfnt_find_table(&out, b"loca").unwrap();
        assert_eq!(loca, &[0u8, 0, 0, 0, 0, 12]);

        // hmtx: the transform carried only the advance; lsbs come from the
        // reconstructed glyf xMins (0 for the empty glyph, 50 for the triangle)
        let hmtx = sfnt_find_table(&out, b"hmtx").unwrap();
        assert_eq!(hmtx, &[0x02u8, 0x58, 0x00, 0x00, 0x00, 0x32]);

        assert_eq!(sfnt_whole_checksum(&out), 0xB1B0_AFBA);
    }

    #[test]
    fn woff2_collection_reassembles_two_fonts() {
        let f = micro_font();
        let specs = micro_specs(&f);
        let indices: Vec<usize> = (0..specs.len()).collect();
        let woff = build_woff2(
            TAG_TTCF,
            &specs,
            Some(&[(0x0001_0000, indices.clone()), (0x0001_0000, indices)]),
        );
        let out = decode_woff2(&woff).unwrap();

        // TTC header: 'ttcf', version, numFonts, one offset-table slot per
        // font (resolved below).
        assert_eq!(&out[0..4], b"ttcf");
        assert_eq!(
            u32::from_be_bytes(out[4..8].try_into().unwrap()),
            0x0001_0000
        );
        assert_eq!(u32::from_be_bytes(out[8..12].try_into().unwrap()), 2);
        let f0 = u32::from_be_bytes(out[12..16].try_into().unwrap()) as usize;
        let f1 = u32::from_be_bytes(out[16..20].try_into().unwrap()) as usize;

        // In a collection the directories carry offsets from the FILE start
        // (TTC semantics), so entries are parsed by hand rather than via
        // sfnt_dir on a per-font slice.
        fn dir(out: &[u8], at: usize) -> Vec<([u8; 4], u32, u32, u32)> {
            let num = u16::from_be_bytes(out[at + 4..at + 6].try_into().unwrap()) as usize;
            (0..num)
                .map(|i| {
                    let e = at + 12 + i * 16;
                    (
                        out[e..e + 4].try_into().unwrap(),
                        u32::from_be_bytes(out[e + 4..e + 8].try_into().unwrap()),
                        u32::from_be_bytes(out[e + 8..e + 12].try_into().unwrap()),
                        u32::from_be_bytes(out[e + 12..e + 16].try_into().unwrap()),
                    )
                })
                .collect()
        }

        let d0 = dir(&out, f0);
        let d1 = dir(&out, f1);
        assert_eq!(d0.len(), 6);
        // The table bodies are emitted once; font 1 reuses them, so its
        // directory entries are identical to font 0's and every offset
        // points into the shared region after both headers.
        assert_eq!(d0, d1);
        for (tag, _, offset, length) in &d0 {
            assert!(*offset as usize >= f1, "table data after both headers");
            let body = &out[*offset as usize..(*offset + *length) as usize];
            if tag == b"glyf" {
                assert_eq!(body, &f.glyf[..]);
            } else if tag == b"loca" {
                assert_eq!(body, &[0u8, 0, 0, 0, 0, 12]);
            } else if tag == b"hmtx" {
                assert_eq!(body, &[0x02u8, 0x58, 0x00, 0x00, 0x00, 0x32]);
            }
        }

        // Font 0 reassembled standalone (its header plus the tables its
        // entries point at) is a valid sfnt: checkSumAdjustment balances.
        let mut standalone: Vec<u8> = out[f0..f0 + 12 + d0.len() * 16].to_vec();
        for (_, _, offset, length) in &d0 {
            standalone.extend_from_slice(&out[*offset as usize..(*offset + *length) as usize]);
            while standalone.len() % 4 != 0 {
                standalone.push(0);
            }
        }
        assert_eq!(sfnt_whole_checksum(&standalone), 0xB1B0_AFBA);
    }

    #[test]
    fn woff2_rejects_malformed() {
        assert!(decode_woff2(&[]).is_err());
        assert!(decode_woff2(b"wOF2").is_err());
        assert!(
            decode_woff2(b"NOPE NOPE NOPE NOPE NOPE NOPE NOPE NOPE NOPE NOPE NOPE NOPE").is_err()
        );
        assert!(decode_woff2(&[0x77, 0x4F, 0x46, 0x32, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());

        // reported length mismatch
        let mut bad = build_woff2_untransformed();
        bad[8..12].copy_from_slice(&999u32.to_be_bytes());
        assert!(decode_woff2(&bad).is_err());

        // numTables = 0
        let mut bad = build_woff2_untransformed();
        bad[12..14].copy_from_slice(&0u16.to_be_bytes());
        assert!(decode_woff2(&bad).is_err());

        // non-minimal Base128 (0x80 as the first length byte)
        let mut bad = build_woff2_untransformed();
        bad[48 + 5] = 0x80;
        assert!(decode_woff2(&bad).is_err());

        // truncated table directory
        let mut bad = build_woff2_untransformed();
        bad.truncate(52);
        let n = bad.len() as u32;
        bad[8..12].copy_from_slice(&n.to_be_bytes());
        assert!(decode_woff2(&bad).is_err());

        // implausible compression ratio: 2 MB claimed out of a ~60-byte file
        let bomb = build_woff2(
            0x0001_0000,
            &[W2Spec {
                tag: *b"cmap",
                xform: 0,
                orig_length: 2_000_000,
                transform_length: None,
                data: vec![0xAB; 16],
            }],
            None,
        );
        assert!(decode_woff2(&bomb).is_err());

        // claimed uncompressed size one past what the stream delivers
        let mut bad = build_woff2_untransformed();
        bad[48 + 5] = 87; // first table's origLength: 86 → 87
        assert!(decode_woff2(&bad).is_err());

        // trailing bytes after the compressed block
        let mut bad = build_woff2_untransformed();
        bad.push(0xAA);
        let n = bad.len() as u32;
        bad[8..12].copy_from_slice(&n.to_be_bytes());
        assert!(decode_woff2(&bad).is_err());

        // 'glyf' without 'loca'
        let no_pair = build_woff2(
            0x0001_0000,
            &[W2Spec {
                tag: *b"glyf",
                xform: 1,
                orig_length: 4,
                transform_length: None,
                data: vec![0; 4],
            }],
            None,
        );
        assert!(decode_woff2(&no_pair).is_err());

        // transforming just one of glyf/loca
        let f = micro_font();
        let mismatch = build_woff2(
            0x0001_0000,
            &[
                W2Spec {
                    tag: *b"glyf",
                    xform: 0,
                    orig_length: 24,
                    transform_length: Some(f.glyf_transform.len() as u32),
                    data: f.glyf_transform.clone(),
                },
                W2Spec {
                    tag: *b"loca",
                    xform: 1, // decoder now expects NO transform length
                    orig_length: 6,
                    transform_length: None,
                    data: vec![0; 6],
                },
            ],
            None,
        );
        assert!(decode_woff2(&mismatch).is_err());

        // 'loca' with a nonzero transform_length
        let bad_loca = build_woff2(
            0x0001_0000,
            &[W2Spec {
                tag: *b"loca",
                xform: 0,
                orig_length: 6,
                transform_length: Some(4),
                data: vec![0; 4],
            }],
            None,
        );
        assert!(decode_woff2(&bad_loca).is_err());

        // unknown transform on a table that has none defined
        let unknown = build_woff2(
            0x0001_0000,
            &[W2Spec {
                tag: *b"head",
                xform: 1,
                orig_length: 54,
                transform_length: Some(54),
                data: vec![0; 54],
            }],
            None,
        );
        assert!(decode_woff2(&unknown).is_err());

        // hmtx transform carrying ALL lsbs
        let all_lsbs = build_woff2(
            0x0001_0000,
            &[W2Spec {
                tag: *b"hmtx",
                xform: 1,
                orig_length: 6,
                transform_length: Some(5),
                data: vec![0x00, 0x02, 0x58, 0x00, 0x00],
            }],
            None,
        );
        assert!(decode_woff2(&all_lsbs).is_err());

        // hmtx transform with reserved flag bits set
        let reserved = build_woff2(
            0x0001_0000,
            &[W2Spec {
                tag: *b"hmtx",
                xform: 1,
                orig_length: 6,
                transform_length: Some(5),
                data: vec![0x04, 0x02, 0x58, 0x00, 0x00],
            }],
            None,
        );
        assert!(decode_woff2(&reserved).is_err());

        // numberOfHMetrics (3) exceeds numGlyphs (2)
        let mut hhea3 = vec![0u8; 36];
        hhea3[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        hhea3[34..36].copy_from_slice(&3u16.to_be_bytes());
        let too_many = build_woff2(
            0x0001_0000,
            &[
                W2Spec {
                    tag: *b"glyf",
                    xform: 0,
                    orig_length: 24,
                    transform_length: Some(f.glyf_transform.len() as u32),
                    data: f.glyf_transform.clone(),
                },
                W2Spec {
                    tag: *b"hhea",
                    xform: 0,
                    orig_length: 36,
                    transform_length: None,
                    data: hhea3,
                },
                W2Spec {
                    tag: *b"hmtx",
                    xform: 1,
                    orig_length: 8,
                    transform_length: Some(7),
                    data: vec![0x03, 0x02, 0x58, 0x02, 0x58, 0x02, 0x58],
                },
                W2Spec {
                    tag: *b"loca",
                    xform: 0,
                    orig_length: 6,
                    transform_length: Some(0),
                    data: Vec::new(),
                },
            ],
            None,
        );
        assert!(decode_woff2(&too_many).is_err());

        // an empty glyph may not carry a bbox
        let mut s = Vec::new();
        s.extend_from_slice(&0u16.to_be_bytes()); // version
        s.extend_from_slice(&0u16.to_be_bytes()); // flags
        s.extend_from_slice(&1u16.to_be_bytes()); // numGlyphs
        s.extend_from_slice(&0u16.to_be_bytes()); // indexFormat
        let n_contours = [0u8, 0].to_vec(); // glyph 0: zero contours
        let glyphs = [0u8].to_vec();
        let bbox = [0x80u8, 0, 0, 0].to_vec(); // bbox bit SET for glyph 0
        for d in [
            &n_contours,
            &Vec::new(),
            &Vec::new(),
            &glyphs,
            &Vec::new(),
            &bbox,
            &Vec::new(),
        ] {
            s.extend_from_slice(&(d.len() as u32).to_be_bytes());
        }
        s.extend_from_slice(&n_contours);
        s.extend_from_slice(&glyphs);
        s.extend_from_slice(&bbox);
        let empty_bbox = build_woff2(
            0x0001_0000,
            &[
                W2Spec {
                    tag: *b"glyf",
                    xform: 0,
                    orig_length: 4,
                    transform_length: Some(s.len() as u32),
                    data: s,
                },
                W2Spec {
                    tag: *b"loca",
                    xform: 0,
                    orig_length: 4,
                    transform_length: Some(0),
                    data: Vec::new(),
                },
            ],
            None,
        );
        assert!(decode_woff2(&empty_bbox).is_err());
    }
}
