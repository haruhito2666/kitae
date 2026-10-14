//! G6: WebSocket (RFC 6455) — the protocol, embeddable without any JS API.
//!
//! Hand-rolled per this crate's style (like the charset decoders and the
//! cookie jar): the §4.1 opening handshake with the accept-key check,
//! client-side masking, fragmentation reassembly on receive, automatic Pong
//! for Pings, and the closing handshake on Close/Drop. `wss://` rides rustls
//! (ring provider) with the webpki-roots trust store; certificate checks can
//! be skipped only through an explicit opt-in flag for loopback tests.
//!
//! Data frames surface as [`WsMessage`]; control frames are handled inside
//! the stream and never surface (Pings are answered automatically, Close
//! runs the closing handshake and ends the stream).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use sha1::{Digest as Sha1Digest, Sha1};
use url::Url;

use crate::FetchError;

/// The GUID from RFC 6455 §1.3, appended to the handshake key.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Upper bound on a single received frame / reassembled message (hostile
/// peer guard — an unbounded length field must not drive allocation).
const WS_MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// A data message surfaced by [`WsStream::recv`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
}

/// Options for [`ws_connect_with`].
#[derive(Debug, Clone)]
pub struct WsConfig {
    /// `Sec-WebSocket-Protocol` values to offer. The server's pick, if any,
    /// is accepted verbatim (no negotiation beyond that — the embedder
    /// speaks the subprotocol, not the engine).
    pub protocols: Vec<String>,
    /// TCP (+TLS) handshake budget.
    pub connect_timeout: Duration,
    /// Per-`recv` blocking budget.
    pub read_timeout: Duration,
    /// Accept any server certificate. Loopback tests only — never set this
    /// on a network socket.
    pub insecure_skip_verify: bool,
}

impl Default for WsConfig {
    fn default() -> Self {
        WsConfig {
            protocols: Vec::new(),
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            insecure_skip_verify: false,
        }
    }
}

/// Open a WebSocket connection (`ws://` or `wss://`) with default config.
pub fn ws_connect(url: &str) -> Result<WsStream, FetchError> {
    ws_connect_with(url, &WsConfig::default())
}

