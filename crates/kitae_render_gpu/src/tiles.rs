//! E4: tiled rasterization with dirty-rect compositing. The surface is cut
//! into a CSS-px tile grid; each tile carries an FNV signature over the
//! paint items whose page-space footprint reaches it (plus the scene-level
//! state that can change any pixel). Unchanged tiles are blitted from the
//! cache and never re-rendered, so partial scene updates — caret blink,
//! hover, an animation frame — repaint only the touched tiles.
//!
//! The cache is viewport-space: the grid key includes the surface origin
//! and scroll, so a scroll re-renders every tile (sticky shifts are
//! scroll-dependent by design); the win is partial invalidation at a steady
//! scroll position. Per-tile renders go through [`crate::cpu::render_with_scroll`]
//! with the tile corner as geometry origin and the surface's scroll/viewport
//! height, which makes tile assembly bit-exact against a whole-surface
//! [`crate::cpu::render`] at scale 1 with integral origins.

use std::collections::HashMap;
use std::sync::Arc;

use kitae_media::RgbaImage;

use crate::{GlyphAtlas, GpuScene, SceneClip, ScenePaint, SceneSticky};

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// FNV-1a over the exact bit pattern of every input that can change a
/// tile's pixels — no float rounding, no derived Hash impls.
struct Sig(u64);

impl Sig {
    fn new() -> Sig {
        Sig(FNV_OFFSET)
    }
    fn u64(&mut self, v: u64) {
        self.0 = (self.0 ^ v).wrapping_mul(FNV_PRIME);
    }
    fn u32(&mut self, v: u32) {
        self.u64(v as u64);
    }
    fn f32(&mut self, v: f32) {
        self.u64(v.to_bits() as u64);
    }
    fn b(&mut self, v: bool) {
        self.u64(v as u64);
    }
    fn slice(&mut self, v: &[f32]) {
        for f in v {
            self.f32(*f);
        }
    }
    fn opt(&mut self, v: Option<f32>) {
        match v {
            Some(x) => {
                self.u64(1);
                self.f32(x);
            }
            None => self.u64(0),
        }
    }
    fn matrix(&mut self, m: Option<&kitae_layout::Matrix2d>) {
        match m {
            None => self.u64(0),
            Some(t) => {
                self.u64(1);
                self.slice(&t.0);
            }
        }
    }
    fn clip(&mut self, c: Option<&SceneClip>) {
        let Some(c) = c else {
            self.u64(0);
            return;
        };
        self.u64(1);
        match c {
            SceneClip::Circle { cx, cy, r } => {
                self.u64(1);
                self.f32(*cx);
                self.f32(*cy);
                self.f32(*r);
            }
            SceneClip::Ellipse { cx, cy, rx, ry } => {
                self.u64(2);
                for f in [cx, cy, rx, ry] {
                    self.f32(*f);
                }
            }
            SceneClip::Inset { x, y, w, h, radii } => {
                self.u64(3);
                for f in [x, y, w, h] {
                    self.f32(*f);
                }
                for (a, b) in radii {
                    self.f32(*a);
                    self.f32(*b);
                }
            }
            SceneClip::Polygon { points, evenodd } => {
                self.u64(4);
                self.b(*evenodd);
                self.u64(points.len() as u64);
                for (x, y) in points {
                    self.f32(*x);
                    self.f32(*y);
                }
            }
        }
    }
    fn paint(&mut self, p: &ScenePaint) {
        match p {
            ScenePaint::Solid => self.u64(0),
            ScenePaint::Linear { angle_deg, stops } => {
                self.u64(1);
                self.f32(*angle_deg);
                self.u64(stops.len() as u64);
                for (o, c) in stops {
                    self.f32(*o);
                    self.u32(*c);
                }
            }
            ScenePaint::Radial {
                cx,
                cy,
                radius,
                stops,
            } => {
                self.u64(2);
                self.f32(*cx);
                self.f32(*cy);
                self.opt(*radius);
                self.u64(stops.len() as u64);
                for (o, c) in stops {
                    self.f32(*o);
                    self.u32(*c);
                }
            }
        }
    }
}

