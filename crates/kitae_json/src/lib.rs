//! A small JSON codec — the agent plane's wire format.
//!
//! The closed-API philosophy applies to dependencies too: this workspace
//! hand-rolls its protocol layers (HTTP/2 cleartext, WebSocket, the Live
//! patch format) and ships no serde. The agent surface needs exactly two
//! operations — parse a request, serialize a result — so they live here,
//! auditable in one file, with the edge cases that actually bite
//! (escapes, surrogate pairs, depth bombs, trailing garbage) covered by
//! tests rather than by a spec-conformance suite.
//!
//! Objects preserve insertion order (`Vec<(String, Value)>`): MCP tool
//! results and evidence records read better when the fields keep the
//! order the producer wrote, and no consumer here needs a hash map.

use std::fmt::Write;

/// One JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    /// `{ "key": value, … }` from pairs, in order.
    pub fn obj(pairs: Vec<(&str, Value)>) -> Value {
        Value::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn s(text: impl Into<String>) -> Value {
        Value::Str(text.into())
    }

    pub fn arr(items: Vec<Value>) -> Value {
        Value::Arr(items)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Serialize compactly (no whitespace).
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        write_value(self, &mut out);
        out
    }
}

/// Parse a complete JSON document. Trailing non-whitespace is an error
/// (`"1 2"` must not parse as `1`): agent transports carry one document
/// per message, and a silently ignored tail is how streams desync.
pub fn parse(input: &str) -> Result<Value, String> {
    let bytes = input.as_bytes();
    let mut p = Parser {
        bytes,
        pos: 0,
        depth: 0,
    };
    p.skip_ws();
    let value = p.value()?;
    p.skip_ws();
    if p.pos != bytes.len() {
        return Err(format!("trailing input at byte {}", p.pos));
    }
    Ok(value)
}

const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, word: &str) -> Result<(), String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(())
        } else {
            Err(format!("expected `{word}` at byte {}", self.pos))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        if self.depth >= MAX_DEPTH {
            return Err("JSON nesting is too deep".into());
        }
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => {
                self.expect("true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.expect("false")?;
                Ok(Value::Bool(false))
            }
            Some(b'n') => {
                self.expect("null")?;
                Ok(Value::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(format!("unexpected byte at {}", self.pos)),
            None => Err("unexpected end of input".into()),
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.depth += 1;
        self.pos += 1; // '{'
        let mut pairs: Vec<(String, Value)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Obj(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(format!("object key expected at byte {}", self.pos));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.bump() != Some(b':') {
                return Err(format!("`:` expected at byte {}", self.pos));
            }
            self.skip_ws();
            let value = self.value()?;
            // Last duplicate wins, matching every mainstream parser.
            if let Some(slot) = pairs.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = value;
            } else {
                pairs.push((key, value));
            }
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(format!("`,` or `}}` expected at byte {}", self.pos - 1)),
            }
        }
        self.depth -= 1;
        Ok(Value::Obj(pairs))
    }

    fn array(&mut self) -> Result<Value, String> {
        self.depth += 1;
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(format!("`,` or `]` expected at byte {}", self.pos - 1)),
            }
        }
        self.depth -= 1;
        Ok(Value::Arr(items))
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let b = self.bump().ok_or("unterminated string")?;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = self.bump().ok_or("unterminated escape")?;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            // Surrogate pair: a high surrogate must be
                            // followed by \uDC00–\uDFFF, or both halves
                            // become U+FFFD (the WHATWG behavior).
                            if (0xD800..=0xDBFF).contains(&cp) {
                                let paired = self.peek() == Some(b'\\')
                                    && self.bytes.get(self.pos + 1) == Some(&b'u');
                                if paired {
                                    self.pos += 2;
                                    let lo = self.hex4()?;
                                    if (0xDC00..=0xDFFF).contains(&lo) {
                                        let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                        out.push(char::from_u32(c).unwrap_or('\u{FFFD}'));
                                    } else {
                                        out.push('\u{FFFD}');
                                        out.push(char::from_u32(lo).unwrap_or('\u{FFFD}'));
                                    }
                                } else {
                                    out.push('\u{FFFD}');
                                }
                            } else if (0xDC00..=0xDFFF).contains(&cp) {
                                out.push('\u{FFFD}');
                            } else {
                                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            }
                        }
                        _ => return Err(format!("bad escape `\\{}`", esc as char)),
                    }
                }
                // Raw control bytes are not legal in JSON strings.
                0x00..=0x1F => return Err("raw control byte in string".into()),
                _ => {
                    // Copy the whole UTF-8 sequence.
                    let start = self.pos - 1;
                    let len = utf8_len(b);
                    let end = start + len;
                    if end > self.bytes.len() {
                        return Err("truncated UTF-8 in string".into());
                    }
                    match std::str::from_utf8(&self.bytes[start..end]) {
                        Ok(s) => {
                            out.push_str(s);
                            self.pos = end;
                        }
                        Err(_) => return Err("invalid UTF-8 in string".into()),
                    }
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut n = 0u32;
        for _ in 0..4 {
            let b = self.bump().ok_or("truncated \\u escape")?;
            let d = (b as char)
                .to_digit(16)
                .ok_or_else(|| format!("bad hex digit `{}` in \\u escape", b as char))?;
            n = n * 16 + d;
        }
        Ok(n)
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // JSON forbids leading zeros: `0` alone, or `0.5`, never `01`.
        if self.peek() == Some(b'0') {
            self.pos += 1;
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(format!("leading zero at byte {start}"));
            }
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or("");
        // A lone `-` or a trailing `.`/`e` parses as NaN in Rust; JSON
        // requires digits on both sides.
        if text.is_empty()
            || text == "-"
            || text.ends_with(['.', 'e', 'E', '+', '-'])
            || !text.bytes().any(|b| b.is_ascii_digit())
        {
            return Err(format!("bad number `{text}` at byte {start}"));
        }
        text.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| format!("bad number `{text}`"))
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1, // invalid lead byte: let from_utf8 reject it
    }
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Num(n) => write_number(*n, out),
        Value::Str(s) => write_string(s, out),
        Value::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Obj(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_value(v, out);
            }
            out.push('}');
        }
    }
}

