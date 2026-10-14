//! wgpu backend (Phase L): real Metal-over-wgpu painting of [`GpuScene`].
//!
//! Pipelines: instanced solid quads (per-corner-radius SDF + up-to-4-stop
//! gradients in-shader) and instanced textured quads (image rasters, glyph
//! atlas pages; bilinear). Rendering is offscreen to a texture with CPU
//! readback — the Swift side presents the pixels through a `CAMetalLayer`
//! (L5); headless environments simply get [`GpuError::NoAdapter`] and the
//! golden tests skip (plan-sanctioned "where available").

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use kitae_media::RgbaImage;
use wgpu::util::DeviceExt;

use crate::{GlyphAtlas, GpuScene};

/// Maximum gradient stops the GPU pipeline interpolates per quad (the CPU
/// reference is exact; golden scenes stay within the cap).
pub const GPU_MAX_STOPS: usize = 4;

#[derive(Debug)]
pub enum GpuError {
    NoAdapter,
    Device(String),
    Queue(String),
    Map(String),
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpuError::NoAdapter => write!(f, "no wgpu adapter (headless environment?)"),
            GpuError::Device(e) => write!(f, "wgpu device: {e}"),
            GpuError::Queue(e) => write!(f, "wgpu queue: {e}"),
            GpuError::Map(e) => write!(f, "wgpu buffer map: {e}"),
        }
    }
}

impl std::error::Error for GpuError {}

/// Instance layout mirrors the WGSL attribute locations 0..15 below.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct SolidInstance {
    pos: [f32; 2],      // loc 0
    size: [f32; 2],     // loc 1
    color: [f32; 4],    // loc 2
    radius: [f32; 4],   // loc 3
    paint: [f32; 4],    // loc 4: (tag, angle_deg, stop_count, unused)
    offs: [f32; 4],     // loc 5: stop offsets
    col0: [f32; 4],     // loc 6
    col1: [f32; 4],     // loc 7
    col2: [f32; 4],     // loc 8
    col3: [f32; 4],     // loc 9
    radial: [f32; 4],   // loc 10: (cx, cy fractions, radius_px or -1, unused)
    viewport: [f32; 2], // loc 11: render target size (clip-space math)
    // Q2 affine warp of the quad's vertex positions (device space). `mat0.w`
    // is the flag: 0 = identity (bit-exact legacy path), 1 = warp
    // p' = [m0 m1 m2; m3 m4 m5]·p. `local`/UV varyings stay unwarped so the
    // SDF, gradients and texture sampling evaluate in box-local space.
    mat0: [f32; 4], // loc 12: (m0, m1, m2, flag)
    mat1: [f32; 4], // loc 13: (m3, m4, m5, unused)
    // E8 clip in box-local px: clip_a = (kind, a, b, c) — kind 0 none,
    // 1 circle(cx,cy,r), 2 ellipse(cx,cy,rx | ry = clip_b.x), 3 inset(x,y,w
    // | h = clip_b.x, uniform radius = clip_b.y; the CPU reference honors
    // per-corner radii — documented deviation). Polygons stay CPU-only.
    clip_a: [f32; 4], // loc 14
    clip_b: [f32; 4], // loc 15
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct TexInstance {
    pos: [f32; 2],      // loc 0
    size: [f32; 2],     // loc 1
    uv_pos: [f32; 2],   // loc 2
    uv_size: [f32; 2],  // loc 3
    tint: [f32; 4],     // loc 4
    viewport: [f32; 2], // loc 5
    mat0: [f32; 4],     // loc 6: Q2 warp (m0, m1, m2, flag) — same as SolidInstance
    mat1: [f32; 4],     // loc 7: (m3, m4, m5, unused)
    clip_a: [f32; 4],   // loc 8: E8 clip, same encoding as SolidInstance
    clip_b: [f32; 4],   // loc 9
}

