//! The kitae_live server: routes, connections, sessions — one thread.
//!
//! kitae_ui signals are thread-local by design, so a session (and every
//! patch it produces) must stay on the thread that mounted it. The server
//! is therefore a classic non-blocking accept/poll loop: all sockets are
//! non-blocking, every session lives on this thread, and no `Send` bounds
//! ever touch a component tree. `Rc` all the way down.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::time::Duration;

use kitae_ui::vnode::View;

use crate::http::{self, Request};
use crate::session::Session;
use crate::wire::{decode_frame, encode_frame, Frame};
use crate::ws::{ReadResult, WsStream, CLOSE_PROTOCOL};

/// Encode a frame, or drop it with a note when a field is over the wire's
/// length limits. A corrupt-but-sent frame would desync the client; not
/// sending is the recoverable failure (the client re-syncs via Ack/Init).
fn encode_or_warn(frame: &Frame) -> Option<Vec<u8>> {
    match encode_frame(frame) {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("[kitae live] frame encode failed (not sent): {e}");
            None
        }
    }
}

/// One mounted route.
pub struct Route {
    /// The path the site serves (`"/"`, `"/todos"`).
    pub path: String,
    /// Render closure: rebuilds the current view from signals.
    pub render: Rc<dyn Fn() -> View>,
    /// `<title>` for the static fallback page.
    pub title: String,
    /// SPA mode: this route catches EVERY path no specific route claims —
    /// the single-mount router story (`Router` + `Link` render in-page).
    pub spa: bool,
}

impl Route {
    /// Mount a render closure with its page title. The closure is wrapped
    /// so every build re-enters the hook scopes — `use_signal` inside
    /// `#[component]` bodies stays stable across re-renders.
    pub fn new(title: &str, render: Rc<dyn Fn() -> View>) -> Route {
        Route {
            path: "/".into(),
            render: kitae_ui::hooks::wrap_render(render),
            title: title.into(),
            spa: false,
        }
    }

    /// Serve at `path` instead of `/`.
    pub fn at(mut self, path: &str) -> Route {
        self.path = path.into();
        self
    }

    /// SPA mode: catch every path no specific route claims. The component
    /// reads the request path via [`crate::request_path`] (deep links) and
    /// navigates in-page with [`kitae_ui::Router`] + [`kitae_ui::Link`].
    pub fn spa(mut self) -> Route {
        self.spa = true;
        self
    }
}

/// The route that serves `path`: the longest matching specific route, or
/// the SPA catch-all. One resolver for connection time and hot reload —
/// a swap must re-point sessions at exactly the route a fresh connection
/// would get.
fn resolve_route<'a>(routes: &'a [Route], path: &str) -> Option<&'a Route> {
    routes
        .iter()
        .filter(|r| !r.spa && path_serves(r.path.as_str(), path))
        .max_by_key(|r| r.path.len())
        .or_else(|| routes.iter().find(|r| r.spa))
}

