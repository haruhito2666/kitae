//! CPU reference rasterizer (Phase L): the permanent software fallback and
//! the golden-test oracle. Same scene in, RGBA8 out — deterministic,
//! dependency-free, small enough to audit. Feature parity target: rounded
//! rects (SDF), linear/radial gradients, box shadows (blurred mask), image
//! blits with object-fit UVs, glyph quads from the atlas.

use std::collections::HashMap;
use std::sync::Arc;

use kitae_media::{object_fit_rect, RgbaImage};

use crate::{GlyphAtlas, GpuScene, SceneClip, ScenePaint};

/// Render `scene` at device-pixel size `w`×`h` (the scene is page-space CSS
/// px; callers scale beforehand — tests use 1:1). Returns RGBA8 straight
/// alpha, row-major.
/// Render `scene` into a `w`×`h` device-px tile (`scale` maps CSS px to
/// device px; `ox`/`oy` is the page-space CSS tile origin — same convention
/// as the GPU backend so golden comparisons are apples-to-apples).
pub fn render(
    scene: &GpuScene,
    w: u32,
    h: u32,
    scale: f32,
    ox: f32,
    oy: f32,
    rasters: &HashMap<u64, Arc<RgbaImage>>,
    atlas: &GlyphAtlas,
) -> Vec<u8> {
    render_with_scroll(
        scene,
        w,
        h,
        scale,
        ox,
        oy,
        oy,
        h as f32 / scale.max(0.001),
        rasters,
        atlas,
    )
}

/// Full-control variant for tiled rendering (E4): a tile's geometry origin
/// is its own page-space corner while the sticky scroll offset and viewport
/// height stay the surface's, so shifts are identical across every tile and
/// tile assembly is bit-exact against a whole-surface [`render`].
#[allow(clippy::too_many_arguments)]
pub fn render_with_scroll(
    scene: &GpuScene,
    w: u32,
    h: u32,
    scale: f32,
    ox: f32,
    oy: f32,
    scroll_y: f32,
    viewport_h: f32,
    rasters: &HashMap<u64, Arc<RgbaImage>>,
    atlas: &GlyphAtlas,
) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    fill(&mut px, w, h, scene.bg);
    let px_ = |v: f32, o: f32| (v - o) * scale;
    // E2: sticky items paint shifted by their scroll math. `scroll_y` is the
    // caller's scroll offset (the surface whose top is `scroll_y`) and
    // `viewport_h` the viewport height — the same inputs the host feeds
    // StickyItem::shift. Each sticky's items move as a unit.
    let (rect_shift, glyph_shift, image_shift, shadow_shift) =
        sticky_shifts(scene, scroll_y, viewport_h);
    for (i, s) in scene.shadows.iter().enumerate() {
        draw_shadow(
            &mut px,
            w,
            h,
            scale,
            ox,
            oy,
            &Transform::new(s.transform.as_ref(), scale, ox, oy).shifted(shadow_shift[i] * scale),
            s,
            s.clip.as_ref(),
        );
    }
    for (i, r) in scene.rects.iter().enumerate() {
        draw_rect(
            &mut px,
            w,
            h,
            &Transform::new(r.transform.as_ref(), scale, ox, oy).shifted(rect_shift[i] * scale),
            px_(r.x, ox),
            px_(r.y, oy),
            r.w * scale,
            r.h * scale,
            [
                r.radius[0] * scale,
                r.radius[1] * scale,
                r.radius[2] * scale,
                r.radius[3] * scale,
            ],
            r.color,
            &r.paint,
            r.clip.as_ref(),
        );
    }
    for (i, im) in scene.images.iter().enumerate() {
        if let Some(raster) = rasters.get(&im.payload) {
            draw_image(
                &mut px,
                w,
                h,
                &Transform::new(im.transform.as_ref(), scale, ox, oy)
                    .shifted(image_shift[i] * scale),
                px_(im.x, ox),
                px_(im.y, oy),
                im.w * scale,
                im.h * scale,
                raster,
                im.fit,
                im.clip.as_ref(),
            );
        }
    }
    for (i, g) in scene.glyphs.iter().enumerate() {
        draw_glyph(
            &mut px,
            w,
            h,
            &Transform::new(g.transform.as_ref(), scale, ox, oy).shifted(glyph_shift[i] * scale),
            px_(g.x, ox),
            px_(g.y, oy),
            scale,
            g,
            atlas,
            scene.lcd_text,
        );
    }
    px
}

/// E2: per-item page-space y shift for every paintable item at scroll
/// position `scroll_y` with a `viewport_h`-tall viewport — zero for items
/// outside any sticky box. Sticky `text` ranges index display-list runs, so
/// glyph shifts expand through the scene's run→glyph span map; link/media
/// ranges have no paint on this path.
pub(crate) fn sticky_shifts(
    scene: &GpuScene,
    scroll_y: f32,
    viewport_h: f32,
) -> (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>) {
    let mut rects = vec![0.0f32; scene.rects.len()];
    let mut glyphs = vec![0.0f32; scene.glyphs.len()];
    let mut images = vec![0.0f32; scene.images.len()];
    let mut shadows = vec![0.0f32; scene.shadows.len()];
    for st in &scene.stickies {
        let dy = st.shift(scroll_y, viewport_h);
        if dy == 0.0 {
            continue;
        }
        let fill = |v: &mut Vec<f32>, range: (u32, u32)| {
            for i in range.0 as usize..(range.0 + range.1) as usize {
                if let Some(slot) = v.get_mut(i) {
                    *slot = dy;
                }
            }
        };
        fill(&mut rects, st.rect);
        for run in st.text.0..st.text.0 + st.text.1 {
            if let Some(span) = scene.text_spans.get(run as usize) {
                for gi in span.0..span.0 + span.1 {
                    if let Some(slot) = glyphs.get_mut(gi as usize) {
                        *slot = dy;
                    }
                }
            }
        }
        fill(&mut images, st.image);
        fill(&mut shadows, st.shadow);
    }
    (rects, glyphs, images, shadows)
}

