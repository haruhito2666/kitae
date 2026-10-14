//! Hot swap of Rust edits: the site's routes dylib, reloaded under a
//! running [`World`](crate::World).
//!
//! `kitae dev` hot mode compiles the site to a routes dylib. When the file
//! changes, the RUNNING SITE (not the CLI — it has no world) dlopens it and
//! hands the fresh routes to [`crate::World::replace_routes`]. Clients stay
//! connected — they get a fresh `Init`, not a page reload. The mechanics
//! this module exists to get right:
//!
//! * cargo replaces the dylib atomically (write-elsewhere, rename), so a
//!   changed mtime+size means a complete file;
//! * dlopen-ing the SAME path returns the already-loaded image — so every
//!   load first copies the dylib to a new versioned path; a new path forces
//!   a fresh image, and the old image stays mapped because live sessions
//!   still hold render closures from it;
//! * handles are NEVER closed: a session's closure may belong to any
//!   previous image. One leaked handle per swap — a dev-loop trade, not a
//!   production path.
//!
//! The host and the dylib must share ONE image of kitae_ui/kitae_live
//! (their state is thread-local): scaffolded sites build with
//! `-C prefer-dynamic` (see the template's `.cargo/config.toml`), which
//! makes dyld coalesce the dependency dylibs into the host's copies.

use crate::server::Route;

/// Watch + load plumbing for one site dylib
/// (`target/<profile>/lib<crate>.dylib`).
pub struct DylibWatch {
    path: std::path::PathBuf,
    /// Versioned copies land next to the real dylib (same target dir, so a
    /// `cargo clean` sweeps them too).
    hot_dir: std::path::PathBuf,
    gen: u64,
    /// (mtime, size) of the dylib as of the last load — `None` until the
    /// first load.
    stamp: Option<(std::time::SystemTime, u64)>,
}

impl DylibWatch {
    /// Watch `path`. The baseline is the dylib as it exists NOW (the build
    /// the site started from); the first swap fires on the NEXT change.
    pub fn new(path: impl Into<std::path::PathBuf>) -> DylibWatch {
        let path = path.into();
        let stamp = stamp_of(&path);
        // Versioned copies land NEXT TO the real dylib: the copy resolves
        // its own deps (libkitae_ui.dylib, …) through @loader_path, so they
        // must be in the same dir. A `cargo clean` sweeps them too.
        let hot_dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(std::path::PathBuf::new);
        DylibWatch {
            path,
            hot_dir,
            gen: 0,
            stamp,
        }
    }

    /// True when the dylib on disk differs from the last one we loaded
    /// (including the never-loaded case: a fresh watch over a file that
    /// appeared after construction). Pure — a failed load keeps `true`.
    pub fn changed(&self) -> bool {
        stamp_of(&self.path) != self.stamp
    }

    /// Load the current dylib into a versioned copy, dlopen it, pull its
    /// `kitae_routes()` export. The stamp moves to the file we saw either
    /// way — a given file either loads or doesn't, and retrying it every
    /// poll tick would just spam. The next build changes the mtime and
    /// re-fires. The copy is stamped AFTER it completes, so a rebuild that
    /// lands mid-copy re-fires the watch and self-heals.
    pub fn load_routes(&mut self) -> Result<Vec<Route>, String> {
        let result = load_once(&self.path, &self.hot_dir, &mut self.gen);
        self.stamp = stamp_of(&self.path);
        result
    }
}

