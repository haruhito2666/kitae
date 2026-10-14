//! `kitae` — the developer CLI for Kitae Live sites (Phase 3 of
//! docs/UI_LIBRARY_PLAN.md).
//!
//! ```text
//! kitae new <name> [--template hello|landing|docs|dashboard|form]
//! kitae dev [dir]    # run the site + watch: rust edits hot-swap it,
//!                    # content/ changes refresh clients in <100 ms
//! kitae build [dir]  # export every route to dist/ as static HTML
//! ```
//!
//! No argument-parser dependency: three subcommands, flags by hand. The
//! heavy lifting (serving, patching, exporting) lives in `kitae_live`;
//! the generated site binary is an ordinary Rust program the CLI drives
//! with `cargo`.

mod templates;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use kitae_live::{dir_changed, dir_stamps};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("new") => cmd_new(&args[1..]),
        Some("dev") => cmd_dev(&args[1..]),
        Some("build") => cmd_build(&args[1..]),
        Some("--help") | Some("-h") | Some("help") | None => {
            print_help();
            0
        }
        Some(other) => {
            eprintln!("unknown command `{other}` — try `kitae --help`");
            2
        }
    };
    std::process::exit(code);
}

fn print_help() {
    println!(
        "kitae — build live sites for the Kitae browser (no script, ever)\n\
         \n\
         USAGE:\n\
         \x20 kitae new <name> [--template hello|landing|docs|dashboard|form]\n\
         \x20     [--registry]  # deps from crates.io (after publish) instead\n\
         \x20                   # of path deps into this checkout\n\
         \x20     Scaffold a site project in ./<name>.\n\
         \x20 kitae dev [dir]\n\
         \x20     Run the site and watch it: saving Rust pages hot-swaps the\n\
         \x20     running server (clients stay connected); saving content/\n\
         \x20     pushes fresh state to connected pages in milliseconds. Open\n\
         \x20     the printed URL in Kitae.\n\
         \x20 kitae build [dir] [--out dist]\n\
         \x20     Export every route to static HTML (a complete static site —\n\
         \x20     the same pages Kitae renders live).\n\
         \n\
         A site is an ordinary Rust binary depending on kitae_ui + kitae_live."
    );
}

// ── kitae new ───────────────────────────────────────────────────────────────

fn cmd_new(args: &[String]) -> i32 {
    let Some(name) = args.first().filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: kitae new <name> [--template ...]");
        return 2;
    };
    let template = args
        .iter()
        .position(|a| a == "--template")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
        .unwrap_or("hello");
    // --registry: depend on the published crates.io versions instead of
    // the local checkout (meaningful once kitae_ui/kitae_live are
    // published; RP-C7 flips the default with one line then).
    let registry = args.iter().any(|a| a == "--registry");
    let Some(spec) = templates::by_name(template) else {
        eprintln!(
            "unknown template `{template}` — choose one of: {}",
            templates::names().join(", ")
        );
        return 2;
    };
    let dir = PathBuf::from(name);
    if dir.exists() {
        eprintln!("{} already exists — refusing to overwrite", dir.display());
        return 1;
    }
    match scaffold(&dir, name, spec, registry) {
        Ok(()) => {
            println!("created {name}/ from the `{template}` template");
            println!("next:");
            println!("  cd {name}");
            println!("  kitae dev        # then open the URL in Kitae");
            0
        }
        Err(e) => {
            eprintln!("scaffold failed: {e}");
            1
        }
    }
}

/// Write a project: Cargo.toml, .cargo/config.toml, src/lib.rs (the
/// pages), src/main.rs (the host), a content/ dir (the dev hot-reload
/// demo), README, .gitignore. `registry_deps` selects crates.io version
/// deps over path deps into the CLI's own checkout.
fn scaffold(
    dir: &Path,
    name: &str,
    spec: templates::Template,
    registry_deps: bool,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::create_dir_all(dir.join("content"))?;
    std::fs::create_dir_all(dir.join(".cargo"))?;
    std::fs::write(
        dir.join("Cargo.toml"),
        templates::cargo_toml_mode(name, registry_deps),
    )?;
    std::fs::write(dir.join(".cargo/config.toml"), templates::cargo_config())?;
    std::fs::write(dir.join("src/lib.rs"), (spec.lib_rs)(name))?;
    std::fs::write(dir.join("src/main.rs"), (spec.main_rs)(name))?;
    std::fs::write(
        dir.join("README.md"),
        templates::readme(name, spec.description),
    )?;
    std::fs::write(dir.join(".gitignore"), "/target\n/dist\n")?;
    std::fs::write(
        dir.join("content/welcome.txt"),
        "Edit this file and save — connected pages refresh instantly.\n",
    )?;
    Ok(())
}