fn fit_tag(f: kitae_style::ObjectFit) -> u64 {
    use kitae_style::ObjectFit as F;
    match f {
        F::Fill => 0,
        F::Contain => 1,
        F::Cover => 2,
        F::None => 3,
        F::ScaleDown => 4,
    }
}

/// Page-space footprint of one item (already sticky-expanded; callers add
/// the per-type spill margin before the tile intersection test).
#[derive(Clone, Copy)]
struct Box2 {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Box2 {
    fn new(x: f32, y: f32, w: f32, h: f32) -> Box2 {
        Box2 {
            x0: x,
            y0: y,
            x1: x + w,
            y1: y + h,
        }
    }
    fn pad_y(&mut self, up: f32, down: f32) {
        self.y0 -= up;
        self.y1 += down;
    }
    fn grow(&mut self, m: f32) {
        self.x0 -= m;
        self.y0 -= m;
        self.x1 += m;
        self.y1 += m;
    }
    fn hits(&self, t: &Box2) -> bool {
        self.x0 < t.x1 && self.x1 > t.x0 && self.y0 < t.y1 && self.y1 > t.y0
    }
}

/// Warped page-space bbox: apply the item's own page-space matrix to its
/// four corners (same `Transform::apply` convention the CPU painter uses).
fn warped(t: &kitae_layout::Matrix2d, b: Box2) -> Box2 {
    let pts = [(b.x0, b.y0), (b.x1, b.y0), (b.x1, b.y1), (b.x0, b.y1)];
    let mut out = Box2 {
        x0: f32::MAX,
        y0: f32::MAX,
        x1: f32::MIN,
        y1: f32::MIN,
    };
    for (x, y) in pts {
        let (px, py) = (
            t.0[0] * x + t.0[1] * y + t.0[2],
            t.0[3] * x + t.0[4] * y + t.0[5],
        );
        out.x0 = out.x0.min(px);
        out.y0 = out.y0.min(py);
        out.x1 = out.x1.max(px);
        out.y1 = out.y1.max(py);
    }
    out
}

/// Per-item worst-case sticky travel (up, down) in page CSS px. A sticky
/// member can paint anywhere between its flow position and its clamp
/// extremes, so its footprint must span the whole travel for tile culling.
type Pads = (
    Vec<(f32, f32)>,
    Vec<(f32, f32)>,
    Vec<(f32, f32)>,
    Vec<(f32, f32)>,
);

fn sticky_pads(scene: &GpuScene) -> Pads {
    let mut rects = vec![(0.0f32, 0.0f32); scene.rects.len()];
    let mut glyphs = vec![(0.0f32, 0.0f32); scene.glyphs.len()];
    let mut images = vec![(0.0f32, 0.0f32); scene.images.len()];
    let mut shadows = vec![(0.0f32, 0.0f32); scene.shadows.len()];
    for st in &scene.stickies {
        let down = if st.has_top {
            (st.container_bottom - st.bottom_offset - st.height - st.base_y).max(0.0)
        } else {
            0.0
        };
        let up = if st.has_bottom {
            (st.base_y - st.container_top - st.top_offset).max(0.0)
        } else {
            0.0
        };
        if down == 0.0 && up == 0.0 {
            continue;
        }
        let mut acc = |v: &mut Vec<(f32, f32)>, range: (u32, u32)| {
            for i in range.0 as usize..(range.0 + range.1) as usize {
                if let Some(slot) = v.get_mut(i) {
                    slot.0 = slot.0.max(up);
                    slot.1 = slot.1.max(down);
                }
            }
        };
        acc(&mut rects, st.rect);
        acc(&mut images, st.image);
        acc(&mut shadows, st.shadow);
        for run in st.text.0..st.text.0 + st.text.1 {
            if let Some(span) = scene.text_spans.get(run as usize) {
                for gi in span.0..span.0 + span.1 {
                    if let Some(slot) = glyphs.get_mut(gi as usize) {
                        slot.0 = slot.0.max(up);
                        slot.1 = slot.1.max(down);
                    }
                }
            }
        }
    }
    (rects, glyphs, images, shadows)
}

struct ItemBoxes {
    rects: Vec<Box2>,
    glyphs: Vec<Box2>,
    images: Vec<Box2>,
    shadows: Vec<Box2>,
}

fn item_boxes(scene: &GpuScene) -> ItemBoxes {
    let (rp, gp, ip, sp) = sticky_pads(scene);
    let rects = scene
        .rects
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut b = match r.transform {
                Some(t) => warped(&t, Box2::new(r.x, r.y, r.w, r.h)),
                None => Box2::new(r.x, r.y, r.w, r.h),
            };
            b.pad_y(rp[i].0, rp[i].1);
            b
        })
        .collect();
    let glyphs = scene
        .glyphs
        .iter()
        .enumerate()
        .map(|(i, g)| {
            // Baseline quad with a generous em-sized halo; the atlas bitmap
            // is looked up only at paint time, so culling can't use it.
            let mut b = Box2::new(g.x - g.size, g.y - g.size * 2.0, g.size * 3.0, g.size * 3.0);
            b.pad_y(gp[i].0, gp[i].1);
            b
        })
        .collect();
    let images = scene
        .images
        .iter()
        .enumerate()
        .map(|(i, im)| {
            // object-fit destinations always land inside the layout box.
            let mut b = Box2::new(im.x, im.y, im.w, im.h);
            b.pad_y(ip[i].0, ip[i].1);
            b
        })
        .collect();
    let shadows = scene
        .shadows
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let spill = s.blur * 2.0 + s.spread + s.offset_x.abs().max(s.offset_y.abs());
            let mut b = Box2::new(
                s.x - spill,
                s.y - spill,
                s.w + spill * 2.0,
                s.h + spill * 2.0,
            );
            b.pad_y(sp[i].0, sp[i].1);
            b
        })
        .collect();
    ItemBoxes {
        rects,
        glyphs,
        images,
        shadows,
    }
}