const SOLID_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4f,
    @location(0) color: vec4f,
    @location(1) local: vec2f,
    @location(2) size: vec2f,
    @location(3) radius: vec4f,
    @location(4) paint: vec4f,
    @location(5) offs: vec4f,
    @location(6) col0: vec4f,
    @location(7) col1: vec4f,
    @location(8) col2: vec4f,
    @location(9) col3: vec4f,
    @location(10) radial: vec4f,
    @location(11) clip_a: vec4f,
    @location(12) clip_b: vec4f,
};
@vertex
fn vs(@builtin(vertex_index) vi: u32,
      @location(0) ipos: vec2f, @location(1) isize: vec2f,
      @location(2) icol: vec4f, @location(3) irad: vec4f,
      @location(4) ipaint: vec4f, @location(5) ioffs: vec4f,
      @location(6) icol0: vec4f, @location(7) icol1: vec4f,
      @location(8) icol2: vec4f, @location(9) icol3: vec4f,
      @location(10) iradial: vec4f,
      @location(11) viewport: vec2f,
      @location(12) imat0: vec4f, @location(13) imat1: vec4f,
      @location(14) iclip_a: vec4f, @location(15) iclip_b: vec4f) -> VsOut {
    var corners = array<vec2f, 6>(
        vec2f(0.0, 0.0), vec2f(1.0, 0.0), vec2f(0.0, 1.0),
        vec2f(0.0, 1.0), vec2f(1.0, 0.0), vec2f(1.0, 1.0));
    let uv = corners[vi];
    let local = uv * isize;
    // Q2: warp the device-space position when the instance carries an
    // affine matrix; varyings stay in unwarped local space so the SDF,
    // gradients and radius math evaluate exactly as in the identity path
    // (affine maps preserve their interpolation across the quad).
    var p = ipos + local;
    if (imat0.w > 0.5) {
        p = vec2f(imat0.x * p.x + imat0.y * p.y + imat0.z,
                  imat1.x * p.x + imat1.y * p.y + imat1.z);
    }
    // y-down page space → y-up clip space.
    let clip = vec2f(p.x / viewport.x * 2.0 - 1.0, 1.0 - p.y / viewport.y * 2.0);
    var out: VsOut;
    out.pos = vec4f(clip, 0.0, 1.0);
    out.color = icol;
    out.local = local;
    out.size = isize;
    out.radius = irad;
    out.paint = ipaint;
    out.offs = ioffs;
    out.col0 = icol0;
    out.col1 = icol1;
    out.col2 = icol2;
    out.col3 = icol3;
    out.radial = iradial;
    out.clip_a = iclip_a;
    out.clip_b = iclip_b;
    return out;
}
fn grad_color(offs: vec4f, c0: vec4f, c1: vec4f, c2: vec4f, c3: vec4f,
              n: i32, t: f32, fallback: vec4f) -> vec4f {
    if (n <= 0) { return fallback; }
    var cols = array<vec4f, 4>(c0, c1, c2, c3);
    var oo = array<f32, 4>(offs.x, offs.y, offs.z, offs.w);
    if (t <= oo[0]) { return cols[0]; }
    for (var i = 1; i < 4; i++) {
        if (i >= n) { break; }
        if (t <= oo[i]) {
            let span = oo[i] - oo[i - 1];
            let k = select((t - oo[i - 1]) / max(span, 1e-5), 1.0, span <= 0.0);
            return mix(cols[i - 1], cols[i], k);
        }
    }
    return cols[n - 1];
}
// E8: hard discard outside the clip shape (box-local px, CPU-translated).
// Same membership rules as SceneClip::contains — circle/ellipse exact,
// inset with a uniform corner radius (per-corner radii are CPU-only).
fn clip_reject(local: vec2f, ca: vec4f, cb: vec4f) {
    if (ca.x < 0.5) { return; }
    if (ca.x < 1.5) {
        let dx = local.x - ca.y;
        let dy = local.y - ca.z;
        if (dx * dx + dy * dy > ca.w * ca.w) { discard; }
    } else if (ca.x < 2.5) {
        let dx = (local.x - ca.y) / max(ca.w, 1e-5);
        let dy = (local.y - ca.z) / max(cb.x, 1e-5);
        if (dx * dx + dy * dy > 1.0) { discard; }
    } else {
        if (local.x < ca.y || local.x > ca.y + ca.w
            || local.y < ca.z || local.y > ca.z + cb.x) {
            discard;
        }
        let r = min(cb.y, min(ca.w, cb.x) * 0.5);
        if (r > 0.0) {
            let left = local.x < ca.y + ca.w * 0.5;
            let top = local.y < ca.z + cb.x * 0.5;
            var ccx: f32;
            var ccy: f32;
            if (left && top) { ccx = ca.y + r; ccy = ca.z + r; }
            else if (!left && top) { ccx = ca.y + ca.w - r; ccy = ca.z + r; }
            else if (left && !top) { ccx = ca.y + r; ccy = ca.z + cb.x - r; }
            else { ccx = ca.y + ca.w - r; ccy = ca.z + cb.x - r; }
            let ox = select(ccx - local.x, local.x - ccx, !left);
            let oy = select(ccy - local.y, local.y - ccy, !top);
            if (ox > 0.0 && oy > 0.0 && length(vec2f(ox, oy)) > r) { discard; }
        }
    }
}
@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
    // Rounded-box mask, 1px AA.
    let m = min(in.size.x, in.size.y) * 0.5;
    let rr = min(in.radius, vec4f(m));
    let left = in.local.x < in.size.x * 0.5;
    let top = in.local.y < in.size.y * 0.5;
    var cc: vec2f;
    var rad: f32;
    if (left && top) { cc = vec2f(rr.x); rad = rr.x; }
    else if (!left && top) { cc = vec2f(in.size.x - rr.y, rr.y); rad = rr.y; }
    else if (left && !top) { cc = vec2f(rr.w, in.size.y - rr.w); rad = rr.w; }
    else { cc = in.size - vec2f(rr.z); rad = rr.z; }
    var d: f32;
    let ox = select(cc.x - in.local.x, in.local.x - cc.x, !left);
    let oy = select(cc.y - in.local.y, in.local.y - cc.y, !top);
    if (ox > 0.0 && oy > 0.0) {
        d = length(vec2f(ox, oy)) - rad;
    } else {
        d = max(max(-in.local.x, in.local.x - in.size.x),
                max(-in.local.y, in.local.y - in.size.y));
    }
    let aa = fwidth(d) * 0.75 + 1e-4;
    // Square boxes skip AA (exact edge match with the CPU reference, which
    // fills square boxes hard); rounded outlines keep 1px smoothing.
    let maxr = max(max(rr.x, rr.y), max(rr.z, rr.w));
    let cover = select(1.0 - smoothstep(-aa, aa, d), 1.0, maxr <= 0.0);
    if (cover <= 0.001) { discard; }
    clip_reject(in.local, in.clip_a, in.clip_b);
    var col = in.color;
    let tag = i32(in.paint.x);
    let n = i32(in.paint.z);
    if (tag == 1) {
        let rad_ang = in.paint.y * 3.14159265 / 180.0;
        let dir = vec2f(sin(rad_ang), -cos(rad_ang));
        // Extent over the box-local corners (the CPU reference anchors the
        // same extent at the box origin, so offset boxes agree exactly).
        let d10 = in.size.x * dir.x;
        let d01 = in.size.y * dir.y;
        let mn = min(min(0.0, d10), min(d01, d10 + d01));
        let mx = max(max(0.0, d10), max(d01, d10 + d01));
        let t = (dot(in.local, dir) - mn) / max(mx - mn, 1e-5);
        col = grad_color(in.offs, in.col0, in.col1, in.col2, in.col3, n,
                         clamp(t, 0.0, 1.0), in.color);
    } else if (tag == 2) {
        let center = vec2f(in.radial.x, in.radial.y) * in.size;
        let dx = max(center.x, in.size.x - center.x);
        let dy = max(center.y, in.size.y - center.y);
        let far = length(vec2f(dx, dy));
        let rmax = select(far, in.radial.z, in.radial.z >= 0.0);
        let t = length(in.local - center) / max(rmax, 1e-5);
        col = grad_color(in.offs, in.col0, in.col1, in.col2, in.col3, n,
                         clamp(t, 0.0, 1.0), in.color);
    }
    col.a *= cover;
    return col;
}
"#;

