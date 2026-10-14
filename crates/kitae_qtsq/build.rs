//! Build script for kitae_qtsq.
//!
//! Compiles the small C shim (`native/qtsq/shim.c`) and links the vendored
//! QTSQ static libraries (`native/qtsq/libqtsq.a`, `libqtc.a`) plus their
//! external dependencies (zlib, Opus via pkg-config, macOS frameworks the
//! library was built against).
//!
//! The vendored snapshot decouples the browser build from the sibling
//! black-hole-disk repo; refreshing it is a deliberate Makefile action
//! (see the top-level Makefile's `native/qtsq` target).

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let native = manifest.join("native/qtsq");
    if !native.join("libqtsq.a").exists() {
        panic!(
            "vendored libqtsq.a missing at {} — run `make native/qtsq/libqtsq.a` \
             from the workspace root to snapshot it from black-hole-disk",
            native.display()
        );
    }

    // 1. Compile the shim with the real QTSQ headers.
    let headers = native.join("include");
    let mut build = cc::Build::new();
    build
        .file(native.join("shim.c"))
        .include(&headers)
        .flag_if_supported("-fno-objc-arc")
        .warnings(false);
    build.compile("kitae_qtsq_shim");

    // 2. Link the vendored QTSQ + qtc static libraries.
    println!("cargo:rustc-link-search=native={}", native.display());
    println!("cargo:rustc-link-lib=static=qtsq");
    println!("cargo:rustc-link-lib=static=qtc");

    // 3. zlib (PIXEL strategy and friends).
    println!("cargo:rustc-link-lib=z");

    // 4. Opus — located via pkg-config like black-hole-disk's own Makefile.
    if let Ok(out) = Command::new("pkg-config").args(["--libs", "opus"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        for tok in s.split_whitespace() {
            if let Some(dir) = tok.strip_prefix("-L") {
                println!("cargo:rustc-link-search=native={dir}");
            } else if let Some(lib) = tok.strip_prefix("-l") {
                println!("cargo:rustc-link-lib={lib}");
            }
        }
    } else {
        println!("cargo:rustc-link-lib=opus");
    }

    // 5. macOS frameworks the vendored library references (CoreAudio capture,
    //    Metal render paths, Security keychain, Foundation, CoreBluetooth).
    //    Static archives never auto-pull frameworks; the final binary must.
    for fw in [
        "Security",
        "CoreFoundation",
        "CoreAudio",
        "AudioUnit",
        "AudioToolbox",
        "Metal",
        "MetalKit",
        "Foundation",
        "CoreBluetooth",
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
    // The library's Objective-C sections need the ObjC runtime and, for its
    // single C++-compiled object (qtsq_metal.o), the C++ runtime.
    println!("cargo:rustc-link-lib=objc");
    println!("cargo:rustc-link-lib=c++");

    println!("cargo:rerun-if-changed=native/qtsq/shim.c");
    println!("cargo:rerun-if-changed=native/qtsq/libqtsq.a");
    println!("cargo:rerun-if-changed=native/qtsq/libqtc.a");
    println!("cargo:rerun-if-changed=native/qtsq/include");
}
