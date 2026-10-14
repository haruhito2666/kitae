// Builds the C++/SDL2 native renderer (native/) and links it statically
// into the Rust binary. SDL2 itself is linked as a dylib via pkg-config.
use std::env;
use std::path::PathBuf;

fn main() {
    let native_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../../native")
        .canonicalize()
        .expect("native/ directory must exist");

    // Build the static lib with CMake (handles pkg-config lookups for SDL2).
    let dst = cmake::Config::new(&native_dir).build();

    // libkitae_renderer.a lands at the build root.
    println!("cargo:rustc-link-search=native={}", dst.display());
    println!("cargo:rustc-link-lib=static=kitae_renderer");
    println!("cargo:rustc-link-lib=c++");

    // SDL2 + SDL2_ttf dylibs from Homebrew, resolved through pkg-config so
    // we mirror whatever flags CMake used.
    let pkg = pkg_config_flags(&["sdl2", "SDL2_ttf"]);
    for flag in &pkg {
        if let Some(dir) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
        } else if let Some(lib) = flag
            .strip_prefix("-l")
            .filter(|l| !l.is_empty() && *l != "c++" && *l != "cxx")
        {
            println!("cargo:rustc-link-lib={lib}");
        }
    }

    println!(
        "cargo:rerun-if-changed={}",
        native_dir.join("include").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        native_dir.join("src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        native_dir.join("CMakeLists.txt").display()
    );
}

/// Run `pkg-config --libs` and split the output into individual flags.
fn pkg_config_flags(pkgs: &[&str]) -> Vec<String> {
    let output = std::process::Command::new("pkg-config")
        .arg("--libs")
        .args(pkgs)
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        // Fallback: Homebrew's standard layout.
        _ => {
            println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
            vec!["-lSDL2".into(), "-lSDL2_ttf".into()]
        }
    }
}