const TEX_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) tint: vec4f,
    @location(2) local: vec2f,
    @location(3) clip_a: vec4f,
    @location(4) clip_b: vec4f,
};
@vertex
fn vs(@builtin(vertex_index) vi: u32,
      @location(0) ipos: vec2f, @location(1) isize: vec2f,
      @location(2) iuv: vec2f, @location(3) iuvsize: vec2f,
      @location(4) itint: vec4f,
      @location(5) viewport: vec2f,
      @location(6) imat0: vec4f, @location(7) imat1: vec4f,
      @location(8) iclip_a: vec4f, @location(9) iclip_b: vec4f) -> VsOut {
    var corners = array<vec2f, 6>(
        vec2f(0.0, 0.0), vec2f(1.0, 0.0), vec2f(0.0, 1.0),
        vec2f(0.0, 1.0), vec2f(1.0, 0.0), vec2f(1.0, 1.0));
    let uv = corners[vi];
    // Q2: warp the position, keep the UV unwarped — texture sampling stays
    // in the box-local mapping exactly as the CPU reference samples it.
    var p = ipos + uv * isize;
    if (imat0.w > 0.5) {
        p = vec2f(imat0.x * p.x + imat0.y * p.y + imat0.z,
                  imat1.x * p.x + imat1.y * p.y + imat1.z);
    }
    var out: VsOut;
    out.pos = vec4f(p.x / viewport.x * 2.0 - 1.0, 1.0 - p.y / viewport.y * 2.0, 0.0, 1.0);
    out.uv = iuv + uv * iuvsize;
    out.local = uv * isize;
    out.tint = itint;
    out.clip_a = iclip_a;
    out.clip_b = iclip_b;
    return out;
}
@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex: texture_2d<f32>;
// E8 clip discard — same helper body as the solid shader (copied; WGSL has
// no cross-module sharing). Runs after the sample so textureSample stays
// in uniform control flow.
fn clip_reject(local: vec2f, ca: vec4f, cb: vec4f) {
    if (ca.x < 0.5) { return; }
    if (ca.x < 1.5) {
        let dx = local.x - ca.y;
        let dy = local.y - ca.z;
        if (dx * dx + dy * dy > ca.w * ca.w) { discard; }
    } else if (ca.x < 2.5) {
        let dx = (local.x - ca.y) / max(ca.w, 1e-5);
        let dy = (local.y - ca.z) / max(cb.x, 1e-5);
        if (dx * dx + dy * dy > 1.0) { discard; }
    } else {
        if (local.x < ca.y || local.x > ca.y + ca.w
            || local.y < ca.z || local.y > ca.z + cb.x) {
            discard;
        }
        let r = min(cb.y, min(ca.w, cb.x) * 0.5);
        if (r > 0.0) {
            let left = local.x < ca.y + ca.w * 0.5;
            let top = local.y < ca.z + cb.x * 0.5;
            var ccx: f32;
            var ccy: f32;
            if (left && top) { ccx = ca.y + r; ccy = ca.z + r; }
            else if (!left && top) { ccx = ca.y + ca.w - r; ccy = ca.z + r; }
            else if (left && !top) { ccx = ca.y + r; ccy = ca.z + cb.x - r; }
            else { ccx = ca.y + ca.w - r; ccy = ca.z + cb.x - r; }
            let ox = select(ccx - local.x, local.x - ccx, !left);
            let oy = select(ccy - local.y, local.y - ccy, !top);
            if (ox > 0.0 && oy > 0.0 && length(vec2f(ox, oy)) > r) { discard; }
        }
    }
}
@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
    let sampled = textureSample(tex, samp, in.uv) * in.tint;
    clip_reject(in.local, in.clip_a, in.clip_b);
    return sampled;
}
"#;

fn rgba_to_linear(c: u32) -> [f32; 4] {
    [
        ((c >> 24) & 0xFF) as f32 / 255.0,
        ((c >> 16) & 0xFF) as f32 / 255.0,
        ((c >> 8) & 0xFF) as f32 / 255.0,
        (c & 0xFF) as f32 / 255.0,
    ]
}

pub struct GpuContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
    solid_pipeline: wgpu::RenderPipeline,
    tex_pipeline: wgpu::RenderPipeline,
    tex_layout: wgpu::BindGroupLayout,
}

