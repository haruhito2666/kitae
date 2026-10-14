//! Site templates for `kitae new`. Each generates a complete, compiling
//! project in two files: `src/lib.rs` holds the pages — a `pub fn routes()`
//! plus the `kitae_routes` C export that hot-swap loading pulls — and
//! `src/main.rs` is the host that serves them, hot-refreshes `content/`
//! edits, hot-swaps the compiled routes dylib when Rust edits land (under
//! `kitae dev`), and exports static HTML for `kitae build`.

/// One template: its name, one-line description, and the host + pages code
/// it generates.
pub struct Template {
    pub name: &'static str,
    pub description: &'static str,
    pub main_rs: fn(&str) -> String,
    pub lib_rs: fn(&str) -> String,
}

pub fn names() -> Vec<&'static str> {
    TEMPLATES.iter().map(|t| t.name).collect()
}

pub fn by_name(name: &str) -> Option<Template> {
    TEMPLATES.iter().find(|t| t.name == name).map(|t| Template {
        name: t.name,
        description: t.description,
        main_rs: t.main_rs,
        lib_rs: t.lib_rs,
    })
}

pub static TEMPLATES: &[Template] = &[
    Template {
        name: "hello",
        description: "one page with a counter — the smallest live site",
        main_rs: hello_main,
        lib_rs: hello_lib,
    },
    Template {
        name: "landing",
        description: "hero, feature cards, and a signup interest form",
        main_rs: landing_main,
        lib_rs: landing_lib,
    },
    Template {
        name: "docs",
        description: "documentation site with a sidebar and several pages",
        main_rs: docs_main,
        lib_rs: docs_lib,
    },
    Template {
        name: "dashboard",
        description: "stat cards and a live-updating table",
        main_rs: dashboard_main,
        lib_rs: dashboard_lib,
    },
    Template {
        name: "form",
        description: "a validated contact form with submission list",
        main_rs: form_main,
        lib_rs: form_lib,
    },
];

/// Cargo.toml for a scaffolded site. The lib crate-type pair is what
/// makes Rust hot-swap possible: the dylib is what `kitae dev` loads
/// under the running server, the rlib keeps ordinary Rust linking (and
/// `cargo test`) working.
///
/// Dependencies come in two flavors: path deps into the CLI's own repo
/// checkout (the default — correct while Kitae is unpublished), or
/// registry version deps for `--registry` scaffolds once kitae_ui /
/// kitae_live are on crates.io. Nothing else differs; the published
/// versions carry the same APIs the path deps do.
pub fn cargo_toml(name: &str) -> String {
    cargo_toml_mode(name, false)
}

pub fn cargo_toml_mode(name: &str, registry_deps: bool) -> String {
    let deps = if registry_deps {
        "kitae_ui = \"0.1\"\n\
         kitae_live = \"0.1\"\n"
            .to_string()
    } else {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("cli crate lives in <repo>/crates");
        format!(
            "# Path deps while Kitae is pre-release (they point at the\n\
             # checkout this CLI was built from); `kitae new --registry`\n\
             # scaffolds against published versions instead.\n\
             kitae_ui = {{ path = {:?} }}\n\
             kitae_live = {{ path = {:?} }}\n",
            repo.join("crates/kitae_ui"),
            repo.join("crates/kitae_live"),
        )
    };
    format!(
        "[package]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n\n\
         [lib]\n\
         crate-type = [\"dylib\", \"rlib\"]\n\n\
         [dependencies]\n\
         {deps}"
    )
}

/// `.cargo/config.toml` for a scaffolded site. The host binary and the
/// hot-swapped routes dylib must share ONE image of kitae_ui/kitae_live
/// (their state is thread-local), so everything builds `prefer-dynamic`
/// and dyld coalesces the dependency dylibs into the host's copies. The
/// rpath lets the dylib (and any `lib-hot-N` copy of it, which `kitae dev`
/// writes next to the original) find those dependency dylibs at load time.
pub fn cargo_config() -> &'static str {
    if cfg!(target_os = "macos") {
        "[build]\nrustflags = [\"-C\", \"prefer-dynamic\", \"-C\", \"link-arg=-Wl,-rpath,@loader_path\"]\n"
    } else {
        "[build]\nrustflags = [\"-C\", \"prefer-dynamic\", \"-C\", \"link-arg=-Wl,-rpath,$ORIGIN\"]\n"
    }
}