/// The single-threaded world the loop owns: accept, handshakes in flight,
/// live sessions, static file dirs, and static responses (a plain GET
/// answers inline).
pub struct World {
    listener: TcpListener,
    routes: Vec<Route>,
    sessions: HashMap<String, Session>,
    session_counter: u64,
    /// Idle sessions evicted after this long without any socket activity.
    session_ttl: Duration,
    /// Mounted static-file directories: (url prefix, filesystem root).
    dirs: Vec<(String, std::path::PathBuf)>,
    /// Sockets mid-HTTP-head, keyed by connection id (raw fds recycle after
    /// close, so they never key state).
    heads: Vec<(u64, TcpStream)>,
    head_bufs: HashMap<u64, Vec<u8>>,
    /// When each in-flight head first arrived — a head that never completes
    /// would otherwise park its socket (and buffer) forever.
    head_since: HashMap<u64, std::time::Instant>,
    /// How long a half-open HTTP head may sit before its socket is dropped.
    head_ttl: Duration,
    conn_counter: u64,
    /// Live sockets with their session ids, plus whether this socket has
    /// sent its Init yet (Init is per-SOCKET — every fresh socket hydrates,
    /// even one resuming a long-lived session).
    live: Vec<(WsStream, String, bool)>,
    /// Set by the [`ShutdownHandle`]; `run_until_stopped` returns when set.
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// A live server: mount routes, then `serve` (blocking until shutdown).
/// `kitae_live` sites serve complete static HTML to plain browsers and a
/// KTLV patch stream to Kitae.
pub struct Server {
    listener: TcpListener,
    routes: Vec<Route>,
    dirs: Vec<(String, std::path::PathBuf)>,
    session_ttl: Duration,
    head_ttl: Duration,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// A handle that shuts the server's `serve()` loop down from another
/// thread or a signal handler.
#[derive(Clone)]
pub struct ShutdownHandle(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl ShutdownHandle {
    /// Ask the serve loop to stop at its next iteration.
    pub fn shutdown(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Server {
    pub fn new(addr: &str) -> std::io::Result<Server> {
        Ok(Server {
            listener: TcpListener::bind(addr)?,
            routes: Vec::new(),
            dirs: Vec::new(),
            session_ttl: Duration::from_secs(3600),
            head_ttl: Duration::from_secs(10),
            shutdown: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// The bound address — `127.0.0.1:0` callers read this for their port.
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Mount one route. A later mount for the same path wins.
    pub fn mount(&mut self, route: Route) {
        self.routes.push(route);
    }

    /// Serve files from `fs_path` under `url_prefix` (e.g. `/static`).
    /// Paths are canonicalized and confined to the root — traversal is
    /// refused. Routes take precedence over files on overlap.
    pub fn mount_dir(&mut self, url_prefix: &str, fs_path: impl Into<std::path::PathBuf>) {
        let prefix = url_prefix.trim_end_matches('/').to_string();
        self.dirs.push((prefix, fs_path.into()));
    }

    /// How long an idle session survives (default 1 h). A resumed session
    /// past its TTL comes back fresh.
    pub fn set_session_ttl(&mut self, ttl: Duration) {
        self.session_ttl = ttl;
    }

    /// How long a half-open HTTP head may sit before its socket is dropped
    /// (default 10 s). A stalled head parks a socket and a buffer — a client
    /// that never finishes its request must not hold them forever.
    pub fn set_head_ttl(&mut self, ttl: Duration) {
        self.head_ttl = ttl;
    }

    /// A handle that stops `serve()`.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle(self.shutdown.clone())
    }

    /// Accept + poll loop. Blocks until [`ShutdownHandle::shutdown`].
    pub fn serve(self) {
        let mut world = self.world();
        world.run_until_stopped();
    }

    /// Test/embedder harness: run a bounded number of loop iterations, then
    /// hand the world back (it resumes where it stopped). kitae_ui state is
    /// thread-local, so the loop must run on the thread that built the
    /// routes — `serve_for` lets a test thread pump the server and its
    /// clients in lockstep.
    pub fn serve_for(self, rounds: usize) -> World {
        let mut world = self.world();
        for _ in 0..rounds {
            world.step();
            std::thread::sleep(Duration::from_millis(1));
        }
        world
    }

    fn world(self) -> World {
        self.listener
            .set_nonblocking(true)
            .expect("listener goes non-blocking");
        World {
            listener: self.listener,
            routes: self.routes,
            sessions: HashMap::new(),
            session_counter: 0,
            session_ttl: self.session_ttl,
            dirs: self.dirs,
            heads: Vec::new(),
            head_bufs: HashMap::new(),
            head_since: HashMap::new(),
            head_ttl: self.head_ttl,
            conn_counter: 0,
            live: Vec::new(),
            shutdown: self.shutdown,
        }
    }
}

impl World {
    /// Poll loop: step every 2 ms until the shutdown handle fires. Same-
    /// thread (the world owns thread-local signal state) — embedders run
    /// it where they built the routes.
    pub fn run_until_stopped(&mut self) {
        while !self.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            self.step();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// One poll iteration: evict idle sessions, accept, HTTP heads, live
    /// sockets, finished fetches.
    pub fn step(&mut self) {
        self.evict_expired_sessions();
        self.accept();
        self.poll_heads();
        self.poll_live();
        // `use_resource` fetches land on background threads; the drain
        // runs here (same thread as the signals). When a result landed,
        // every session's tree may have changed: re-render + broadcast.
        if kitae_ui::resource::drain() {
            self.broadcast_updates();
        }
    }

    /// Drop sessions idle past the TTL — state is server-side and finite;
    /// a resumed session past eviction comes back fresh.
    fn evict_expired_sessions(&mut self) {
        let cutoff = std::time::Instant::now() - self.session_ttl;
        self.sessions.retain(|_, s| s.last_seen() > cutoff);
    }

    /// Re-render ONE session and stream the diff to its sockets — the
    /// targeted server-push primitive (`broadcast_updates` is the global
    /// form).
    pub fn push_session(&mut self, id: &str) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let (seq, ops) = session.update();
        if ops.is_empty() {
            return;
        }
        let frame = encode_or_warn(&Frame::Patch { seq, ops });
        for (ws, live_id, _) in self.live.iter_mut() {
            if live_id == id {
                if let Some(bytes) = &frame {
                    let _ = ws.send_binary(bytes);
                }
            }
        }
    }

    /// Re-render every session; stream non-empty diffs to all its sockets.
    /// This is also the server-push primitive: any code that changes a
    /// signal outside an event (resources, background jobs) ends with one
    /// call here.
    pub fn broadcast_updates(&mut self) {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            let Some(session) = self.sessions.get_mut(&id) else {
                continue;
            };
            let (seq, ops) = session.update();
            if ops.is_empty() {
                continue;
            }
            let frame = encode_or_warn(&Frame::Patch { seq, ops });
            for (ws, live_id, _) in self.live.iter_mut() {
                if live_id == &id {
                    if let Some(bytes) = &frame {
                        let _ = ws.send_binary(bytes);
                    }
                }
            }
        }
    }

    /// Push a fresh Init + hydration patch to EVERY connected socket —
    /// the hot-reload primitive: `kitae dev` calls this when a content
    /// file changes, and mounted pages update in well under 100 ms
    /// (one KTLV write per socket, no client round-trip needed).
    pub fn refresh_all(&mut self) {
        let live: Vec<(WsStream, String, bool)> = std::mem::take(&mut self.live);
        for (mut ws, id, _init_sent) in live {
            let Some(session) = self.sessions.get_mut(&id) else {
                continue;
            };
            let (html, ops) = session.init();
            let init_bytes = encode_or_warn(&Frame::Init {
                session: id.clone(),
                html,
            });
            let Some(init_bytes) = init_bytes else {
                continue; // dead socket: dropped (reconnect resumes)
            };
            if ws.send_binary(&init_bytes).is_err() {
                continue; // dead socket: dropped (reconnect resumes)
            }
            if !ops.is_empty() {
                if let Some(bytes) = encode_or_warn(&Frame::Patch { seq: 0, ops }) {
                    let _ = ws.send_binary(&bytes);
                }
            }
            if !ws.is_closed() {
                self.live.push((ws, id, true));
            }
        }
    }

    /// Hot reload of Rust edits: swap in a new route table and re-point
    /// every connected session at the route its path resolves to now,
    /// then [`refresh_all`] so each socket gets a fresh Init from the new
    /// render. A session whose path no longer resolves keeps its old
    /// render (its route was removed — the page still works, a reload
    /// would 404).
    pub fn replace_routes(&mut self, routes: Vec<Route>) {
        self.routes = routes;
        let routes = &self.routes;
        let sessions = &mut self.sessions;
        for (_, session) in sessions.iter_mut() {
            if let Some(route) = resolve_route(routes, session.path()) {
                session.set_render(route.render.clone());
            }
        }
        self.refresh_all();
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((sock, _)) => {
                    let _ = sock.set_nonblocking(true);
                    self.conn_counter += 1;
                    let id = self.conn_counter;
                    self.heads.push((id, sock));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(_) => return,
            }
        }
    }

    /// Sockets still speaking HTTP: read what's there, try to finish the
    /// head, and answer (static page, 404, or WebSocket upgrade). A head
    /// over the size cap is rejected (431) instead of silently parking; a
    /// head idle past [`World::head_ttl`] is dropped.
    fn poll_heads(&mut self) {
        // Reap heads that never completed — drop the socket AND its buffer.
        let cutoff = std::time::Instant::now() - self.head_ttl;
        let expired: Vec<u64> = self
            .head_since
            .iter()
            .filter(|(_, t)| **t < cutoff)
            .map(|(k, _)| *k)
            .collect();
        if !expired.is_empty() {
            let expired_set: std::collections::HashSet<u64> = expired.into_iter().collect();
            self.heads.retain(|(k, _)| !expired_set.contains(k));
            for k in &expired_set {
                self.head_bufs.remove(k);
                self.head_since.remove(k);
            }
        }

        let mut still_heads = Vec::new();
        let heads: Vec<(u64, TcpStream)> = std::mem::take(&mut self.heads);
        for (key, mut sock) in heads {
            self.head_since
                .entry(key)
                .or_insert_with(std::time::Instant::now);
            let mut over_cap = false;
            {
                let buf = self.head_bufs.entry(key).or_default();
                let mut tmp = [0u8; 4096];
                loop {
                    match sock.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.len() > 32 * 1024 {
                                over_cap = true;
                                break;
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::Interrupted =>
                        {
                            break
                        }
                        Err(_) => {
                            buf.clear();
                            break;
                        }
                    }
                }
            }
            if over_cap {
                // Hostile head: reject outright — not "keep waiting".
                let _ = http::write_response(
                    &mut sock,
                    "431 Request Header Fields Too Large",
                    "text/plain",
                    b"request head too large",
                );
                self.head_bufs.remove(&key);
                self.head_since.remove(&key);
                continue;
            }
            let buf = self.head_bufs.get(&key).cloned().unwrap_or_default();
            if let Some(head_end) = find_head_end(&buf) {
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let body_rest = buf[head_end + 4..].to_vec();
                self.head_bufs.remove(&key);
                self.head_since.remove(&key);
                if let Some(req) = parse_head(&head) {
                    self.answer(sock, req, body_rest);
                    continue;
                }
                let _ = http::write_response(
                    &mut sock,
                    "400 Bad Request",
                    "text/plain",
                    b"malformed request",
                );
                continue;
            }
            if buf.is_empty() {
                // Nothing arrived yet (or the socket reset): keep waiting.
                self.head_bufs.insert(key, buf);
                still_heads.push((key, sock));
                continue;
            }
            // Partial head: keep the bytes and the socket (until the TTL).
            self.head_bufs.insert(key, buf);
            still_heads.push((key, sock));
        }
        self.heads = still_heads;
    }

    fn answer(&mut self, sock: TcpStream, req: Request, _body_rest: Vec<u8>) {
        // S-4 (SECURITY_AUDIT.md): a browser-tagged foreign origin means
        // some other website is scripting this server — the localhost-CSRF
        // class. Refused before any routing. Header-less clients (curl,
        // Kitae's engine client, same-thread tests) pass untouched.
        let server_port = self
            .listener
            .local_addr()
            .ok()
            .map(|address| address.port());
        if !origin_permitted(req.origin.as_deref(), server_port) {
            let mut s = sock;
            let _ = http::write_response(
                &mut s,
                "403 Forbidden",
                "text/plain",
                b"cross-origin request refused",
            );
            return;
        }
        // Longest route whose path is the request path or a path-segment
        // prefix of it ("/todos" serves "/todos" and "/todos/x"; "/" serves
        // everything — but never a non-segment prefix like "/nope").
        // Live sessions ride GET upgrades; HEAD is served like a GET
        // without the body.
        let head_only = req.method == "HEAD";
        if req.method != "GET" && !head_only {
            let mut s = sock;
            let _ =
                http::write_response(&mut s, "405 Method Not Allowed", "text/plain", b"GET only");
            return;
        }
        crate::set_request_path(&req.target);
        // Mounted static dirs: files under a routeless prefix ("/static").
        if let Some(response) = self.serve_static_dir(&req.path, head_only) {
            let mut s = sock;
            let _ = response.write_to(&mut s);
            return;
        }
        // Specific routes first; an SPA route catches whatever no specific
        // route claimed (single-mount in-page routing).
        let Some(route) = resolve_route(&self.routes, &req.path) else {
            let mut s = sock;
            let body = not_found_page(&req.path);
            let _ = http::write_response(
                &mut s,
                "404 Not Found",
                "text/html; charset=utf-8",
                body.as_bytes(),
            );
            return;
        };
        if let Some(key) = &req.ws_key {
            let mut out = match sock.try_clone() {
                Ok(s) => s,
                Err(_) => return,
            };
            if out
                .write_all(http::ws_accept_response(key).as_bytes())
                .is_err()
            {
                return;
            }
            // Session id: a resuming client names its old one via the
            // `Sec-WebSocket-Protocol` line ("kitae, resume <id>"); a fresh
            // connection gets a new one — an unforgeable token, not a
            // counter: session ids ARE the resume capability, and a
            // guessable id would let any peer hijack another tab's state.
            let resume = resume_token(req.ws_protocol.as_deref());
            let id = resume.unwrap_or_else(|| {
                self.session_counter += 1;
                new_session_token(self.session_counter)
            });
            crate::set_request_path(&req.target);
            self.sessions
                .entry(id.clone())
                .or_insert_with(|| Session::new(route.render.clone()));
            let mut ws = WsStream::new(sock, true);
            let _ = ws.set_nonblocking(true);
            self.live.push((ws, id, false));
            return;
        }
        let mut s = sock;
        let html = static_page(&route.title, &route.render);
        let _ = http::write_response(
            &mut s,
            "200 OK",
            "text/html; charset=utf-8",
            html.as_bytes(),
        );
    }

    /// Serve a file from a mounted static dir, if the path falls under one.
    /// `None` = not a static-dir path (fall through to routes).
    fn serve_static_dir(&mut self, path: &str, head_only: bool) -> Option<StaticResponse> {
        for (prefix, root) in &self.dirs {
            let rest = path.strip_prefix(prefix.as_str())?;
            if !rest.is_empty() && !rest.starts_with('/') {
                continue; // "/staticX" must not match prefix "/static"
            }
            let rel = rest.trim_start_matches('/');
            if rel.is_empty() {
                continue; // a bare dir hit is the route table's job
            }
            // Confine to the root: canonicalize both sides and compare.
            let target = root.join(rel);
            let Ok(canonical_root) = root.canonicalize() else {
                return Some(StaticResponse::not_found());
            };
            let Ok(canonical) = target.canonicalize() else {
                return Some(StaticResponse::not_found());
            };
            if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
                return Some(StaticResponse::not_found());
            }
            let body = std::fs::read(&canonical).ok()?;
            let mut response = StaticResponse::file(body, head_only);
            response.content_type = content_type_for(&canonical);
            return Some(response);
        }
        None
    }

    /// Live sockets: Init on entry, then event → dispatch → patch streaming.
    fn poll_live(&mut self) {
        let mut keep = Vec::new();
        let live: Vec<(WsStream, String, bool)> = std::mem::take(&mut self.live);
        for (mut ws, id, mut init_sent) in live {
            if !init_sent {
                // First poll after the upgrade: send Init + hydration patch.
                let Some(session) = self.sessions.get_mut(&id) else {
                    continue;
                };
                session.touch();
                let (html, ops) = session.init();
                let init_ok = encode_or_warn(&Frame::Init {
                    session: id.clone(),
                    html,
                })
                .map(|bytes| ws.send_binary(&bytes).is_ok())
                .unwrap_or(false);
                if !init_ok {
                    continue;
                }
                if !ops.is_empty() {
                    if let Some(bytes) = encode_or_warn(&Frame::Patch { seq: 0, ops }) {
                        if ws.send_binary(&bytes).is_err() {
                            continue;
                        }
                    }
                }
                init_sent = true;
            }
            // Drain every complete message the socket has.
            loop {
                match ws.read_message() {
                    ReadResult::Message(bytes) => {
                        let Some(frame) = decode_frame(&bytes) else {
                            let _ = ws.close(CLOSE_PROTOCOL, "bad KTLV frame");
                            break;
                        };
                        let Some(session) = self.sessions.get_mut(&id) else {
                            break;
                        };
                        match frame {
                            Frame::Event {
                                element,
                                name,
                                value,
                            } => {
                                if session.dispatch(element, &name, value) {
                                    session.touch();
                                    let (seq, ops) = session.update();
                                    if !ops.is_empty() {
                                        if let Some(bytes) =
                                            encode_or_warn(&Frame::Patch { seq, ops })
                                        {
                                            if ws.send_binary(&bytes).is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            Frame::Ping => {
                                let _ = ws.send_control(false, &[]);
                            }
                            Frame::Ack { seq: acked } => {
                                let Some(session) = self.sessions.get_mut(&id) else {
                                    break;
                                };
                                match session.resume_from(acked) {
                                    Some(replay) => {
                                        let mut failed = false;
                                        for (seq, ops) in replay {
                                            let sent = encode_or_warn(&Frame::Patch { seq, ops })
                                                .map(|bytes| ws.send_binary(&bytes).is_ok())
                                                .unwrap_or(false);
                                            if !sent {
                                                failed = true;
                                                break;
                                            }
                                        }
                                        if failed {
                                            break;
                                        }
                                    }
                                    None => {
                                        // History window gone: re-Init.
                                        let (html, ops) = session.init();
                                        let init_ok = encode_or_warn(&Frame::Init {
                                            session: id.clone(),
                                            html,
                                        })
                                        .map(|bytes| ws.send_binary(&bytes).is_ok())
                                        .unwrap_or(false);
                                        if !init_ok {
                                            break;
                                        }
                                        if !ops.is_empty() {
                                            if let Some(bytes) =
                                                encode_or_warn(&Frame::Patch { seq: 0, ops })
                                            {
                                                if ws.send_binary(&bytes).is_err() {
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Frame::Pong => {}
                            _ => {
                                let _ = ws.close(CLOSE_PROTOCOL, "unexpected frame");
                                break;
                            }
                        }
                        continue;
                    }
                    ReadResult::WouldBlock => break, // keep the socket
                    ReadResult::Closed => {
                        // Sessions stay: reconnect resumes them by id.
                        break;
                    }
                }
            }
            if !ws.is_closed() {
                keep.push((ws, id, init_sent));
            }
        }
        self.live = keep;
    }
}

/// The static (progressive-enhancement) document for a route.
pub(crate) fn render_static_page(title: &str, render: &Rc<dyn Fn() -> View>) -> String {
    static_page(title, render)
}

/// The static (progressive-enhancement) document for a route.
fn static_page(title: &str, render: &Rc<dyn Fn() -> View>) -> String {
    let view = render();
    let body = kitae_ui::render::render_html(&view.nodes);
    format!(
        "<!doctype html><html><head><meta name=\"kitae-live\" content=\"1\">\
<title>{}</title></head><body>{}</body></html>",
        escape(title),
        body
    )
}

/// Minimal HTML escaping for authored text (titles).
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn not_found_page(path: &str) -> String {
    format!(
        "<!doctype html><html><head><title>Not found — Kitae Live</title></head>\
         <body><h1>Not found</h1><p>No route is mounted at <b>{}</b>.</p></body></html>",
        escape(path)
    )
}

/// Find the `\r\n\r\n` that ends an HTTP head, if fully arrived.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Does route `route` serve request path `req`? Exact match, or a
/// path-segment prefix. `/` serves exactly `/` — it is not a catch-all;
/// mount a route at `/docs/` (trailing slash) to serve a subtree.
fn path_serves(route: &str, req: &str) -> bool {
    if route == req {
        return true;
    }
    if route == "/" {
        return false;
    }
    if route.ends_with('/') {
        // Trailing-slash routes own their subtree ("/docs/" serves
        // "/docs/x"); the slash itself IS the segment boundary.
        return req.starts_with(route);
    }
    // A bare route serves exactly itself and deeper segment-aligned paths
    // ("/todos" → "/todos", "/todos/new"; never "/todo-x").
    req.starts_with(route) && req.as_bytes().get(route.len()) == Some(&b'/')
}

/// A prepared static-file response (HEAD-aware: headers always, body only
/// for GET).
struct StaticResponse {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    head_only: bool,
}

impl StaticResponse {
    fn file(body: Vec<u8>, head_only: bool) -> StaticResponse {
        StaticResponse {
            status: "200 OK",
            content_type: "application/octet-stream",
            body,
            head_only,
        }
    }

    fn not_found() -> StaticResponse {
        StaticResponse {
            status: "404 Not Found",
            content_type: "text/plain",
            body: b"no such file".to_vec(),
            head_only: false,
        }
    }

    fn write_to(&self, s: &mut TcpStream) -> std::io::Result<()> {
        let head = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        s.write_all(head.as_bytes())?;
        if !self.head_only {
            s.write_all(&self.body)?;
        }
        s.flush()
    }
}

/// Content type by file extension (the dev-server minimum).
fn content_type_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// S-4 gate (SECURITY_AUDIT.md): may this request proceed? A request that
/// carries an `Origin` header must originate from this server's own origin
/// (`http://127.0.0.1:<port>` / `http://localhost:<port>` / `null` as a
/// sandboxed page sends) — any other value is a browser-scripted
/// cross-site request (the localhost-CSRF class) and is refused. Clients
/// that send no `Origin` (curl, Kitae's engine client, the tests) pass.
///
/// Host matching is EXACT on the authority: a prefix match would let
/// `http://127.0.0.1.evil.com` (an attacker's domain that merely starts
/// with our host) through the gate.
fn origin_permitted(origin: Option<&str>, server_port: Option<u16>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let o = origin.trim();
    if o == "null" {
        return true;
    }
    let Some(authority) = o.strip_prefix("http://").and_then(|h| h.split('/').next()) else {
        return false; // https:// or anything else: not this server's origin
    };
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    // A port, if present, must be numeric: "127.0.0.1:8080.evil.com" is a
    // foreign HOST, not our port.
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !port_ok {
        return false;
    }
    if let Some(expected) = server_port {
        let origin_port = port
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(80);
        if origin_port != expected {
            return false;
        }
    }
    host == "127.0.0.1" || host == "localhost"
}

/// The session id a client asked to resume, parsed STRICTLY: the
/// `Sec-WebSocket-Protocol` header must be the list form `kitae, resume
/// <token>` — exactly the wire format our own client sends. The old
/// `split("resume")` accepted any header CONTAINING the substring
/// ("presumable", "resumeXYZ", a client-named protocol literally called
/// `resume`) as a resume attempt with attacker-chosen garbage for the id.
fn resume_token(ws_protocol: Option<&str>) -> Option<String> {
    let p = ws_protocol?.trim();
    let mut parts = p.split(',').map(str::trim);
    let first = parts.next()?;
    if !first.eq_ignore_ascii_case("kitae") {
        return None;
    }
    let rest = parts.next()?;
    let (word, token) = rest.split_once(char::is_whitespace)?;
    if !word.eq_ignore_ascii_case("resume") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() || token.contains(',') {
        return None;
    }
    Some(token.to_string())
}

/// A session token: the resume capability. Drawn from the operating
/// system's CSPRNG via `getrandom` (S-3, SECURITY_AUDIT.md) — the previous
/// SHA-1-of-clock+counter scheme left the entropy at "whatever the wall
/// clock happened to be", which an attacker who can read the server's
/// `Date` header can brute-force offline. (The doc comment used to claim
/// "unforgeable"; the RNG is what actually makes it true.)
fn new_session_token(_counter: u64) -> String {
    let mut bytes = [0u8; 10];
    getrandom::getrandom(&mut bytes).expect("OS RNG unavailable");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("s-{hex}")
}

/// Parse a complete head into a [`Request`].
fn parse_head(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let path = target.split('?').next().unwrap_or("").to_string();
    let mut ws_key = None;
    let mut ws_protocol = None;
    let mut origin = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "sec-websocket-key" => ws_key = Some(value.to_string()),
            "sec-websocket-protocol" => ws_protocol = Some(value.to_string()),
            "origin" => origin = Some(value.to_string()),
            _ => {}
        }
    }
    if method.is_empty() || path.is_empty() {
        return None;
    }
    Some(Request {
        method,
        path,
        target,
        ws_key,
        ws_protocol,
        origin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{decode_frame, encode_frame, Frame};
    use crate::ws::{ReadResult, WsStream};
    use kitae_ui::patch::PatchOp;
    use kitae_ui::prelude::*;

    /// A raw HTTP/WebSocket test client.
    struct TestClient;

    impl TestClient {
        /// Send a plain GET and pump the loop until the full response lands.
        /// Same-thread lockstep: every client read is wedged between
        /// `world.step()`s so neither side can starve the other.
        fn static_get(world: &mut World, addr: &str, path: &str) -> String {
            let mut s = TcpStream::connect(addr).unwrap();
            // Non-blocking or the read below parks the whole thread — and
            // this thread is also the server.
            s.set_nonblocking(true).unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .unwrap();
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            for _ in 0..500 {
                world.step();
                // Give the kernel a beat to move bytes; a zero-sleep spin can
                // burn the whole window before the NIC thread runs.
                std::thread::sleep(Duration::from_millis(2));
                match s.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        }

        /// Send an upgrade request; pump until the 101 head arrives, then
        /// hand back the negotiated client socket. Any post-101 bytes ride
        /// along in the stream for `read_message` to find.
        fn upgrade(world: &mut World, addr: &str, path: &str, protocol: Option<&str>) -> WsStream {
            let mut s = TcpStream::connect(addr).unwrap();
            s.set_nonblocking(true).unwrap();
            let extra = protocol
                .map(|p| format!("Sec-WebSocket-Protocol: {p}\r\n"))
                .unwrap_or_default();
            s.write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                     Sec-WebSocket-Version: 13\r\n{extra}\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
            let mut head: Vec<u8> = Vec::new();
            let mut byte = [0u8; 1];
            for _ in 0..500 {
                world.step();
                loop {
                    match s.read(&mut byte) {
                        Ok(_) => {
                            head.push(byte[0]);
                            if head.ends_with(b"\r\n\r\n") {
                                let text = String::from_utf8_lossy(&head);
                                assert!(
                                    text.contains("101 Switching Protocols"),
                                    "upgrade head: {text}"
                                );
                                assert!(text.contains(
                                    "Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
                                ));
                                let mut ws = WsStream::new(s, false);
                                ws.set_nonblocking(true).unwrap();
                                return ws;
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::Interrupted =>
                        {
                            break
                        }
                        Err(_) => panic!("socket died mid-upgrade"),
                    }
                }
            }
            panic!("no 101 within the pump window");
        }

        /// Read one KTLV frame, pumping the loop while we wait.
        fn next_frame(world: &mut World, ws: &mut WsStream) -> Frame {
            for _ in 0..500 {
                match ws.read_message() {
                    ReadResult::Message(bytes) => {
                        return decode_frame(&bytes).expect("valid KTLV from server")
                    }
                    ReadResult::WouldBlock => {
                        world.step();
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    ReadResult::Closed => panic!("server closed before a frame arrived"),
                }
            }
            panic!("no frame within the retry window")
        }

        /// Send a frame and pump the loop until it has been handled.
        fn send(world: &mut World, ws: &mut WsStream, frame: &Frame) {
            let bytes = encode_frame(frame).expect("test frame encodes");
            ws.send_binary(&bytes).unwrap();
            for _ in 0..5 {
                world.step();
            }
        }

        fn send_raw(world: &mut World, ws: &mut WsStream, bytes: &[u8]) {
            ws.send_binary(bytes).unwrap();
            for _ in 0..5 {
                world.step();
            }
        }
    }

    fn counter_route() -> Route {
        let count = use_signal(|| 0u32);
        let render = move || {
            view! {
                h1 { "Count: {count}" }
                button(on:click = move |_| count.update(|c| *c += 1)) { "+1" }
            }
        };
        Route::new("Counter", std::rc::Rc::new(render))
    }

    /// The same-thread harness the plan calls for: bind, mount, and run the
    /// loop ON this thread (kitae_ui signals are thread-local — the world
    /// can never hop threads).
    fn server_and_world(routes: Vec<Route>) -> (String, World) {
        let mut server = Server::new("127.0.0.1:0").unwrap();
        for r in routes {
            server.mount(r);
        }
        let addr = server.local_addr().unwrap().to_string();
        let world = server.serve_for(0);
        (addr, world)
    }

    #[test]
    fn static_fallback_serves_complete_html() {
        let (addr, mut world) = server_and_world(vec![counter_route()]);
        let page = TestClient::static_get(&mut world, &addr, "/");
        assert!(page.contains("HTTP/1.1 200 OK"), "got: {page}");
        assert!(page.contains("Count: 0"));
        assert!(page.contains("<title>Counter</title>"));
        assert!(page.contains("<!doctype html>"));
        // No handler text, no script — a complete, working static page.
        assert!(!page.to_lowercase().contains("<script"));
    }

    #[test]
    fn unknown_path_is_404() {
        let (addr, mut world) = server_and_world(vec![counter_route()]);
        let page = TestClient::static_get(&mut world, &addr, "/nope");
        assert!(page.contains("404 Not Found"), "got: {page}");
        assert!(page.contains("No route is mounted"));
    }

    #[test]
    fn live_session_sends_init_and_streams_patches() {
        let (addr, mut world) = server_and_world(vec![counter_route()]);
        let mut ws = TestClient::upgrade(&mut world, &addr, "/", None);
        match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Init { session, html } => {
                assert!(session.starts_with("s-"));
                assert!(html.contains("Count: 0"));
            }
            other => panic!("expected Init, got {other:?}"),
        }
        // Hydration patch: every InsertNode, ending in the button + its text.
        let button = match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Patch { seq: 0, ops } => ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    PatchOp::InsertNode { node, .. } if node.handler.is_some() => {
                        Some(node.handler.unwrap())
                    }
                    _ => None,
                })
                .expect("a handler-bearing node (the button)"),
            other => panic!("expected Patch, got {other:?}"),
        };

        // Click the button → the server runs the authored handler → streams
        // the one-op patch (seq 1: 0 is the hydration marker).
        TestClient::send(
            &mut world,
            &mut ws,
            &Frame::Event {
                element: button,
                name: "click".into(),
                value: None,
            },
        );
        match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Patch { seq, ops } => {
                assert_eq!(seq, 1);
                assert!(matches!(&ops[0], PatchOp::SetText { text, .. } if text == "Count: 1"));
            }
            other => panic!("expected Patch, got {other:?}"),
        }
    }

    #[test]
    fn resume_by_session_id_replays_state() {
        let (addr, mut world) = server_and_world(vec![counter_route()]);
        let mut ws = TestClient::upgrade(&mut world, &addr, "/", None);
        let session = match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Init { session, .. } => session,
            other => panic!("expected Init, got {other:?}"),
        };
        let button = match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Patch { ops, .. } => ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    PatchOp::InsertNode { node, .. } if node.handler.is_some() => {
                        Some(node.handler.unwrap())
                    }
                    _ => None,
                })
                .expect("a handler-bearing node (the button)"),
            other => panic!("expected Patch, got {other:?}"),
        };
        TestClient::send(
            &mut world,
            &mut ws,
            &Frame::Event {
                element: button,
                name: "click".into(),
                value: None,
            },
        );
        let _ = TestClient::next_frame(&mut world, &mut ws); // Count: 1 patch
        TestClient::send(
            &mut world,
            &mut ws,
            &Frame::Event {
                element: button,
                name: "click".into(),
                value: None,
            },
        );
        let _ = TestClient::next_frame(&mut world, &mut ws); // Count: 2 patch
                                                             // Forced disconnect: drop the socket without a close handshake.
        drop(ws);

        // Reconnect mid-session naming the old id: the server resumes the
        // SAME session — signal state intact, no handler re-run — this is
        // the plan's reconnect/resume proof.
        let resume = format!("kitae, resume {session}");
        let mut ws2 = TestClient::upgrade(&mut world, &addr, "/", Some(&resume));
        // A resumed socket still receives Init (a fresh view of the live
        // state) — what matters is the state survived the disconnect.
        match TestClient::next_frame(&mut world, &mut ws2) {
            Frame::Init { html, .. } => assert!(
                html.contains("Count: 2"),
                "the session kept the signal state across the disconnect: {html}"
            ),
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    /// The hot-reload primitive: refresh_all pushes a fresh Init + hydration
    /// to every connected socket without any client round-trip.
    fn refresh_all_pushes_fresh_state_to_all_sockets() {
        let (addr, mut world) = server_and_world(vec![counter_route()]);
        let mut ws1 = TestClient::upgrade(&mut world, &addr, "/", None);
        let mut ws2 = TestClient::upgrade(&mut world, &addr, "/", Some("kitae, resume s-1"));
        let _ = TestClient::next_frame(&mut world, &mut ws1); // Init
        let button = match TestClient::next_frame(&mut world, &mut ws1) {
            Frame::Patch { ops, .. } => ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    PatchOp::InsertNode { node, .. } if node.handler.is_some() => {
                        Some(node.handler.unwrap())
                    }
                    _ => None,
                })
                .expect("button"),
            other => panic!("expected Patch, got {other:?}"),
        };
        let _ = TestClient::next_frame(&mut world, &mut ws2); // Init
        let _ = TestClient::next_frame(&mut world, &mut ws2); // hydrate

        // Click on socket 1: the session state moves to 1 for BOTH.
        TestClient::send(
            &mut world,
            &mut ws1,
            &Frame::Event {
                element: button,
                name: "click".into(),
                value: None,
            },
        );
        let _ = TestClient::next_frame(&mut world, &mut ws1); // Count: 1

        // Refresh: BOTH sockets receive a fresh Init showing Count: 1.
        world.refresh_all();
        for ws in [&mut ws1, &mut ws2] {
            match TestClient::next_frame(&mut world, ws) {
                Frame::Init { html, .. } => assert!(
                    html.contains("Count: 1"),
                    "refresh carries live state: {html}"
                ),
                other => panic!("expected Init after refresh, got {other:?}"),
            }
        }
    }

    /// A second page with its own signal — the "new" side of a hot swap.
    fn fresh_route() -> Route {
        let visits = use_signal(|| 0u32);
        let render = move || {
            view! {
                h1 { "Fresh: {visits}" }
                button(on:click = move |_| visits.update(|v| *v += 1)) { "visit" }
            }
        };
        Route::new("Fresh", std::rc::Rc::new(render))
    }

    /// The Rust-hot-reload primitive: `replace_routes` re-points connected
    /// sessions at the new render — their next Init comes from the NEW
    /// code, and clicks after the swap run the NEW handlers.
    #[test]
    fn replace_routes_swaps_connected_sessions_to_the_new_render() {
        let (addr, mut world) = server_and_world(vec![counter_route()]);
        let mut ws = TestClient::upgrade(&mut world, &addr, "/", None);
        match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Init { html, .. } => assert!(html.contains("Count: 0"), "{html}"),
            other => panic!("expected Init, got {other:?}"),
        }
        let _ = TestClient::next_frame(&mut world, &mut ws); // old hydration patch

        world.replace_routes(vec![fresh_route()]);

        // The live session gets a fresh Init rendered by the new code.
        match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Init { html, .. } => assert!(
                html.contains("Fresh: 0"),
                "the swap reached the connected session: {html}"
            ),
            other => panic!("expected Init after replace_routes, got {other:?}"),
        }
        let button = match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Patch { ops, .. } => ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    PatchOp::InsertNode { node, .. } if node.handler.is_some() => node.handler,
                    _ => None,
                })
                .expect("the new render's button"),
            other => panic!("expected hydration Patch, got {other:?}"),
        };

        // Clicks after the swap run the new handler against the new signal.
        TestClient::send(
            &mut world,
            &mut ws,
            &Frame::Event {
                element: button,
                name: "click".into(),
                value: None,
            },
        );
        match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Patch { ops, .. } => assert!(matches!(
                &ops[0],
                PatchOp::SetText { text, .. } if text == "Fresh: 1"
            )),
            other => panic!("expected Patch from the new handler, got {other:?}"),
        }

        // New connections resolve against the new table too. The static
        // render runs the LIVE signal — our click above moved it to 1.
        let page = TestClient::static_get(&mut world, &addr, "/");
        assert!(
            page.contains("200 OK") && page.contains("Fresh: 1"),
            "got: {page}"
        );
    }

    /// A session whose path no longer resolves keeps its old render — a
    /// removed route must not tear down a page someone is looking at.
    #[test]
    fn replace_routes_keeps_sessions_of_removed_routes_on_the_old_render() {
        let (addr, mut world) = server_and_world(vec![counter_route().at("/old")]);
        let mut ws = TestClient::upgrade(&mut world, &addr, "/old", None);
        match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Init { html, .. } => assert!(html.contains("Count: 0"), "{html}"),
            other => panic!("expected Init, got {other:?}"),
        }
        let _ = TestClient::next_frame(&mut world, &mut ws);

        // The new table has no route for /old.
        world.replace_routes(vec![fresh_route().at("/new")]);

        // The live session still gets its refresh — from the OLD render.
        match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Init { html, .. } => assert!(
                html.contains("Count: 0") && !html.contains("Fresh"),
                "the removed route's session keeps the old render: {html}"
            ),
            other => panic!("expected Init after replace_routes, got {other:?}"),
        }

        // The route table itself moved on: /old is gone for new requests.
        let page = TestClient::static_get(&mut world, &addr, "/old");
        assert!(
            page.contains("404"),
            "removed route no longer resolves: {page}"
        );
    }