// ── kitae dev ───────────────────────────────────────────────────────────────

fn cmd_dev(args: &[String]) -> i32 {
    let dir = positional_dir(args);
    if !dir.join("Cargo.toml").is_file() {
        eprintln!("{} is not a site project (no Cargo.toml)", dir.display());
        return 2;
    }
    println!("kitae dev — building…");
    if cfg!(unix) && dir.join("src/lib.rs").is_file() {
        cmd_dev_hot(&dir)
    } else {
        cmd_dev_restart(&dir)
    }
}

/// Hot dev loop for scaffolded sites (a `src/lib.rs` exists): the child is
/// the site binary itself — not a `cargo run` wrapper, so a restart kills
/// the site, not cargo around it — and the CLI only ever rebuilds:
/// * `src/lib.rs` (the pages) → `cargo build`; on success the RUNNING
///   server swaps in the fresh routes dylib itself, clients stay
///   connected. A failed build only prints — the server keeps serving the
///   last good routes.
/// * `src/main.rs` or `Cargo.toml` → rebuild + restart the host binary.
fn cmd_dev_hot(dir: &Path) -> i32 {
    let main_rs = dir.join("src/main.rs");
    let lib_rs = dir.join("src/lib.rs");
    let cargo_toml = dir.join("Cargo.toml");
    let mut main_stamp = file_stamp(&main_rs);
    let mut lib_stamp = file_stamp(&lib_rs);
    let mut toml_stamp = file_stamp(&cargo_toml);

    if !build_site(dir) {
        println!("fix the errors and save — the dev loop rebuilds on save");
    }
    let mut child = spawn_built(dir);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let toml_now = file_stamp(&cargo_toml);
        let main_now = file_stamp(&main_rs);
        let lib_now = file_stamp(&lib_rs);
        if main_now != main_stamp || toml_now != toml_stamp {
            main_stamp = main_now;
            toml_stamp = toml_now;
            println!("host changed — rebuilding…");
            if let Some(c) = child.as_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
            build_site(dir);
            child = spawn_built(dir);
        } else if lib_now != lib_stamp {
            lib_stamp = lib_now;
            println!("pages changed — rebuilding…");
            if build_site(dir) {
                // The running server watches its routes dylib and swaps it
                // in itself; clients stay connected through the swap.
                println!("rebuilt — the running server picks it up");
                if child.is_none() {
                    child = spawn_built(dir);
                }
            } else {
                println!("build failed — server keeps serving the last good routes");
            }
        }
        if let Some(c) = child.as_mut() {
            match c.try_wait() {
                Ok(Some(status)) => {
                    eprintln!("site exited ({status}) — stopping dev loop");
                    return if status.success() { 0 } else { 1 };
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("site poll failed: {e}");
                    return 1;
                }
            }
        }
    }
}

/// Fallback dev loop for sites without `src/lib.rs` (nothing to hot-swap):
/// any src/ edit rebuilds and restarts the whole server.
fn cmd_dev_restart(dir: &Path) -> i32 {
    let mut child = match spawn_dev(dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cargo run failed: {e}");
            return 1;
        }
    };
    // Watch the project's sources (recursive) for edits → restart. The
    // child itself watches content/ and hot-refreshes connected pages.
    let src = dir.join("src");
    let mut stamps = dir_stamps(&src);
    let cargo_toml_path = dir.join("Cargo.toml");
    let mut cargo_toml = file_stamp(&cargo_toml_path);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        if dir_changed(&mut stamps, &src) || cargo_toml != file_stamp(&cargo_toml_path) {
            println!("change detected — rebuilding…");
            let _ = child.kill();
            let _ = child.wait();
            child = match spawn_dev(&dir) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("cargo run failed: {e}");
                    return 1;
                }
            };
            stamps = dir_stamps(&src);
            cargo_toml = file_stamp(&cargo_toml_path);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                eprintln!("site exited ({status}) — stopping dev loop");
                return if status.success() { 0 } else { 1 };
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("site poll failed: {e}");
                return 1;
            }
        }
    }
}

