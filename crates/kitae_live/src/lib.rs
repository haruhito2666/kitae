//! kitae_live — mount kitae_ui components as live websites.
//!
//! A dev's `Cargo.toml` needs exactly two Kitae crates: `kitae_ui` (the
//! components) and this one (the server). One route = one component tree:
//!
//! ```no_run
//! use kitae_ui::prelude::*;
//! use kitae_live::{Route, Server};
//! use std::rc::Rc;
//!
//! fn home() -> View {
//!     view! { h1 { "Hello, live web." } }
//! }
//!
//! let mut server = Server::new("127.0.0.1:8787").unwrap();
//! server.mount(Route::new("Home", Rc::new(home)));
//! server.serve(); // blocks; Ctrl-C to stop
//! ```
//!
//! What a plain browser (or curl, or a bot) gets: a complete static page —
//! [`crate::wire`] frames never leave the server. What a Kitae browser gets:
//! the same page plus a `KTLV` patch stream over WebSocket, so a signal
//! change on the server updates the page with [`kitae_ui::patch::PatchOp`]s
//! — never script.

use std::path::Path;

mod client;
mod dev;
mod hotreload;
mod http;
mod server;
mod session;
mod sha1;
mod wire;
mod ws;

pub use client::LiveClient;
pub use dev::{dir_changed, dir_stamps, Stamp};
pub use hotreload::{preload_dylib, target_dylib, DylibWatch};
pub use server::{Route, Server, ShutdownHandle, World};
pub use wire::{decode_frame, encode_frame, Frame, FrameKind, LIVE_VERSION};

thread_local! {
    /// The path of the request currently being answered — the SPA router's
    /// deep-link seed (`Router::new(request_path())`). Re-set before every
    /// session render, so each session reads ITS path.
    static REQUEST_PATH: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// The request path of the session whose render is running — read it when
/// creating a [`kitae_ui::Router`] so deep links (`/docs/intro`) open the
/// right page.
pub fn request_path() -> String {
    REQUEST_PATH.with(|p| p.borrow().clone())
}

pub(crate) fn set_request_path(path: &str) {
    REQUEST_PATH.with(|p| *p.borrow_mut() = path.to_string());
}

/// Static export (`kitae build`): render every route's current state to
/// `dist/<path>/index.html` — the complete no-socket document each route
/// already serves to plain browsers. Route paths map to directories
/// (`/` → `index.html`, `/docs` → `docs/index.html`); a trailing-slash
/// route keeps its name (`/docs/` → `docs/index.html`).
pub fn write_static(routes: &[Route], dist: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    use std::fs;
    fs::create_dir_all(dist)?;
    let mut written = Vec::new();
    for route in routes {
        let rel = route.path.trim_matches('/');
        let dir = if rel.is_empty() {
            dist.to_path_buf()
        } else {
            dist.join(rel)
        };
        fs::create_dir_all(&dir)?;
        let body = crate::server::render_static_page(&route.title, &route.render);
        let file = dir.join("index.html");
        fs::write(&file, body)?;
        written.push(file);
    }
    Ok(written)
}