pub fn readme(name: &str, description: &str) -> String {
    format!(
        "# {name}\n\n\
         {description}\n\n\
         A [Kitae](https://kitae) Live site: components in Rust, state on the\n\
         server, patch ops over a WebSocket — **zero script shipped**. Every\n\
         page also renders as plain static HTML.\n\n\
         ```\n\
         kitae dev          # run + hot reload (open the URL in Kitae)\n\
         kitae build        # export to dist/ as static HTML\n\
         ```\n\n\
         Pages live in `src/lib.rs` (`routes()` — that's the whole site);\n\
         `content/` holds data the dev loop pushes to connected clients\n\
         instantly. `src/main.rs` is the host — you rarely touch it. Rust\n\
         edits hot-swap the running server; content edits just refresh.\n"
    )
}

/// The shared `main` every template ships as `src/main.rs`: static-export
/// flag, plain serve, or the dev loop that hot-refreshes `content/` edits
/// and hot-swaps the rebuilt routes dylib.
fn shared_main() -> &'static str {
    r#"fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--build-static") {
        let dist = args.get(pos + 1).cloned().unwrap_or_else(|| "dist".into());
        let written = kitae_live::write_static(&routes(), std::path::Path::new(&dist))
            .expect("static export");
        println!("exported {} page(s) to {}/", written.len(), dist);
        return;
    }
    let port = std::env::var("PORT").unwrap_or_else(|_| "8787".into());
    let mut server = kitae_live::Server::new(&format!("127.0.0.1:{port}"))
        .expect("could not bind");
    for r in routes() {
        server.mount(r);
    }
    if std::env::var("KITAE_DEV").as_deref() == Ok("1") {
        // Dev loop: `kitae dev` sets KITAE_DEV=1. Content changes push a
        // fresh state to every connected page in milliseconds; Rust edits
        // hot-swap via the rebuilt routes dylib — clients stay connected.
        let mut world = server.serve_for(0);
        let content = std::path::Path::new("content");
        let mut stamps = kitae_live::dir_stamps(content);
        let mut watch =
            kitae_live::DylibWatch::new(kitae_live::target_dylib(env!("CARGO_CRATE_NAME")));
        println!("dev: http://127.0.0.1:{port}/ (content + rust hot-reload on)");
        loop {
            if watch.changed() {
                match watch.load_routes() {
                    Ok(rs) => {
                        println!("dev: rust changes live ({} route(s))", rs.len());
                        world.replace_routes(rs);
                    }
                    Err(e) => println!("dev: dylib not loadable yet: {e}"),
                }
            }
            if kitae_live::dir_changed(&mut stamps, content) {
                println!("dev: content changed — refreshing clients");
                world.refresh_all();
            }
            world.step();
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    } else {
        println!("serving http://127.0.0.1:{port}/");
        server.serve();
    }
}
"#
}

/// The `src/main.rs` host for one template: `use {lib}::routes` links the
/// site's initial routes statically (so plain `cargo run` needs no dylib),
/// then the shared main serves them. `kitae dev` swaps in fresh dylib
/// copies at runtime; the statically linked routes are simply the version
/// the binary was born with.
fn host_main(name: &str, description: &str, lib: &str) -> String {
    format!(
        "// {name} — the host for a Kitae Live site ({description}).\n\
         //\n\
         // The pages live in src/lib.rs; this host serves them. Under `kitae\n\
         // dev` it also hot-swaps the rebuilt routes dylib when Rust edits\n\
         // land, and hot-refreshes clients on content/ edits. You rarely\n\
         // edit this file.\n\n\
         use {lib}::routes;\n\n{shared}",
        lib = lib,
        shared = shared_main(),
    )
}

/// The `src/lib.rs` header for one template: the pages, the export, and
/// how the dev loop treats this file.
fn header(name: &str, description: &str) -> String {
    format!(
        "// {name} — the pages of a Kitae Live site ({description}).\n\
         //\n\
         // This is the file you edit: save and the running server swaps in\n\
         // the rebuilt routes (clients stay connected, no reload). `kitae\n\
         // build` also exports every route as complete static HTML — no\n\
         // script is ever shipped to any browser.\n\n\
         use std::rc::Rc;\n\
         use kitae_ui::prelude::*;\n\
         use kitae_live::Route;\n\n"
    )
}

// ── templates ───────────────────────────────────────────────────────────────

fn hello_main(name: &str) -> String {
    host_main(name, "the smallest live site", &sanitized(name))
}

