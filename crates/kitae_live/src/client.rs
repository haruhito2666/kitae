//! The Live client's socket half: upgrade a TCP stream to WebSocket and
//! speak KTLV — non-blocking-friendly, for the engine thread.
//!
//! The engine (and its FFI) is synchronous and single-threaded per tab;
//! it cannot `select` over sockets and channels. It therefore **polls**:
//! `LiveClient::read_frame` never blocks (the socket is non-blocking), so
//! the engine thread can call it between commands. `ws::WsStream` (the
//! non-blocking reader) is reused as-is; this module adds the handshake
//! the client side performs: send the upgrade request, read the 101,
//! then hand the socket (and any post-101 bytes) to `WsStream`.

use std::io::{Read, Write};
use std::net::TcpStream;

use crate::wire::{decode_frame, encode_frame, Frame};

/// A fresh `Sec-WebSocket-Key`: 16 random bytes, base64 — exactly what the
/// spec asks for. The previous constant was the RFC 6455 sample
/// ("dGhlIHNhbXBsZSBub25jZQ=="): every connection announced itself with a
/// signature no real client sends. The key's accept value is not a
/// security control, but a fingerprint-free client is the honest default.
fn fresh_ws_key() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS RNG unavailable");
    crate::sha1::b64(&bytes)
}

/// (`host:port`, `/path`) from an http(s) URL. Default ports are dropped
/// (Host headers must not carry them).
fn validate_handshake(head: &str, key: &str) -> Result<(), String> {
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    if !status.starts_with("HTTP/1.1 101 ") {
        return Err(format!("server refused the upgrade: {status}"));
    }
    let mut upgrade = false;
    let mut connection = false;
    let mut accept = false;
    let mut actual_accept = String::new();
    let mut protocol = false;
    let expected = crate::sha1::b64(&crate::sha1::sha1(
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    ));
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("upgrade") {
            upgrade = value.eq_ignore_ascii_case("websocket");
        } else if name.eq_ignore_ascii_case("connection") {
            connection = value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
        } else if name.eq_ignore_ascii_case("sec-websocket-accept") {
            actual_accept = value.to_string();
            accept = value == expected;
        } else if name.eq_ignore_ascii_case("sec-websocket-protocol") {
            protocol = value.eq_ignore_ascii_case("kitae")
                || value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("kitae"));
        }
    }
    if !upgrade || !connection || !accept || !protocol {
        return Err(format!(
            "invalid websocket upgrade response (upgrade={upgrade}, connection={connection}, accept={accept}, protocol={protocol}, actual_accept={actual_accept}, expected_accept={expected})"
        ));
    }
    Ok(())
}

fn split_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| format!("not an http(s) URL: {url}"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
        None => (rest.to_string(), "/".to_string()),
    };
    let host = host
        .strip_suffix(":80")
        .or_else(|| host.strip_suffix(":443"))
        .unwrap_or(&host)
        .to_string();
    Ok((host, path))
}

/// A connected, upgrading Live socket: the KTLV client.
pub struct LiveClient {
    pub ws: crate::ws::WsStream,
}

impl LiveClient {
    /// Connect + upgrade + wrap (blocking handshake — the remote server is
    /// another process; a browser's fetch blocks exactly the same way).
    pub fn connect(url: &str) -> Result<LiveClient, String> {
        Self::connect_inner(url, None)
    }

    /// Connect + upgrade while pumping `step` whenever the handshake read
    /// would block — for same-thread tests where THIS thread also runs the
    /// server world.
    pub fn connect_pumped<F: FnMut()>(url: &str, mut step: F) -> Result<LiveClient, String> {
        Self::connect_inner(url, Some(&mut step))
    }