impl GpuContext {
    /// Headless device bring-up (no surface): `NoAdapter` where Metal (or
    /// any backend) is unavailable — CI without a GPU, for example.
    pub fn new() -> Result<GpuContext, GpuError> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok_or(GpuError::NoAdapter)?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("kitae"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .map_err(|e| GpuError::Device(e.to_string()))?;

        let solid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kitae-solid"),
            source: wgpu::ShaderSource::Wgsl(SOLID_WGSL.into()),
        });
        let solid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kitae-solid-pipe"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &solid_shader,
                entry_point: Some("vs"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<SolidInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32x4,
                        4 => Float32x4, 5 => Float32x4, 6 => Float32x4, 7 => Float32x4,
                        8 => Float32x4, 9 => Float32x4, 10 => Float32x4, 11 => Float32x2,
                        12 => Float32x4, 13 => Float32x4, 14 => Float32x4, 15 => Float32x4,
                    ],
                }],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &solid_shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview: None,
            cache: None,
        });

        let tex_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kitae-tex"),
            source: wgpu::ShaderSource::Wgsl(TEX_WGSL.into()),
        });
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kitae-tex-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let tex_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kitae-tex-layout-pipe"),
            bind_group_layouts: &[&tex_layout],
            push_constant_ranges: &[],
        });
        let tex_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kitae-tex-pipe"),
            layout: Some(&tex_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &tex_shader,
                entry_point: Some("vs"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<TexInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x2, 2 => Float32x2,
                        3 => Float32x2, 4 => Float32x4, 5 => Float32x2,
                        6 => Float32x4, 7 => Float32x4, 8 => Float32x4, 9 => Float32x4,
                    ],
                }],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &tex_shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview: None,
            cache: None,
        });

        Ok(GpuContext {
            device,
            queue,
            solid_pipeline,
            tex_pipeline,
            tex_layout,
        })
    }

    fn upload_instances<T: Pod>(&self, label: &str, data: &[T]) -> wgpu::Buffer {
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytemuck::cast_slice(data),
                usage: wgpu::BufferUsages::VERTEX,
            })
    }

    fn upload_texture(&self, label: &str, w: u32, h: u32, rgba: &[u8]) -> wgpu::Texture {
        let pitch = align_up(w * 4, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let mut padded = vec![0u8; (pitch * h) as usize];
        for y in 0..h {
            padded[(y * pitch) as usize..(y * pitch + w * 4) as usize]
                .copy_from_slice(&rgba[(y * w * 4) as usize..((y + 1) * w * 4) as usize]);
        }
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &padded,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(pitch),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        tex
    }

    /// Render `scene` into a `w`×`h` device-px tile. `scale` maps CSS px to
    /// device px; (`ox`, `oy`) is the page-space CSS origin of the tile's
    /// top-left (the present loop renders only the visible window; it also
    /// doubles as the scroll offset for E2 sticky items).
    /// `rasters` supplies image pixels by payload id; `atlas` supplies glyph
    /// bitmaps. Returns straight-alpha RGBA8. Shadows render as translucent
    /// offset shells (outset; inset renders as a solid inner shell — exact
    /// softness lives in the CPU reference, the golden oracle). Glyph blur
    /// is a 5-copy approximation; LCD subpixel AA is CPU-only (grayscale
    /// here); clips are hard discards with uniform-radius insets.
    pub fn render(
        &self,
        scene: &GpuScene,
        w: u32,
        h: u32,
        scale: f32,
        ox: f32,
        oy: f32,
        rasters: &HashMap<u64, Arc<RgbaImage>>,
        atlas: &GlyphAtlas,
    ) -> Result<Vec<u8>, GpuError> {
        let w = w.max(1);
        let h = h.max(1);
        let viewport = [w as f32, h as f32];
        let px = |v: f32, o: f32| (v - o) * scale;
        // E2: sticky items move as a unit at this tile's scroll offset —
        // the same per-item shift vectors the CPU reference computes.
        let (rect_shift, glyph_shift, image_shift, shadow_shift) =
            crate::cpu::sticky_shifts(scene, oy, h as f32 / scale.max(0.001));
        let mut solids: Vec<SolidInstance> = Vec::new();
        for (i, r) in scene.rects.iter().enumerate() {
            let dy = rect_shift[i] * scale;
            // Untransformed stickies shift the quad position (the identity
            // fast path stays intact); transformed ones fold the shift into
            // the warp matrix's device translation — same picture either way.
            let mut y = px(r.y, oy);
            if dy != 0.0 && r.transform.is_none() {
                y += dy;
            }
            let mut inst = solid_instance(
                px(r.x, ox),
                y,
                r.w * scale,
                r.h * scale,
                r.color,
                [
                    r.radius[0] * scale,
                    r.radius[1] * scale,
                    r.radius[2] * scale,
                    r.radius[3] * scale,
                ],
                &r.paint,
            );
            inst.viewport = viewport;
            (inst.mat0, inst.mat1) = matrix_cols(r.transform.as_ref(), scale, ox, oy);
            if dy != 0.0 && r.transform.is_some() {
                inst.mat1[2] += dy;
            }
            (inst.clip_a, inst.clip_b) = clip_attrs(r.clip.as_ref(), (r.x, r.y), scale);
            solids.push(inst);
        }
        for (si, s) in scene.shadows.iter().enumerate() {
            if s.inset {
                continue;
            }
            let dy = shadow_shift[si] * scale;
            let layers = 4u32;
            for i in 1..=layers {
                let k = i as f32 / layers as f32;
                let expand = (s.spread + s.blur * k) * scale;
                let a = (((s.color & 0xFF) as f32) / 255.0) * (1.0 - k) / layers as f32 * 2.0;
                if a <= 0.004 {
                    continue;
                }
                let faded = (s.color & 0xFFFFFF00) | ((a * 255.0 + 0.5) as u32);
                let mut inst = solid_instance(
                    px(s.x + s.offset_x, ox) - expand,
                    px(s.y + s.offset_y, oy) - expand,
                    (s.w * scale + expand * 2.0).max(0.0),
                    (s.h * scale + expand * 2.0).max(0.0),
                    faded,
                    [
                        s.radius[0] * scale,
                        s.radius[1] * scale,
                        s.radius[2] * scale,
                        s.radius[3] * scale,
                    ],
                    &crate::ScenePaint::Solid,
                );
                inst.viewport = viewport;
                // The shell is built around box+offset in page space, then
                // the whole shell warps with the shadow's matrix. GPU shells
                // ignore clips (CPU-only precision there — documented).
                (inst.mat0, inst.mat1) = matrix_cols(s.transform.as_ref(), scale, ox, oy);
                if dy != 0.0 {
                    if s.transform.is_none() {
                        inst.pos[1] += dy;
                    } else {
                        inst.mat1[2] += dy;
                    }
                }
                solids.push(inst);
            }
        }

        let mut tex_draws: Vec<(wgpu::Texture, Vec<TexInstance>)> = Vec::new();
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("kitae-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let mut by_raster: HashMap<u64, Vec<TexInstance>> = HashMap::new();
        for (i, im) in scene.images.iter().enumerate() {
            let Some(raster) = rasters.get(&im.payload) else {
                continue;
            };
            if raster.width == 0 || raster.height == 0 {
                continue;
            }
            let fr = kitae_media::object_fit_rect(
                im.w,
                im.h,
                raster.width as f32,
                raster.height as f32,
                im.fit,
            );
            let (tw, th) = (raster.width as f32, raster.height as f32);
            let (m0, mut m1) = matrix_cols(im.transform.as_ref(), scale, ox, oy);
            let dy = image_shift[i] * scale;
            let mut py = px(im.y + fr.dst_y, oy);
            if dy != 0.0 {
                if im.transform.is_none() {
                    py += dy;
                } else {
                    m1[2] += dy;
                }
            }
            // The clip rides in the quad's box-local frame, whose origin is
            // the object-fit destination rect, not the layout box.
            let (ca, cb) = clip_attrs(im.clip.as_ref(), (im.x + fr.dst_x, im.y + fr.dst_y), scale);
            by_raster.entry(im.payload).or_default().push(TexInstance {
                pos: [px(im.x + fr.dst_x, ox), py],
                size: [fr.dst_w.max(0.0) * scale, fr.dst_h.max(0.0) * scale],
                uv_pos: [fr.src_x / tw, fr.src_y / th],
                uv_size: [fr.src_w / tw, fr.src_h / th],
                tint: [1.0, 1.0, 1.0, 1.0],
                viewport,
                mat0: m0,
                mat1: m1,
                clip_a: ca,
                clip_b: cb,
            });
        }
        for (payload, instances) in &by_raster {
            let raster = &rasters[payload];
            let tex = self.upload_texture("kitae-img", raster.width, raster.height, &raster.pixels);
            tex_draws.push((tex, instances.clone()));
        }
        let mut by_page: HashMap<usize, Vec<TexInstance>> = HashMap::new();
        // E6: LCD subpixel AA is a CPU-reference feature — the GPU samples
        // the atlas grayscale regardless of scene.lcd_text (documented).
        for (i, g) in scene.glyphs.iter().enumerate() {
            let Some(loc) = atlas.lookup(&g.key) else {
                continue;
            };
            let Some((pw, ph, _)) = atlas.page_pixels(loc.page) else {
                continue;
            };
            let tint = rgba_to_linear(g.color);
            // Half-texel UV inset: bilinear edge samples must not bleed
            // into neighboring atlas shelves.
            let (pwf, phf) = (pw as f32, ph as f32);
            let (m0, mut m1) = matrix_cols(g.transform.as_ref(), scale, ox, oy);
            let dy = glyph_shift[i] * scale;
            let gx = px(g.x, ox);
            let mut gy = px(g.y - loc.h as f32, oy);
            if dy != 0.0 {
                if g.transform.is_none() {
                    gy += dy;
                } else {
                    m1[2] += dy;
                }
            }
            // E3: blurred glyphs approximate the CPU box-blur with a center
            // copy at half strength plus four axis offsets at eighth
            // strength (energy ≈ 1.0). Crisp glyphs stay a single full copy.
            let spots: Vec<([f32; 2], f32)> = if g.blur > 0.0 && g.blur * scale >= 1.0 {
                let r = ((g.blur * scale * 0.5).ceil() as i32).min(16) as f32;
                vec![
                    ([0.0, 0.0], 0.5),
                    ([r, 0.0], 0.125),
                    ([-r, 0.0], 0.125),
                    ([0.0, r], 0.125),
                    ([0.0, -r], 0.125),
                ]
            } else {
                vec![([0.0, 0.0], 1.0)]
            };
            for (off, k) in &spots {
                let mut t = tint;
                t[3] *= *k;
                by_page.entry(loc.page).or_default().push(TexInstance {
                    pos: [gx + off[0], gy + off[1]],
                    size: [loc.w as f32 * scale, loc.h as f32 * scale],
                    uv_pos: [(loc.x as f32 + 0.5) / pwf, (loc.y as f32 + 0.5) / phf],
                    uv_size: [
                        (loc.w as f32 - 1.0).max(0.0) / pwf,
                        (loc.h as f32 - 1.0).max(0.0) / phf,
                    ],
                    tint: t,
                    viewport,
                    mat0: m0,
                    mat1: m1,
                    clip_a: [0.0; 4],
                    clip_b: [0.0; 4],
                });
            }
        }
        for (page, instances) in &by_page {
            let Some((pw, ph, bits)) = atlas.page_pixels(*page) else {
                continue;
            };
            // r8 alpha → white RGBA (tint carries the color).
            let mut rgba = vec![0u8; (pw * ph * 4) as usize];
            for (i, &a) in bits.iter().enumerate() {
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, a]);
            }
            let tex = self.upload_texture("kitae-atlas", pw, ph, &rgba);
            tex_draws.push((tex, instances.clone()));
        }

        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("kitae-target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = rgba_to_linear(scene.bg);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("kitae-enc"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("kitae-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if !solids.is_empty() {
                let buf = self.upload_instances("kitae-solids", &solids);
                pass.set_pipeline(&self.solid_pipeline);
                pass.set_scissor_rect(0, 0, w, h);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..6, 0..solids.len() as u32);
            }
            for (tex, instances) in &tex_draws {
                if instances.is_empty() {
                    continue;
                }
                let view_tex = tex.create_view(&wgpu::TextureViewDescriptor::default());
                let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("kitae-tex-bind"),
                    layout: &self.tex_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Sampler(&sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&view_tex),
                        },
                    ],
                });
                let buf = self.upload_instances("kitae-texinst", instances);
                pass.set_pipeline(&self.tex_pipeline);
                pass.set_bind_group(0, &bind, &[]);
                pass.set_scissor_rect(0, 0, w, h);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..6, 0..instances.len() as u32);
            }
        }
        let pitch = align_up(w * 4, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kitae-readback"),
            size: (pitch * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(pitch),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|_| GpuError::Map("map callback dropped".into()))?
            .map_err(|e| GpuError::Map(format!("{e:?}")))?;
        let mapped = slice.get_mapped_range();
        let mut out = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            out[(y * w * 4) as usize..((y + 1) * w * 4) as usize]
                .copy_from_slice(&mapped[(y * pitch) as usize..(y * pitch + w * 4) as usize]);
        }
        drop(mapped);
        readback.unmap();
        Ok(out)
    }
}

