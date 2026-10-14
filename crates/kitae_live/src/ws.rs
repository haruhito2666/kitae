//! RFC 6455 WebSocket framing — server and client sides of the same code,
//! since the Live protocol is symmetric (server → client patches, client →
//! server events).
//!
//! Reading is **buffered and non-blocking-friendly**: Live servers poll all
//! sockets on one thread (kitae_ui signals are thread-local by design, so a
//! session can never hop threads), so a read that would block keeps its
//! partial frame bytes and reports [`ReadResult::WouldBlock`] — the poll
//! loop revisits the socket later and the frame resumes exactly where it
//! stopped.

use std::io::{Read, Write};
use std::net::TcpStream;

/// RFC 6455 close codes we send (normal closure / protocol error).
/// `CLOSE_NORMAL` is exported for embedder closes; the server itself only
/// initiates protocol-error closes today.
#[allow(dead_code)]
pub const CLOSE_NORMAL: u16 = 1000;
pub const CLOSE_PROTOCOL: u16 = 1002;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opcode {
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl Opcode {
    fn from_u8(v: u8) -> Option<Opcode> {
        Some(match v {
            0x1 => Opcode::Text,
            0x2 => Opcode::Binary,
            0x8 => Opcode::Close,
            0x9 => Opcode::Ping,
            0xA => Opcode::Pong,
            _ => return None,
        })
    }

    fn as_u8(self) -> u8 {
        match self {
            Opcode::Text => 0x1,
            Opcode::Binary => 0x2,
            Opcode::Close => 0x8,
            Opcode::Ping => 0x9,
            Opcode::Pong => 0xA,
        }
    }
}

/// What a poll saw on a socket.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadResult {
    /// One complete binary message.
    Message(Vec<u8>),
    /// No complete message right now; partial bytes are buffered.
    WouldBlock,
    /// The peer closed (or the close handshake completed).
    Closed,
}

/// A framed, role-aware WebSocket over an owned TCP stream.
pub struct WsStream {
    stream: TcpStream,
    /// true = we are the server (send unmasked, expect masked client frames).
    server: bool,
    closed: bool,
    /// Bytes read from the socket but not yet consumed by a frame.
    buf: Vec<u8>,
    /// A ping received mid-frame-poll must be answered immediately — park
    /// it until the next read call completes the outer state machine.
    pending_pong: Option<Vec<u8>>,
}

impl WsStream {
    /// Wrap a stream post-handshake. `server: true` after an Upgrade reply,
    /// `false` for a client that received 101.
    pub fn new(stream: TcpStream, server: bool) -> WsStream {
        WsStream {
            stream,
            server,
            closed: false,
            buf: Vec::new(),
            pending_pong: None,
        }
    }

    /// Pre-seed the read buffer with bytes that arrived bundled with the
    /// handshake tail (the Live client feeds any post-101 socket bytes it
    /// already had in hand, so they can't strand behind a WouldBlock).
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The underlying socket — the poll loop tunes its blocking mode.
    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    /// Toggle the socket's blocking mode (the Live poll loop reads all
    /// sockets on one thread).
    pub fn set_nonblocking(&mut self, on: bool) -> std::io::Result<()> {
        self.stream.set_nonblocking(on)
    }