/// Scene-level signature shared by every tile: background, the LCD flag,
/// and the sticky set (positions, clamps, membership ranges) whose shifts
/// depend on the grid key's scroll offset.
fn base_sig(scene: &GpuScene) -> u64 {
    let mut s = Sig::new();
    s.u32(scene.bg);
    s.b(scene.lcd_text);
    s.u64(scene.stickies.len() as u64);
    for st in &scene.stickies {
        let SceneSticky {
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
        } = *st;
        for (a, b) in [rect, text, link, image, media, shadow] {
            s.u64(a as u64);
            s.u64(b as u64);
        }
        for f in [
            base_y,
            height,
            container_top,
            container_bottom,
            top_offset,
            bottom_offset,
        ] {
            s.f32(f);
        }
        s.b(has_top);
        s.b(has_bottom);
    }
    s.u64(scene.text_spans.len() as u64);
    for (a, b) in &scene.text_spans {
        s.u64(*a as u64);
        s.u64(*b as u64);
    }
    s.0
}

#[allow(clippy::too_many_arguments)]
fn tile_sig(scene: &GpuScene, boxes: &ItemBoxes, tile: Box2, m: f32, base: u64) -> u64 {
    let mut s = Sig::new();
    s.u64(base);
    for (i, (b, r)) in boxes.rects.iter().zip(&scene.rects).enumerate() {
        let mut b = *b;
        b.grow(m);
        if !b.hits(&tile) {
            continue;
        }
        s.u64(0x100 + i as u64);
        for f in [r.x, r.y, r.w, r.h] {
            s.f32(f);
        }
        s.u32(r.color);
        s.slice(&r.radius);
        s.paint(&r.paint);
        s.matrix(r.transform.as_ref());
        s.clip(r.clip.as_ref());
    }
    for (i, (b, im)) in boxes.images.iter().zip(&scene.images).enumerate() {
        let mut b = *b;
        b.grow(m);
        if !b.hits(&tile) {
            continue;
        }
        s.u64(0x200 + i as u64);
        for f in [im.x, im.y, im.w, im.h] {
            s.f32(f);
        }
        s.u64(im.payload);
        s.u64(fit_tag(im.fit));
        s.f32(im.img_w);
        s.f32(im.img_h);
        s.matrix(im.transform.as_ref());
        s.clip(im.clip.as_ref());
    }
    for (i, (b, g)) in boxes.glyphs.iter().zip(&scene.glyphs).enumerate() {
        let mut b = *b;
        b.grow(m);
        if !b.hits(&tile) {
            continue;
        }
        s.u64(0x300 + i as u64);
        for f in [g.x, g.y, g.size] {
            s.f32(f);
        }
        s.u32(g.color);
        s.u32(g.key.ch as u32);
        s.u32(g.key.size_px);
        s.b(g.key.bold);
        s.b(g.key.italic);
        s.b(g.key.monospace);
        s.u32(g.key.family);
        s.matrix(g.transform.as_ref());
        s.f32(g.blur);
    }
    for (i, (b, sh)) in boxes.shadows.iter().zip(&scene.shadows).enumerate() {
        let mut b = *b;
        b.grow(m);
        if !b.hits(&tile) {
            continue;
        }
        s.u64(0x400 + i as u64);
        for f in [sh.x, sh.y, sh.w, sh.h] {
            s.f32(f);
        }
        s.u32(sh.color);
        s.slice(&sh.radius);
        s.f32(sh.blur);
        s.f32(sh.spread);
        s.f32(sh.offset_x);
        s.f32(sh.offset_y);
        s.b(sh.inset);
        s.matrix(sh.transform.as_ref());
        s.clip(sh.clip.as_ref());
    }
    s.0
}