/// Device-space forward/inverse pair for one item's page-space transform
/// (Q2). Paint evaluates in the item's untransformed local space and the
/// geometry warps through the forward matrix; per output pixel the inverse
/// maps device space back to local so the SDF/gradient/UV math is untouched
/// local-space code. Identity items carry the identity pair — the same code
/// path with zero behavior change.
struct Transform {
    fwd: [f32; 6],
    inv: [f32; 6],
}

impl Transform {
    fn new(m: Option<&kitae_layout::Matrix2d>, scale: f32, ox: f32, oy: f32) -> Transform {
        match m {
            None => Transform {
                fwd: crate::MATRIX_IDENTITY,
                inv: crate::MATRIX_IDENTITY,
            },
            Some(t) => {
                let fwd = crate::device_matrix(t, scale, ox, oy);
                let inv = crate::invert_matrix(&fwd);
                Transform { fwd, inv }
            }
        }
    }

    fn apply(m: &[f32; 6], x: f32, y: f32) -> (f32, f32) {
        (m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5])
    }

    /// E2: the same transform with a device-space y shift folded in —
    /// geometry moves down by `dy` px while per-pixel samples inverse-map
    /// back to the item's stored (unshifted) local space. Zero is a no-op.
    fn shifted(mut self, dy: f32) -> Transform {
        if dy != 0.0 {
            // fwd' = T ∘ fwd ; inv' = inv ∘ T⁻¹
            self.fwd[5] += dy;
            self.inv[2] -= self.inv[1] * dy;
            self.inv[5] -= self.inv[4] * dy;
        }
        self
    }

    /// Output-pixel loop bounds for an item's device box: the axis-aligned
    /// box when untransformed, else the warped quad's canvas-clamped
    /// bounding box (ceil on the maxes so partial edge pixels paint).
    fn bbox(&self, x: f32, y: f32, bw: f32, bh: f32, cw: u32, ch: u32) -> (u32, u32, u32, u32) {
        let (cwf, chf) = (cw as f32, ch as f32);
        if self.fwd == crate::MATRIX_IDENTITY {
            return (
                x.max(0.0) as u32,
                y.max(0.0) as u32,
                (x + bw).min(cwf).max(0.0) as u32,
                (y + bh).min(chf).max(0.0) as u32,
            );
        }
        let (x1, y1) = (x + bw, y + bh);
        let pts = [
            Self::apply(&self.fwd, x, y),
            Self::apply(&self.fwd, x1, y),
            Self::apply(&self.fwd, x1, y1),
            Self::apply(&self.fwd, x, y1),
        ];
        let mut mnx = f32::INFINITY;
        let mut mxx = f32::NEG_INFINITY;
        let mut mny = f32::INFINITY;
        let mut mxy = f32::NEG_INFINITY;
        for (px, py) in pts {
            mnx = mnx.min(px);
            mxx = mxx.max(px);
            mny = mny.min(py);
            mxy = mxy.max(py);
        }
        (
            mnx.max(0.0) as u32,
            mny.max(0.0) as u32,
            mxx.min(cwf).ceil().min(cwf).max(0.0) as u32,
            mxy.min(chf).ceil().min(chf).max(0.0) as u32,
        )
    }
}

/// FNV-1a hash of a pixel buffer (golden comparisons).
pub fn hash_pixels(px: &[u8]) -> u64 {
    let mut h = 1469598103934665603u64;
    for &b in px {
        h = (h ^ b as u64).wrapping_mul(1099511628211);
    }
    h
}

fn rgba(c: u32) -> [f32; 4] {
    [
        ((c >> 24) & 0xFF) as f32 / 255.0,
        ((c >> 16) & 0xFF) as f32 / 255.0,
        ((c >> 8) & 0xFF) as f32 / 255.0,
        (c & 0xFF) as f32 / 255.0,
    ]
}