    /// Send one binary message (KTLV frame).
    pub fn send_binary(&mut self, payload: &[u8]) -> std::io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.write_frame(Opcode::Binary, payload)
    }

    /// Send a control frame.
    pub fn send_control(&mut self, ping: bool, payload: &[u8]) -> std::io::Result<()> {
        self.write_frame(if ping { Opcode::Ping } else { Opcode::Pong }, payload)
    }

    /// Initiate the closing handshake (best-effort close frame, then FIN).
    pub fn close(&mut self, code: u16, reason: &str) -> std::io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let mut body = code.to_be_bytes().to_vec();
        body.extend_from_slice(reason.as_bytes().get(..123).unwrap_or(reason.as_bytes()));
        let _ = self.write_frame(Opcode::Close, &body);
        self.closed = true;
        self.stream.shutdown(std::net::Shutdown::Both)
    }

    fn write_frame(&mut self, op: Opcode, payload: &[u8]) -> std::io::Result<()> {
        let mut head = Vec::with_capacity(payload.len() + 14);
        head.push(0x80 | op.as_u8()); // FIN + opcode (no fragmentation)
        let mut body = payload.to_vec();
        if !self.server {
            // Client frames must be masked (RFC 6455 §5.3). The mask exists
            // to keep intermediaries honest, not as a security control; a
            // per-frame key seeded from the OS clock avoids a rand dep.
            let key: [u8; 4] = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos().to_le_bytes())
                .unwrap_or([1, 2, 3, 4]);
            for (i, b) in body.iter_mut().enumerate() {
                *b ^= key[i % 4];
            }
            match payload.len() {
                n if n < 126 => head.push(0x80 | n as u8),
                n if n <= u16::MAX as usize => {
                    head.push(0x80 | 126);
                    head.extend_from_slice(&(n as u16).to_be_bytes());
                }
                n => {
                    head.push(0x80 | 127);
                    head.extend_from_slice(&(n as u64).to_be_bytes());
                }
            }
            head.extend_from_slice(&key);
        } else {
            match payload.len() {
                n if n < 126 => head.push(n as u8),
                n if n <= u16::MAX as usize => {
                    head.push(126);
                    head.extend_from_slice(&(n as u16).to_be_bytes());
                }
                n => {
                    head.push(127);
                    head.extend_from_slice(&(n as u64).to_be_bytes());
                }
            }
        }
        self.stream.write_all(&head)?;
        self.stream.write_all(&body)?;
        self.stream.flush()
    }

    /// Poll for one message. Control frames are answered inline and never
    /// surface to the caller. Partial frames stay buffered across calls.
    pub fn read_message(&mut self) -> ReadResult {
        // A parked pong from the previous poll goes out first.
        if let Some(payload) = self.pending_pong.take() {
            let _ = self.send_control(false, &payload);
        }
        loop {
            match self.try_frame() {
                TryFrame::Complete(op, payload) => match op {
                    Opcode::Ping => {
                        let _ = self.send_control(false, &payload);
                        continue;
                    }
                    Opcode::Pong => continue,
                    Opcode::Close => {
                        let _ = self.write_frame(Opcode::Close, &payload);
                        self.closed = true;
                        let _ = self.stream.shutdown(std::net::Shutdown::Both);
                        return ReadResult::Closed;
                    }
                    Opcode::Text | Opcode::Binary => return ReadResult::Message(payload),
                },
                TryFrame::Partial => {
                    // Try to refill from the socket; WouldBlock keeps the bytes.
                    match self.refill() {
                        Refill::Data => continue,
                        Refill::Blocked => return ReadResult::WouldBlock,
                        Refill::Eof => {
                            self.closed = true;
                            return ReadResult::Closed;
                        }
                    }
                }
                TryFrame::Protocol => {
                    let _ = self.close(CLOSE_PROTOCOL, "frame");
                    return ReadResult::Closed;
                }
            }
        }
    }

    /// Pull more bytes into the buffer.
    fn refill(&mut self) -> Refill {
        let mut tmp = [0u8; 8192];
        match self.stream.read(&mut tmp) {
            Ok(0) => Refill::Eof,
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Refill::Data
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                Refill::Blocked
            }
            Err(_) => Refill::Eof,
        }
    }

    /// Try to parse one frame out of the buffer without touching the socket.
    fn try_frame(&mut self) -> TryFrame {
        if self.buf.len() < 2 {
            return TryFrame::Partial;
        }
        let fin = self.buf[0] & 0x80 != 0;
        let op = match Opcode::from_u8(self.buf[0] & 0x0F) {
            Some(op) => op,
            None => return TryFrame::Protocol,
        };
        // Fragmentation is refused (Live frames are single-frame messages).
        if !fin || self.buf[0] & 0x0F == 0x0 {
            return TryFrame::Protocol;
        }
        let masked = self.buf[1] & 0x80 != 0;
        let len7 = (self.buf[1] & 0x7F) as u64;
        let (len_len, len) = match len7 {
            126 => {
                if self.buf.len() < 4 {
                    return TryFrame::Partial;
                }
                (2, u16::from_be_bytes([self.buf[2], self.buf[3]]) as u64)
            }
            127 => {
                if self.buf.len() < 10 {
                    return TryFrame::Partial;
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&self.buf[2..10]);
                (8, u64::from_be_bytes(b))
            }
            n => (0, n),
        };
        // Frame sanity: a Kitae Live patch is at most hundreds of KBs;
        // 16 MiB is a hostile peer, not a page.
        if len > 16 * 1024 * 1024 {
            return TryFrame::Protocol;
        }
        let mask_len = if masked { 4 } else { 0 };
        // The RFC mandates client→server masking: a server READING an
        // unmasked frame is a protocol error the client must have sent
        // masked. Server→client frames are unmasked by design, so the
        // check only bites when the reader is the server.
        if !masked && self.server {
            return TryFrame::Protocol;
        }
        let head_len = (2 + len_len + mask_len) as usize;
        let total = head_len as u64 + len;
        if (self.buf.len() as u64) < total {
            return TryFrame::Partial;
        }
        let mut key = [0u8; 4];
        if masked {
            key.copy_from_slice(&self.buf[2 + len_len..2 + len_len + 4]);
        }
        let payload: Vec<u8> = self.buf[head_len..total as usize]
            .iter()
            .enumerate()
            .map(|(i, &b)| b ^ key[i % 4])
            .collect();
        self.buf.drain(..total as usize);
        TryFrame::Complete(op, payload)
    }
}