fn write_number(n: f64, out: &mut String) {
    if !n.is_finite() {
        // JSON has no NaN/Infinity: honest null beats a parse failure on
        // the other side.
        out.push_str("null");
    } else if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        let _ = write!(out, "{}", n as i64);
    } else {
        // `{:?}` prints the shortest round-trip form for f64.
        let _ = write!(out, "{n:?}");
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_shapes_the_agent_plane_uses() {
        let doc = Value::obj(vec![
            ("url", Value::s("https://example.test/")),
            ("words", Value::Num(42.0)),
            ("ok", Value::Bool(true)),
            ("blocks", Value::arr(vec![Value::Null, Value::s("heading")])),
        ]);
        let text = doc.to_json();
        assert_eq!(
            text,
            "{\"url\":\"https://example.test/\",\"words\":42,\"ok\":true,\"blocks\":[null,\"heading\"]}"
        );
        assert_eq!(parse(&text).unwrap(), doc);
    }

    #[test]
    fn parses_escapes_and_surrogate_pairs() {
        let value = parse(r#"{"s":"a\"b\\c\/d\n\t\u0041\u00e9\ud83d\ude00\ud800"}"#).unwrap();
        // The lone high surrogate degrades to U+FFFD; the pair is an emoji.
        assert_eq!(
            value.get("s").and_then(Value::as_str),
            Some("a\"b\\c/d\n\tAé😀\u{FFFD}")
        );
        // Serializing escapes the control characters back.
        let back = Value::s("a\nb\tc\u{8}\u{1f}").to_json();
        assert_eq!(back, "\"a\\nb\\tc\\b\\u001f\"");
        assert_eq!(parse(&back).unwrap().as_str(), Some("a\nb\tc\u{8}\u{1f}"));
    }

    #[test]
    fn rejects_malformed_input_instead_of_guessing() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":}",
            "{\"a\" 1}",
            "01",
            "-",
            "1.",
            "1e",
            "tru",
            "\"unterminated",
            "\"raw\tcontrol\"",
            "1 2",
            "{} {}",
            "nul",
        ] {
            assert!(parse(bad).is_err(), "must reject {bad:?}");
        }
        // Depth bomb: nesting past the cap is an error, not a stack crash.
        let deep = format!("{}{}", "[".repeat(80), "]".repeat(80));
        assert!(parse(&deep).is_err());
        // At the cap it parses.
        let ok = format!("{}{}", "[".repeat(60), "]".repeat(60));
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn duplicate_keys_take_the_last_and_numbers_round_trip() {
        let value = parse(r#"{"a":1,"a":2,"n":-1.5e3,"big":9007199254740993}"#).unwrap();
        assert_eq!(value.get("a").and_then(Value::as_f64), Some(2.0));
        assert_eq!(value.get("n").and_then(Value::as_f64), Some(-1500.0));
        // Beyond 2^53 the double loses precision — documented, not special-cased.
        assert!(value.get("big").and_then(Value::as_f64).unwrap() >= 9007199254740992.0);
        // Non-finite values serialize as null (JSON has no NaN).
        assert_eq!(Value::Num(f64::NAN).to_json(), "null");
        assert_eq!(Value::Num(2.5).to_json(), "2.5");
    }
}
