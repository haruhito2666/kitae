//! The Live wire protocol (v1) — `KTLV` frames.
//!
//! Deliberately mirrors the engine's FFI philosophy: versioned, explicit,
//! binary-first. Every frame is
//!
//! ```text
//! MAGIC "KTLV" | u8 version | u16 kind (LE) | u32 payload_len (LE) | payload
//! ```
//!
//! Kinds: `Init` (session id + static HTML + hydration tokens) · `Patch`
//! (monotonic seq + op list) · `Event` (element ref + event name + value) ·
//! `Ping`/`Pong` · `Redirect` · `Error`.
//!
//! The patch-op payload is the same [`PatchOp`] enum the local runtime
//! applies — the op set IS the DOM-mutation surface, which keeps the
//! no-script guarantee auditable in one place.

use kitae_ui::patch::{NodeData, NodeRef, PatchOp};

/// Wire version this crate speaks.
pub const LIVE_VERSION: u8 = 1;
/// The frame magic: "Kitae LiVe".
pub const LIVE_MAGIC: &[u8; 4] = b"KTLV";

/// Frame discriminant (`u16` on the wire, little-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum FrameKind {
    Init = 1,
    Patch = 2,
    Event = 3,
    Ping = 4,
    Pong = 5,
    Redirect = 6,
    Error = 7,
    /// Client → server: "I have applied everything below this seq" —
    /// drives resume-from on reconnect.
    Ack = 8,
}

impl FrameKind {
    fn from_u16(v: u16) -> Option<FrameKind> {
        Some(match v {
            1 => FrameKind::Init,
            2 => FrameKind::Patch,
            3 => FrameKind::Event,
            4 => FrameKind::Ping,
            5 => FrameKind::Pong,
            6 => FrameKind::Redirect,
            7 => FrameKind::Error,
            8 => FrameKind::Ack,
            _ => return None,
        })
    }
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Session id + the full static page (already HTML) + the mount ref.
    Init {
        session: String,
        html: String,
    },
    /// Monotonic sequence + ops; a session replays from an acked seq.
    Patch {
        seq: u64,
        ops: Vec<PatchOp>,
    },
    /// Element ref + event name + optional value (input text, checkbox).
    Event {
        element: u64,
        name: String,
        value: Option<String>,
    },
    Ping,
    Pong,
    Redirect {
        url: String,
    },
    Error {
        message: String,
    },
    Ack {
        seq: u64,
    },
}

// ── encode ─────────────────────────────────────────────────────────────────

