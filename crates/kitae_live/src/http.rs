//! A thread-per-connection HTTP/1.1 listener: the minimum a kitae_live site
//! needs (GET routes + one WebSocket upgrade), no frameworks.
//!
//! Two kinds of connection come in:
//! - plain GET → static HTML (the progressive-enhancement path — every
//!   kitae_live page is a working page before any socket);
//! - GET with `Upgrade: websocket` + the Live path → the KTLV session.
//!
//! (Head parsing lives in `server.rs` — it must be non-blocking; the
//! `BufReader` request reader this module once had could not poll.)

use std::io::Write;
use std::net::TcpStream;

/// One parsed HTTP request head (method, target, headers).
#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// Path without the query string.
    pub path: String,
    /// Path and query string, used as the SPA deep-link seed.
    pub target: String,
    /// `Sec-WebSocket-Key` when the client asked for an upgrade.
    pub ws_key: Option<String>,
    /// `Sec-WebSocket-Protocol` values, verbatim (Live registers `kitae`).
    pub ws_protocol: Option<String>,
    /// `Origin` header, when the client sent one (S-4: browsers attach it
    /// to cross-site requests — the localhost-CSRF signal).
    pub origin: Option<String>,
}

/// Write a full HTTP response (status line + headers + body, `Connection:
/// close`).
pub fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// The RFC 6455 handshake response for a client key.
pub fn ws_accept_response(key: &str) -> String {
    use crate::sha1::{b64, sha1};
    let token = b64(&sha1(
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    ));
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {token}\r\nSec-WebSocket-Protocol: kitae\r\n\r\n"
    )
}
