//! The C ABI the app uses to install this rasterizer on its own engine.
//!
//! Why it exists: `PageRaster` is called with the `Browser` the *engine*
//! owns, and the CPU compositor this rasterizer paints with lives above
//! kitae_core in the crate graph (`kitae_render_gpu` depends on the engine),
//! so the engine cannot name this crate's types. The app links this static
//! library and makes one call — `kitae_headless_install_raster(engine,
//! scale)` — which builds the rasterizer and hands the engine a vtable of C
//! function pointers (the same injection shape the app already uses for Core
//! Text measurement).
//!
//! The wire contract (buffer sizing, error convention) is documented on
//! `kitae_core::agent::PageRasterVtable`.

use std::ffi::c_void;

use kitae_core::agent::PageRaster;

use crate::HeadlessRaster;

/// The engine's raster-installation entry point (`kitae_core::ffi`). Declared
/// here rather than imported so this crate keeps its single dependency edge:
/// the symbol is in the same final link, and the app resolves it because
/// `kitae_core` is force-loaded.
extern "C" {
    fn kitae_engine_set_raster(
        engine: *mut c_void,
        ctx: *mut c_void,
        png: Option<RasterFn>,
        pdf: Option<RasterFn>,
        free_ctx: Option<unsafe extern "C" fn(*mut c_void)>,
    );
}

type RasterFn = unsafe extern "C" fn(*mut c_void, *const c_void, *mut u8, usize) -> isize;

/// Fill `buf` with the PNG of `browser` (see `PageRasterVtable` for the
/// buffer protocol). `scale` device pixels per CSS pixel is baked into the
/// context, so the same function serves every screenshot size the engine
/// asks for at the scale the host chose.
/// The PNG half of the vtable. The engine receives it as a pointer from
/// [`kitae_headless_install_raster`], never by name — it is crate-visible so
/// this crate's own test can exercise the buffer protocol directly.
pub(crate) unsafe extern "C" fn raster_png(
    ctx: *mut c_void,
    browser: *const c_void,
    buf: *mut u8,
    cap: usize,
) -> isize {
    let Some(raster) = ctx.cast::<HeadlessRaster>().as_ref() else {
        return -1;
    };
    let Some(browser) = browser.cast::<kitae_core::Browser>().as_ref() else {
        return -1;
    };
    copy_out(raster.png(browser), buf, cap)
}

/// The PDF half of the vtable (same contract as [`raster_png`]).
/// The PDF half of the vtable (same contract as [`raster_png`]).
pub(crate) unsafe extern "C" fn raster_pdf(
    ctx: *mut c_void,
    browser: *const c_void,
    buf: *mut u8,
    cap: usize,
) -> isize {
    let Some(raster) = ctx.cast::<HeadlessRaster>().as_ref() else {
        return -1;
    };
    let Some(browser) = browser.cast::<kitae_core::Browser>().as_ref() else {
        return -1;
    };
    copy_out(raster.pdf(browser), buf, cap)
}

/// The shared buffer protocol: a negative return is an error whose message is
/// the first `n` bytes, otherwise the answer is `n` bytes (truncated to what
/// fit, in which case the full length still comes back).
unsafe fn copy_out(outcome: Result<Vec<u8>, String>, buf: *mut u8, cap: usize) -> isize {
    let (bytes, failed) = match outcome {
        Ok(bytes) => (bytes, false),
        Err(message) => (message.into_bytes(), true),
    };
    if !buf.is_null() && cap > 0 {
        let n = bytes.len().min(cap);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
    }
    let len = bytes.len() as isize;
    if failed {
        -len
    } else {
        len
    }
}

/// Release the rasterizer context once the engine is done with it.
pub(crate) unsafe extern "C" fn raster_free(ctx: *mut c_void) {
    if !ctx.is_null() {
        drop(Box::from_raw(ctx.cast::<HeadlessRaster>()));
    }
}

/// Install the pure-Rust rasterizer on `engine`: `scale` device pixels per
/// CSS pixel (2.0 for a retina-shaped screenshot, 1.0 for a budget one).
///
/// Returns 1 when a rasterizer was installed and 0 when the system has no
/// usable fonts — in which case the engine keeps its "no rasterizer" error,
/// which names the reason, rather than returning a blank image.
///
/// # Safety
/// `engine` must be a live engine handle from `kitae_engine_new`; the
/// rasterizer is freed when that engine's thread ends.
#[no_mangle]
pub unsafe extern "C" fn kitae_headless_install_raster(engine: *mut c_void, scale: f64) -> i32 {
    if engine.is_null() {
        return 0;
    }
    let raster = match HeadlessRaster::load() {
        Ok(raster) => raster.with_scale(if scale.is_finite() { scale as f32 } else { 1.0 }),
        Err(_) => return 0,
    };
    let ctx = Box::into_raw(Box::new(raster)).cast::<c_void>();
    kitae_engine_set_raster(
        engine,
        ctx,
        Some(raster_png),
        Some(raster_pdf),
        Some(raster_free),
    );
    1
}