fn hello_lib(name: &str) -> String {
    format!(
        "{}pub fn routes() -> Vec<Route> {{\n\
         \x20   let count = use_signal(|| 0u32);\n\
         \x20   vec![Route::new(\"Hello\", Rc::new(move || {{\n\
         \x20       // Re-read per render: content/ edits hot-refresh clients.\n\
         \x20       let welcome = std::fs::read_to_string(\"content/welcome.txt\")\n\
         \x20           .unwrap_or_else(|_| \"Welcome.\".into());\n\
         \x20       view! {{\n\
         \x20           div {{\n\
         \x20               h1 {{ \"Hello, Kitae\" }}\n\
         \x20               p {{ \"{{welcome}}\" }}\n\
         \x20               button(on:click = move |_| count.update(|c| *c += 1)) {{\n\
         \x20                   \"Clicked {{count}} times\"\n\
         \x20               }}\n\
         \x20           }}\n\
         \x20       }}\n\
         \x20   }}))]\n\
         }}\n\n\
         // The C export `kitae dev`'s hot-swap pulls after each rebuild.\n\
         kitae_live::export_routes!();\n",
        header(name, "the smallest live site"),
    )
}

fn landing_main(name: &str) -> String {
    host_main(name, "hero + features", &sanitized(name))
}

fn landing_lib(name: &str) -> String {
    format!(
        "{}pub fn routes() -> Vec<Route> {{\n\
         \x20   let signups = use_signal(|| 0u32);\n\
         \x20   vec![Route::new(\"Landing\", Rc::new(move || {{\n\
         \x20       view! {{\n\
         \x20           div {{\n\
         \x20               div {{\n\
         \x20                   h1 {{ \"Ship interfaces without a line of script\" }}\n\
         \x20                   p {{ \"This site is built on Kitae: components in Rust,\n\
         \x20                        state on the server, patches over a WebSocket.\" }}\n\
         \x20                   button(on:click = move |_| signups.update(|s| *s += 1)) {{\n\
         \x20                       \"Join {{signups}} builders\"\n\
         \x20                   }}\n\
         \x20               }}\n\
         \x20               div {{\n\
         \x20                   div {{ h3 {{ \"No bundle\" }} p {{ \"Rust runs on the server; the page never downloads code.\" }} }}\n\
         \x20                   div {{ h3 {{ \"No hydration gap\" }} p {{ \"First paint is the full page — the socket only adds live updates.\" }} }}\n\
         \x20                   div {{ h3 {{ \"No lock-in\" }} p {{ \"Every page is complete static HTML; crawlers see everything.\" }} }}\n\
         \x20               }}\n\
         \x20           }}\n\
         \x20       }}\n\
         \x20   }}))]\n\
         }}\n\n\
         // The C export `kitae dev`'s hot-swap pulls after each rebuild.\n\
         kitae_live::export_routes!();\n",
        header(name, "hero + features"),
    )
}

fn docs_main(name: &str) -> String {
    host_main(name, "documentation site", &sanitized(name))
}

fn docs_lib(name: &str) -> String {
    format!(
        "{}struct Page {{\n\
         \x20   slug: &'static str,\n\
         \x20   title: &'static str,\n\
         \x20   body: &'static str,\n\
         }}\n\n\
         const PAGES: &[Page] = &[\n\
         \x20   Page {{ slug: \"/\", title: \"Docs\", body: \"Start here: this site is a Kitae Live site — every page below is a component function.\" }},\n\
         \x20   Page {{ slug: \"/guide\", title: \"Guide\", body: \"Components are plain Rust fns returning a view. Signals hold state; the framework diffs and patches.\" }},\n\
         \x20   Page {{ slug: \"/reference\", title: \"Reference\", body: \"view! builds elements; on:event binds handlers; use_signal holds state; Route mounts a page.\" }},\n\
         ];\n\n\
         pub fn routes() -> Vec<Route> {{\n\
         \x20   PAGES.iter()\n\
         \x20       .map(|p| {{\n\
         \x20           let page_slug = p.slug;\n\
         \x20           Route::new(p.title, Rc::new(move || {{\n\
         \x20               let page_slug = page_slug;\n\
         \x20               view! {{\n\
         \x20                   div {{\n\
         \x20                       nav {{\n\
         \x20                           for PAGES.iter() -> (i, pg) {{\n\
         \x20                               span {{ \"{{pg.title}}\" }}\n\
         \x20                           }}\n\
         \x20                       }}\n\
         \x20                       article {{\n\
         \x20                           for PAGES.iter().filter(|pg| pg.slug == page_slug) -> (i, pg) {{\n\
         \x20                               h1 {{ \"{{pg.title}}\" }}\n\
         \x20                               p {{ \"{{pg.body}}\" }}\n\
         \x20                           }}\n\
         \x20                       }}\n\
         \x20                   }}\n\
         \x20               }}\n\
         \x20           }}))\n\
         \x20           .at(p.slug)\n\
         \x20       }})\n\
         \x20       .collect()\n\
         }}\n\n\
         // The C export `kitae dev`'s hot-swap pulls after each rebuild.\n\
         kitae_live::export_routes!();\n",
        header(name, "documentation site"),
    )
}