fn pack(c: [f32; 4]) -> [u8; 4] {
    [
        (c[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        (c[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        (c[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        (c[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
    ]
}

/// Source-over onto straight-alpha buffer.
fn blend(dst: &mut [u8], x: u32, y: u32, w: u32, src: [f32; 4]) {
    if x >= w {
        return;
    }
    let i = ((y * w + x) * 4) as usize;
    if i + 4 > dst.len() {
        return;
    }
    let d = [
        dst[i] as f32 / 255.0,
        dst[i + 1] as f32 / 255.0,
        dst[i + 2] as f32 / 255.0,
        dst[i + 3] as f32 / 255.0,
    ];
    let a = src[3] + d[3] * (1.0 - src[3]);
    let mix = |s: f32, dd: f32| {
        if a <= 0.0 {
            0.0
        } else {
            (s * src[3] + dd * d[3] * (1.0 - src[3])) / a
        }
    };
    let out = pack([mix(src[0], d[0]), mix(src[1], d[1]), mix(src[2], d[2]), a]);
    dst[i..i + 4].copy_from_slice(&out);
}

fn fill(px: &mut [u8], w: u32, h: u32, color: u32) {
    let c = rgba(color);
    // Opaque fast path (page backgrounds always are).
    if c[3] >= 1.0 {
        let p = pack(c);
        for px in px.chunks_exact_mut(4) {
            px.copy_from_slice(&p);
        }
        return;
    }
    for y in 0..h {
        for x in 0..w {
            blend(px, x, y, w, c);
        }
    }
}

/// Signed distance to a rounded box (negative inside), corners uniformly
/// clamped to half the min dimension.
fn rounded_sdf(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: [f32; 4]) -> f32 {
    if w <= 0.0 || h <= 0.0 {
        return 1.0;
    }
    let m = (w.min(h) * 0.5).max(0.0);
    let r = [
        r[0].clamp(0.0, m),
        r[1].clamp(0.0, m),
        r[2].clamp(0.0, m),
        r[3].clamp(0.0, m),
    ];
    // Nearest corner arc center by quadrant.
    let left = px < x + w * 0.5;
    let top = py < y + h * 0.5;
    let (ccx, ccy, rad) = if left {
        if top {
            (x + r[0], y + r[0], r[0])
        } else {
            (x + r[3], y + h - r[3], r[3])
        }
    } else if top {
        (x + w - r[1], y + r[1], r[1])
    } else {
        (x + w - r[2], y + h - r[2], r[2])
    };
    // Outside both tangent lines → arc distance governs.
    let ox = if left { ccx - px } else { px - ccx };
    let oy = if top { ccy - py } else { py - ccy };
    if ox > 0.0 && oy > 0.0 {
        return (ox * ox + oy * oy).sqrt() - rad;
    }
    // Otherwise plain box distance (negative inside).
    (x - px).max(px - (x + w)).max((y - py).max(py - (y + h)))
}

fn gradient_at(
    paint: &ScenePaint,
    px: f32,
    py: f32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fallback: u32,
) -> [f32; 4] {
    match paint {
        ScenePaint::Solid => rgba(fallback),
        ScenePaint::Linear { angle_deg, stops } => {
            if stops.is_empty() || w <= 0.0 || h <= 0.0 {
                return rgba(fallback);
            }
            let rad = angle_deg * std::f32::consts::PI / 180.0;
            let (dx, dy) = (rad.sin(), -rad.cos());
            // Extent and projection are box-relative (E4: identical bits at
            // every tile origin — absolute coordinates round differently per
            // translation and would break bit-exact tile assembly).
            let d10 = w * dx;
            let d01 = h * dy;
            let (mn, mx) = {
                let mut mn = (0.0f32).min(d10).min(d01).min(d10 + d01);
                let mut mx = (0.0f32).max(d10).max(d01).max(d10 + d01);
                if mx < mn {
                    std::mem::swap(&mut mn, &mut mx);
                }
                (mn, mx)
            };
            let t = if mx > mn {
                (((px - x) * dx + (py - y) * dy) - mn) / (mx - mn)
            } else {
                0.0
            };
            rgba(sample_stops(stops, t.clamp(0.0, 1.0), fallback))
        }
        ScenePaint::Radial {
            cx,
            cy,
            radius,
            stops,
        } => {
            if stops.is_empty() || w <= 0.0 || h <= 0.0 {
                return rgba(fallback);
            }
            let ccx = cx * w;
            let ccy = cy * h;
            let far = (ccx.max(w - ccx)).powi(2) + (ccy.max(h - ccy)).powi(2);
            let far = far.sqrt().max(0.001);
            let rmax = radius.unwrap_or(far);
            let d = ((px - x - ccx).powi(2) + (py - y - ccy).powi(2)).sqrt();
            rgba(sample_stops(stops, (d / rmax).clamp(0.0, 1.0), fallback))
        }
    }
}

fn sample_stops(stops: &[(f32, u32)], t: f32, fallback: u32) -> u32 {
    if stops.is_empty() {
        return fallback;
    }
    if t <= stops[0].0 {
        return stops[0].1;
    }
    for w in stops.windows(2) {
        if t <= w[1].0 {
            let span = w[1].0 - w[0].0;
            let k = if span > 0.0 { (t - w[0].0) / span } else { 1.0 };
            return lerp_color(w[0].1, w[1].1, k);
        }
    }
    stops.last().map(|s| s.1).unwrap_or(fallback)
}

fn lerp_color(a: u32, b: u32, t: f32) -> u32 {
    let mut out = 0u32;
    for s in [24, 16, 8, 0] {
        let ca = ((a >> s) & 0xFF) as f32;
        let cb = ((b >> s) & 0xFF) as f32;
        out |= ((ca + (cb - ca) * t).round().clamp(0.0, 255.0) as u32) << s;
    }
    out
}

fn draw_rect(
    px: &mut [u8],
    w: u32,
    h: u32,
    tf: &Transform,
    x: f32,
    y: f32,
    bw: f32,
    bh: f32,
    radius: [f32; 4],
    color: u32,
    paint: &ScenePaint,
    clip: Option<&SceneClip>,
) {
    if bw <= 0.0 || bh <= 0.0 {
        return;
    }
    let (x0, y0, x1, y1) = tf.bbox(x, y, bw, bh, w, h);
    for iy in y0..y1 {
        for ix in x0..x1 {
            // Sample in local space (inverse-mapped), blend at the screen px.
            let (sx, sy) = Transform::apply(&tf.inv, ix as f32 + 0.5, iy as f32 + 0.5);
            // E8: hard discard outside the clip region (same local space as
            // the item's own coordinates).
            if let Some(c) = clip {
                if !c.contains(sx, sy) {
                    continue;
                }
            }
            let d = rounded_sdf(sx, sy, x, y, bw, bh, radius);
            if d >= 0.5 {
                continue;
            }
            let mut c = gradient_at(paint, sx, sy, x, y, bw, bh, color);
            if d > -0.5 {
                c[3] *= 0.5 - d;
            }
            blend(px, ix, iy, w, c);
        }
    }
}

fn draw_shadow(
    px: &mut [u8],
    w: u32,
    _h: u32,
    scale: f32,
    ox: f32,
    oy: f32,
    tf: &Transform,
    s: &crate::SceneShadow,
    clip: Option<&SceneClip>,
) {
    if s.blur < 0.0 || s.w <= 0.0 || s.h <= 0.0 || scale <= 0.0 {
        return;
    }
    // Work in device px: transform the border box once.
    let (bx, by, bw, bh) = (
        (s.x - ox) * scale,
        (s.y - oy) * scale,
        s.w * scale,
        s.h * scale,
    );
    let (blur, spread, offx, offy) = (
        s.blur * scale,
        s.spread * scale,
        s.offset_x * scale,
        s.offset_y * scale,
    );
    let radius = [
        s.radius[0] * scale,
        s.radius[1] * scale,
        s.radius[2] * scale,
        s.radius[3] * scale,
    ];
    // Shape mask at box+spread, blurred, tinted, composited under content.
    let pad = (blur * 2.0 + spread + offx.abs().max(offy.abs())).ceil() as i32;
    // Mask bbox: the padded box when untransformed; the warped padded box
    // (plus the screen-space blur radius as margin) when transformed. For a
    // transformed shadow the offset lives in the shadow's own (pre-transform)
    // space, so it rotates with the shape — the per-cell inverse mapping
    // below applies it to the local sample point.
    let r_screen = (blur * 0.5).ceil() + 1.0;
    let (bx0, by0, bx1, by1) = if tf.fwd == crate::MATRIX_IDENTITY {
        (
            (bx - pad as f32) as i32,
            (by - pad as f32) as i32,
            (bx + bw + pad as f32).ceil() as i32,
            (by + bh + pad as f32).ceil() as i32,
        )
    } else {
        let (px0, py0) = Transform::apply(&tf.fwd, bx - pad as f32, by - pad as f32);
        let (px1, py1) = Transform::apply(&tf.fwd, bx + bw + pad as f32, by - pad as f32);
        let (px2, py2) = Transform::apply(&tf.fwd, bx + bw + pad as f32, by + bh + pad as f32);
        let (px3, py3) = Transform::apply(&tf.fwd, bx - pad as f32, by + bh + pad as f32);
        let mnx = px0.min(px1).min(px2).min(px3) - r_screen;
        let mxx = px0.max(px1).max(px2).max(px3) + r_screen;
        let mny = py0.min(py1).min(py2).min(py3) - r_screen;
        let mxy = py0.max(py1).max(py2).max(py3) + r_screen;
        (mnx as i32, mny as i32, mxx.ceil() as i32, mxy.ceil() as i32)
    };
    let mw = (bx1 - bx0).max(1) as u32;
    let mh = (by1 - by0).max(1) as u32;
    let mut mask = vec![0f32; (mw * mh) as usize];
    let ex = bx + spread;
    let ey = by + spread;
    let ew = (bw - spread * 2.0).max(0.0);
    let eh = (bh - spread * 2.0).max(0.0);
    for y in 0..mh {
        for x in 0..mw {
            // Mask cell → local (pre-transform) sample point + local offset.
            let (lx, ly) = Transform::apply(
                &tf.inv,
                bx0 as f32 + x as f32 + 0.5,
                by0 as f32 + y as f32 + 0.5,
            );
            let (wx, wy) = (lx + offx, ly + offy);
            let inside = if s.inset {
                // Inner band: inside the box but within blur distance of an edge.
                wx >= bx
                    && wx < bx + bw
                    && wy >= by
                    && wy < by + bh
                    && (wx - bx).min(bx + bw - wx).min(wy - by).min(by + bh - wy)
                        < (blur.max(1.0) + spread.abs())
            } else {
                rounded_sdf(wx, wy, ex, ey, ew, eh, radius) < 0.0
            } && clip.map_or(true, |c| c.contains(wx, wy));
            mask[(y * mw + x) as usize] = if inside { 1.0 } else { 0.0 };
        }
    }
    let r = (blur * 0.5).ceil() as u32;
    if r > 0 {
        mask = blur_box(&mask, mw, mh, r.min(16));
    }
    let tint = rgba(s.color);
    for y in 0..mh {
        for x in 0..mw {
            let a = mask[(y * mw + x) as usize] * tint[3];
            if a <= 0.003 {
                continue;
            }
            let (wx, wy) = (bx0 + x as i32, by0 + y as i32);
            if wx < 0 || wy < 0 {
                continue;
            }
            blend(px, wx as u32, wy as u32, w, [tint[0], tint[1], tint[2], a]);
        }
    }
}

fn blur_box(mask: &[f32], w: u32, h: u32, r: u32) -> Vec<f32> {
    let mut tmp = vec![0f32; (w * h) as usize];
    let mut out = vec![0f32; (w * h) as usize];
    let n = (r * 2 + 1) as f32;
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for k in 0..=r * 2 {
                let sx = (x as i32 + k as i32 - r as i32).clamp(0, w as i32 - 1) as u32;
                acc += mask[(y * w + sx) as usize];
            }
            tmp[(y * w + x) as usize] = acc / n;
        }
    }
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for k in 0..=r * 2 {
                let sy = (y as i32 + k as i32 - r as i32).clamp(0, h as i32 - 1) as u32;
                acc += tmp[(sy * w + x) as usize];
            }
            out[(y * w + x) as usize] = acc / n;
        }
    }
    out
}

fn draw_image(
    px: &mut [u8],
    w: u32,
    h: u32,
    tf: &Transform,
    x: f32,
    y: f32,
    bw: f32,
    bh: f32,
    raster: &RgbaImage,
    fit: kitae_style::ObjectFit,
    clip: Option<&SceneClip>,
) {
    if bw <= 0.0 || bh <= 0.0 || raster.width == 0 || raster.height == 0 {
        return;
    }
    let fr = object_fit_rect(bw, bh, raster.width as f32, raster.height as f32, fit);
    let (x0, y0, x1, y1) = tf.bbox(x, y, bw, bh, w, h);
    for yy in y0..y1 {
        for xx in x0..x1 {
            // Local-space destination point → source pixel (bilinear).
            let (lx, ly) = Transform::apply(&tf.inv, xx as f32 + 0.5, yy as f32 + 0.5);
            if let Some(c) = clip {
                if !c.contains(lx, ly) {
                    continue;
                }
            }
            let u = (lx - (x + fr.dst_x)) / fr.dst_w.max(0.001);
            let v = (ly - (y + fr.dst_y)) / fr.dst_h.max(0.001);
            if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                continue;
            }
            let sx = fr.src_x + u * fr.src_w;
            let sy = fr.src_y + v * fr.src_h;
            blend(px, xx, yy, w, bilinear(raster, sx, sy));
        }
    }
}

fn bilinear(raster: &RgbaImage, sx: f32, sy: f32) -> [f32; 4] {
    let (w, h) = (raster.width as f32, raster.height as f32);
    let x0 = sx.floor().clamp(0.0, w - 1.0) as u32;
    let y0 = sy.floor().clamp(0.0, h - 1.0) as u32;
    let x1 = (x0 + 1).min(raster.width - 1);
    let y1 = (y0 + 1).min(raster.height - 1);
    let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
    let at = |x: u32, y: u32| {
        let i = ((y * raster.width + x) * 4) as usize;
        [
            raster.pixels[i] as f32 / 255.0,
            raster.pixels[i + 1] as f32 / 255.0,
            raster.pixels[i + 2] as f32 / 255.0,
            raster.pixels[i + 3] as f32 / 255.0,
        ]
    };
    let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
    [
        a[0] * (1.0 - fx) * (1.0 - fy)
            + b[0] * fx * (1.0 - fy)
            + c[0] * (1.0 - fx) * fy
            + d[0] * fx * fy,
        a[1] * (1.0 - fx) * (1.0 - fy)
            + b[1] * fx * (1.0 - fy)
            + c[1] * (1.0 - fx) * fy
            + d[1] * fx * fy,
        a[2] * (1.0 - fx) * (1.0 - fy)
            + b[2] * fx * (1.0 - fy)
            + c[2] * (1.0 - fx) * fy
            + d[2] * fx * fy,
        a[3] * (1.0 - fx) * (1.0 - fy)
            + b[3] * fx * (1.0 - fy)
            + c[3] * (1.0 - fx) * fy
            + d[3] * fx * fy,
    ]
}

fn draw_glyph(
    px: &mut [u8],
    w: u32,
    h: u32,
    tf: &Transform,
    x: f32,
    y_baseline: f32,
    scale: f32,
    g: &crate::GlyphQuad,
    atlas: &GlyphAtlas,
    lcd: bool,
) {
    if scale <= 0.0 {
        return;
    }
    let Some(loc) = atlas.lookup(&g.key) else {
        return;
    };
    let Some((pw, _, bits)) = atlas.page_pixels(loc.page) else {
        return;
    };
    // Baseline-anchored: bitmap top sits ascent above the baseline. Hosts
    // record bitmaps top-row-first with ascent baked as top padding, so the
    // quad origin is (x, baseline - height), all scaled to device px.
    let tint = rgba(g.color);
    let (dw, dh) = (
        (loc.w as f32 * scale).ceil() as u32,
        (loc.h as f32 * scale).ceil() as u32,
    );
    let qy = y_baseline - dh as f32;
    // E3: a blurred run (text-shadow duplicate) stamps its coverage into a
    // padded device-space mask, blurs it with the same box kernel shadows
    // use, and composites the result — the alpha-blur mirror of draw_shadow.
    if g.blur > 0.0 && g.blur * scale >= 1.0 {
        let r = ((g.blur * scale * 0.5).ceil() as u32).min(16);
        let (mx0, my0) = (x as i32 - r as i32, qy as i32 - r as i32);
        let (mw, mh) = (dw as i32 + 2 * r as i32, dh as i32 + 2 * r as i32);
        if mw > 0 && mh > 0 {
            let mut mask = vec![0f32; (mw * mh) as usize];
            for my in 0..mh {
                for mx in 0..mw {
                    let (lx, ly) =
                        Transform::apply(&tf.inv, (mx0 + mx) as f32 + 0.5, (my0 + my) as f32 + 0.5);
                    let (col, row) = (lx - x, ly - qy);
                    if col < 0.0 || row < 0.0 {
                        continue;
                    }
                    let (sx, sy) = ((col / scale) as u32, (row / scale) as u32);
                    if sx >= loc.w || sy >= loc.h {
                        continue;
                    }
                    mask[(my * mw + mx) as usize] =
                        bits[((loc.y + sy) * pw + loc.x + sx) as usize] as f32 / 255.0;
                }
            }
            let mask = blur_box(&mask, mw as u32, mh as u32, r);
            for my in 0..mh {
                for mx in 0..mw {
                    let a = mask[(my * mw + mx) as usize] * tint[3];
                    if a <= 0.003 {
                        continue;
                    }
                    let (wx, wy) = (mx0 + mx, my0 + my);
                    if wx < 0 || wy < 0 || wx >= w as i32 || wy >= h as i32 {
                        continue;
                    }
                    blend(px, wx as u32, wy as u32, w, [tint[0], tint[1], tint[2], a]);
                }
            }
        }
        return;
    }
    let (x0, y0, x1, y1) = tf.bbox(x, qy, dw as f32, dh as f32, w, h);
    for iy in y0..y1 {
        for ix in x0..x1 {
            // Local-space quad position → atlas texel (same nearest-texel
            // mapping the untransformed loop uses at integral origins).
            let (lx, ly) = Transform::apply(&tf.inv, ix as f32 + 0.5, iy as f32 + 0.5);
            let (col, row) = (lx - x, ly - qy);
            if col < 0.0 || row < 0.0 {
                continue;
            }
            let sx = (col / scale) as u32;
            let sy = (row / scale) as u32;
            if sx >= loc.w || sy >= loc.h {
                continue;
            }
            if lcd {
                // E6 LCD (subpixel) coverage: sample the atlas alpha a third
                // of a device pixel left/right of the center (in texel units
                // 1/(3·scale)) with linear interpolation, and composite each
                // channel with its own coverage. Decomposition-friendly on
                // the CPU reference; the GPU backend stays grayscale.
                let base = col / scale;
                let alpha_at = |tx: f32| -> f32 {
                    let f = tx - tx.floor();
                    let t0 = tx.floor().clamp(0.0, (loc.w - 1) as f32) as u32;
                    let t1 = (t0 + 1).min(loc.w - 1);
                    let a0 = bits[((loc.y + sy) * pw + loc.x + t0) as usize] as f32 / 255.0;
                    let a1 = bits[((loc.y + sy) * pw + loc.x + t1) as usize] as f32 / 255.0;
                    a0 + (a1 - a0) * f
                };
                let third = 1.0 / (3.0 * scale);
                let a = [
                    alpha_at(base - third) * tint[3],
                    alpha_at(base) * tint[3],
                    alpha_at(base + third) * tint[3],
                ];
                if a.iter().all(|&v| v <= 0.003) {
                    continue;
                }
                blend_lcd(px, ix, iy, w, [tint[0], tint[1], tint[2]], a);
                continue;
            }
            let a = bits[((loc.y + sy) * pw + loc.x + sx) as usize] as f32 / 255.0 * tint[3];
            if a <= 0.003 {
                continue;
            }
            blend(px, ix, iy, w, [tint[0], tint[1], tint[2], a]);
        }
    }
}

/// E6: source-over with per-channel coverage (straight alpha). The output
/// alpha is the channels' mean — decomposition on the RGB fringes is the
/// point of LCD AA.
fn blend_lcd(dst: &mut [u8], x: u32, y: u32, w: u32, rgb: [f32; 3], a: [f32; 3]) {
    if x >= w {
        return;
    }
    let i = ((y * w + x) * 4) as usize;
    if i + 4 > dst.len() {
        return;
    }
    let d = [
        dst[i] as f32 / 255.0,
        dst[i + 1] as f32 / 255.0,
        dst[i + 2] as f32 / 255.0,
        dst[i + 3] as f32 / 255.0,
    ];
    let mut out = [0f32; 4];
    for c in 0..3 {
        out[c] = rgb[c] * a[c] + d[c] * (1.0 - a[c]);
    }
    let am = (a[0] + a[1] + a[2]) / 3.0;
    out[3] = am + d[3] * (1.0 - am);
    let p = pack(out);
    dst[i..i + 4].copy_from_slice(&p);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GlyphKey, GpuScene, ScenePaint, SceneRect};

    fn solid_scene() -> GpuScene {
        GpuScene {
            w: 32,
            h: 32,
            bg: 0xFFFFFFFF,
            rects: vec![SceneRect {
                x: 4.0,
                y: 4.0,
                w: 24.0,
                h: 24.0,
                color: 0xFF0000FF,
                radius: [0.0; 4],
                paint: ScenePaint::Solid,
                transform: None,
                clip: None,
            }],
            ..GpuScene::default()
        }
    }

    fn at(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    #[test]
    fn solid_rect_and_hash_determinism() {
        let scene = solid_scene();
        let a = render(
            &scene,
            32,
            32,
            1.0,
            0.0,
            0.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        let b = render(
            &scene,
            32,
            32,
            1.0,
            0.0,
            0.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        assert_eq!(hash_pixels(&a), hash_pixels(&b));
        assert_eq!(at(&a, 32, 0, 0), [255, 255, 255, 255]);
        assert_eq!(at(&a, 32, 16, 16), [255, 0, 0, 255]);
        // Perturbing the scene changes the hash.
        let mut scene2 = scene;
        scene2.rects[0].color = 0x00FF00FF;
        assert_ne!(
            hash_pixels(&a),
            hash_pixels(&render(
                &scene2,
                32,
                32,
                1.0,
                0.0,
                0.0,
                &HashMap::new(),
                &GlyphAtlas::new()
            ))
        );
    }

    #[test]
    fn linear_gradient_midpoint_and_direction() {
        let scene = GpuScene {
            bg: 0xFFFFFFFF,
            rects: vec![SceneRect {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 10.0,
                color: 0xFF0000FF,
                radius: [0.0; 4],
                paint: ScenePaint::Linear {
                    angle_deg: 90.0, // to the right
                    stops: vec![(0.0, 0xFF0000FF), (1.0, 0x0000FFFF)],
                },
                transform: None,
                clip: None,
            }],
            ..GpuScene::default()
        };
        let px = render(
            &scene,
            100,
            10,
            1.0,
            0.0,
            0.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        let left = at(&px, 100, 2, 5);
        let mid = at(&px, 100, 50, 5);
        let right = at(&px, 100, 97, 5);
        assert!(left[0] > 200 && left[2] < 60, "{left:?}");
        assert!((mid[0] as i32 - mid[2] as i32).abs() < 30, "{mid:?}");
        assert!(right[2] > 200 && right[0] < 60, "{right:?}");
    }

    #[test]
    fn rounded_corners_cut_and_shadow_softens() {
        let scene = GpuScene {
            bg: 0xFFFFFFFF,
            rects: vec![SceneRect {
                x: 4.0,
                y: 4.0,
                w: 24.0,
                h: 24.0,
                color: 0x0000FFFF,
                radius: [8.0; 4],
                paint: ScenePaint::Solid,
                transform: None,
                clip: None,
            }],
            shadows: vec![crate::SceneShadow {
                x: 40.0,
                y: 4.0,
                w: 20.0,
                h: 20.0,
                radius: [0.0; 4],
                color: 0x000000FF,
                blur: 6.0,
                spread: 0.0,
                offset_x: 0.0,
                offset_y: 0.0,
                inset: false,
                transform: None,
                clip: None,
            }],
            ..GpuScene::default()
        };
        let px = render(
            &scene,
            72,
            32,
            1.0,
            0.0,
            0.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        // Corner pixel of the rounded box stays background.
        assert_eq!(at(&px, 72, 4, 4), [255, 255, 255, 255]);
        // Center is solid blue.
        assert_eq!(at(&px, 72, 16, 16), [0, 0, 255, 255]);
        // Shadow core is dark, halo is softer (monotone falloff outward).
        let core = at(&px, 72, 50, 14);
        let halo = at(&px, 72, 38, 14);
        assert!(core[0] < 120, "core dark: {core:?}");
        assert!(halo[0] > core[0] && halo[0] < 255, "halo softer: {halo:?}");
    }

    #[test]
    fn image_blit_with_cover_crops() {
        // 3x2: x=0,1 red, x=2 green. Cover into a 2x2 box samples src
        // 0.5..2.5, so pixel centers land purely on red / green.
        let mut pixels = Vec::new();
        for y in 0..2 {
            for x in 0..3 {
                let _ = y;
                pixels.extend_from_slice(if x < 2 {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 255, 0, 255]
                });
            }
        }
        let raster = Arc::new(RgbaImage {
            width: 3,
            height: 2,
            pixels,
        });
        let scene = GpuScene {
            bg: 0xFFFFFFFF,
            images: vec![crate::SceneImage {
                x: 0.0,
                y: 0.0,
                w: 2.0,
                h: 2.0,
                payload: 1,
                fit: kitae_style::ObjectFit::Cover,
                img_w: 3.0,
                img_h: 2.0,
                transform: None,
                clip: None,
            }],
            ..GpuScene::default()
        };
        let mut rasters = HashMap::new();
        rasters.insert(1u64, raster);
        let px = render(&scene, 4, 4, 1.0, 0.0, 0.0, &rasters, &GlyphAtlas::new());
        // Cover 4x2 → 2x2 crops the middle: samples land on both halves.
        let mut samples = Vec::new();
        for y in 0..2 {
            for x in 0..2 {
                samples.push(at(&px, 4, x, y));
            }
        }
        assert!(
            samples.iter().any(|s| s[0] > 200),
            "red present: {samples:?}"
        );
        assert!(
            samples.iter().any(|s| s[1] > 200),
            "green present: {samples:?}"
        );
    }

    #[test]
    fn glyph_quad_tints_atlas_bitmap() {
        let mut atlas = GlyphAtlas::new();
        // 3x5 solid block for 'x' at 16px.
        atlas
            .upload(
                GlyphKey {
                    ch: 'x',
                    size_px: 16,
                    bold: false,
                    italic: false,
                    monospace: false,
                    family: 0,
                },
                3,
                5,
                &vec![255u8; 15],
            )
            .expect("upload");
        let scene = GpuScene {
            bg: 0xFFFFFFFF,
            glyphs: vec![crate::GlyphQuad {
                x: 2.0,
                y: 10.0,
                size: 16.0,
                color: 0x0000FFFF,
                key: GlyphKey {
                    ch: 'x',
                    size_px: 16,
                    bold: false,
                    italic: false,
                    monospace: false,
                    family: 0,
                },
                transform: None,
                blur: 0.0,
            }],
            ..GpuScene::default()
        };
        let px = render(&scene, 16, 16, 1.0, 0.0, 0.0, &HashMap::new(), &atlas);
        assert_eq!(at(&px, 16, 3, 7), [0, 0, 255, 255]);
        assert_eq!(at(&px, 16, 0, 0), [255, 255, 255, 255]);
    }

    /// Q2: a 45°-rotated box paints as a diamond — the warp preserves the
    /// box's page-space center and area, and identity stays byte-exact.
    #[test]
    fn rotated_rect_warps_geometry() {
        let make = |tf: Option<kitae_layout::Matrix2d>| GpuScene {
            bg: 0xFFFFFFFF,
            rects: vec![SceneRect {
                x: 4.0,
                y: 4.0,
                w: 24.0,
                h: 24.0,
                color: 0xFF0000FF,
                radius: [0.0; 4],
                paint: ScenePaint::Solid,
                transform: tf,
                clip: None,
            }],
            ..GpuScene::default()
        };
        // rotate(45°) about the box center (16, 16): M = T·R = [c,-s,tx; s,c,ty].
        let (s, c) = (
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
        );
        // tx = 16 - (c*16 - s*16), ty = 16 - (s*16 + c*16).
        let tx = 16.0 - (c * 16.0 - s * 16.0);
        let ty = 16.0 - (s * 16.0 + c * 16.0);
        let m = kitae_layout::Matrix2d([c, -s, tx, s, c, ty]);
        let untransformed = render(
            &make(None),
            32,
            32,
            1.0,
            0.0,
            0.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        let rotated = render(
            &make(Some(m)),
            32,
            32,
            1.0,
            0.0,
            0.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        // Identity transform is byte-identical to no transform at all.
        let ident = make(Some(kitae_layout::Matrix2d(crate::MATRIX_IDENTITY)));
        assert_eq!(
            hash_pixels(&untransformed),
            hash_pixels(&render(
                &ident,
                32,
                32,
                1.0,
                0.0,
                0.0,
                &HashMap::new(),
                &GlyphAtlas::new()
            ))
        );
        // Rotation changes the picture (hash differs)...
        assert_ne!(hash_pixels(&untransformed), hash_pixels(&rotated));
        // ...keeps the page-space center red...
        assert_eq!(at(&rotated, 32, 16, 16), [255, 0, 0, 255]);
        // ...and the diamond's cardinal points red where the square's
        // corners now reach: (4,16) and (28,16) sit on the rotated edges.
        assert_eq!(at(&rotated, 32, 5, 16), [255, 0, 0, 255]);
        assert_eq!(at(&rotated, 32, 27, 16), [255, 0, 0, 255]);
        // But the old untransformed corners are background now: the rotated
        // quad's corners sit at (16, −1), (33, 16), (16, 33), (−1, 16).
        assert_eq!(at(&rotated, 32, 4, 4), [255, 255, 255, 255]);
        assert_eq!(at(&rotated, 32, 27, 4), [255, 255, 255, 255]);
        assert_eq!(at(&rotated, 32, 1, 1), [255, 255, 255, 255]);
    }

    /// E2: a sticky box's items paint at their flow position at scroll 0
    /// and pin to `top_offset` once scrolled — the painter applies
    /// SceneSticky::shift with (scroll_y=oy, viewport_h=h/scale).
    #[test]
    fn sticky_items_shift_at_scroll() {
        let scene = GpuScene {
            bg: 0xFFFFFFFF,
            rects: vec![SceneRect {
                x: 4.0,
                y: 60.0,
                w: 24.0,
                h: 30.0,
                color: 0xFF0000FF,
                radius: [0.0; 4],
                paint: ScenePaint::Solid,
                transform: None,
                clip: None,
            }],
            stickies: vec![crate::SceneSticky {
                rect: (0, 1),
                text: (0, 0),
                link: (0, 0),
                image: (0, 0),
                media: (0, 0),
                shadow: (0, 0),
                base_y: 60.0,
                height: 30.0,
                container_top: 0.0,
                container_bottom: 600.0,
                top_offset: 12.0,
                bottom_offset: 0.0,
                has_top: true,
                has_bottom: false,
            }],
            ..GpuScene::default()
        };
        // At scroll 0 the box paints in flow (y=60..90; canvas is 64 tall).
        let px = render(
            &scene,
            32,
            64,
            1.0,
            0.0,
            0.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        assert_eq!(at(&px, 32, 16, 61), [255, 0, 0, 255]);
        // At scroll 100 the shift is 100+12−60 = 52 → device y = 12.
        let px = render(
            &scene,
            32,
            64,
            1.0,
            0.0,
            100.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        assert_eq!(at(&px, 32, 16, 12), [255, 0, 0, 255]);
        assert_eq!(at(&px, 32, 16, 11), [255, 255, 255, 255]);
        // Without the sticky registration the scrolled render shows nothing
        // (the flow position is 60 rows above the tile top).
        let mut bare = scene.clone();
        bare.stickies.clear();
        let px = render(
            &bare,
            32,
            64,
            1.0,
            0.0,
            100.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        assert_eq!(at(&px, 32, 16, 12), [255, 255, 255, 255]);
    }

    /// E8: clip regions discard samples in the item's local space — a
    /// circle leaves the rect's corners unpainted, an inset with corner
    /// radii cuts them too.
    #[test]
    fn clip_discards_outside_samples() {
        let make = |clip: Option<crate::SceneClip>| GpuScene {
            bg: 0xFFFFFFFF,
            rects: vec![SceneRect {
                x: 4.0,
                y: 4.0,
                w: 24.0,
                h: 24.0,
                color: 0xFF0000FF,
                radius: [0.0; 4],
                paint: ScenePaint::Solid,
                transform: None,
                clip,
            }],
            ..GpuScene::default()
        };
        let px = render(
            &make(Some(crate::SceneClip::Circle {
                cx: 16.0,
                cy: 16.0,
                r: 6.0,
            })),
            32,
            32,
            1.0,
            0.0,
            0.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        assert_eq!(at(&px, 32, 16, 16), [255, 0, 0, 255]);
        assert_eq!(at(&px, 32, 6, 6), [255, 255, 255, 255]);
        // Inset with a 6px top-left radius: the diagonal of the corner box
        // (9,9) falls outside the quarter-ellipse, the far side keeps full
        // bleed to the inset edge.
        let px = render(
            &make(Some(crate::SceneClip::Inset {
                x: 4.0,
                y: 4.0,
                w: 24.0,
                h: 24.0,
                radii: [(6.0, 6.0), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0)],
            })),
            32,
            32,
            1.0,
            0.0,
            0.0,
            &HashMap::new(),
            &GlyphAtlas::new(),
        );
        assert_eq!(at(&px, 32, 9, 9), [255, 255, 255, 255]);
        assert_eq!(at(&px, 32, 27, 27), [255, 0, 0, 255]);
    }

    /// E3: a blurred glyph run spreads its ink beyond the crisp footprint
    /// and dims the core — the same box-blur shadows use.
    #[test]
    fn blurred_glyph_softens() {
        let mut atlas = GlyphAtlas::new();
        atlas
            .upload(
                GlyphKey {
                    ch: 'x',
                    size_px: 16,
                    bold: false,
                    italic: false,
                    monospace: false,
                    family: 0,
                },
                3,
                5,
                &vec![255u8; 15],
            )
            .expect("upload");
        let make = |blur: f32| GpuScene {
            bg: 0xFFFFFFFF,
            glyphs: vec![crate::GlyphQuad {
                x: 2.0,
                y: 10.0,
                size: 16.0,
                color: 0x000000FF,
                key: GlyphKey {
                    ch: 'x',
                    size_px: 16,
                    bold: false,
                    italic: false,
                    monospace: false,
                    family: 0,
                },
                transform: None,
                blur,
            }],
            ..GpuScene::default()
        };
        let crisp = render(&make(0.0), 16, 16, 1.0, 0.0, 0.0, &HashMap::new(), &atlas);
        assert_eq!(at(&crisp, 16, 3, 7), [0, 0, 0, 255]);
        // One pixel left of the quad stays clean when crisp.
        assert_eq!(at(&crisp, 16, 1, 7), [255, 255, 255, 255]);
        let blurred = render(&make(4.0), 16, 16, 1.0, 0.0, 0.0, &HashMap::new(), &atlas);
        // The core lightens (3-wide block under a 5-tap window ≈ 60%)...
        let core = at(&blurred, 16, 3, 7);
        assert!(core[0] > 40 && core[0] < 200, "core softened: {core:?}");
        // ...and ink bleeds past the crisp footprint.
        assert!(at(&blurred, 16, 1, 7)[0] < 255, "bleeds left");
        assert!(at(&blurred, 16, 6, 7)[0] < 255, "bleeds right");
    }

    /// E6: LCD mode composites each channel with coverage sampled a third
    /// of a pixel apart — a horizontal coverage ramp decomposes into
    /// channel fringes; grayscale mode stays channel-equal.
    #[test]
    fn lcd_text_channels_decompose() {
        let mut atlas = GlyphAtlas::new();
        // 3x5 'x' whose columns ramp 64 → 192 → 255.
        let mut bmp = Vec::new();
        for _ in 0..5 {
            bmp.extend_from_slice(&[64, 192, 255]);
        }
        atlas
            .upload(
                GlyphKey {
                    ch: 'x',
                    size_px: 16,
                    bold: false,
                    italic: false,
                    monospace: false,
                    family: 0,
                },
                3,
                5,
                &bmp,
            )
            .expect("upload");
        let make = |lcd: bool| GpuScene {
            bg: 0xFFFFFFFF,
            lcd_text: lcd,
            glyphs: vec![crate::GlyphQuad {
                x: 2.0,
                y: 10.0,
                size: 16.0,
                color: 0x000000FF,
                key: GlyphKey {
                    ch: 'x',
                    size_px: 16,
                    bold: false,
                    italic: false,
                    monospace: false,
                    family: 0,
                },
                transform: None,
                blur: 0.0,
            }],
            ..GpuScene::default()
        };
        let gray = render(&make(false), 16, 16, 1.0, 0.0, 0.0, &HashMap::new(), &atlas);
        let lcd = render(&make(true), 16, 16, 1.0, 0.0, 0.0, &HashMap::new(), &atlas);
        // Column 1 (texel value 192): grayscale is channel-equal...
        let g = at(&gray, 16, 3, 7);
        assert!(
            g[0].abs_diff(g[1]) <= 1 && g[1].abs_diff(g[2]) <= 1,
            "gray neutral: {g:?}"
        );
        // ...LCD spreads the taps (149/192/213 coverage → 106/63/42 ink on
        // white), so red is lightest and blue darkest.
        let l = at(&lcd, 16, 3, 7);
        assert!(l[0] > l[1] && l[1] > l[2], "lcd fringe: {l:?}");
    }
}
