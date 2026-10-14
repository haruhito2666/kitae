//! kitae_cli tests: template invariants, scaffolding, a full
//! scaffold → cargo check round-trip for one template (the generated
//! sites must COMPILE — that is the template contract), and a full
//! scaffold → cargo build --lib → dlopen round-trip (the hot-swap
//! contract: the generated lib must export what `kitae dev` loads).

use std::path::PathBuf;
use std::process::Command;

use kitae_live::{write_static, DylibWatch, Route};
use kitae_ui::prelude::*;

// Re-export the bin crate's template module (bins have no lib target, so
// integration tests include the source directly).
#[path = "../src/templates.rs"]
mod templates;

// ── template invariants ─────────────────────────────────────────────────────

#[test]
fn templates_generate_expected_shapes() {
    for t in templates::TEMPLATES {
        let main = (t.main_rs)("demo");
        // The host: statically linked initial routes + serve + dev loop.
        assert!(
            main.contains("use demo::routes"),
            "{}: static routes",
            t.name
        );
        assert!(main.contains("write_static"), "{}: static export", t.name);
        assert!(main.contains("KITAE_DEV"), "{}: dev loop", t.name);
        assert!(
            main.contains("refresh_all"),
            "{}: content hot reload",
            t.name
        );
        assert!(main.contains("DylibWatch"), "{}: dylib hot swap", t.name);
        assert!(main.contains("replace_routes"), "{}: route swap", t.name);
        // No script — the whole point.
        assert!(
            !main.to_lowercase().contains("<script"),
            "{}: no script",
            t.name
        );

        // The pages: routes() plus the C export the dev loop pulls.
        let lib = (t.lib_rs)("demo");
        assert!(
            lib.contains("pub fn routes() -> Vec<Route>"),
            "{}: routes()",
            t.name
        );
        assert!(
            lib.contains("export_routes!()"),
            "{}: kitae_routes export",
            t.name
        );
    }
    // The scaffold's build config: the lib is a dylib (that's what gets
    // hot-swapped) next to an rlib (ordinary linking), and everything
    // builds prefer-dynamic so host and dylib share dependency images.
    assert!(
        templates::cargo_toml("demo").contains("crate-type = [\"dylib\", \"rlib\"]"),
        "Cargo.toml template must build the lib as a dylib for hot swap"
    );
    // RP-C7: the two dependency flavors. Default scaffolds point at the
    // local checkout (Kitae is not published); `--registry` scaffolds
    // depend on the published version line.
    let path_toml = templates::cargo_toml("demo");
    assert!(
        path_toml.contains("kitae_ui = { path ="),
        "default scaffold uses path deps"
    );
    let registry_toml = templates::cargo_toml_mode("demo", true);
    assert!(
        registry_toml.contains("kitae_ui = \"0.1\"")
            && registry_toml.contains("kitae_live = \"0.1\"")
            && !registry_toml.contains("path ="),
        "--registry scaffold uses version deps"
    );
    let config = templates::cargo_config();
    assert!(
        config.contains("prefer-dynamic"),
        "host + dylib must share dependency images"
    );
    assert!(
        config.contains(if cfg!(target_os = "macos") {
            "@loader_path"
        } else {
            "$ORIGIN"
        }),
        "hot-swap copies resolve their deps next to themselves"
    );
}

// ── scaffolding ─────────────────────────────────────────────────────────────

#[test]
fn scaffold_writes_complete_project() {
    let dir = std::env::temp_dir().join(format!("kitae_scaffold_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let spec = templates::by_name("hello").unwrap();

    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("content")).unwrap();
    std::fs::create_dir_all(dir.join(".cargo")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), templates::cargo_toml("demo-site")).unwrap();
    std::fs::write(dir.join(".cargo/config.toml"), templates::cargo_config()).unwrap();
    std::fs::write(dir.join("src/lib.rs"), (spec.lib_rs)("demo-site")).unwrap();
    std::fs::write(dir.join("src/main.rs"), (spec.main_rs)("demo-site")).unwrap();
    std::fs::write(
        dir.join("README.md"),
        templates::readme("demo-site", spec.description),
    )
    .unwrap();
    std::fs::write(dir.join("content/welcome.txt"), "hi").unwrap();

    assert!(dir.join("Cargo.toml").is_file());
    assert!(dir.join(".cargo/config.toml").is_file());
    assert!(dir.join("src/lib.rs").is_file());
    assert!(dir.join("src/main.rs").is_file());
    assert!(dir.join("content/welcome.txt").is_file());
    let lib = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    assert!(lib.contains("pub fn routes()"));
    let main = std::fs::read_to_string(dir.join("src/main.rs")).unwrap();
    assert!(main.contains("use demo_site::routes"));
    std::fs::remove_dir_all(&dir).ok();
}

// ── static export (kitae build's engine) ────────────────────────────────────

#[test]
fn write_static_exports_every_route() {
    let count = use_signal(|| 7u32);
    let routes = vec![
        Route::new(
            "Home",
            std::rc::Rc::new(move || {
                view! { h1 { "Home" } p { "count is {count}" } }
            }),
        ),
        Route::new(
            "About",
            std::rc::Rc::new(|| {
                view! { h1 { "About" } }
            }),
        )
        .at("/about"),
    ];
    let dist = std::env::temp_dir().join(format!("kitae_export_{}", std::process::id()));
    let written = write_static(&routes, &dist).expect("export");
    assert_eq!(written.len(), 2);
    let home = std::fs::read_to_string(dist.join("index.html")).unwrap();
    assert!(home.contains("<meta name=\"kitae-live\" content=\"1\">"));
    assert!(home.contains("<!doctype html>"));
    assert!(home.contains("Home"));
    let about = std::fs::read_to_string(dist.join("about/index.html")).unwrap();
    assert!(about.contains("About"));
    std::fs::remove_dir_all(&dist).ok();
}

