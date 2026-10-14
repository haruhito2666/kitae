//! C ABI for the Swift Metal present loop (Phase L5).
//!
//! Staging: the GPU (or CPU-fallback) paints rects, shadows and images;
//! Swift composites Core Text over the returned bitmap until the L2 atlas
//! upload path lands in the app. Pixels are straight-alpha RGBA8.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::sync::Arc;

use kitae_media::RgbaImage;

use crate::frame::parse_frame_scene;

/// Host-side mutable state behind the const handle: the E5 whole-layer
/// raster cache and the E6 LCD override. Mutex-guarded because the C ABI
/// hands out `const` handles (BridgingHeader.h) — interior mutability is
/// the only way to keep that contract while caching/mutating in Rust.
struct HostState {
    /// Whole-layer rasters keyed on raw frame bytes + render state (see
    /// [`crate::layers`]); bypassed for frames whose paint inputs changed
    /// out-of-band until the host clears it.
    layers: crate::layers::LayerCache,
    /// E6 host-side LCD request (`kru_gpu_set_lcd_text`): overrides every
    /// subsequently painted frame's LCD flag. `None` = per-frame value.
    lcd_override: Option<bool>,
}

/// Opaque render handle (owns an optional GPU context; `None` = CPU only).
pub struct GpuHandle {
    ctx: Option<crate::backend::GpuContext>,
    host: std::sync::Mutex<HostState>,
}

/// Bring up the GPU path. Always succeeds (CPU fallback inside); use
/// [`kru_gpu_available`] to learn whether Metal is really behind it.
#[no_mangle]
pub extern "C" fn kru_gpu_init() -> *mut GpuHandle {
    let handle = Box::new(GpuHandle {
        ctx: crate::backend::GpuContext::new().ok(),
        host: std::sync::Mutex::new(HostState {
            layers: crate::layers::LayerCache::new(),
            lcd_override: None,
        }),
    });
    Box::into_raw(handle)
}

/// 1 when the handle has a live GPU context, 0 for CPU fallback.
#[no_mangle]
pub extern "C" fn kru_gpu_available(handle: *const GpuHandle) -> std::ffi::c_int {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return 0;
    };
    i32::from(handle.ctx.is_some())
}