/// Q2: pack a scene item's affine matrix into the two instance columns the
/// WGSL vertex shaders warp with (`flag` = 1). `None` leaves both columns
/// zeroed — flag 0 keeps the legacy bit-exact identity path.
fn matrix_cols(
    tf: Option<&kitae_layout::Matrix2d>,
    scale: f32,
    ox: f32,
    oy: f32,
) -> ([f32; 4], [f32; 4]) {
    match tf {
        None => ([0.0; 4], [0.0; 4]),
        Some(m) => {
            let d = crate::device_matrix(m, scale, ox, oy);
            ([d[0], d[1], d[2], 1.0], [d[3], d[4], d[5], 0.0])
        }
    }
}

/// E8: translate a page-space clip onto a quad's box-local device frame
/// (origin = the box's page-space top-left, lengths × scale). Circle and
/// ellipse match the CPU membership test exactly; inset collapses to a
/// uniform corner radius and polygons stay CPU-only (documented deviations).
fn clip_attrs(
    clip: Option<&crate::SceneClip>,
    origin: (f32, f32),
    scale: f32,
) -> ([f32; 4], [f32; 4]) {
    let Some(c) = clip else {
        return ([0.0; 4], [0.0; 4]);
    };
    let sx = |v: f32| (v - origin.0) * scale;
    let sy = |v: f32| (v - origin.1) * scale;
    match c {
        crate::SceneClip::Circle { cx, cy, r } => {
            ([1.0, sx(*cx), sy(*cy), r.max(0.0) * scale], [0.0; 4])
        }
        crate::SceneClip::Ellipse { cx, cy, rx, ry } => (
            [2.0, sx(*cx), sy(*cy), rx.max(0.0) * scale],
            [ry.max(0.0) * scale, 0.0, 0.0, 0.0],
        ),
        crate::SceneClip::Inset { x, y, w, h, radii } => {
            let rmin = radii
                .iter()
                .map(|(a, b)| a.min(*b))
                .fold(f32::MAX, f32::min)
                .max(0.0)
                * scale;
            (
                [3.0, sx(*x), sy(*y), w.max(0.0) * scale],
                [h.max(0.0) * scale, rmin, 0.0, 0.0],
            )
        }
        crate::SceneClip::Polygon { .. } => ([0.0; 4], [0.0; 4]),
    }
}