    #[test]
    fn garbage_frames_close_the_session() {
        let (addr, mut world) = server_and_world(vec![counter_route()]);
        let mut ws = TestClient::upgrade(&mut world, &addr, "/", None);
        let _init = TestClient::next_frame(&mut world, &mut ws);
        let _hydrate = TestClient::next_frame(&mut world, &mut ws);
        TestClient::send_raw(&mut world, &mut ws, b"not-a-ktlv-frame");
        for _ in 0..500 {
            match ws.read_message() {
                ReadResult::Closed => return,
                ReadResult::WouldBlock => {
                    world.step();
                    std::thread::sleep(Duration::from_millis(1));
                }
                ReadResult::Message(_) => continue,
            }
        }
        panic!("server never closed the bad-frame socket");
    }

    /// Session ids are resume CAPABILITIES — they must be unforgeable
    /// tokens, not sequential counters a peer could guess.
    #[test]
    fn session_ids_are_unforgeable_tokens() {
        let (addr, mut world) = server_and_world(vec![counter_route()]);
        let mut ws = TestClient::upgrade(&mut world, &addr, "/", None);
        let id = match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Init { session, .. } => session,
            other => panic!("expected Init, got {other:?}"),
        };
        assert!(id.starts_with("s-"));
        assert!(id.len() >= 20, "token carries real entropy: {id}");
        assert_ne!(id, "s-1", "never a bare counter");
    }