fn blit(tile: &[u8], tw: u32, out: &mut [u8], sw: u32, sh: u32, dx: u32, dy: u32) {
    let cols = tw.min(sw.saturating_sub(dx));
    let rows = tw.min(sh.saturating_sub(dy));
    for row in 0..rows {
        let src = (row * tw * 4) as usize;
        let dst = (((dy + row) * sw + dx) * 4) as usize;
        let n = (cols * 4) as usize;
        out[dst..dst + n].copy_from_slice(&tile[src..src + n]);
    }
}

/// E4 tile grid cache. `tile` is the CSS-px edge length; the device-px tile
/// size is `tile * scale` rounded, so a scale change regrids. `hits`/`misses`
/// count blitted vs re-rendered tiles since the last grid (re)build.
pub struct TileCache {
    pub tile: u32,
    grid: Option<(u32, u32, u64, u64, u64, u32)>,
    sigs: Vec<u64>,
    tiles: Vec<Vec<u8>>,
    pub hits: u64,
    pub misses: u64,
}

impl TileCache {
    pub fn new(tile_css: u32) -> TileCache {
        TileCache {
            tile: tile_css.max(1),
            grid: None,
            sigs: Vec::new(),
            tiles: Vec::new(),
            hits: 0,
            misses: 0,
        }
    }

    pub fn clear(&mut self) {
        self.grid = None;
        self.sigs.clear();
        self.tiles.clear();
    }