enum TryFrame {
    Complete(Opcode, Vec<u8>),
    Partial,
    Protocol,
}

enum Refill {
    Data,
    Blocked,
    Eof,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Full loopback: one client + one server socket exchanging KTLV bytes
    /// through the frame layer, both roles in one test.
    #[test]
    fn client_server_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let sock = TcpStream::connect(addr).unwrap();
            let mut ws = WsStream::new(sock, false);
            ws.send_binary(b"hello-from-client").unwrap();
            match ws.read_message() {
                ReadResult::Message(echo) => assert_eq!(echo, b"server-echo"),
                other => panic!("expected message, got {other:?}"),
            }
        });
        let (sock, _) = listener.accept().unwrap();
        let mut ws = WsStream::new(sock, true);
        match ws.read_message() {
            ReadResult::Message(first) => assert_eq!(first, b"hello-from-client"),
            other => panic!("expected message, got {other:?}"),
        }
        ws.send_binary(b"server-echo").unwrap();
        client.join().unwrap();
    }

    #[test]
    fn pings_are_answered_and_hidden() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let sock = TcpStream::connect(addr).unwrap();
            let mut ws = WsStream::new(sock, false);
            ws.send_control(true, b"ping-body").unwrap();
            ws.send_binary(b"after-ping").unwrap();
            match ws.read_message() {
                ReadResult::Message(got) => assert_eq!(got, b"after-ping"),
                other => panic!("expected message, got {other:?}"),
            }
        });
        let (sock, _) = listener.accept().unwrap();
        let mut ws = WsStream::new(sock, true);
        // The client's ping is consumed and answered inline; the caller only
        // ever sees data messages.
        match ws.read_message() {
            ReadResult::Message(got) => assert_eq!(got, b"after-ping"),
            other => panic!("expected message, got {other:?}"),
        }
        // Echo: the client then reads the inline PONG (hidden) followed
        // by this data frame — both orderings of the assertions hold.
        ws.send_binary(b"after-ping").unwrap();
        client.join().unwrap();
    }

    #[test]
    fn close_handshake_ends_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let sock = TcpStream::connect(addr).unwrap();
            let mut ws = WsStream::new(sock, false);
            assert_eq!(ws.read_message(), ReadResult::Closed);
            assert!(ws.is_closed());
        });
        let (sock, _) = listener.accept().unwrap();
        let mut ws = WsStream::new(sock, true);
        ws.close(CLOSE_NORMAL, "bye").unwrap();
        assert!(ws.is_closed());
        client.join().unwrap();
    }

    #[test]
    fn big_binary_survives() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let want = payload.clone();
        let client = std::thread::spawn(move || {
            let sock = TcpStream::connect(addr).unwrap();
            let mut ws = WsStream::new(sock, false);
            ws.send_binary(&payload).unwrap();
        });
        let (sock, _) = listener.accept().unwrap();
        let mut ws = WsStream::new(sock, true);
        match ws.read_message() {
            ReadResult::Message(got) => {
                assert_eq!(got.len(), want.len());
                assert_eq!(got, want);
            }
            other => panic!("expected message, got {other:?}"),
        }
        client.join().unwrap();
    }
}