/// Render one serialized engine frame (`kitae_engine_frame` bytes) into a
/// `w`×`h` device-px tile (`scale` maps CSS px to device px; `scroll_x/y`
/// is the page-space CSS tile origin — the present loop renders only the
/// visible window). Returns malloc'd RGBA8 (`w*h*4` bytes, free with
/// [`kru_gpu_pixels_free`]), null on malformed input. Image pixels resolve
/// through the engine's raster side-table (`qtsq_raster_for_id`); glyphs are
/// skipped (Swift composites text).
#[no_mangle]
pub extern "C" fn kru_gpu_render(
    handle: *const GpuHandle,
    frame: *const u8,
    frame_len: usize,
    w: u32,
    h: u32,
    scale: f32,
    scroll_x: f32,
    scroll_y: f32,
) -> *mut u8 {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return std::ptr::null_mut();
    };
    if frame.is_null() || frame_len == 0 || w == 0 || h == 0 || w > 4096 || h > 4096 {
        return std::ptr::null_mut();
    }
    let scale = scale.clamp(0.25, 4.0);
    let (ox, oy) = (scroll_x.max(0.0), scroll_y.max(0.0));
    let bytes = unsafe { std::slice::from_raw_parts(frame, frame_len) };
    // E5: an unchanged frame at the same render state is a straight blit —
    // skip parse + rasterization entirely.
    let (key, lcd) = {
        let host = handle.host.lock().expect("gpu host state");
        (
            crate::layers::layer_key(
                bytes,
                w,
                h,
                scale,
                ox,
                oy,
                host.lcd_override.unwrap_or(false),
            ),
            host.lcd_override,
        )
    };
    if let Some(px) = handle
        .host
        .lock()
        .expect("gpu host state")
        .layers
        .lookup(key)
    {
        let mut owned = px.into_boxed_slice();
        let ptr = owned.as_mut_ptr();
        std::mem::forget(owned);
        return ptr;
    }
    let Some(mut scene) = parse_frame_scene(bytes, w, h) else {
        return std::ptr::null_mut();
    };
    if let Some(lcd) = lcd {
        scene.lcd_text = lcd;
    }
    // Resolve payload ids against the engine side-table (same process).
    let mut rasters: HashMap<u64, Arc<RgbaImage>> = HashMap::new();
    for im in &scene.images {
        if im.payload != 0 && !rasters.contains_key(&im.payload) {
            if let Some(r) = kitae_core::qtsq_raster_for_id(im.payload) {
                rasters.insert(
                    im.payload,
                    Arc::new(RgbaImage {
                        width: r.width,
                        height: r.height,
                        pixels: (*r.pixels).clone(),
                    }),
                );
            }
        }
    }
    let atlas = crate::GlyphAtlas::new();
    let pixels = match &handle.ctx {
        Some(ctx) => ctx
            .render(&scene, w, h, scale, ox, oy, &rasters, &atlas)
            .unwrap_or_else(|_| crate::cpu::render(&scene, w, h, scale, ox, oy, &rasters, &atlas)),
        None => crate::cpu::render(&scene, w, h, scale, ox, oy, &rasters, &atlas),
    };
    let mut pixels = pixels.into_boxed_slice();
    let ptr = pixels.as_mut_ptr();
    std::mem::forget(pixels);
    handle
        .host
        .lock()
        .expect("gpu host state")
        .layers
        .store(key, unsafe {
            std::slice::from_raw_parts(ptr, (w * h * 4) as usize)
        });
    ptr
}

/// E5: drop cached whole-layer rasters (frame bytes unchanged but paint
/// inputs changed out-of-band — engine raster table updates, forced
/// repaints). The next render re-rasterizes.
#[no_mangle]
pub extern "C" fn kru_gpu_clear_layers(handle: *const GpuHandle) {
    if let Some(handle) = (unsafe { handle.as_ref() }) {
        handle.host.lock().expect("gpu host state").layers.clear();
    }
}

/// E6: force LCD (subpixel) text AA for every subsequently painted frame
/// (1 = on, 0 = off, other = back to per-frame value). Returns the previous
/// mode: 1/0 when an override was set, -1 when per-frame.
#[no_mangle]
pub extern "C" fn kru_gpu_set_lcd_text(
    handle: *const GpuHandle,
    mode: std::ffi::c_int,
) -> std::ffi::c_int {
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    let mut host = handle.host.lock().expect("gpu host state");
    let prev = host.lcd_override.map(i32::from).unwrap_or(-1);
    host.lcd_override = match mode {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    prev
}

/// Free pixels from [`kru_gpu_render`]. `len` must be the `w*h*4` length.
#[no_mangle]
pub extern "C" fn kru_gpu_pixels_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    unsafe {
        drop(Box::from_raw(std::slice::from_raw_parts_mut(ptr, len)));
    }
}