    /// Render the `w`×`h` device-px surface at page-space origin (`ox`,
    /// `oy`), reusing unchanged tiles. The returned buffer is always a full
    /// fresh assembly (identical pixels to `crate::cpu::render` at scale 1
    /// with integral origins).
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        scene: &GpuScene,
        w: u32,
        h: u32,
        scale: f32,
        ox: f32,
        oy: f32,
        rasters: &HashMap<u64, Arc<RgbaImage>>,
        atlas: &GlyphAtlas,
    ) -> Vec<u8> {
        let w = w.max(1);
        let h = h.max(1);
        let tile_dev = (((self.tile as f32) * scale).round() as u32).max(1);
        let cols = w.div_ceil(tile_dev);
        let rows = h.div_ceil(tile_dev);
        let key = (
            w,
            h,
            scale.to_bits() as u64,
            ox.to_bits() as u64,
            oy.to_bits() as u64,
            tile_dev,
        );
        if self.grid != Some(key) {
            self.grid = Some(key);
            self.sigs = vec![0; (cols * rows) as usize];
            self.tiles = vec![Vec::new(); (cols * rows) as usize];
            self.hits = 0;
            self.misses = 0;
        }
        let mut out = vec![0u8; (w * h * 4) as usize];
        let boxes = item_boxes(scene);
        let base = base_sig(scene);
        let m = 1.0 / scale.max(0.001);
        let viewport_h = h as f32 / scale.max(0.001);
        let need = (tile_dev * tile_dev * 4) as usize;
        for r in 0..rows {
            for c in 0..cols {
                let i = (r * cols + c) as usize;
                let tox = ox + (c * tile_dev) as f32 / scale;
                let toy = oy + (r * tile_dev) as f32 / scale;
                let tb = Box2::new(tox, toy, tile_dev as f32 / scale, tile_dev as f32 / scale);
                let sig = tile_sig(scene, &boxes, tb, m, base);
                if sig == self.sigs[i] && self.tiles[i].len() == need {
                    self.hits += 1;
                    blit(
                        &self.tiles[i],
                        tile_dev,
                        &mut out,
                        w,
                        h,
                        c * tile_dev,
                        r * tile_dev,
                    );
                    continue;
                }
                let tpx = crate::cpu::render_with_scroll(
                    scene, tile_dev, tile_dev, scale, tox, toy, oy, viewport_h, rasters, atlas,
                );
                blit(&tpx, tile_dev, &mut out, w, h, c * tile_dev, r * tile_dev);
                self.sigs[i] = sig;
                self.tiles[i] = tpx;
                self.misses += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu;

    /// Rects straddling tile borders + an image + a glyph + a shadow, on a
    /// 128×192 surface (tile 64 → 2×3 grid).
    fn sample_scene() -> (GpuScene, HashMap<u64, Arc<RgbaImage>>, GlyphAtlas) {
        let mut atlas = GlyphAtlas::new();
        atlas
            .upload(
                crate::GlyphKey {
                    ch: 'g',
                    size_px: 16,
                    bold: false,
                    italic: false,
                    monospace: false,
                    family: 0,
                },
                5,
                7,
                &vec![200u8; 35],
            )
            .expect("atlas upload");
        let mut rasters = HashMap::new();
        rasters.insert(9u64, Arc::new(RgbaImage::solid(4, 4, [0, 128, 255, 255])));
        let scene = GpuScene {
            w: 128,
            h: 192,
            bg: 0xFFFAF0FF,
            rects: vec![
                crate::SceneRect {
                    x: 56.0,
                    y: 56.0,
                    w: 24.0,
                    h: 24.0,
                    color: 0xFF0000FF,
                    radius: [0.0; 4],
                    paint: crate::ScenePaint::Solid,
                    transform: None,
                    clip: None,
                },
                crate::SceneRect {
                    x: 90.0,
                    y: 100.0,
                    w: 30.0,
                    h: 30.0,
                    color: 0x00FF00FF,
                    radius: [6.0; 4],
                    paint: crate::ScenePaint::Linear {
                        angle_deg: 45.0,
                        stops: vec![(0.0, 0xFF0000FF), (1.0, 0x0000FFFF)],
                    },
                    transform: None,
                    clip: None,
                },
            ],
            images: vec![crate::SceneImage {
                x: 20.0,
                y: 130.0,
                w: 16.0,
                h: 16.0,
                payload: 9,
                fit: kitae_style::ObjectFit::Fill,
                img_w: 4.0,
                img_h: 4.0,
                transform: None,
                clip: None,
            }],
            glyphs: vec![crate::GlyphQuad {
                x: 70.0,
                y: 30.0,
                size: 16.0,
                color: 0x000000FF,
                key: crate::GlyphKey {
                    ch: 'g',
                    size_px: 16,
                    bold: false,
                    italic: false,
                    monospace: false,
                    family: 0,
                },
                transform: None,
                blur: 0.0,
            }],
            shadows: vec![crate::SceneShadow {
                x: 30.0,
                y: 150.0,
                w: 40.0,
                h: 30.0,
                color: 0x00000080,
                radius: [0.0; 4],
                blur: 6.0,
                spread: 1.0,
                offset_x: 2.0,
                offset_y: 3.0,
                inset: false,
                transform: None,
                clip: None,
            }],
            ..GpuScene::default()
        };
        (scene, rasters, atlas)
    }

    #[test]
    fn tiles_assemble_bit_exact() {
        let (scene, rasters, atlas) = sample_scene();
        let expect = cpu::render(&scene, 128, 192, 1.0, 0.0, 0.0, &rasters, &atlas);
        let mut cache = TileCache::new(64);
        let got = cache.render(&scene, 128, 192, 1.0, 0.0, 0.0, &rasters, &atlas);
        assert_eq!(expect, got, "tile assembly must equal the surface render");
        assert_eq!((cache.hits, cache.misses), (0, 6));
        // Same scene again: every tile hits, pixels identical.
        let got2 = cache.render(&scene, 128, 192, 1.0, 0.0, 0.0, &rasters, &atlas);
        assert_eq!(got2, expect);
        assert_eq!((cache.hits, cache.misses), (6, 6));
    }

    #[test]
    fn dirty_rect_repaints_only_touched_tiles() {
        let build = |x: f32| {
            let (mut scene, rasters, atlas) = sample_scene();
            // Fully inside tile row 0 (y 32..56) so the horizontal move only
            // touches the top tile row.
            scene.rects[0].y = 32.0;
            scene.rects[0].x = x;
            (scene, rasters, atlas)
        };
        let (a, rasters, atlas) = build(8.0);
        let (b, _, _) = build(80.0);
        let expect_b = cpu::render(&b, 128, 128, 1.0, 0.0, 0.0, &rasters, &atlas);
        let mut cache = TileCache::new(64);
        cache.render(&a, 128, 128, 1.0, 0.0, 0.0, &rasters, &atlas);
        let (h0, m0) = (cache.hits, cache.misses);
        assert_eq!((h0, m0), (0, 4));
        // The rect moves from tile (0,0) (x 8..32) to tile (1,0) (x 80..104):
        // exactly those two tiles re-render; the bottom row stays cached.
        let got = cache.render(&b, 128, 128, 1.0, 0.0, 0.0, &rasters, &atlas);
        assert_eq!(got, expect_b);
        assert_eq!((cache.hits, cache.misses), (2, 6));
    }

    #[test]
    fn sticky_tiles_match_surface_render_at_scroll() {
        let (mut scene, rasters, atlas) = sample_scene();
        scene.rects[0] = crate::SceneRect {
            x: 20.0,
            y: 60.0,
            w: 24.0,
            h: 30.0,
            color: 0xFF0000FF,
            radius: [0.0; 4],
            paint: crate::ScenePaint::Solid,
            transform: None,
            clip: None,
        };
        scene.stickies.push(crate::SceneSticky {
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
        });
        let expect = cpu::render(&scene, 64, 200, 1.0, 0.0, 100.0, &rasters, &atlas);
        let mut cache = TileCache::new(64);
        let got = cache.render(&scene, 64, 200, 1.0, 0.0, 100.0, &rasters, &atlas);
        assert_eq!(expect, got, "sticky tiles must match the surface render");
    }

    #[test]
    fn scale_change_regrids_and_stays_exact() {
        let (scene, rasters, atlas) = sample_scene();
        let expect = cpu::render(&scene, 256, 384, 2.0, 0.0, 0.0, &rasters, &atlas);
        let mut cache = TileCache::new(64);
        let got = cache.render(&scene, 256, 384, 2.0, 0.0, 0.0, &rasters, &atlas);
        assert_eq!(got.len(), expect.len());
        // At 2x the per-tile origins are still integral CSS px, so assembly
        // matches the surface render byte-for-byte here too. 2×3 = 6 tiles.
        assert_eq!(got, expect);
        assert_eq!(cache.misses, 6);
    }
}