    /// Idle sessions evict after the TTL; a resume past eviction comes
    /// back fresh (state is finite).
    #[test]
    fn idle_sessions_evict_after_ttl() {
        let mut server = Server::new("127.0.0.1:0").unwrap();
        server.set_session_ttl(std::time::Duration::from_millis(80));
        let count = use_signal(|| 0u32);
        let render = move || {
            view! {
                h1 { "Count: {count}" }
                button(on:click = move |_| count.update(|c| *c += 1)) { "+1" }
            }
        };
        server.mount(Route::new("Counter", std::rc::Rc::new(render)));
        let addr = server.local_addr().unwrap().to_string();
        let mut world = server.serve_for(0);
        let mut ws = TestClient::upgrade(&mut world, &addr, "/", None);
        let session = match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Init { session, .. } => session,
            other => panic!("expected Init, got {other:?}"),
        };
        let button = match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Patch { ops, .. } => ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    PatchOp::InsertNode { node, .. } if node.handler.is_some() => node.handler,
                    _ => None,
                })
                .expect("button"),
            other => panic!("expected Patch, got {other:?}"),
        };

        // Interact, then let the session idle past the TTL.
        TestClient::send(
            &mut world,
            &mut ws,
            &Frame::Event {
                element: button,
                name: "click".into(),
                value: None,
            },
        );
        let _ = TestClient::next_frame(&mut world, &mut ws);
        std::thread::sleep(std::time::Duration::from_millis(120));
        world.step(); // eviction pass

        // A resume with the evicted id: the capability stays valid (same
        // id) and a NEW session object mounts. Note the signal itself is
        // the author's state (shared by the render closure) — eviction
        // drops the session (diff shadow, history, handler table), not
        // the author's data.
        assert!(
            !world.sessions.contains_key(&session),
            "the idle session object was evicted"
        );
        let mut ws2 = TestClient::upgrade(
            &mut world,
            &addr,
            "/",
            Some(&format!("kitae, resume {session}")),
        );
        match TestClient::next_frame(&mut world, &mut ws2) {
            Frame::Init { html, .. } => {
                assert!(
                    html.contains("Count: 1"),
                    "resume re-mounts the page: {html}"
                );
            }
            other => panic!("expected Init, got {other:?}"),
        }
    }

    /// `push_session` re-renders ONE session and streams the diff — the
    /// targeted server-push for background state changes.
    #[test]
    fn push_session_streams_diff_without_events() {
        let count = use_signal(|| 0u32);
        let render = move || {
            view! {
                h1 { "Count: {count}" }
            }
        };
        let mut server = Server::new("127.0.0.1:0").unwrap();
        server.mount(Route::new("Counter", std::rc::Rc::new(render)));
        let addr = server.local_addr().unwrap().to_string();
        let mut world = server.serve_for(0);
        let mut ws = TestClient::upgrade(&mut world, &addr, "/", None);
        let session = match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Init { session, .. } => session,
            other => panic!("expected Init, got {other:?}"),
        };
        let _ = TestClient::next_frame(&mut world, &mut ws);

        // The author mutates state OUTSIDE any event (background job) and
        // pushes: the socket receives the patch.
        count.set(7);
        world.push_session(&session);
        match TestClient::next_frame(&mut world, &mut ws) {
            Frame::Patch { ops, .. } => assert!(matches!(
                &ops[0],
                PatchOp::SetText { text, .. } if text == "Count: 7"
            )),
            other => panic!("expected Patch, got {other:?}"),
        }
    }

    /// Mounted static dirs serve files with content types; traversal is
    /// refused; routes win on overlap.
    #[test]
    fn static_dirs_serve_files_and_refuse_traversal() {
        let dir = std::env::temp_dir().join(format!("kitae_static_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("css")).unwrap();
        std::fs::write(dir.join("css/site.css"), "body { color: #111 }").unwrap();
        std::fs::write(dir.join("secret.txt"), "top secret").unwrap();

        let mut server = Server::new("127.0.0.1:0").unwrap();
        server.mount_dir("/static", &dir);
        server.mount(Route::new(
            "Home",
            std::rc::Rc::new(|| {
                view! { h1 { "route wins" } }
            }),
        ));
        let addr = server.local_addr().unwrap().to_string();
        let mut world = server.serve_for(0);

        let css = TestClient::static_get(&mut world, &addr, "/static/css/site.css");
        assert!(
            css.contains("200 OK") && css.contains("text/css"),
            "css: {css}"
        );
        assert!(css.contains("color: #111"));

        let traversal = TestClient::static_get(&mut world, &addr, "/static/../secret.txt");
        assert!(traversal.contains("404"), "traversal refused: {traversal}");
        let escape = TestClient::static_get(&mut world, &addr, "/static/%2e%2e/secret.txt");
        assert!(
            escape.contains("404"),
            "encoded traversal refused: {escape}"
        );
        let missing = TestClient::static_get(&mut world, &addr, "/static/nope.css");
        assert!(missing.contains("404"), "missing file: {missing}");

        // HEAD: headers only.
        let mut s = TcpStream::connect(&addr).unwrap();
        s.set_nonblocking(true).unwrap();
        s.write_all(b"HEAD /static/css/site.css HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        for _ in 0..200 {
            world.step();
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }
        let head = String::from_utf8_lossy(&buf);
        assert!(
            head.contains("200 OK") && head.contains("text/css"),
            "HEAD: {head}"
        );
        assert!(!head.contains("color"), "HEAD carries no body: {head}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The shutdown handle stops the run loop — from another thread (the
    /// handle is the only cross-thread surface; the world stays put).
    #[test]
    fn shutdown_handle_stops_serve() {
        let mut server = Server::new("127.0.0.1:0").unwrap();
        server.mount(Route::new(
            "Static",
            std::rc::Rc::new(|| {
                view! { h1 { "static page" } }
            }),
        ));
        let handle = server.shutdown_handle();
        let mut world = server.serve_for(0);
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            handle.shutdown();
        });
        // Returns once the flipper fires; a broken mechanism would hang.
        world.run_until_stopped();
        flipper.join().unwrap();
    }

    /// The origin gate matches the host EXACTLY: a domain that merely
    /// starts with our host (`127.0.0.1.evil.com`) is a different machine.
    #[test]
    fn request_target_preserves_spa_query() {
        let request =
            parse_head("GET /docs/hooks?lang=de HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        assert_eq!(request.path, "/docs/hooks");
        assert_eq!(request.target, "/docs/hooks?lang=de");
    }

    #[test]
    fn origin_gate_matches_host_exactly() {
        assert!(origin_permitted(None, None), "no Origin header: pass");
        assert!(origin_permitted(Some("null"), None), "sandboxed page: pass");
        assert!(origin_permitted(Some("http://127.0.0.1"), None));
        assert!(origin_permitted(Some("http://127.0.0.1:8080"), None));
        assert!(origin_permitted(Some("http://localhost"), None));
        assert!(origin_permitted(Some("http://localhost:3000"), None));
        assert!(origin_permitted(Some("http://127.0.0.1:8080"), Some(8080)));
        assert!(!origin_permitted(Some("http://127.0.0.1:8081"), Some(8080)));
        // The prefix-look-alike class the old starts_with admitted:
        assert!(!origin_permitted(Some("http://127.0.0.1.evil.com"), None));
        assert!(!origin_permitted(Some("http://localhost.evil.com"), None));
        assert!(!origin_permitted(
            Some("http://127.0.0.1:8080.evil.com"),
            None
        ));
        // Anything that isn't this http server is foreign.
        assert!(!origin_permitted(Some("http://evil.com"), None));
        assert!(!origin_permitted(Some("https://localhost"), None));
        assert!(!origin_permitted(Some("file:///etc/passwd"), None));
        assert!(!origin_permitted(Some(""), None));
    }

    /// The resume protocol line is parsed as tokens, not substrings — a
    /// header that merely CONTAINS "resume" must not hand attacker-chosen
    /// garbage to the session table.
    #[test]
    fn resume_protocol_is_parsed_strictly() {
        assert_eq!(
            resume_token(Some("kitae, resume s-abc")),
            Some("s-abc".to_string())
        );
        assert_eq!(resume_token(Some("kitae")), None);
        assert_eq!(resume_token(Some("kitae, resume")), None);
        // Contains "resume" but is not a `resume <id>` entry:
        assert_eq!(resume_token(Some("kitae, resumetoken s-x")), None);
        assert_eq!(resume_token(Some("kitae, presumable s-x")), None);
        assert_eq!(resume_token(Some("resume-stealer s-x")), None);
        assert_eq!(resume_token(None), None);
    }

    /// A head over the size cap is REFUSED (431) and the socket dropped —
    /// it must not park in the head table growing its buffer forever.
    #[test]
    fn oversized_head_is_refused_431() {
        let (addr, mut world) = server_and_world(vec![counter_route()]);
        let mut s = TcpStream::connect(&addr).unwrap();
        s.set_nonblocking(true).unwrap();
        let junk = vec![b'x'; 40 * 1024];
        let mut written = 0;
        for _ in 0..500 {
            world.step();
            match s.write(&junk[written..]) {
                Ok(0) => {}
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            if written >= junk.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        for _ in 0..200 {
            world.step();
            std::thread::sleep(Duration::from_millis(2));
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("431"), "hostile head refused: {text}");
        assert!(!world.head_bufs.values().any(|b| b.len() > 32 * 1024));
    }

    /// A head that never completes is dropped after the head TTL — it must
    /// not hold its socket (and buffer) forever.
    #[test]
    fn stalled_head_is_reaped_after_ttl() {
        let mut server = Server::new("127.0.0.1:0").unwrap();
        server.set_head_ttl(Duration::from_millis(80));
        server.mount(counter_route());
        let addr = server.local_addr().unwrap().to_string();
        let mut world = server.serve_for(0);
        let mut s = TcpStream::connect(&addr).unwrap();
        s.set_nonblocking(true).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\n").unwrap(); // partial, never finished
        std::thread::sleep(Duration::from_millis(120));
        // Pump until the reap pass runs.
        for _ in 0..30 {
            world.step();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(world.heads.is_empty(), "stalled head dropped");
        // The socket is gone server-side: reads end (EOF / reset).
        let _ = s.write_all(b"\r\nHost: x\r\n\r\n");
        let mut gone = false;
        let mut chunk = [0u8; 256];
        for _ in 0..200 {
            match s.read(&mut chunk) {
                Ok(0) => {
                    gone = true;
                    break;
                }
                Ok(_) => {
                    world.step();
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    world.step();
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => {
                    gone = true; // reset — the drop was noticed either way
                    break;
                }
            }
        }
        assert!(gone, "the stalled head's socket was closed by the server");
    }
}