fn solid_instance(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: u32,
    radius: [f32; 4],
    paint: &crate::ScenePaint,
) -> SolidInstance {
    let mut inst = SolidInstance {
        pos: [x, y],
        size: [w.max(0.0), h.max(0.0)],
        color: rgba_to_linear(color),
        radius,
        paint: [0.0; 4],
        offs: [0.0; 4],
        col0: [0.0; 4],
        col1: [0.0; 4],
        col2: [0.0; 4],
        col3: [0.0; 4],
        radial: [0.5, 0.5, -1.0, 0.0],
        viewport: [0.0; 2],
        mat0: [0.0; 4],
        mat1: [0.0; 4],
        clip_a: [0.0; 4],
        clip_b: [0.0; 4],
    };
    match paint {
        crate::ScenePaint::Solid => {}
        crate::ScenePaint::Linear { angle_deg, stops } => {
            inst.paint = [1.0, *angle_deg, stops.len().min(GPU_MAX_STOPS) as f32, 0.0];
            fill_paint(&mut inst, stops);
        }
        crate::ScenePaint::Radial {
            cx,
            cy,
            radius,
            stops,
        } => {
            inst.paint = [2.0, 0.0, stops.len().min(GPU_MAX_STOPS) as f32, 0.0];
            inst.radial = [*cx, *cy, radius.unwrap_or(-1.0), 0.0];
            fill_paint(&mut inst, stops);
        }
    }
    inst
}

fn fill_paint(inst: &mut SolidInstance, stops: &[(f32, u32)]) {
    let cols = [
        &mut inst.col0,
        &mut inst.col1,
        &mut inst.col2,
        &mut inst.col3,
    ];
    for (i, (offset, color)) in stops.iter().take(GPU_MAX_STOPS).enumerate() {
        inst.offs[i] = *offset;
        *cols[i] = rgba_to_linear(*color);
    }
}