/// Open a WebSocket connection with explicit options.
pub fn ws_connect_with(raw_url: &str, cfg: &WsConfig) -> Result<WsStream, FetchError> {
    let mut parsed =
        Url::parse(raw_url).map_err(|e| FetchError::InvalidUrl(format!("{raw_url}: {e}")))?;
    if parsed.scheme() == "ws" && crate::hsts_covers(parsed.as_str()) {
        parsed
            .set_scheme("wss")
            .map_err(|_| FetchError::HttpsUpgrade(format!("{raw_url}: HSTS upgrade failed")))?;
    }
    match parsed.scheme() {
        "ws" | "wss" => {}
        other => return Err(FetchError::UnsupportedScheme(other.to_string())),
    }
    let secure = parsed.scheme() == "wss";
    let host = parsed.host_str().unwrap_or("").to_string();
    if host.is_empty() {
        return Err(FetchError::InvalidUrl(format!("{raw_url}: no host")));
    }
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if secure { 443 } else { 80 });
    let base_path = if parsed.path().is_empty() {
        "/"
    } else {
        parsed.path()
    };
    let path_qs = match parsed.query() {
        Some(q) => format!("{base_path}?{q}"),
        None => base_path.to_string(),
    };

    let tcp = connect_tcp(&host, port, cfg.connect_timeout)?;
    tcp.set_read_timeout(Some(cfg.connect_timeout))
        .map_err(|e| FetchError::Transport(format!("websocket timeouts: {e}")))?;
    tcp.set_write_timeout(Some(cfg.connect_timeout))
        .map_err(|e| FetchError::Transport(format!("websocket timeouts: {e}")))?;

    let mut io: WsIo = if secure {
        let builder = rustls::ClientConfig::builder();
        let config = if cfg.insecure_skip_verify {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(crate::h3::InsecureSkipVerify))
                .with_no_client_auth()
        } else {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots).with_no_client_auth()
        };
        let name = rustls_pki_types::ServerName::try_from(host.clone())
            .map_err(|e| FetchError::InvalidUrl(format!("{host}: {e}")))?
            .to_owned();
        let conn = rustls::ClientConnection::new(Arc::new(config), name)
            .map_err(|e| FetchError::Transport(format!("websocket tls: {e}")))?;
        WsIo::Tls(Box::new(rustls::StreamOwned::new(conn, tcp)))
    } else {
        WsIo::Plain(tcp)
    };

    // -- Opening handshake (RFC 6455 §4.1) ----------------------------------
    let key = {
        let mut raw = [0u8; 16];
        let _ = getrandom::getrandom(&mut raw);
        base64::engine::general_purpose::STANDARD.encode(raw)
    };
    let mut req = format!(
        "GET {path_qs} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    );
    if !cfg.protocols.is_empty() {
        req.push_str(&format!(
            "Sec-WebSocket-Protocol: {}\r\n",
            cfg.protocols.join(", ")
        ));
    }
    req.push_str(&format!("User-Agent: {}\r\n", crate::USER_AGENT));
    if crate::dnt_enabled() {
        req.push_str("DNT: 1\r\n");
    }
    req.push_str("\r\n");
    io.write_all(req.as_bytes())
        .and_then(|_| io.flush())
        .map_err(|e| FetchError::Transport(format!("websocket handshake: {e}")))?;

    let (head, leftover) = read_http_head(&mut io)?;
    let head = String::from_utf8_lossy(&head).into_owned();
    if secure {
        if let Some(value) = head.lines().find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("strict-transport-security"))
                .map(|(_, value)| value.trim())
        }) {
            let mut secure_url = parsed.clone();
            secure_url
                .set_scheme("https")
                .map_err(|_| FetchError::HttpsUpgrade(format!("{raw_url}: invalid HSTS URL")))?;
            crate::observe_hsts(secure_url.as_str(), value);
        }
    }
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    if !status_line.contains(" 101 ") {
        return Err(FetchError::Transport(format!(
            "websocket handshake rejected: {}",
            status_line.trim()
        )));
    }
    let mut accept_ok = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("sec-websocket-accept") {
                accept_ok = v.trim() == compute_accept(&key);
            }
        }
    }
    if !accept_ok {
        return Err(FetchError::Transport(
            "websocket handshake: Sec-WebSocket-Accept mismatch".into(),
        ));
    }
    io.set_timeouts(Some(cfg.read_timeout))
        .map_err(|e| FetchError::Transport(format!("websocket timeouts: {e}")))?;

    Ok(WsStream {
        io,
        // Frame data may legally arrive coalesced with the 101's final CRLF
        // (servers push right after the handshake); adopt it, never drop it.
        buf: leftover,
        open: true,
    })
}

fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, FetchError> {
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|e| FetchError::Transport(format!("websocket resolve {host}:{port}: {e}")))?;
    let mut last = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(FetchError::Transport(format!(
        "websocket connect {host}:{port}: {}",
        last.map(|error| error.to_string())
            .unwrap_or_else(|| "no addresses".to_string())
    )))
}

/// `base64(SHA-1(key + GUID))` — RFC 6455 §1.3.
fn compute_accept(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

/// Read until the blank line that ends an HTTP/1.1 head (never the body —
/// the handshake response has none). Returns the head plus any bytes read
/// past the terminator: coalesced early frame data, which the caller must
/// feed into the stream buffer rather than lose.
fn read_http_head(io: &mut WsIo) -> Result<(Vec<u8>, Vec<u8>), FetchError> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(pos) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            let leftover = head.split_off(pos + 4);
            return Ok((head, leftover));
        }
        let n = io
            .read(&mut chunk)
            .map_err(|e| FetchError::Transport(format!("websocket handshake: {e}")))?;
        if n == 0 {
            return Err(FetchError::Transport(
                "websocket: connection closed during handshake".into(),
            ));
        }
        head.extend_from_slice(&chunk[..n]);
        if head.len() > 64 * 1024 {
            return Err(FetchError::Transport(
                "websocket: oversized handshake".into(),
            ));
        }
    }
}

/// Plain TCP or TLS — everything downstream is byte-level framing.
enum WsIo {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl WsIo {
    fn set_timeouts(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            WsIo::Plain(stream) => {
                stream.set_read_timeout(timeout)?;
                stream.set_write_timeout(timeout)
            }
            WsIo::Tls(stream) => {
                stream.sock.set_read_timeout(timeout)?;
                stream.sock.set_write_timeout(timeout)
            }
        }
    }
}