    fn connect_inner(url: &str, mut step: Option<&mut dyn FnMut()>) -> Result<LiveClient, String> {
        let (host, port) = host_port(url)?;
        let addr = format!("{host}:{port}");
        let mut stream = tcp_connect(&addr)?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .and_then(|_| stream.set_write_timeout(Some(std::time::Duration::from_secs(2))))
            .map_err(|e| format!("handshake timeout setup: {e}"))?;
        let (_h, path) = split_url(url)?;
        let key = fresh_ws_key();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: kitae\r\n\r\n"
        );
        stream
            .write_all(req.as_bytes())
            .and_then(|_| stream.flush())
            .map_err(|e| format!("upgrade write: {e}"))?;
        // Non-blocking for the head read: a same-thread test pumps the
        // server through `step`; the engine just spins briefly.
        if step.is_some() {
            stream
                .set_nonblocking(true)
                .map_err(|e| format!("nonblocking: {e}"))?;
        }
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        let mut since_data = 0;
        loop {
            match stream.read(&mut byte) {
                Ok(0) => return Err("server closed mid-handshake".into()),
                Ok(_) => {
                    since_data = 0;
                    buf.push(byte[0]);
                    if buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted =>
                {
                    since_data += 1;
                    if since_data > 2000 {
                        return Err("handshake stalled".into());
                    }
                    if let Some(step) = step.as_deref_mut() {
                        step();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => return Err(format!("upgrade read: {e}")),
            }
        }
        let head = String::from_utf8_lossy(&buf);
        validate_handshake(&head, &key)?;
        let mut ws = crate::ws::WsStream::new(stream, false);
        ws.set_nonblocking(true)
            .map_err(|e| format!("nonblocking: {e}"))?;
        Ok(LiveClient { ws })
    }

    /// Connect naming an old session id — the resume path after a forced
    /// disconnect. The server keeps the session; state survives.
    pub fn connect_resume(url: &str, session: &str) -> Result<LiveClient, String> {
        Self::connect_resume_inner(url, session, None)
    }

    /// [`Self::connect_resume`] with a same-thread server pump (tests).
    pub fn connect_resume_pumped<F: FnMut()>(
        url: &str,
        session: &str,
        mut step: F,
    ) -> Result<LiveClient, String> {
        Self::connect_resume_inner(url, session, Some(&mut step))
    }

    fn connect_resume_inner(
        url: &str,
        session: &str,
        mut step: Option<&mut dyn FnMut()>,
    ) -> Result<LiveClient, String> {
        let (host, port) = host_port(url)?;
        let addr = format!("{host}:{port}");
        let mut stream = tcp_connect(&addr)?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .and_then(|_| stream.set_write_timeout(Some(std::time::Duration::from_secs(2))))
            .map_err(|e| format!("handshake timeout setup: {e}"))?;
        let (_h, path) = split_url(url)?;
        let key = fresh_ws_key();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: kitae, resume {session}\r\n\r\n"
        );
        stream
            .write_all(req.as_bytes())
            .and_then(|_| stream.flush())
            .map_err(|e| format!("resume write: {e}"))?;
        if step.is_some() {
            stream
                .set_nonblocking(true)
                .map_err(|e| format!("nonblocking: {e}"))?;
        }
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        let mut since_data = 0;
        loop {
            match stream.read(&mut byte) {
                Ok(0) => return Err("server closed mid-resume".into()),
                Ok(_) => {
                    since_data = 0;
                    buf.push(byte[0]);
                    if buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted =>
                {
                    since_data += 1;
                    if since_data > 2000 {
                        return Err("resume stalled".into());
                    }
                    if let Some(step) = step.as_deref_mut() {
                        step();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => return Err(format!("resume read: {e}")),
            }
        }
        let head = String::from_utf8_lossy(&buf);
        validate_handshake(&head, &key).map_err(|error| format!("resume: {error}"))?;
        let mut ws = crate::ws::WsStream::new(stream, false);
        ws.set_nonblocking(true)
            .map_err(|e| format!("nonblocking: {e}"))?;
        Ok(LiveClient { ws })
    }

    /// Send a KTLV frame (masked, as every client frame must be).
    pub fn send(&mut self, frame: &Frame) -> Result<(), String> {
        let bytes = encode_frame(frame).map_err(|e| format!("encode: {e}"))?;
        self.ws
            .send_binary(&bytes)
            .map_err(|e| format!("send: {e}"))
    }

    /// Try to read one KTLV frame. `Ok(None)` = nothing yet (the socket is
    /// non-blocking); `Err` = dead socket (the caller reconnects).
    pub fn read_frame(&mut self) -> Result<Option<Frame>, String> {
        match self.ws.read_message() {
            crate::ws::ReadResult::Message(bytes) => {
                let f = decode_frame(&bytes).ok_or_else(|| "bad KTLV from server".to_string())?;
                Ok(Some(f))
            }
            crate::ws::ReadResult::WouldBlock => Ok(None),
            crate::ws::ReadResult::Closed => Err("socket closed".into()),
        }
    }
}

/// (`host`, port) from an http(s) URL; defaults 80/443 by scheme.
fn host_port(url: &str) -> Result<(String, u16), String> {
    let https = url.starts_with("https://");
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| format!("not an http(s) URL: {url}"))?;
    let host = rest.split('/').next().unwrap_or(rest);
    if let Some((h, p)) = host.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            return Ok((h.to_string(), port));
        }
    }
    Ok((host.to_string(), if https { 443 } else { 80 }))
}

/// TCP connect with a hard 2s cap — a dead host must not park the caller
/// for the OS-default timeout (the engine thread is synchronous).
fn tcp_connect(addr: &str) -> Result<TcpStream, String> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<std::net::SocketAddr> = addr
        .to_socket_addrs()
        .map_err(|e| format!("resolve {addr}: {e}"))?
        .collect();
    let mut last = String::from("no addresses");
    for a in addrs {
        match TcpStream::connect_timeout(&a, std::time::Duration::from_secs(2)) {
            Ok(s) => return Ok(s),
            Err(e) => last = format!("{a}: {e}"),
        }
    }
    Err(format!("connect {addr}: {last}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Server;
    use kitae_ui::prelude::*;

    /// What does a plain `GET /` (no upgrade headers) get? The static page.
    #[test]
    fn probe_plain_get_root() {
        let (addr, mut world) = counter_server();
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        s.set_nonblocking(true).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        for _ in 0..200 {
            world.step();
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(2))
                }
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("HTTP/1.1 200 OK"),
            "plain GET / answers 200: {text}"
        );
    }