fn put_u16(v: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(v: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(v: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Length-prefixed UTF-8 (`u16` length). A string over the 64 KiB wire
/// limit is an ERROR, never a silent truncation — a truncated prefix would
/// decode as valid garbage (or split a UTF-8 sequence).
fn put_str(s: &str, out: &mut Vec<u8>) -> Result<(), String> {
    let bytes = s.as_bytes();
    let n = u16::try_from(bytes.len())
        .map_err(|_| format!("string of {} bytes exceeds the u16 wire limit", bytes.len()))?;
    put_u16(n, out);
    out.extend_from_slice(bytes);
    Ok(())
}

/// Length-prefixed UTF-8 with a `u32` length — for payloads where 64 KiB
/// would be tight (full documents). Over 4 GiB is an error, not a cut.
fn put_str32(s: &str, out: &mut Vec<u8>) -> Result<(), String> {
    let bytes = s.as_bytes();
    let n = u32::try_from(bytes.len())
        .map_err(|_| format!("string of {} bytes exceeds the u32 wire limit", bytes.len()))?;
    put_u32(n, out);
    out.extend_from_slice(bytes);
    Ok(())
}

fn encode_node_data(n: &NodeData, out: &mut Vec<u8>) -> Result<(), String> {
    put_str(&n.tag, out)?;
    match &n.text {
        Some(t) => {
            out.push(1);
            put_str32(t, out)?;
        }
        None => out.push(0),
    }
    let n_attrs = u16::try_from(n.attrs.len())
        .map_err(|_| format!("{} attrs exceeds the u16 wire limit", n.attrs.len()))?;
    put_u16(n_attrs, out);
    for (k, v) in &n.attrs {
        put_str(k, out)?;
        put_str(v, out)?;
    }
    let n_styles = u16::try_from(n.styles.len())
        .map_err(|_| format!("{} styles exceeds the u16 wire limit", n.styles.len()))?;
    put_u16(n_styles, out);
    for (k, v) in &n.styles {
        put_str(k, out)?;
        put_str(v, out)?;
    }
    // The handler ref: how a Live client knows where to aim Event frames
    // without shipping code. Trailing optional-u64 keeps old v1 frames
    // decodable (absent → None) — the field was simply never emitted
    // before the Live client existed.
    match n.handler {
        Some(h) => {
            out.push(1);
            put_u64(h, out);
        }
        None => out.push(0),
    }
    // Node handles ride along the same way (client-local imperative
    // access: focus, scroll, measured rects).
    match n.node_ref {
        Some(h) => {
            out.push(1);
            put_u64(h, out);
        }
        None => out.push(0),
    }
    Ok(())
}

/// Encode one op. Tagged: `u8` op kind then the op's fields. Fallible: an
/// index over the `u16` wire limit is an error, never a wrapped value.
pub fn encode_op(op: &PatchOp, out: &mut Vec<u8>) -> Result<(), String> {
    fn u16_idx(v: usize) -> Result<u16, String> {
        u16::try_from(v).map_err(|_| format!("index {v} exceeds the u16 wire limit"))
    }
    match op {
        PatchOp::InsertNode {
            parent,
            index,
            id,
            node,
        } => {
            out.push(1);
            put_u64(parent.0, out);
            put_u16(u16_idx(*index)?, out);
            put_u64(id.0, out);
            encode_node_data(node, out)
        }
        PatchOp::RemoveNode { parent, index } => {
            out.push(2);
            put_u64(parent.0, out);
            put_u16(u16_idx(*index)?, out);
            Ok(())
        }
        PatchOp::MoveNode { parent, from, to } => {
            out.push(3);
            put_u64(parent.0, out);
            put_u16(u16_idx(*from)?, out);
            put_u16(u16_idx(*to)?, out);
            Ok(())
        }
        PatchOp::SetText { node, text } => {
            out.push(4);
            put_u64(node.0, out);
            put_str32(text, out)
        }
        PatchOp::SetAttr { node, name, value } => {
            out.push(5);
            put_u64(node.0, out);
            put_str(name, out)?;
            put_str(value, out)
        }
        PatchOp::RemoveAttr { node, name } => {
            out.push(6);
            put_u64(node.0, out);
            put_str(name, out)
        }
        PatchOp::SetStyle { node, name, value } => {
            out.push(7);
            put_u64(node.0, out);
            put_str(name, out)?;
            put_str(value, out)
        }
        PatchOp::SetFocus { node } => {
            out.push(8);
            put_u64(node.0, out);
            Ok(())
        }
        PatchOp::ScrollIntoView { node } => {
            out.push(9);
            put_u64(node.0, out);
            Ok(())
        }
    }
}

/// A frame with header + payload. Fallible everywhere a field could exceed
/// the wire's length prefixes — the caller gets an error naming the field,
/// not a silently-corrupt stream.
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, String> {
    let mut payload = Vec::new();
    let kind = match frame {
        Frame::Init { session, html } => {
            put_str(session, &mut payload)?;
            put_str32(html, &mut payload)?;
            FrameKind::Init
        }
        Frame::Patch { seq, ops } => {
            put_u64(*seq, &mut payload);
            let n_ops = u16::try_from(ops.len())
                .map_err(|_| format!("{} ops exceeds the u16 wire limit", ops.len()))?;
            put_u16(n_ops, &mut payload);
            for op in ops {
                encode_op(op, &mut payload)?;
            }
            FrameKind::Patch
        }
        Frame::Event {
            element,
            name,
            value,
        } => {
            put_u64(*element, &mut payload);
            put_str(name, &mut payload)?;
            match value {
                Some(v) => {
                    payload.push(1);
                    put_str32(v, &mut payload)?;
                }
                None => payload.push(0),
            }
            FrameKind::Event
        }
        Frame::Ping => FrameKind::Ping,
        Frame::Pong => FrameKind::Pong,
        Frame::Redirect { url } => {
            put_str(url, &mut payload)?;
            FrameKind::Redirect
        }
        Frame::Error { message } => {
            put_str32(message, &mut payload)?;
            FrameKind::Error
        }
        Frame::Ack { seq } => {
            put_u64(*seq, &mut payload);
            FrameKind::Ack
        }
    };
    let mut out = Vec::with_capacity(payload.len() + 11);
    out.extend_from_slice(LIVE_MAGIC);
    out.push(LIVE_VERSION);
    put_u16(kind as u16, &mut out);
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        format!(
            "payload of {} bytes exceeds the u32 wire limit",
            payload.len()
        )
    })?;
    put_u32(payload_len, &mut out);
    out.extend_from_slice(&payload);
    Ok(out)
}

// ── decode ─────────────────────────────────────────────────────────────────

/// A cursor over a byte slice — decode is fallible everywhere a hostile
/// peer could lie about lengths.
struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Rd<'a> {
        Rd { b, p: 0 }
    }

    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.p)?;
        self.p += 1;
        Some(v)
    }

    fn u16(&mut self) -> Option<u16> {
        let s = self.b.get(self.p..self.p + 2)?;
        self.p += 2;
        Some(u16::from_le_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let s = self.b.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn u64(&mut self) -> Option<u64> {
        let s = self.b.get(self.p..self.p + 8)?;
        self.p += 8;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Some(u64::from_le_bytes(a))
    }

    fn str(&mut self) -> Option<String> {
        let n = self.u16()? as usize;
        let s = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(String::from_utf8_lossy(s).into_owned())
    }

    fn str32(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let s = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(String::from_utf8_lossy(s).into_owned())
    }
}

fn decode_node_data(r: &mut Rd) -> Option<NodeData> {
    let tag = r.str()?;
    let text = match r.u8()? {
        0 => None,
        1 => Some(r.str32()?),
        _ => None,
    };
    let mut attrs = Vec::new();
    for _ in 0..r.u16()? {
        attrs.push((r.str()?, r.str()?));
    }
    let mut styles = Vec::new();
    for _ in 0..r.u16()? {
        styles.push((r.str()?, r.str()?));
    }
    let handler = match r.u8()? {
        1 => Some(r.u64()?),
        _ => None,
    };
    let node_ref = match r.u8()? {
        1 => Some(r.u64()?),
        _ => None,
    };
    Some(NodeData {
        tag,
        text,
        attrs,
        styles,
        handler,
        node_ref,
    })
}

fn decode_op(r: &mut Rd) -> Option<PatchOp> {
    Some(match r.u8()? {
        1 => PatchOp::InsertNode {
            parent: NodeRef(r.u64()?),
            index: r.u16()? as usize,
            id: NodeRef(r.u64()?),
            node: decode_node_data(r)?,
        },
        2 => PatchOp::RemoveNode {
            parent: NodeRef(r.u64()?),
            index: r.u16()? as usize,
        },
        3 => PatchOp::MoveNode {
            parent: NodeRef(r.u64()?),
            from: r.u16()? as usize,
            to: r.u16()? as usize,
        },
        4 => PatchOp::SetText {
            node: NodeRef(r.u64()?),
            text: r.str32()?,
        },
        5 => PatchOp::SetAttr {
            node: NodeRef(r.u64()?),
            name: r.str()?,
            value: r.str()?,
        },
        6 => PatchOp::RemoveAttr {
            node: NodeRef(r.u64()?),
            name: r.str()?,
        },
        7 => PatchOp::SetStyle {
            node: NodeRef(r.u64()?),
            name: r.str()?,
            value: r.str()?,
        },
        8 => PatchOp::SetFocus {
            node: NodeRef(r.u64()?),
        },
        9 => PatchOp::ScrollIntoView {
            node: NodeRef(r.u64()?),
        },
        _ => return None,
    })
}

/// Decode one frame from a complete buffer. `None` on any malformed input.
pub fn decode_frame(buf: &[u8]) -> Option<Frame> {
    if buf.len() < 11 || &buf[0..4] != LIVE_MAGIC || buf[4] != LIVE_VERSION {
        return None;
    }
    let kind = FrameKind::from_u16(u16::from_le_bytes([buf[5], buf[6]]))?;
    let len = u32::from_le_bytes([buf[7], buf[8], buf[9], buf[10]]) as usize;
    let payload = buf.get(11..11 + len)?;
    let mut r = Rd::new(payload);
    Some(match kind {
        FrameKind::Init => Frame::Init {
            session: r.str()?,
            html: r.str32()?,
        },
        FrameKind::Patch => {
            let seq = r.u64()?;
            let mut ops = Vec::new();
            for _ in 0..r.u16()? {
                ops.push(decode_op(&mut r)?);
            }
            Frame::Patch { seq, ops }
        }
        FrameKind::Event => Frame::Event {
            element: r.u64()?,
            name: r.str()?,
            value: match r.u8()? {
                0 => None,
                1 => Some(r.str32()?),
                _ => None,
            },
        },
        FrameKind::Ping => Frame::Ping,
        FrameKind::Pong => Frame::Pong,
        FrameKind::Redirect => Frame::Redirect { url: r.str()? },
        FrameKind::Error => Frame::Error {
            message: r.str32()?,
        },
        FrameKind::Ack => Frame::Ack { seq: r.u64()? },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(f: Frame) -> Frame {
        let bytes = encode_frame(&f).expect("roundtrip encodes");
        decode_frame(&bytes).expect("roundtrip decodes")
    }

    #[test]
    fn header_shape() {
        let bytes = encode_frame(&Frame::Ping).expect("encodes");
        assert_eq!(&bytes[0..4], b"KTLV");
        assert_eq!(bytes[4], 1);
        // kind 4 (Ping), len 0
        assert_eq!(&bytes[5..11], &[4, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn version_is_1() {
        assert_eq!(LIVE_VERSION, 1);
    }

    #[test]
    fn patch_roundtrip_all_ops() {
        let ops = vec![
            PatchOp::InsertNode {
                parent: NodeRef(0),
                index: 2,
                id: NodeRef(7),
                node: NodeData {
                    tag: "div".into(),
                    text: None,
                    attrs: vec![("class".into(), "card".into())],
                    styles: vec![("background".into(), "#fff".into())],
                    handler: Some(7),
                    node_ref: Some(42),
                },
            },
            PatchOp::SetText {
                node: NodeRef(3),
                text: "Count: 1".into(),
            },
            PatchOp::SetAttr {
                node: NodeRef(3),
                name: "value".into(),
                value: "42".into(),
            },
            PatchOp::RemoveAttr {
                node: NodeRef(3),
                name: "value".into(),
            },
            PatchOp::SetStyle {
                node: NodeRef(3),
                name: "color".into(),
                value: "red".into(),
            },
            PatchOp::MoveNode {
                parent: NodeRef(0),
                from: 1,
                to: 4,
            },
            PatchOp::RemoveNode {
                parent: NodeRef(0),
                index: 0,
            },
            PatchOp::SetFocus { node: NodeRef(9) },
            PatchOp::ScrollIntoView { node: NodeRef(9) },
        ];
        let back = roundtrip(Frame::Patch { seq: 12, ops });
        match back {
            Frame::Patch { seq, ops } => {
                assert_eq!(seq, 12);
                assert_eq!(ops.len(), 9);
                assert!(matches!(&ops[0], PatchOp::InsertNode { node, .. }
                    if node.tag == "div" && node.attrs[0].1 == "card" && node.handler == Some(7)
                        && node.node_ref == Some(42)));
                assert!(matches!(&ops[1], PatchOp::SetText { text, .. } if text == "Count: 1"));
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn text_nodes_roundtrip_with_long_text() {
        let long = "x".repeat(70_000);
        let back = roundtrip(Frame::Init {
            session: "s-1".into(),
            html: long.clone(),
        });
        match back {
            Frame::Init { session, html } => {
                assert_eq!(session, "s-1");
                assert_eq!(html, long);
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn event_roundtrip() {
        let back = roundtrip(Frame::Event {
            element: 62,
            name: "change".into(),
            value: Some("typed text".into()),
        });
        assert_eq!(
            back,
            Frame::Event {
                element: 62,
                name: "change".into(),
                value: Some("typed text".into())
            }
        );
    }

    #[test]
    fn garbage_rejected() {
        assert!(decode_frame(b"").is_none());
        assert!(decode_frame(b"KTLV\x02").is_none()); // wrong version
        assert!(decode_frame(b"XXXX\x01\x04\x00\x00\x00\x00\x00").is_none()); // wrong magic
                                                                              // Payload claimed but missing: Pong with a 4-byte body cut short.
        let mut b = encode_frame(&Frame::Pong).expect("encodes");
        b.truncate(11);
        assert!(
            decode_frame(&b).is_some(),
            "a zero-length payload is still valid"
        );
        // A real truncation: Init (session + html strings) cut mid-payload.
        let mut b = encode_frame(&Frame::Init {
            session: "s-1".into(),
            html: "body".into(),
        })
        .expect("encodes");
        b.truncate(13);
        assert!(decode_frame(&b).is_none());
    }

    /// An oversized field is an ERROR, never a silent truncation (a cut
    /// string would decode as valid garbage on the far end).
    #[test]
    fn oversized_fields_error_not_truncate() {
        let long = "y".repeat(70_000);
        // Event names ride put_str (u16): 70 KB is refused.
        let err = encode_frame(&Frame::Event {
            element: 1,
            name: long.clone(),
            value: None,
        })
        .expect_err("u16 overflow refused");
        assert!(err.contains("u16"), "names the limit: {err}");
        // The same 70 KB through a u32 field (Init html) encodes fine.
        assert!(encode_frame(&Frame::Init {
            session: "s-1".into(),
            html: long
        })
        .is_ok());
        // An op index past u16 is refused, not wrapped.
        let err = encode_frame(&Frame::Patch {
            seq: 0,
            ops: vec![PatchOp::RemoveNode {
                parent: NodeRef(0),
                index: u16::MAX as usize + 1,
            }],
        })
        .expect_err("index overflow refused");
        assert!(err.contains("u16"), "names the limit: {err}");
    }

    #[test]
    fn ack_roundtrip() {
        assert_eq!(roundtrip(Frame::Ack { seq: 999 }), Frame::Ack { seq: 999 });
    }
}