impl Read for WsIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            WsIo::Plain(s) => s.read(buf),
            WsIo::Tls(s) => s.read(buf),
        }
    }
}

impl Write for WsIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            WsIo::Plain(s) => s.write(buf),
            WsIo::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            WsIo::Plain(s) => s.flush(),
            WsIo::Tls(s) => s.flush(),
        }
    }
}

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// An open WebSocket connection. Dropping it sends a polite Close (1000)
/// when the closing handshake hasn't already run.
pub struct WsStream {
    io: WsIo,
    /// Bytes read from the socket but not yet consumed by the frame parser.
    buf: Vec<u8>,
    open: bool,
}

impl std::fmt::Debug for WsStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsStream")
            .field("open", &self.open)
            .field("buffered", &self.buf.len())
            .finish()
    }
}

impl WsStream {
    /// Next data message, or `None` once the connection is closed (by us, by
    /// the peer, or by a previous recv that ran the closing handshake).
    /// Pings are answered with Pongs automatically and never surface; a
    /// peer Close is answered and ends the stream; fragmented messages are
    /// reassembled.
    pub fn recv(&mut self) -> Result<Option<WsMessage>, FetchError> {
        if !self.open {
            return Ok(None);
        }
        let mut message: Option<(u8, Vec<u8>)> = None; // (first opcode, bytes so far)
        loop {
            let frame = self.read_frame()?;
            match frame.opcode {
                0x9 => self.send_frame(true, 0xA, &frame.payload)?, // Ping → Pong
                0x8 => {
                    // Echo the peer's status code and end the stream.
                    let code = if frame.payload.len() >= 2 {
                        u16::from_be_bytes([frame.payload[0], frame.payload[1]])
                    } else {
                        1000
                    };
                    self.open = false;
                    self.send_frame(true, 0x8, &code.to_be_bytes())?;
                    return Ok(None);
                }
                0xA => {} // unsolicited Pong: ignore
                0x1 | 0x2 => {
                    if frame.fin {
                        return Ok(Some(decode_data(frame.opcode, frame.payload)));
                    }
                    message = Some((frame.opcode, frame.payload));
                }
                0x0 => {
                    let Some((_, acc)) = message.as_mut() else {
                        return Err(FetchError::Transport(
                            "websocket: continuation frame with nothing to continue".into(),
                        ));
                    };
                    if acc.len() + frame.payload.len() > WS_MAX_MESSAGE {
                        return Err(FetchError::Transport("websocket: message too large".into()));
                    }
                    acc.extend_from_slice(&frame.payload);
                    if frame.fin {
                        let (op, acc) = message.take().expect("just matched");
                        return Ok(Some(decode_data(op, acc)));
                    }
                }
                other => {
                    return Err(FetchError::Transport(format!(
                        "websocket: reserved opcode {other:#x}"
                    )))
                }
            }
        }
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), FetchError> {
        self.send_frame(true, 0x1, text.as_bytes())
    }

    pub fn send_binary(&mut self, data: &[u8]) -> Result<(), FetchError> {
        self.send_frame(true, 0x2, data)
    }

    pub fn send_ping(&mut self, data: &[u8]) -> Result<(), FetchError> {
        self.send_frame(true, 0x9, data)
    }

    pub fn send_pong(&mut self, data: &[u8]) -> Result<(), FetchError> {
        self.send_frame(true, 0xA, data)
    }