// ── end-to-end: a scaffolded site must compile ──────────────────────────────

/// `kitae new` + `cargo check` of the generated project (full scaffold,
/// including the lib crate and the prefer-dynamic build config).
///
/// The dedicated persistent target dir is deliberate: scaffolded sites
/// build prefer-dynamic, the workspace builds its own crates static, and
/// the two flavors must never share a target dir or they endlessly
/// invalidate each other (and a static-flavored libkitae_ui.dylib breaks
/// the site dylib link with "std only shows up once"). A private dir
/// keeps the flavor fixed and the kitae deps warm between runs.
fn e2e_target_dir() -> PathBuf {
    [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "..",
        "target",
        "kitae_site_e2e",
    ]
    .iter()
    .collect()
}

#[test]
fn scaffolded_site_compiles() {
    let target = e2e_target_dir();
    let dir = std::env::temp_dir().join(format!("kitae_compile_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("content")).unwrap();
    std::fs::create_dir_all(dir.join(".cargo")).unwrap();
    std::fs::write(dir.join("content/welcome.txt"), "hello from the test").unwrap();
    let spec = templates::by_name("hello").unwrap();
    std::fs::write(dir.join("src/lib.rs"), (spec.lib_rs)("demo-compile")).unwrap();
    std::fs::write(dir.join("src/main.rs"), (spec.main_rs)("demo-compile")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        templates::cargo_toml("demo-compile"),
    )
    .unwrap();
    std::fs::write(dir.join(".cargo/config.toml"), templates::cargo_config()).unwrap();

    let out = Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(&dir)
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("spawn cargo check");
    assert!(
        out.status.success(),
        "scaffolded site must compile:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── end-to-end: the hot-swap contract ───────────────────────────────────────

/// Scaffold → `cargo build --lib` → dlopen → routes. This is the whole
/// `kitae dev` hot-swap path minus the editor: the generated lib must
/// compile to a dylib that loads and exports `kitae_routes`, and a
/// rebuild must re-fire the watch and load again.
#[cfg(unix)]
#[test]
fn scaffolded_dylib_hot_loads() {
    let target = e2e_target_dir();
    let dir = std::env::temp_dir().join(format!("kitae_dylib_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let spec = templates::by_name("hello").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("content")).unwrap();
    std::fs::create_dir_all(dir.join(".cargo")).unwrap();
    std::fs::write(dir.join("content/welcome.txt"), "hello from the test").unwrap();
    std::fs::write(dir.join("Cargo.toml"), templates::cargo_toml("demo-hot")).unwrap();
    std::fs::write(dir.join(".cargo/config.toml"), templates::cargo_config()).unwrap();
    std::fs::write(dir.join("src/lib.rs"), (spec.lib_rs)("demo-hot")).unwrap();
    std::fs::write(dir.join("src/main.rs"), (spec.main_rs)("demo-hot")).unwrap();

    let build = || {
        Command::new("cargo")
            .args(["build", "--quiet", "--lib"])
            .current_dir(&dir)
            .env("CARGO_TARGET_DIR", &target)
            .output()
            .expect("spawn cargo build")
    };
    let out = build();
    assert!(
        out.status.success(),
        "scaffolded site lib must build:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let dylib = target.join("debug").join("libdemo_hot.dylib");
    // The bare dlopen must resolve the dependency images: preload the
    // toolchain's std dylib (install name @rpath/libstd-<hash>.dylib) and
    // the workspace's kitae dylibs — dyld then matches the site dylib's
    // dependency install names against these loaded images. The real
    // host gets the same coalescing via prefer-dynamic instead.
    let libdir = Command::new("rustc")
        .args(["--print", "target-libdir"])
        .output()
        .expect("rustc --print target-libdir");
    let libdir = PathBuf::from(String::from_utf8_lossy(&libdir.stdout).trim());
    if let Ok(entries) = std::fs::read_dir(&libdir) {
        for e in entries.flatten() {
            let p = e.path();
            let std_dylib = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("libstd-") && n.ends_with(".dylib"));
            if std_dylib {
                let _ = kitae_live::preload_dylib(&p);
            }
        }
    }
    for dep in ["libkitae_ui.dylib", "libkitae_live.dylib"] {
        let p = target.join("debug").join(dep);
        if p.is_file() {
            kitae_live::preload_dylib(&p).expect("preload kitae dep");
        }
    }

    let mut watch = DylibWatch::new(&dylib);
    let routes = watch.load_routes().expect("fresh dylib loads");
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].title, "Hello");
    assert_eq!(routes[0].path, "/");

    // An edit + rebuild produces a NEW dylib: the watch re-fires and the
    // fresh routes load — the swap `kitae dev` performs under the server.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let edited = (spec.lib_rs)("demo-hot") + "\n// edited\n";
    std::fs::write(dir.join("src/lib.rs"), edited).unwrap();
    let out = build();
    assert!(
        out.status.success(),
        "rebuilt site lib must build:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(watch.changed(), "rebuilt dylib re-fires the watch");
    let routes = watch.load_routes().expect("rebuilt dylib loads");
    assert_eq!(routes[0].title, "Hello");

    std::fs::remove_dir_all(&dir).ok();
}