/// Cheap fingerprint of one file — (mtime, size), mirroring the live
/// crate's dylib watch. Size catches same-second saves that only touch
/// mtime resolution (and vice versa); None when it doesn't exist yet.
fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// `cargo build` in the site dir: silent on success (the caller already
/// announced the rebuild), compiler errors on failure. True on success.
fn build_site(dir: &Path) -> bool {
    match Command::new("cargo").arg("build").current_dir(dir).output() {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            eprint!("{}", String::from_utf8_lossy(&o.stderr));
            false
        }
        Err(e) => {
            eprintln!("cargo build failed: {e}");
            false
        }
    }
}

/// Run the freshly built site binary with the dev env; the generated main
/// then watches content/ (refresh) and its routes dylib (hot swap).
///
/// The exec'd program is always the literal launcher `env`; the site
/// binary's path — parsed from Cargo.toml, so untrusted — travels only as
/// an argument, and only after passing two gates: `site_crate` accepts a
/// plain crate name (ASCII letters, digits, `-`, `_`) and the path must
/// canonicalize to a regular file directly inside the site's own
/// `target/debug`. Nothing is ever run through a shell. The canonicalized
/// absolute path also matters mechanically: the child chdirs into `dir`
/// before exec, so a relative path would resolve from the wrong place.
fn spawn_built(dir: &Path) -> Option<Child> {
    let name = site_crate(dir)?;
    let build_dir = dir.join("target").join("debug");
    let bin = build_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let (bin, build_dir) = match (bin.canonicalize(), build_dir.canonicalize()) {
        (Ok(b), Ok(d)) => (b, d),
        _ => {
            eprintln!("site is not built yet — fix the build first");
            return None;
        }
    };
    if bin.parent() != Some(build_dir.as_path()) || !bin.is_file() {
        eprintln!("unexpected file at {} — refusing to run it", bin.display());
        return None;
    }
    match Command::new("env")
        .arg(&bin)
        .current_dir(dir)
        .env("KITAE_DEV", "1")
        .spawn()
    {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("could not run {}: {e}", bin.display());
            None
        }
    }
}

/// The site's package name — the first `name = "…"` in Cargo.toml — which
/// names both the bin (`target/debug/<name>`) and, sanitized, the lib
/// dylib the running server hot-swaps. Only a plain crate name is
/// accepted (ASCII letters, digits, `-`, `_`): this value becomes a path
/// component and an executable name, so a Cargo.toml containing anything
/// else there is refused rather than executed.
fn site_crate(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name = \"") {
            let name = rest.strip_suffix('"')?;
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Some(name.to_string());
            }
            return None;
        }
    }
    None
}

/// `cargo run` with the dev env: the generated main serves + watches
/// content/ when KITAE_DEV=1.
fn spawn_dev(dir: &Path) -> std::io::Result<Child> {
    Command::new("cargo")
        .arg("run")
        .current_dir(dir)
        .env("KITAE_DEV", "1")
        .spawn()
}

// ── kitae build ─────────────────────────────────────────────────────────────

fn cmd_build(args: &[String]) -> i32 {
    let dir = positional_dir(args);
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "dist".to_string());
    if !dir.join("Cargo.toml").is_file() {
        eprintln!("{} is not a site project (no Cargo.toml)", dir.display());
        return 2;
    }
    println!("kitae build — exporting to {out}/");
    let status = Command::new("cargo")
        .args(["run", "--release", "--", "--build-static", &out])
        .current_dir(&dir)
        .status();
    match status {
        Ok(s) if s.success() => 0,
        Ok(s) => {
            eprintln!("site build failed ({s})");
            1
        }
        Err(e) => {
            eprintln!("cargo run failed: {e}");
            1
        }
    }
}

/// First non-flag arg, defaulting to the current directory.
fn positional_dir(args: &[String]) -> PathBuf {
    args.first()
        .filter(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