    /// Start the closing handshake (RFC 6455 §5.5.1). The stream is dead
    /// from the caller's perspective afterwards; `recv` returns `None`.
    pub fn send_close(&mut self, code: u16, reason: &str) -> Result<(), FetchError> {
        let mut payload = code.to_be_bytes().to_vec();
        payload.extend_from_slice(reason.as_bytes());
        let r = self.send_frame(true, 0x8, &payload);
        self.open = false;
        r
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// One masked client→server frame (RFC 6455 §5.3: every client frame
    /// carries a fresh random mask).
    fn send_frame(&mut self, fin: bool, opcode: u8, payload: &[u8]) -> Result<(), FetchError> {
        if !self.open && opcode != 0x8 {
            return Err(FetchError::Transport("websocket: connection closed".into()));
        }
        let mut out = Vec::with_capacity(payload.len() + 14);
        out.push((if fin { 0x80 } else { 0 }) | opcode);
        let len = payload.len();
        if len < 126 {
            out.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            out.push(0x80 | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.push(0x80 | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mut mask = [0u8; 4];
        let _ = getrandom::getrandom(&mut mask);
        out.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            out.push(b ^ mask[i % 4]);
        }
        self.io
            .write_all(&out)
            .and_then(|_| self.io.flush())
            .map_err(|e| FetchError::Transport(format!("websocket send: {e}")))
    }

    fn read_exact_from_io(&mut self, need: usize) -> Result<(), FetchError> {
        while self.buf.len() < need {
            let mut chunk = [0u8; 4096];
            let n = self
                .io
                .read(&mut chunk)
                .map_err(|e| FetchError::Transport(format!("websocket read: {e}")))?;
            if n == 0 {
                return Err(FetchError::Transport(
                    "websocket: connection closed mid-frame".into(),
                ));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
        Ok(())
    }

    fn read_frame(&mut self) -> Result<Frame, FetchError> {
        self.read_exact_from_io(2)?;
        let b0 = self.buf[0];
        let fin = b0 & 0x80 != 0;
        // No extensions are negotiated, so RSV bits must be clear.
        if b0 & 0x70 != 0 {
            return Err(FetchError::Transport("websocket: RSV bits set".into()));
        }
        let opcode = b0 & 0x0F;
        let b1 = self.buf[1];
        // Server→client frames are never masked (RFC 6455 §5.1); a masked
        // one is a protocol violation.
        if b1 & 0x80 != 0 {
            return Err(FetchError::Transport(
                "websocket: server sent a masked frame".into(),
            ));
        }
        let mut len = (b1 & 0x7F) as usize;
        let mut header = 2usize;
        match len {
            126 => {
                self.read_exact_from_io(4)?;
                len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                header = 4;
            }
            127 => {
                self.read_exact_from_io(10)?;
                len = u64::from_be_bytes([
                    self.buf[2],
                    self.buf[3],
                    self.buf[4],
                    self.buf[5],
                    self.buf[6],
                    self.buf[7],
                    self.buf[8],
                    self.buf[9],
                ]) as usize;
                header = 10;
            }
            _ => {}
        }
        if len > WS_MAX_MESSAGE {
            return Err(FetchError::Transport("websocket: frame too large".into()));
        }
        self.read_exact_from_io(header + len)?;
        let payload = self.buf[header..header + len].to_vec();
        self.buf.drain(..header + len);
        Ok(Frame {
            fin,
            opcode,
            payload,
        })
    }
}

fn decode_data(opcode: u8, payload: Vec<u8>) -> WsMessage {
    match opcode {
        0x1 => WsMessage::Text(String::from_utf8_lossy(&payload).into_owned()),
        _ => WsMessage::Binary(payload),
    }
}

impl Drop for WsStream {
    fn drop(&mut self) {
        if self.open {
            let _ = self.send_close(1000, "");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::privacy_test_lock;

    /// Server-side read of one masked client frame.
    fn read_client_frame<S: Read>(stream: &mut S) -> (bool, u8, Vec<u8>) {
        fn read_n<S: Read>(stream: &mut S, n: usize) -> Vec<u8> {
            let mut buf = vec![0u8; n];
            stream.read_exact(&mut buf).expect("read frame bytes");
            buf
        }
        let b0 = read_n(stream, 1)[0];
        let fin = b0 & 0x80 != 0;
        let opcode = b0 & 0x0F;
        let b1 = read_n(stream, 1)[0];
        assert!(b1 & 0x80 != 0, "client frames must be masked");
        let len = (b1 & 0x7F) as usize;
        assert!(len < 126, "test fixtures use short frames");
        let mask = read_n(stream, 4);
        let mut payload = read_n(stream, len);
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
        (fin, opcode, payload)
    }

    /// Server-side send of one unmasked frame.
    fn send_server_frame<S: Write>(
        stream: &mut S,
        fin: bool,
        opcode: u8,
        payload: &[u8],
    ) -> std::io::Result<()> {
        let mut out = Vec::new();
        out.push((if fin { 0x80 } else { 0 }) | opcode);
        assert!(payload.len() < 126, "test fixtures use short frames");
        out.push(payload.len() as u8);
        out.extend_from_slice(payload);
        stream.write_all(&out)?;
        stream.flush()
    }

    /// The §1.3 example: key "dGhlIHNhbXBsZSBub25jZQ==" must produce
    /// "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".
    #[test]
    fn accept_key_matches_the_rfc_example() {
        assert_eq!(
            compute_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    /// A loopback ws server: performs the §4.1 handshake (computing the
    /// accept key from the client's), then runs `script` once.
    fn ws_server(
        script: impl FnOnce(&mut TcpStream) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        ws_server_with(move |_head| None, script)
    }

    /// Variant with a custom handshake response (`None` = the correct 101).
    fn ws_server_with(
        override_response: impl Fn(&str) -> Option<String> + Send + 'static,
        script: impl FnOnce(&mut TcpStream) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut head = Vec::new();
            let mut chunk = [0u8; 512];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).expect("read head");
                assert!(n > 0, "client vanished during handshake");
                head.extend_from_slice(&chunk[..n]);
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            if let Some(resp) = override_response(&head) {
                stream.write_all(resp.as_bytes()).unwrap();
                stream.flush().unwrap();
                return; // script skipped: the client rejects the handshake
            }
            let key = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
                .expect("client must send Sec-WebSocket-Key");
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                compute_accept(&key)
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
            script(&mut stream);
        });
        (format!("ws://127.0.0.1:{port}/socket"), handle)
    }

    #[test]
    fn dnt_reaches_websocket_handshakes() {
        let _lock = privacy_test_lock();
        crate::reset_privacy_config();
        crate::set_dnt_enabled(true);
        let (url, handle) = ws_server_with(
            |head| {
                assert!(head.to_ascii_lowercase().contains("dnt: 1"));
                None
            },
            |_| {},
        );
        let mut ws = ws_connect(&url).expect("handshake");
        let _ = ws.send_close(1000, "");
        handle.join().unwrap();
        crate::reset_privacy_config();
    }

    #[test]
    fn text_echo_roundtrip() {
        let (url, handle) = ws_server(|stream| {
            let (_, op, payload) = read_client_frame(stream);
            assert_eq!(op, 0x1, "expected a text frame");
            assert_eq!(payload, b"hello kitae");
            send_server_frame(stream, true, 0x1, b"hello back").unwrap();
        });
        let mut ws = ws_connect(&url).expect("handshake");
        assert!(ws.is_open());
        ws.send_text("hello kitae").expect("send");
        match ws.recv().expect("recv") {
            Some(WsMessage::Text(t)) => assert_eq!(t, "hello back"),
            other => panic!("expected text, got {other:?}"),
        }
        ws.send_close(1000, "done").unwrap();
        assert!(!ws.is_open());
        assert!(ws.recv().unwrap().is_none(), "recv after close is None");
        handle.join().unwrap();
    }

    #[test]
    fn binary_roundtrip() {
        let (url, handle) = ws_server(|stream| {
            let (_, op, payload) = read_client_frame(stream);
            assert_eq!(op, 0x2, "expected a binary frame");
            assert_eq!(payload, vec![0, 1, 2, 0xFF]);
            send_server_frame(stream, true, 0x2, &[9, 9, 9]).unwrap();
        });
        let mut ws = ws_connect(&url).expect("handshake");
        ws.send_binary(&[0, 1, 2, 0xFF]).expect("send");
        match ws.recv().expect("recv") {
            Some(WsMessage::Binary(b)) => assert_eq!(b, vec![9, 9, 9]),
            other => panic!("expected binary, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn fragmented_messages_are_reassembled() {
        let (url, handle) = ws_server(|stream| {
            send_server_frame(stream, false, 0x1, b"Hel").unwrap();
            send_server_frame(stream, false, 0x0, b"lo ").unwrap();
            send_server_frame(stream, true, 0x0, b"ws").unwrap();
        });
        let mut ws = ws_connect(&url).expect("handshake");
        match ws.recv().expect("recv") {
            Some(WsMessage::Text(t)) => assert_eq!(t, "Hello ws"),
            other => panic!("expected reassembled text, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn pings_are_answered_and_never_surface() {
        let (url, handle) = ws_server(|stream| {
            send_server_frame(stream, true, 0x9, b"hb").unwrap();
            send_server_frame(stream, true, 0x1, b"after ping").unwrap();
            // The automatic Pong must arrive with the ping payload echoed.
            let (_, op, payload) = read_client_frame(stream);
            assert_eq!(op, 0xA, "client must answer Ping with Pong");
            assert_eq!(payload, b"hb");
        });
        let mut ws = ws_connect(&url).expect("handshake");
        match ws.recv().expect("recv") {
            Some(WsMessage::Text(t)) => assert_eq!(t, "after ping"),
            other => panic!("ping must be swallowed, got {other:?}"),
        }
        // Client-initiated ping: silent until we choose to look.
        ws.send_ping(b"mine").unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn peer_close_runs_the_closing_handshake() {
        let (url, handle) = ws_server(|stream| {
            // Peer closes with 1000 "bye"…
            send_server_frame(stream, true, 0x8, &1000u16.to_be_bytes()).unwrap();
            // …and expects the echoed code back.
            let (_, op, payload) = read_client_frame(stream);
            assert_eq!(op, 0x8, "client must reply to Close");
            assert_eq!(payload, 1000u16.to_be_bytes().to_vec());
        });
        let mut ws = ws_connect(&url).expect("handshake");
        assert!(ws.recv().expect("recv").is_none(), "close ends the stream");
        assert!(!ws.is_open());
        handle.join().unwrap();
    }

    #[test]
    fn client_initiated_close_is_echoed() {
        let (url, handle) = ws_server(|stream| {
            let (_, op, payload) = read_client_frame(stream);
            assert_eq!(op, 0x8);
            assert_eq!(payload, 1000u16.to_be_bytes().to_vec());
            // Server echoes the close; the client is already done and just
            // drops the socket — fine per RFC 6455 §7.1.1.
        });
        let mut ws = ws_connect(&url).expect("handshake");
        ws.send_close(1000, "").unwrap();
        assert!(!ws.is_open());
        handle.join().unwrap();
    }

    #[test]
    fn bad_accept_key_is_rejected() {
        let (url, handle) = ws_server_with(
            |_head| {
                Some(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: WRONGWRONGWRONG=\r\n\r\n"
                        .to_string(),
                )
            },
            |_| {},
        );
        let err = ws_connect(&url).expect_err("bad accept must fail");
        assert!(err.to_string().contains("Sec-WebSocket-Accept"), "{err}");
        handle.join().unwrap();
    }

    #[test]
    fn handshake_obeys_connect_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            std::thread::sleep(Duration::from_millis(200));
        });
        let cfg = WsConfig {
            connect_timeout: Duration::from_millis(25),
            read_timeout: Duration::from_secs(1),
            ..WsConfig::default()
        };
        let started = std::time::Instant::now();
        assert!(ws_connect_with(&format!("ws://127.0.0.1:{port}/"), &cfg).is_err());
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "opening handshake must honor connect_timeout"
        );
        handle.join().unwrap();
    }

    #[test]
    fn non_websocket_schemes_are_rejected() {
        assert!(matches!(
            ws_connect("http://example.com/"),
            Err(FetchError::UnsupportedScheme(_))
        ));
    }

    /// wss smoke test: self-signed loopback cert, insecure opt-in, one text
    /// echo over TLS.
    #[test]
    fn wss_roundtrip_over_tls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let cert_der = cert.cert.der().clone();
            let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
                rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()),
            );
            let tls = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der], key_der)
                .expect("server cert");
            let (tcp, _) = listener.accept().unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(std::sync::Arc::new(tls)).unwrap(),
                tcp,
            );
            let mut head = Vec::new();
            let mut chunk = [0u8; 512];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).expect("tls read head");
                assert!(n > 0);
                head.extend_from_slice(&chunk[..n]);
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let key = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
                .unwrap();
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                compute_accept(&key)
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
            let (_, op, payload) = read_client_frame(&mut stream);
            assert_eq!(op, 0x1);
            assert_eq!(payload, b"secure hello");
            send_server_frame(&mut stream, true, 0x1, b"secure back").unwrap();
        });
        let cfg = WsConfig {
            insecure_skip_verify: true,
            ..WsConfig::default()
        };
        let mut ws = ws_connect_with(&format!("wss://localhost:{port}/tls"), &cfg).expect("wss");
        ws.send_text("secure hello").unwrap();
        match ws.recv().expect("recv") {
            Some(WsMessage::Text(t)) => assert_eq!(t, "secure back"),
            other => panic!("expected text over TLS, got {other:?}"),
        }
        handle.join().unwrap();
    }
}