/// (mtime, size) of a file — the cheap fingerprint. Missing file = `None`
/// (a site that hasn't built its dylib yet is legal; the watch just waits).
fn stamp_of(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Where a site's routes dylib lands for the binary that calls this
/// (`target/debug` under `cargo run`, `target/release` under `--release`).
/// Relative to the site dir — the generated main runs with the site dir as
/// its working directory.
pub fn target_dylib(lib_crate_name: &str) -> std::path::PathBuf {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    std::path::Path::new("target")
        .join(profile)
        .join(format!("lib{lib_crate_name}.{ext}"))
}

/// Define the C symbol `kitae dev`'s dylib loader pulls. Drop this in the
/// site's lib crate (next to `pub fn routes()`).
#[macro_export]
macro_rules! export_routes {
    () => {
        #[no_mangle]
        pub extern "C" fn kitae_routes() -> *mut Vec<$crate::Route> {
            Box::into_raw(Box::new(routes()))
        }
    };
}

#[cfg(unix)]
fn load_once(
    path: &std::path::Path,
    hot_dir: &std::path::Path,
    gen: &mut u64,
) -> Result<Vec<Route>, String> {
    *gen += 1;
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let copy = hot_dir.join(format!("lib-hot-{gen}.{ext}"));
    std::fs::create_dir_all(hot_dir).map_err(|e| format!("hot dir: {e}"))?;
    std::fs::copy(path, &copy).map_err(|e| format!("copy dylib: {e}"))?;
    let routes = open_and_pull(&copy)?;
    // Keep the newest `lib-hot-*` copy only — old images stay mapped by the
    // OS even once their files are gone, and the copies would pile up
    // across a long dev session otherwise. Only our own copies are touched:
    // hot_dir is the site's real target dir.
    if let Ok(entries) = std::fs::read_dir(hot_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let ours = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("lib-hot-") && n.ends_with(&format!(".{ext}")));
            if ours && p != copy {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    Ok(routes)
}

/// Non-unix: no dlopen in std. The generated dev loop prints this once
/// per changed dylib and keeps serving the statically linked routes.
#[cfg(not(unix))]
fn load_once(
    _path: &std::path::Path,
    _hot_dir: &std::path::Path,
    _gen: &mut u64,
) -> Result<Vec<Route>, String> {
    Err("hot swap of Rust edits needs unix (dlopen); content hot-reload still works".into())
}

/// dlopen `path` and hand back whatever `kitae_routes()` exports.
#[cfg(unix)]
fn open_and_pull(path: &std::path::Path) -> Result<Vec<Route>, String> {
    use std::ffi::CStr;
    let cpath = std::ffi::CString::new(
        path.to_str()
            .ok_or_else(|| "dylib path is not utf-8".to_string())?,
    )
    .map_err(|_| "dylib path has a NUL".to_string())?;
    unsafe {
        let handle = sys::dlopen(cpath.as_ptr(), sys::RTLD_NOW);
        if handle.is_null() {
            let err = sys::dlerror();
            let msg = if err.is_null() {
                "dlopen failed".to_string()
            } else {
                CStr::from_ptr(err).to_string_lossy().into_owned()
            };
            return Err(format!("{}: {msg}", path.display()));
        }
        let sym = sys::dlsym(handle, c"kitae_routes".as_ptr());
        if sym.is_null() {
            return Err(format!(
                "{} has no `kitae_routes` export — rebuild the site",
                path.display()
            ));
        }
        let get: extern "C" fn() -> *mut Vec<Route> = std::mem::transmute(sym);
        let routes = Box::from_raw(get());
        Ok(*routes)
    }
    // `handle` intentionally leaks: never dlclose — the returned routes'
    // render closures have code and vtables in this image.
}

#[cfg(unix)]
mod sys {
    use std::ffi::{c_char, c_int, c_void};

    extern "C" {
        pub fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        pub fn dlerror() -> *mut c_char;
    }

    /// RTLD_NOW on both macOS and glibc. Resolves every symbol up front so
    /// a half-built dylib fails at load, not mid-render.
    pub const RTLD_NOW: c_int = 2;

    /// Load a dylib by absolute path and keep it forever. Exposed for
    /// embedders and tests that never linked the site's dependencies:
    /// dyld resolves a dylib's deps against already-loaded images matched
    /// by install name, so preloading `libkitae_ui.dylib` etc. makes a
    /// bare dlopen of the site dylib work.
    pub fn preload(path: &std::path::Path) -> Result<(), String> {
        use std::ffi::CStr;
        let cpath = std::ffi::CString::new(path.to_str().ok_or("non-utf8 path")?)
            .map_err(|_| "NUL in path")?;
        unsafe {
            let handle = dlopen(cpath.as_ptr(), RTLD_NOW);
            if handle.is_null() {
                let err = dlerror();
                let msg = if err.is_null() {
                    "dlopen failed".to_string()
                } else {
                    CStr::from_ptr(err).to_string_lossy().into_owned()
                };
                return Err(format!("{}: {msg}", path.display()));
            }
        }
        Ok(())
    }
}

/// Load a dylib by absolute path and keep it forever (see [`sys::preload`]).
#[cfg(unix)]
pub fn preload_dylib(path: &std::path::Path) -> Result<(), String> {
    sys::preload(path)
}

#[cfg(not(unix))]
pub fn preload_dylib(_path: &std::path::Path) -> Result<(), String> {
    Err("hot swap of Rust edits needs unix (dlopen); content hot-reload still works".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn load_rejects_dylib_without_export_and_consumes_the_stamp() {
        // A real dlopen of a real dylib that simply has no `kitae_routes`:
        // the error names the missing export, and the failed attempt still
        // consumes the stamp (a retry would see the same file and fail the
        // same way — that's spam, not signal). libSystem lives in the dyld
        // shared cache on modern macOS, so pick whichever real dylib is on
        // disk.
        let real = std::fs::read_dir("/usr/lib")
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|ext| ext == "dylib") && p.is_file())
            .expect("a real dylib in /usr/lib");
        let dir = std::env::temp_dir().join(format!("kitae_hot_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lib = dir.join("no_export.dylib");
        std::fs::copy(&real, &lib).unwrap();

        let mut watch = DylibWatch::new(&lib);
        assert!(watch.load_routes().is_err());
        assert!(!watch.changed(), "a failed load must consume the stamp");

        // And a genuinely changed file re-fires the watch.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&lib, b"rebuilt - different bytes").unwrap();
        assert!(watch.changed());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn watch_waits_for_the_dylib_and_fires_on_change() {
        let dir = std::env::temp_dir().join(format!("kitae_watch_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lib = dir.join("libsite.dylib");

        // Not built yet: no stamp, no change — the watch just waits.
        let watch = DylibWatch::new(&lib);
        assert!(!watch.changed());

        // The file appears: the pre-build watch fires (first load).
        std::fs::write(&lib, b"one").unwrap();
        assert!(watch.changed());
        assert!(
            !DylibWatch::new(&lib).changed(),
            "a fresh watch baselines at the file that now exists"
        );

        // Same content, no change. Rewritten content: fires.
        let watch = DylibWatch::new(&lib);
        assert!(!watch.changed());
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&lib, b"one-two").unwrap();
        assert!(watch.changed());
        assert!(!DylibWatch::new(&lib).changed()); // baseline moved with it

        std::fs::remove_dir_all(&dir).ok();
    }
}
