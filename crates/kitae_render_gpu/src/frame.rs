//! Serialized-frame parser (subset) for the C ABI path: the same bytes
//! `kitae_engine_frame` emits, parsed into paint items. Versions 1–20
//! accepted (missing sections default); links/media/focus carry no paint
//! and are skipped exactly. The v12 sparse transform tail (Q2) fills the
//! per-item matrices on rects/images/shadows; text- and link-kind entries
//! are ignored here because the C ABI path composites them on the host.
//!
//! Later tail sections: v13 adds the per-image `animated` byte, v14 the
//! per-text family string (texts stay unpainted here — walked to keep the
//! reader in sync), v15/v17 the sticky and scroll-container side-lists
//! (parsed onto the scene), v16 the per-item filter chains (walked only —
//! the host applies CIFilters), v18 the per-item clip descriptors (parsed
//! onto rects/images/shadows; text entries walked, blend/group are
//! host-layer concerns and ignored), v19 the per-text blur radii (walked;
//! no glyphs are emitted on this path), and v20 the per-media-item controls
//! flag (walked because the host owns the transport chrome).

use crate::{GpuScene, SceneClip, SceneImage, ScenePaint, SceneRect, SceneShadow};

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
    ok: bool,
}

impl<'a> Reader<'a> {
    fn u32(&mut self) -> u32 {
        if !self.ok || self.pos + 4 > self.b.len() {
            self.ok = false;
            return 0;
        }
        let v = u32::from_le_bytes(self.b[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        v
    }
    fn u64(&mut self) -> u64 {
        let lo = self.u32() as u64;
        let hi = self.u32() as u64;
        lo | hi << 32
    }
    fn f32(&mut self) -> f32 {
        f32::from_bits(self.u32())
    }
    fn byte(&mut self) -> u8 {
        if !self.ok || self.pos + 1 > self.b.len() {
            self.ok = false;
            return 0;
        }
        let v = self.b[self.pos];
        self.pos += 1;
        v
    }
    fn string(&mut self) -> Option<String> {
        let len = self.u32() as usize;
        if !self.ok || self.pos + len > self.b.len() {
            self.ok = false;
            return None;
        }
        let s = String::from_utf8(self.b[self.pos..self.pos + len].to_vec()).ok()?;
        self.pos += len;
        Some(s)
    }
    fn skip_str(&mut self) {
        let len = self.u32() as usize;
        if self.pos + len > self.b.len() {
            self.ok = false;
            return;
        }
        self.pos += len;
    }
}

/// Parse a serialized engine frame into a [`GpuScene`] (glyph quads are
/// skipped — the C ABI path composites text on the host; in-process callers
/// wanting glyphs use [`GpuScene::from_display_list`] with a measurer).
/// `w`/`h` are recorded on the scene for the caller's present loop.
pub fn parse_frame_scene(bytes: &[u8], w: u32, h: u32) -> Option<GpuScene> {
    let mut r = Reader {
        b: bytes,
        pos: 0,
        ok: true,
    };
    let ver = r.u32();
    if !r.ok || ver < 1 || ver > 20 {
        return None;
    }
    let _flags = r.u32();
    let _scroll = r.f32();
    let _content_h = r.f32();
    let bg = r.u32();
    r.skip_str(); // url
    r.skip_str(); // title
    let rect_n = r.u32();
    let text_n = r.u32();
    let link_n = r.u32();
    let image_n = if ver >= 2 { r.u32() } else { 0 };
    if rect_n > 1_000_000 || text_n > 1_000_000 || link_n > 1_000_000 || image_n > 1_000_000 {
        return None;
    }
    let mut scene = GpuScene {
        w,
        h,
        bg,
        ..GpuScene::default()
    };
    for _ in 0..rect_n {
        let (x, y, w, h) = (r.f32(), r.f32(), r.f32(), r.f32());
        let color = r.u32();
        let mut radius = [0.0; 4];
        let mut paint = ScenePaint::Solid;
        if ver >= 7 {
            radius = [r.f32(), r.f32(), r.f32(), r.f32()];
            paint = match r.byte() {
                1 => {
                    let angle = r.f32();
                    ScenePaint::Linear {
                        angle_deg: angle,
                        stops: read_stops(&mut r),
                    }
                }
                2 => {
                    let (cx, cy) = (r.f32(), r.f32());
                    let radius = if r.byte() == 1 { Some(r.f32()) } else { None };
                    ScenePaint::Radial {
                        cx,
                        cy,
                        radius,
                        stops: read_stops(&mut r),
                    }
                }
                3 => {
                    let _ = r.string();
                    ScenePaint::Solid
                }
                // B5 conic-gradient: the GPU scene has no conic paint yet —
                // the rect's `color` fast path already carries the first
                // stop, so walk the bytes and paint solid (documented).
                4 => {
                    let _angle = r.f32();
                    let _cx = r.f32();
                    let _cy = r.f32();
                    ScenePaint::Solid
                }
                _ => ScenePaint::Solid,
            };
        }
        if !r.ok {
            return None;
        }
        scene.rects.push(SceneRect {
            x,
            y,
            w,
            h,
            color,
            radius,
            paint,
            transform: None,
            clip: None,
        });
    }
    // Texts: no paint in the GPU C-ABI path (Swift composites Core Text);
    // skip exactly. v10 (Q3) appended a letter-spacing f32 after color and
    // v14 (Q11) a family string before the text itself.
    for _ in 0..text_n {
        r.pos += 3 * 4 + 4 * 4 + if ver >= 10 { 4 } else { 0 }; // x,y,size; weight,italic,mono,color; spacing
        if ver >= 14 {
            r.skip_str(); // family
        }
        r.skip_str(); // text
        if !r.ok {
            return None;
        }
    }
    // Links carry no paint.
    for _ in 0..link_n {
        r.pos += 4 * 4;
        r.skip_str();
        if !r.ok {
            return None;
        }
    }
    for _ in 0..image_n {
        let (x, y, w, h) = (r.f32(), r.f32(), r.f32(), r.f32());
        let fit = r.u32();
        let _src = r.string();
        let _alt = r.string();
        let payload = if ver >= 4 { r.u64() } else { 0 };
        let _animated = if ver >= 13 { r.byte() != 0 } else { false };
        if !r.ok {
            return None;
        }
        scene.images.push(SceneImage {
            x,
            y,
            w,
            h,
            payload,
            fit: match fit {
                1 => kitae_style::ObjectFit::Contain,
                2 => kitae_style::ObjectFit::Cover,
                3 => kitae_style::ObjectFit::None,
                4 => kitae_style::ObjectFit::ScaleDown,
                _ => kitae_style::ObjectFit::Fill,
            },
            img_w: w.max(1.0),
            img_h: h.max(1.0),
            transform: None,
            clip: None,
        });
    }
    if ver >= 3 {
        r.skip_str(); // focused
        r.skip_str(); // selection
    }
    if ver >= 5 {
        // Media section: present flag + optional body (no paint).
        if r.byte() == 1 {
            let _kind = r.byte();
            r.pos += 4 + 4 + 1 + 1 + 4; // duration, rate, channels, playing, position
            r.skip_str(); // url
            r.skip_str(); // caf
        }
        if !r.ok {
            return None;
        }
    }
    if ver >= 6 {
        let n = r.u32();
        if n > 1_000_000 {
            return None;
        }
        for _ in 0..n {
            let _id = r.u64();
            let _box = (r.f32(), r.f32(), r.f32(), r.f32());
            r.skip_str(); // src
            let _kind = r.byte();
            let _playing = r.byte();
            let _position = r.f32();
            if ver >= 20 {
                let _controls = r.byte();
            }
            if !r.ok {
                return None;
            }
        }
    }
    if ver >= 7 {
        let n = r.u32();
        if n > 1_000_000 {
            return None;
        }
        for _ in 0..n {
            let (x, y, w, h) = (r.f32(), r.f32(), r.f32(), r.f32());
            let radius = [r.f32(), r.f32(), r.f32(), r.f32()];
            let color = r.u32();
            let (blur, spread, ox, oy) = (r.f32(), r.f32(), r.f32(), r.f32());
            let inset = r.byte() != 0;
            if !r.ok {
                return None;
            }
            scene.shadows.push(SceneShadow {
                x,
                y,
                w,
                h,
                radius,
                color,
                blur,
                spread,
                offset_x: ox,
                offset_y: oy,
                inset,
                transform: None,
                clip: None,
            });
        }
    }
    // v8 find + AX sections are trailing side-tables (no paint): accepted by
    // the version guard above and ignored for ver < 12 — `r.ok` only fails
    // on overreads, never trailing bytes. For ver ≥ 12 they sit between the
    // shadows and the v12 transform tail, so walk them to reach the tail.
    if ver >= 12 {
        // v8 find: match count + active index + per-match rects.
        let find_n = r.u32();
        let _find_active = r.u32();
        if find_n > 1_000_000 {
            return None;
        }
        for _ in 0..find_n {
            let rc = r.u32();
            if rc > 1_000_000 {
                return None;
            }
            r.pos += rc as usize * 16;
        }
        // v8 AX: per node id + role + label + rect + parent, then focused id.
        let ax_n = r.u32();
        if ax_n > 1_000_000 {
            return None;
        }
        for _ in 0..ax_n {
            r.skip_str(); // id
            r.pos += 1; // role
            r.skip_str(); // label
            r.pos += 4 * 4 + 4; // rect + parent
        }
        r.skip_str(); // ax_focused
        let _qtsq_locked = r.byte(); // v9
        let _qtsq_sig = r.byte(); // v11
                                  // v12: sparse transform tail — (kind u8, index u32, 2×3 f32) per
                                  // transformed item, in rects→texts→images→shadows→links order.
        let tail_n = r.u32();
        if tail_n > 1_000_000 {
            return None;
        }
        for _ in 0..tail_n {
            let kind = r.byte();
            let idx = r.u32() as usize;
            let mut m = [0.0f32; 6];
            for slot in &mut m {
                *slot = r.f32();
            }
            if !r.ok {
                return None;
            }
            let slot = match kind {
                0 => scene.rects.get_mut(idx).map(|i| &mut i.transform),
                // 1 = text: no paint in this path (host composites).
                2 => scene.images.get_mut(idx).map(|i| &mut i.transform),
                3 => scene.shadows.get_mut(idx).map(|i| &mut i.transform),
                _ => None,
            };
            if let Some(slot) = slot {
                *slot = Some(kitae_layout::Matrix2d(m));
            }
        }
        // v15: sticky side-list (Q5/E2) — parsed onto the scene so GPU
        // consumers can shift the boxes at paint time like the host does.
        if ver >= 15 {
            let sticky_n = r.u32();
            if sticky_n > 1_000_000 {
                return None;
            }
            for _ in 0..sticky_n {
                let mut ranges = [(0u32, 0u32); 6];
                for range in &mut ranges {
                    *range = (r.u32(), r.u32());
                }
                let (base_y, height) = (r.f32(), r.f32());
                let (container_top, container_bottom) = (r.f32(), r.f32());
                let (top_offset, bottom_offset) = (r.f32(), r.f32());
                let (has_top, has_bottom) = (r.byte() != 0, r.byte() != 0);
                if !r.ok {
                    return None;
                }
                let [rect, text, link, image, media, shadow] = ranges;
                scene.stickies.push(crate::SceneSticky {
                    rect,
                    text,
                    link,
                    image,
                    media,
                    shadow,
                    base_y,
                    height,
                    container_top,
                    container_bottom,
                    top_offset,
                    bottom_offset,
                    has_top,
                    has_bottom,
                });
            }
            // v16: per-item filter chains — walked to stay in sync; the host
            // applies them via CIFilter (GPU painting ignores them, documented).
            if ver >= 16 {
                let filter_n = r.u32();
                if filter_n > 1_000_000 {
                    return None;
                }
                for _ in 0..filter_n {
                    r.pos += 1 + 4; // kind u8, index u32
                    let fn_n = r.u32();
                    if fn_n > 1_000_000 {
                        return None;
                    }
                    for _ in 0..fn_n {
                        let tag = r.byte();
                        // tag 1 (drop-shadow) carries 4 f32s; every other function
                        // exactly one (kitae_core's `put_filter_fn` is the spec).
                        r.pos += if tag == 1 { 4 * 4 } else { 4 };
                        if !r.ok {
                            return None;
                        }
                    }
                }
                // v17: scroll containers (B2) — parsed onto the scene like stickies.
                if ver >= 17 {
                    let scroll_n = r.u32();
                    if scroll_n > 1_000_000 {
                        return None;
                    }
                    for _ in 0..scroll_n {
                        let node = r.u32();
                        let (x, y, w, h, content_h) = (r.f32(), r.f32(), r.f32(), r.f32(), r.f32());
                        if !r.ok {
                            return None;
                        }
                        scene.scroll_containers.push(crate::SceneScroll {
                            node,
                            x,
                            y,
                            w,
                            h,
                            content_h,
                        });
                    }
                    // v18: per-item clips (B7/E8) — parsed onto rects/images/shadows.
                    // Text entries are walked but dropped (no glyphs on this path);
                    // blend/group ride along on the wire and stay host-side.
                    if ver >= 18 {
                        let clip_n = r.u32();
                        if clip_n > 1_000_000 {
                            return None;
                        }
                        for _ in 0..clip_n {
                            let kind = r.byte();
                            let idx = r.u32() as usize;
                            let clip = read_clip(&mut r);
                            r.pos += 1 + 4; // blend u8, group u32
                            if !r.ok {
                                return None;
                            }
                            let Some(clip) = clip else { continue };
                            let slot = match kind {
                                0 => scene.rects.get_mut(idx).map(|i| &mut i.clip),
                                // 1 = text: no paint in this path (host composites).
                                2 => scene.images.get_mut(idx).map(|i| &mut i.clip),
                                3 => scene.shadows.get_mut(idx).map(|i| &mut i.clip),
                                _ => None,
                            };
                            if let Some(slot) = slot {
                                *slot = Some(clip);
                            }
                        }
                        // v19: per-text blur radii (E3) — walked; no glyphs are emitted here
                        // so the radii have no consumer on this path.
                        if ver >= 19 {
                            let blur_n = r.u32();
                            if blur_n > 1_000_000 {
                                return None;
                            }
                            r.pos += blur_n as usize * 8; // index u32 + blur f32 per entry
                        }
                    }
                }
            }
        }
    }
    r.ok.then_some(scene)
}

/// Read one v18 clip descriptor (kitae_core's `put_clip` is the spec):
/// tag 0 = none (blend/group-only entry), 1 circle, 2 ellipse, 3 inset
/// (rect + 4 corner radii), 4 polygon (evenodd u8, count, points).
fn read_clip(r: &mut Reader) -> Option<SceneClip> {
    let tag = r.byte();
    match tag {
        1 => {
            let (cx, cy, radius) = (r.f32(), r.f32(), r.f32());
            Some(SceneClip::Circle { cx, cy, r: radius })
        }
        2 => {
            let (cx, cy, rx, ry) = (r.f32(), r.f32(), r.f32(), r.f32());
            Some(SceneClip::Ellipse { cx, cy, rx, ry })
        }
        3 => {
            let (x, y, w, h) = (r.f32(), r.f32(), r.f32(), r.f32());
            let mut radii = [(0.0f32, 0.0f32); 4];
            for pair in &mut radii {
                *pair = (r.f32(), r.f32());
            }
            Some(SceneClip::Inset { x, y, w, h, radii })
        }
        4 => {
            let evenodd = r.byte() != 0;
            let n = r.u32().min(1_000_000);
            let mut points = Vec::with_capacity(n as usize);
            for _ in 0..n {
                points.push((r.f32(), r.f32()));
            }
            Some(SceneClip::Polygon { points, evenodd })
        }
        _ => None,
    }
}

fn read_stops(r: &mut Reader) -> Vec<(f32, u32)> {
    let n = r.u32().min(32);
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        out.push((r.f32(), r.u32()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_garbage_and_future_versions() {
        assert!(parse_frame_scene(b"", 10, 10).is_none());
        assert!(parse_frame_scene(b"nope-nope-nope-nope", 10, 10).is_none());
        let mut v99 = 99u32.to_le_bytes().to_vec();
        v99.extend_from_slice(&[0u8; 64]);
        assert!(parse_frame_scene(&v99, 10, 10).is_none());
    }

    #[test]
    fn parses_minimal_v1_frame() {
        // version, flags, scroll, content_h, bg, url, title, 0/0/0 counts.
        let mut b = Vec::new();
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0f32.to_le_bytes());
        b.extend_from_slice(&100f32.to_le_bytes());
        b.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes());
        for s in ["https://x.test/", "T"] {
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s.as_bytes());
        }
        b.extend_from_slice(&[0u8; 12]);
        let scene = parse_frame_scene(&b, 10, 10).expect("parses");
        assert_eq!(scene.bg, 0xFFFFFFFF);
        assert!(scene.rects.is_empty() && scene.images.is_empty());
    }

    /// Q2: a v12 frame whose one rect carries a rotate(90°) matrix in the
    /// sparse tail; the scene must land it on `rects[0].transform`.
    #[test]
    fn parses_v12_transform_tail() {
        let mut b = Vec::new();
        b.extend_from_slice(&12u32.to_le_bytes()); // version
        b.extend_from_slice(&0u32.to_le_bytes()); // flags
        b.extend_from_slice(&0f32.to_le_bytes()); // scroll
        b.extend_from_slice(&64f32.to_le_bytes()); // content_h
        b.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // bg
        for s in ["u", "t"] {
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s.as_bytes());
        }
        b.extend_from_slice(&1u32.to_le_bytes()); // rect_n, text_n, link_n, image_n = 1,0,0,0
        b.extend_from_slice(&[0u8; 12]);
        for v in [4f32, 4.0, 8.0, 8.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&0xFF0000FFu32.to_le_bytes());
        b.extend_from_slice(&[0u8; 16]); // radii
        b.push(0); // solid
        b.extend_from_slice(&[0u8; 8]); // focused + selection (empty)
        b.push(0); // media absent
        b.extend_from_slice(&0u32.to_le_bytes()); // media items
        b.extend_from_slice(&0u32.to_le_bytes()); // shadows
        b.extend_from_slice(&0u32.to_le_bytes()); // find matches
        b.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // find active = none
        b.extend_from_slice(&0u32.to_le_bytes()); // ax nodes
        b.extend_from_slice(&0u32.to_le_bytes()); // ax_focused (empty)
        b.push(0); // v9 qtsq_locked
        b.push(0); // v11 qtsq_sig
        b.extend_from_slice(&1u32.to_le_bytes()); // v12 tail: 1 entry
        b.push(0); // kind = rect
        b.extend_from_slice(&0u32.to_le_bytes()); // index 0
                                                  // rotate(90°) about origin: [0, -1, 0, 1, 0, 0] row-major.
        for v in [0f32, -1.0, 0.0, 1.0, 0.0, 0.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let scene = parse_frame_scene(&b, 32, 32).expect("parses v12");
        assert_eq!(scene.rects.len(), 1);
        assert_eq!(
            scene.rects[0].transform,
            Some(kitae_layout::Matrix2d([0.0, -1.0, 0.0, 1.0, 0.0, 0.0]))
        );
        // An identity-matrix entry overwrites to identity (not None).
        assert_eq!(scene.images.len(), 0);
    }

    /// E2/E8: a v19 frame carrying a sticky, a scroll container and one
    /// clipped rect parses onto the scene; the sticky's shift matches the
    /// layout-side math.
    #[test]
    fn parses_v19_sticky_scroll_and_clip() {
        let mut b = Vec::new();
        b.extend_from_slice(&19u32.to_le_bytes()); // version
        b.extend_from_slice(&0u32.to_le_bytes()); // flags
        b.extend_from_slice(&0f32.to_le_bytes()); // scroll
        b.extend_from_slice(&600f32.to_le_bytes()); // content_h
        b.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // bg
        for s in ["u", "t"] {
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s.as_bytes());
        }
        b.extend_from_slice(&1u32.to_le_bytes()); // rect_n
        b.extend_from_slice(&0u32.to_le_bytes()); // text_n
        b.extend_from_slice(&0u32.to_le_bytes()); // link_n
        b.extend_from_slice(&0u32.to_le_bytes()); // image_n
        for v in [4f32, 4.0, 8.0, 8.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&0xFF0000FFu32.to_le_bytes());
        b.extend_from_slice(&[0u8; 16]); // radii
        b.push(0); // solid
        b.extend_from_slice(&[0u8; 8]); // focused + selection (empty)
        b.push(0); // media absent
        b.extend_from_slice(&0u32.to_le_bytes()); // media items
        b.extend_from_slice(&0u32.to_le_bytes()); // shadows
        b.extend_from_slice(&0u32.to_le_bytes()); // find matches
        b.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // find active = none
        b.extend_from_slice(&0u32.to_le_bytes()); // ax nodes
        b.extend_from_slice(&0u32.to_le_bytes()); // ax_focused (empty)
        b.push(0); // v9 qtsq_locked
        b.push(0); // v11 qtsq_sig
        b.extend_from_slice(&0u32.to_le_bytes()); // v12 transforms
                                                  // v15: one top-sticky header (six (start,count) range pairs).
        b.extend_from_slice(&1u32.to_le_bytes());
        for (start, count) in [(0u32, 1u32), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0)] {
            b.extend_from_slice(&start.to_le_bytes());
            b.extend_from_slice(&count.to_le_bytes());
        }
        for v in [60f32, 30.0, 0.0, 600.0, 12.0, 0.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.push(1); // has_top
        b.push(0); // has_bottom
        b.extend_from_slice(&0u32.to_le_bytes()); // v16 filters
                                                  // v17: one scroll container.
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&7u32.to_le_bytes()); // node
        for v in [8f32, 9.0, 40.0, 50.0, 900.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        // v18: one clip entry — circle on rect 0, blend 0, group 0.
        b.extend_from_slice(&1u32.to_le_bytes());
        b.push(0); // kind = rect
        b.extend_from_slice(&0u32.to_le_bytes()); // index 0
        b.push(1); // circle
        for v in [8f32, 8.0, 4.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.push(0); // blend normal
        b.extend_from_slice(&0u32.to_le_bytes()); // group
        b.extend_from_slice(&0u32.to_le_bytes()); // v19 blur entries
        let scene = parse_frame_scene(&b, 64, 64).expect("parses v19");
        assert_eq!(scene.rects.len(), 1);
        assert_eq!(
            scene.rects[0].clip,
            Some(SceneClip::Circle {
                cx: 8.0,
                cy: 8.0,
                r: 4.0
            })
        );
        assert_eq!(scene.stickies.len(), 1);
        assert_eq!(scene.stickies[0].top_offset, 12.0);
        // Parity with the layout-side shift: at scroll 100 the box pins
        // 12px below the viewport top → shift = 100 + 12 − 60 = 52.
        assert_eq!(scene.stickies[0].shift(100.0, 600.0), 52.0);
        assert_eq!(scene.scroll_containers.len(), 1);
        assert_eq!(scene.scroll_containers[0].content_h, 900.0);
    }

    #[test]
    fn parses_v20_and_ignores_link_transform_kind() {
        let mut b = Vec::new();
        b.extend_from_slice(&20u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0f32.to_le_bytes());
        b.extend_from_slice(&100f32.to_le_bytes());
        b.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes());
        for s in ["u", "t"] {
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s.as_bytes());
        }
        b.extend_from_slice(&[0u8; 16]);
        b.extend_from_slice(&[0u8; 8]);
        b.push(0);
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&42u64.to_le_bytes());
        for v in [1f32, 2.0, 3.0, 4.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let src = "https://x.test/v.mp4";
        b.extend_from_slice(&(src.len() as u32).to_le_bytes());
        b.extend_from_slice(src.as_bytes());
        b.push(1);
        b.push(0);
        b.extend_from_slice(&0f32.to_le_bytes());
        b.push(1);
        b.extend_from_slice(&1u32.to_le_bytes());
        for v in [1f32, 2.0, 3.0, 4.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&[0u8; 16]);
        b.extend_from_slice(&0x11223344u32.to_le_bytes());
        for v in [5f32, 6.0, 0.0, 0.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.push(0);
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.push(0);
        b.push(0);
        b.extend_from_slice(&1u32.to_le_bytes());
        b.push(4);
        b.extend_from_slice(&0u32.to_le_bytes());
        for v in [0f32, -1.0, 0.0, 1.0, 0.0, 0.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        let scene = parse_frame_scene(&b, 32, 32).expect("parses v20");
        assert_eq!(scene.shadows.len(), 1);
        assert_eq!(scene.shadows[0].color, 0x11223344);
        assert_eq!(scene.shadows[0].blur, 5.0);
    }
}