fn align_up(v: u32, align: u32) -> u32 {
    v.div_ceil(align) * align
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu;

    fn test_scene() -> (GpuScene, HashMap<u64, Arc<RgbaImage>>, GlyphAtlas) {
        // Square corners here (rounded-corner AA differs subtly between
        // backends by design; CPU-only tests pin the rounded math exactly).
        let mut atlas = GlyphAtlas::new();
        atlas
            .upload(
                crate::GlyphKey {
                    ch: 'a',
                    size_px: 16,
                    bold: false,
                    italic: false,
                    monospace: false,
                    family: 0,
                },
                5,
                7,
                &vec![255u8; 35],
            )
            .expect("atlas upload");
        let mut rasters = HashMap::new();
        rasters.insert(
            7u64,
            Arc::new(kitae_media::RgbaImage::solid(4, 4, [0, 128, 255, 255])),
        );
        let scene = GpuScene {
            w: 64,
            h: 64,
            bg: 0xFFFFFFFF,
            rects: vec![
                crate::SceneRect {
                    x: 4.0,
                    y: 4.0,
                    w: 24.0,
                    h: 24.0,
                    color: 0xFF0000FF,
                    radius: [0.0; 4],
                    paint: crate::ScenePaint::Solid,
                    transform: None,
                    clip: None,
                },
                crate::SceneRect {
                    x: 32.0,
                    y: 4.0,
                    w: 28.0,
                    h: 24.0,
                    color: 0xFF0000FF,
                    radius: [0.0; 4],
                    paint: crate::ScenePaint::Linear {
                        angle_deg: 90.0,
                        stops: vec![(0.0, 0xFF0000FF), (1.0, 0x0000FFFF)],
                    },
                    transform: None,
                    clip: None,
                },
            ],
            images: vec![crate::SceneImage {
                x: 4.0,
                y: 32.0,
                w: 16.0,
                h: 16.0,
                payload: 7,
                fit: kitae_style::ObjectFit::Fill,
                img_w: 4.0,
                img_h: 4.0,
                transform: None,
                clip: None,
            }],
            glyphs: vec![crate::GlyphQuad {
                x: 32.0,
                y: 48.0,
                size: 16.0,
                color: 0x000000FF,
                key: crate::GlyphKey {
                    ch: 'a',
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
        (scene, rasters, atlas)
    }

    fn compare(name: &str, a: &[u8], b: &[u8]) {
        assert_eq!(a.len(), b.len(), "{name}: length");
        let mut bad = 0usize;
        let mut worst = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            let d = x.abs_diff(*y);
            worst = worst.max(d);
            if d > 2 {
                bad += 1;
            }
        }
        let frac = bad as f64 / a.len() as f64;
        assert!(
            frac < 0.01,
            "{name}: {frac:.4} of bytes differ by >2 (worst {worst})"
        );
    }

    #[test]
    fn gpu_matches_cpu_reference_where_available() {
        let (scene, rasters, atlas) = test_scene();
        let expected = cpu::render(&scene, 64, 64, 1.0, 0.0, 0.0, &rasters, &atlas);
        let ctx = match GpuContext::new() {
            Ok(ctx) => ctx,
            Err(GpuError::NoAdapter) => {
                eprintln!("SKIP gpu_matches_cpu_reference: no adapter in this environment");
                return;
            }
            Err(e) => panic!("GPU bring-up failed: {e}"),
        };
        let got = ctx
            .render(&scene, 64, 64, 1.0, 0.0, 0.0, &rasters, &atlas)
            .expect("gpu render");
        compare("gpu-vs-cpu", &expected, &got);
        // Scaled tile (2x, offset origin) exercises the present-loop path.
        let expected_tile = cpu::render(&scene, 32, 32, 2.0, 8.0, 8.0, &rasters, &atlas);
        let got_tile = ctx
            .render(&scene, 32, 32, 2.0, 8.0, 8.0, &rasters, &atlas)
            .expect("gpu tile render");
        compare("gpu-vs-cpu-tile", &expected_tile, &got_tile);
    }

    /// Q2: CPU↔GPU parity under the affine warp. 90° about each box's own
    /// center keeps edges axis-aligned at integer coordinates — both
    /// backends then fill exactly the same pixel centers and the
    /// whole-buffer compare stays a hard gate (no AA tolerance needed; the
    /// vertex shader takes the same warp path for any angle).
    #[test]
    fn gpu_matches_cpu_on_rotated_items() {
        let rot = |cx: f32, cy: f32| {
            // rotate(90°) about (cx, cy): x' = (cx+cy) - y, y' = x + (cy-cx).
            kitae_layout::Matrix2d([0.0, -1.0, cx + cy, 1.0, 0.0, cy - cx])
        };
        let mut rasters = HashMap::new();
        rasters.insert(
            7u64,
            Arc::new(kitae_media::RgbaImage::solid(4, 4, [0, 128, 255, 255])),
        );
        let scene = GpuScene {
            w: 64,
            h: 64,
            bg: 0xFFFFFFFF,
            rects: vec![crate::SceneRect {
                x: 8.0,
                y: 16.0,
                w: 32.0,
                h: 20.0,
                color: 0xFF0000FF,
                radius: [0.0; 4],
                paint: crate::ScenePaint::Solid,
                transform: Some(rot(24.0, 26.0)),
                clip: None,
            }],
            images: vec![crate::SceneImage {
                x: 8.0,
                y: 44.0,
                w: 16.0,
                h: 8.0,
                payload: 7,
                fit: kitae_style::ObjectFit::Fill,
                img_w: 4.0,
                img_h: 4.0,
                transform: Some(rot(16.0, 48.0)),
                clip: None,
            }],
            ..GpuScene::default()
        };
        let at = |buf: &[u8], x: u32, y: u32| {
            let i = ((y * 64 + x) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };
        let expected = cpu::render(&scene, 64, 64, 1.0, 0.0, 0.0, &rasters, &GlyphAtlas::new());
        // The rect [8..40]×[16..36] rotates to [14..34]×[10..42]: the
        // original-only side strips go white, the rotated-only strips go red.
        let white = [255, 255, 255, 255];
        assert_eq!(at(&expected, 10, 20), white);
        assert_eq!(at(&expected, 36, 20), white);
        assert_eq!(at(&expected, 20, 12), [255, 0, 0, 255]);
        assert_eq!(at(&expected, 20, 38), [255, 0, 0, 255]);
        // The image [8..24]×[44..52] rotates to [12..20]×[40..56].
        assert_eq!(at(&expected, 14, 42), [0, 128, 255, 255]);
        assert_eq!(at(&expected, 22, 48), white);
        let ctx = match GpuContext::new() {
            Ok(ctx) => ctx,
            Err(GpuError::NoAdapter) => {
                eprintln!("SKIP gpu_matches_cpu_on_rotated_items: no adapter in this environment");
                return;
            }
            Err(e) => panic!("GPU bring-up failed: {e}"),
        };
        let got = ctx
            .render(&scene, 64, 64, 1.0, 0.0, 0.0, &rasters, &GlyphAtlas::new())
            .expect("gpu render");
        compare("gpu-vs-cpu-rotated", &expected, &got);
        assert_eq!(at(&got, 20, 12), [255, 0, 0, 255]);
        assert_eq!(at(&got, 14, 42), [0, 128, 255, 255]);
    }

    /// E2: sticky items shift with scroll on the GPU exactly as on the CPU.
    #[test]
    fn gpu_sticky_matches_cpu_at_scroll() {
        let scene = GpuScene {
            w: 32,
            h: 200,
            bg: 0xFFFFFFFF,
            rects: vec![crate::SceneRect {
                x: 4.0,
                y: 60.0,
                w: 24.0,
                h: 30.0,
                color: 0xFF0000FF,
                radius: [0.0; 4],
                paint: crate::ScenePaint::Solid,
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
        let empty: HashMap<u64, Arc<RgbaImage>> = HashMap::new();
        let ctx = match GpuContext::new() {
            Ok(ctx) => ctx,
            Err(GpuError::NoAdapter) => {
                eprintln!("SKIP gpu_sticky_matches_cpu_at_scroll: no adapter in this environment");
                return;
            }
            Err(e) => panic!("GPU bring-up failed: {e}"),
        };
        // Flow position at oy=0, pinned to top_offset=12 at oy=100.
        for oy in [0.0f32, 100.0] {
            let expected = cpu::render(&scene, 32, 200, 1.0, 0.0, oy, &empty, &GlyphAtlas::new());
            let got = ctx
                .render(&scene, 32, 200, 1.0, 0.0, oy, &empty, &GlyphAtlas::new())
                .expect("gpu render");
            compare("gpu-vs-cpu-sticky", &expected, &got);
        }
    }

    /// E8: the GPU discards outside a circle clip exactly like the CPU.
    #[test]
    fn gpu_clip_matches_cpu() {
        let scene = GpuScene {
            w: 32,
            h: 32,
            bg: 0xFFFFFFFF,
            rects: vec![crate::SceneRect {
                x: 4.0,
                y: 4.0,
                w: 24.0,
                h: 24.0,
                color: 0xFF0000FF,
                radius: [0.0; 4],
                paint: crate::ScenePaint::Solid,
                transform: None,
                clip: Some(crate::SceneClip::Circle {
                    cx: 16.0,
                    cy: 16.0,
                    r: 6.0,
                }),
            }],
            ..GpuScene::default()
        };
        let empty: HashMap<u64, Arc<RgbaImage>> = HashMap::new();
        let expected = cpu::render(&scene, 32, 32, 1.0, 0.0, 0.0, &empty, &GlyphAtlas::new());
        let ctx = match GpuContext::new() {
            Ok(ctx) => ctx,
            Err(GpuError::NoAdapter) => {
                eprintln!("SKIP gpu_clip_matches_cpu: no adapter in this environment");
                return;
            }
            Err(e) => panic!("GPU bring-up failed: {e}"),
        };
        let got = ctx
            .render(&scene, 32, 32, 1.0, 0.0, 0.0, &empty, &GlyphAtlas::new())
            .expect("gpu render");
        compare("gpu-vs-cpu-clip", &expected, &got);
        let at = |buf: &[u8], x: u32, y: u32| {
            let i = ((y * 32 + x) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };
        assert_eq!(at(&got, 16, 16), [255, 0, 0, 255]);
        assert_eq!(at(&got, 6, 6), [255, 255, 255, 255]);
    }

    /// E3: the GPU's 5-copy glyph blur spreads ink past the crisp footprint
    /// while the core stays dark (approximation — the CPU box-blur is the
    /// golden oracle).
    #[test]
    fn gpu_blurred_glyph_spreads_ink() {
        let key = crate::GlyphKey {
            ch: 'x',
            size_px: 16,
            bold: false,
            italic: false,
            monospace: false,
            family: 0,
        };
        let mut atlas = GlyphAtlas::new();
        atlas
            .upload(key, 3, 5, &vec![255u8; 15])
            .expect("atlas upload");
        let scene = GpuScene {
            w: 24,
            h: 16,
            bg: 0xFFFFFFFF,
            glyphs: vec![crate::GlyphQuad {
                x: 6.0,
                y: 10.0,
                size: 16.0,
                color: 0x0000FFFF,
                key,
                transform: None,
                blur: 4.0,
            }],
            ..GpuScene::default()
        };
        let ctx = match GpuContext::new() {
            Ok(ctx) => ctx,
            Err(GpuError::NoAdapter) => {
                eprintln!("SKIP gpu_blurred_glyph_spreads_ink: no adapter in this environment");
                return;
            }
            Err(e) => panic!("GPU bring-up failed: {e}"),
        };
        let got = ctx
            .render(&scene, 24, 16, 1.0, 0.0, 0.0, &HashMap::new(), &atlas)
            .expect("gpu render");
        let at = |x: u32, y: u32| {
            let i = ((y * 24 + x) * 4) as usize;
            [got[i], got[i + 1], got[i + 2], got[i + 3]]
        };
        // Blue ink on white lowers the red channel: dark core, lighter halo
        // one blur step left of the crisp block, untouched corner.
        let core = at(7, 7);
        let halo = at(4, 7);
        assert!(core[0] < 160, "core inked: {core:?}");
        assert!(halo[0] > core[0] && halo[0] < 255, "halo spreads: {halo:?}");
        assert_eq!(at(0, 0), [255, 255, 255, 255]);
    }
}