/// Shut down the render handle.
#[no_mangle]
pub extern "C" fn kru_gpu_shutdown(handle: *mut GpuHandle) {
    if handle.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(handle));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v20_frame_one_rect() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&20u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0f32.to_le_bytes());
        b.extend_from_slice(&64f32.to_le_bytes());
        b.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes());
        for s in ["u", "t"] {
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s.as_bytes());
        }
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&[0u8; 12]);
        for v in [4f32, 4.0, 8.0, 8.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&0xFF0000FFu32.to_le_bytes());
        b.extend_from_slice(&[0u8; 16]);
        b.push(0);
        b.extend_from_slice(&[0u8; 8]);
        b.push(0);
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.push(0);
        b.push(0);
        b.extend_from_slice(&[0u8; 24]);
        b
    }

    #[test]
    fn ffi_renders_frame_to_red_pixels() {
        let frame = v20_frame_one_rect();
        let handle = kru_gpu_init();
        assert!(!handle.is_null());
        let ptr = kru_gpu_render(handle, frame.as_ptr(), frame.len(), 16, 16, 1.0, 0.0, 0.0);
        assert!(!ptr.is_null());
        let px = unsafe { std::slice::from_raw_parts(ptr, 16 * 16 * 4) };
        let at = |x: usize, y: usize| &px[(y * 16 + x) * 4..(y * 16 + x) * 4 + 4];
        assert_eq!(at(0, 0), &[255, 255, 255, 255]);
        assert_eq!(at(8, 8), &[255, 0, 0, 255]);
        kru_gpu_pixels_free(ptr, 16 * 16 * 4);
        kru_gpu_shutdown(handle);
    }

    #[test]
    fn ffi_rejects_garbage() {
        let handle = kru_gpu_init();
        assert!(kru_gpu_render(handle, std::ptr::null(), 0, 16, 16, 1.0, 0.0, 0.0).is_null());
        assert!(kru_gpu_render(handle, [0u8; 4].as_ptr(), 4, 16, 16, 1.0, 0.0, 0.0).is_null());
        kru_gpu_shutdown(handle);
        kru_gpu_shutdown(std::ptr::null_mut());
        kru_gpu_pixels_free(std::ptr::null_mut(), 0);
    }

    /// E5: the same frame at the same render state blits from the layer
    /// cache (identical pixels by construction), a changed state misses,
    /// and `kru_gpu_clear_layers` drops everything without breaking the
    /// next render.
    #[test]
    fn ffi_layer_cache_blits_and_clears() {
        let frame = v20_frame_one_rect();
        let n = 16 * 16 * 4;
        let handle = kru_gpu_init();
        let p1 = kru_gpu_render(handle, frame.as_ptr(), frame.len(), 16, 16, 1.0, 0.0, 0.0);
        assert!(!p1.is_null());
        let first = unsafe { std::slice::from_raw_parts(p1, n) }.to_vec();
        kru_gpu_pixels_free(p1, n);
        let p2 = kru_gpu_render(handle, frame.as_ptr(), frame.len(), 16, 16, 1.0, 0.0, 0.0);
        assert_eq!(
            unsafe { std::slice::from_raw_parts(p2, n) },
            &first[..],
            "cache hit blits"
        );
        kru_gpu_pixels_free(p2, n);
        // Different scroll → different key → fresh render, still correct.
        let p3 = kru_gpu_render(handle, frame.as_ptr(), frame.len(), 16, 16, 1.0, 0.0, 4.0);
        assert!(!p3.is_null());
        kru_gpu_pixels_free(p3, n);
        kru_gpu_clear_layers(handle);
        kru_gpu_clear_layers(std::ptr::null());
        let p4 = kru_gpu_render(handle, frame.as_ptr(), frame.len(), 16, 16, 1.0, 0.0, 0.0);
        assert!(!p4.is_null(), "renders normally after a clear");
        kru_gpu_pixels_free(p4, n);
        kru_gpu_shutdown(handle);
    }

    /// E6: the LCD override round-trips its previous mode (-1 = per-frame)
    /// and an unset (mode ∉ {0,1}) returns to per-frame.
    #[test]
    fn ffi_lcd_override_round_trip() {
        let handle = kru_gpu_init();
        assert_eq!(kru_gpu_set_lcd_text(handle, 1), -1, "prev = per-frame");
        assert_eq!(kru_gpu_set_lcd_text(handle, 0), 1);
        assert_eq!(
            kru_gpu_set_lcd_text(handle, 2),
            0,
            "unset reports prev override"
        );
        assert_eq!(kru_gpu_set_lcd_text(handle, 1), -1, "back to per-frame");
        kru_gpu_shutdown(handle);
        assert_eq!(kru_gpu_set_lcd_text(std::ptr::null(), 1), -1);
    }
}