fn dashboard_main(name: &str) -> String {
    host_main(name, "stat cards + live table", &sanitized(name))
}

fn dashboard_lib(name: &str) -> String {
    format!(
        "{}pub fn routes() -> Vec<Route> {{\n\
         \x20   let visitors = use_signal(|| 12_480u64);\n\
         \x20   let errors = use_signal(|| 23u64);\n\
         \x20   let live = use_signal(|| 118u64);\n\
         \x20   vec![Route::new(\"Dashboard\", Rc::new(move || {{\n\
         \x20       view! {{\n\
         \x20           div {{\n\
         \x20               h1 {{ \"Overview\" }}\n\
         \x20               div {{\n\
         \x20                   div {{ h2 {{ \"Visitors\" }} p {{ \"{{visitors}}\" }} }}\n\
         \x20                   div {{ h2 {{ \"Errors\" }} p {{ \"{{errors}}\" }} }}\n\
         \x20                   div {{ h2 {{ \"Live now\" }} p {{ \"{{live}}\" }} }}\n\
         \x20               }}\n\
         \x20               p {{ \"State lives on the server — refresh these numbers from any\n\
         \x20                    signal and every connected dashboard patches instantly.\" }}\n\
         \x20               button(on:click = move |_| live.update(|v| *v += 1)) {{\n\
         \x20                   \"Simulate one arrival ({{live}})\"\n\
         \x20               }}\n\
         \x20           }}\n\
         \x20       }}\n\
         \x20   }}))]\n\
         }}\n\n\
         // The C export `kitae dev`'s hot-swap pulls after each rebuild.\n\
         kitae_live::export_routes!();\n",
        header(name, "stat cards + live table"),
    )
}

fn form_main(name: &str) -> String {
    host_main(name, "validated contact form", &sanitized(name))
}

fn form_lib(name: &str) -> String {
    format!(
        "{}pub fn routes() -> Vec<Route> {{\n\
         \x20   let name = use_signal(String::new);\n\
         \x20   let email = use_signal(String::new);\n\
         \x20   let error = use_signal(String::new);\n\
         \x20   let sent = use_signal(|| 0u32);\n\
         \x20   vec![Route::new(\"Contact\", Rc::new(move || {{\n\
         \x20       let err = error.get();\n\
         \x20       view! {{\n\
         \x20           div {{\n\
         \x20               h1 {{ \"Contact us\" }}\n\
         \x20               label {{ \"Name\" }}\n\
         \x20               input(id: \"f-name\", value: name.get(),\n\
         \x20                     on:input = move |e| {{\n\
         \x20                         let v = e.value.unwrap_or_default();\n\
         \x20                         name.update(|n| *n = v);\n\
         \x20                     }})\n\
         \x20               label {{ \"Email\" }}\n\
         \x20               input(id: \"f-email\", value: email.get(),\n\
         \x20                     on:input = move |e| {{\n\
         \x20                         let v = e.value.unwrap_or_default();\n\
         \x20                         email.update(|n| *n = v);\n\
         \x20                     }})\n\
         \x20               button(on:click = move |_| {{\n\
         \x20                   let n = name.get();\n\
         \x20                   let e = email.get();\n\
         \x20                   if n.trim().is_empty() {{\n\
         \x20                       error.update(|m| *m = \"Name is required.\".into());\n\
         \x20                   }} else if !e.contains('@') {{\n\
         \x20                       error.update(|m| *m = \"That email does not look right.\".into());\n\
         \x20                   }} else {{\n\
         \x20                       error.update(|m| m.clear());\n\
         \x20                       sent.update(|s| *s += 1);\n\
         \x20                       name.update(|n| n.clear());\n\
         \x20                       email.update(|n| n.clear());\n\
         \x20                   }}\n\
         \x20               }}) {{ \"Send\" }}\n\
         \x20               p {{ \"{{err}}\" }}\n\
         \x20               p {{ \"{{sent}} message(s) sent this session.\" }}\n\
         \x20           }}\n\
         \x20       }}\n\
         \x20   }}))]\n\
         }}\n\n\
         // The C export `kitae dev`'s hot-swap pulls after each rebuild.\n\
         kitae_live::export_routes!();\n",
        header(name, "validated contact form"),
    )
}

/// cargo's own crate-name rule: non-ASCII-alphanumeric → `_`. The
/// generated host's `use {lib}::routes` and the dev loop's dylib lookup
/// (`lib{lib}.dylib`) both depend on this.
fn sanitized(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