    #[test]
    fn rejects_a_wrong_websocket_accept_token() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut chunk = [0u8; 512];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                head.extend_from_slice(&chunk[..n]);
            }
            stream
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: wrong\r\nSec-WebSocket-Protocol: kitae\r\n\r\n")
                .unwrap();
        });
        let error = match LiveClient::connect(&format!("http://127.0.0.1:{port}/")) {
            Ok(_) => panic!("wrong accept token must fail"),
            Err(error) => error,
        };
        assert!(error.contains("accept=false"), "{error}");
        handle.join().unwrap();
    }

    /// Same-thread pump: the test thread runs BOTH the world loop and the
    /// client (they cannot starve each other — every client read is wedged
    /// between steps).
    fn counter_server() -> (String, crate::server::World) {
        let count = use_signal(|| 0u32);
        let render = move || {
            view! {
                h1 { "Count: {count}" }
                button(on:click = move |_| count.update(|c| *c += 1)) { "+1" }
            }
        };
        let mut server = Server::new("127.0.0.1:0").unwrap();
        server.mount(crate::server::Route::new(
            "Counter",
            std::rc::Rc::new(render),
        ));
        let addr = server.local_addr().unwrap().to_string();
        let world = server.serve_for(0);
        (addr, world)
    }

    fn until<T>(
        world: &mut crate::server::World,
        c: &mut LiveClient,
        f: impl Fn(&mut LiveClient) -> Option<T>,
    ) -> T {
        for _ in 0..500 {
            match f(c) {
                Some(t) => return t,
                None => {
                    world.step();
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        panic!("client pump window exhausted");
    }

    #[test]
    fn client_connects_and_streams() {
        let (addr, mut world) = counter_server();
        let mut c = LiveClient::connect_pumped(&format!("http://{addr}"), || {
            world.step();
        })
        .unwrap();
        let init = until(&mut world, &mut c, |c| c.read_frame().ok().flatten());
        match init {
            Frame::Init { session, html } => {
                assert!(session.starts_with("s-"));
                assert!(html.contains("Count: 0"));
            }
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn client_roundtrip_events_and_reconnect_resume() {
        let (addr, mut world) = counter_server();
        let url = format!("http://{addr}");
        let mut c = LiveClient::connect_pumped(&url, || {
            world.step();
        })
        .unwrap();
        // Init + hydrate.
        let init = match until(&mut world, &mut c, |c| c.read_frame().ok().flatten()) {
            Frame::Init { session, .. } => session,
            other => panic!("expected Init, got {other:?}"),
        };
        let button = match until(&mut world, &mut c, |c| c.read_frame().ok().flatten()) {
            Frame::Patch { ops, .. } => ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    kitae_ui::patch::PatchOp::InsertNode { node, .. } if node.handler.is_some() => {
                        Some(node.handler.unwrap())
                    }
                    _ => None,
                })
                .expect("button ref"),
            other => panic!("expected Patch, got {other:?}"),
        };
        // Click → server handler → patch.
        c.send(&Frame::Event {
            element: button,
            name: "click".into(),
            value: None,
        })
        .unwrap();
        match until(&mut world, &mut c, |c| c.read_frame().ok().flatten()) {
            Frame::Patch { ops, .. } => assert!(matches!(
                &ops[0],
                kitae_ui::patch::PatchOp::SetText { text, .. } if text == "Count: 1"
            )),
            other => panic!("expected Patch, got {other:?}"),
        }
        // Forced disconnect: drop without a close handshake.
        drop(c);

        // Reconnect naming the session: the server state survived.
        let mut c2 = LiveClient::connect_resume_pumped(&url, &init, || {
            world.step();
        })
        .unwrap();
        match until(&mut world, &mut c2, |c| c.read_frame().ok().flatten()) {
            Frame::Init { html, .. } => assert!(
                html.contains("Count: 1"),
                "resume kept the signal state: {html}"
            ),
            other => panic!("expected Init on resume, got {other:?}"),
        }
    }
}
