//! WPT tokenizer conformance harness (I5).
//!
//! Runs every case in the vendored html5lib tokenizer corpus
//! (`tests/data/tokenizer/*.test`) through the batch tokenizer
//! `kitae_html::tokenizer::tokenize` and compares the token stream against
//! the expected output. `docs/WPT_SCORECARD.md` records the measured pass
//! rate; the baseline assertion guards against regressions only.
//!
//! Documented normalizations and skips:
//!   * Consecutive kitae `Text` tokens merge into one `Character` token —
//!     the corpus emits one per contiguous run.
//!   * Attribute dicts compare order-insensitively (sorted).
//!   * Cases with `initialStates` other than the Data state are skipped
//!     (kitae's batch entry point always starts in Data).
//!   * Cases with `lastStartTag` are skipped (fragment-context states).
//!   * `xmlViolation.test` is skipped whole (it drives fragment/context
//!     states the batch entry point cannot reach).
//!   * `doubleEscaped` cases are unescaped (`\uXXXX`, including surrogate
//!     pairs) before running, on both input and expectations.
//!   * Error arrays are never compared — kitae has no error channel
//!     (scorecard gap).
//!
//! Set `KITAE_WPT_DIFF=1` to print the first divergences per file.

use kitae_html::tokenizer::{tokenize, Token};
use std::fs;
use std::path::Path;

/// Recorded in docs/WPT_SCORECARD.md; set after the first measured run.
/// Measured 2026-09-25: 6256/6256 (100.0%).
const BASELINE_PASS_RATE: f64 = 1.000;

// ---- Minimal JSON parser (no external deps; lenient about raw control
// characters inside strings, strict about structure) ----

#[derive(Debug, Clone, PartialEq)]
enum JVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<JVal>),
    Obj(Vec<(String, JVal)>),
}

struct P<'a> {
    s: &'a [char],
    i: usize,
}

fn parse_json(text: &str) -> Option<JVal> {
    let chars: Vec<char> = text.chars().collect();
    let mut p = P { s: &chars, i: 0 };
    let v = p.value()?;
    Some(v)
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn value(&mut self) -> Option<JVal> {
        self.ws();
        match self.peek()? {
            '{' => self.object(),
            '[' => self.array(),
            '"' => self.string().map(JVal::Str),
            't' => self.lit("true").map(|_| JVal::Bool(true)),
            'f' => self.lit("false").map(|_| JVal::Bool(false)),
            'n' => self.lit("null").map(|_| JVal::Null),
            _ => self.number(),
        }
    }

    fn lit(&mut self, want: &str) -> Option<()> {
        for (k, c) in want.chars().enumerate() {
            if self.s.get(self.i + k) != Some(&c) {
                return None;
            }
        }
        self.i += want.len();
        Some(())
    }

    fn number(&mut self) -> Option<JVal> {
        let start = self.i;
        if self.peek() == Some('-') {
            self.i += 1;
        }
        while self.peek().map_or(false, |c| {
            c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')
        }) {
            self.i += 1;
        }
        let text: String = self.s[start..self.i].iter().collect();
        text.parse::<f64>().ok().map(JVal::Num)
    }

    fn string(&mut self) -> Option<String> {
        if self.peek() != Some('"') {
            return None;
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                '"' => return Some(out),
                '\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    match e {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                // Surrogate pair: \uXXXX\uXXXX.
                                let paired = self.peek() == Some('\\')
                                    && self.s.get(self.i + 1) == Some(&'u')
                                    && self
                                        .s
                                        .get(self.i + 2..self.i + 6)
                                        .map_or(false, |w| w.iter().all(|c| c.is_ascii_hexdigit()))
                                    && self
                                        .s
                                        .get(self.i + 2..self.i + 6)
                                        .and_then(|w| {
                                            u32::from_str_radix(&String::from_iter(w), 16).ok()
                                        })
                                        .map_or(false, |lo| (0xDC00..0xE000).contains(&lo));
                                if paired {
                                    let lo: u32 = u32::from_str_radix(
                                        &String::from_iter(self.s[self.i + 2..self.i + 6].iter()),
                                        16,
                                    )
                                    .ok()?;
                                    self.i += 6;
                                    let combined = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(combined).unwrap_or('\u{FFFD}'));
                                } else {
                                    out.push('\u{FFFD}');
                                }
                            } else if (0xDC00..0xE000).contains(&cp) {
                                out.push('\u{FFFD}');
                            } else {
                                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            }
                        }
                        _ => return None,
                    }
                }
                _ => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut cp = 0u32;
        for k in 0..4 {
            let d = self.s.get(self.i + k)?.to_digit(16)?;
            cp = cp * 16 + d;
        }
        self.i += 4;
        Some(cp)
    }

    fn array(&mut self) -> Option<JVal> {
        self.i += 1; // '['
        let mut items = vec![];
        self.ws();
        if self.peek() == Some(']') {
            self.i += 1;
            return Some(JVal::Arr(items));
        }
        loop {
            items.push(self.value()?);
            self.ws();
            match self.peek()? {
                ',' => self.i += 1,
                ']' => {
                    self.i += 1;
                    return Some(JVal::Arr(items));
                }
                _ => return None,
            }
        }
    }

    fn object(&mut self) -> Option<JVal> {
        self.i += 1; // '{'
        let mut fields = vec![];
        self.ws();
        if self.peek() == Some('}') {
            self.i += 1;
            return Some(JVal::Obj(fields));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(':') {
                return None;
            }
            self.i += 1;
            let val = self.value()?;
            fields.push((key, val));
            self.ws();
            match self.peek()? {
                ',' => self.i += 1,
                '}' => {
                    self.i += 1;
                    return Some(JVal::Obj(fields));
                }
                _ => return None,
            }
        }
    }
}

// ---- doubleEscaped support: the corpus double-encodes some cases, so
// after the JSON unescape another `\uXXXX` pass runs (with surrogate
// pairs; lone surrogates become U+FFFD). ----

fn unescape_unicode(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1) == Some(&'u') {
            let mut cp = 0u32;
            let mut ok = true;
            for k in 0..4 {
                match chars.get(i + 2 + k).and_then(|c| c.to_digit(16)) {
                    Some(d) => cp = cp * 16 + d,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                i += 6;
                if (0xD800..0xDC00).contains(&cp) {
                    let mut lo = 0u32;
                    let mut ok2 = true;
                    for k in 0..4 {
                        match chars.get(i + 2 + k).and_then(|c| c.to_digit(16)) {
                            Some(d) => lo = lo * 16 + d,
                            None => {
                                ok2 = false;
                                break;
                            }
                        }
                    }
                    if ok2
                        && chars.get(i) == Some(&'\\')
                        && chars.get(i + 1) == Some(&'u')
                        && (0xDC00..0xE000).contains(&lo)
                    {
                        i += 6;
                        let combined = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        out.push(char::from_u32(combined).unwrap_or('\u{FFFD}'));
                    } else {
                        out.push('\u{FFFD}');
                    }
                } else {
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                }
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---- Expected token model ----

#[derive(Debug, PartialEq, Clone)]
enum ETok {
    Doctype {
        name: Option<String>,
        public_id: Option<String>,
        system_id: Option<String>,
        correct: bool,
    },
    StartTag {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag(String),
    Comment(String),
    Character(String),
}

/// JSON string field: null counts as "" (attr values use it).
fn jstr(v: &JVal, double: bool) -> Option<String> {
    match v {
        JVal::Str(s) => Some(if double {
            unescape_unicode(s)
        } else {
            s.clone()
        }),
        JVal::Null => Some(String::new()),
        _ => None,
    }
}

/// JSON string-or-null field kept as Option (DOCTYPE ids/names).
fn joptstr(v: &JVal, double: bool) -> Option<Option<String>> {
    match v {
        JVal::Str(s) => Some(Some(if double {
            unescape_unicode(s)
        } else {
            s.clone()
        })),
        JVal::Null => Some(None),
        _ => None,
    }
}

fn expected_tokens(output: &JVal, double: bool) -> Option<Vec<ETok>> {
    let JVal::Arr(tokens) = output else {
        return None;
    };
    let mut out = vec![];
    for t in tokens {
        let JVal::Arr(parts) = t else {
            return None;
        };
        let kind = jstr(parts.first()?, false)?;
        match kind.as_str() {
            "DOCTYPE" => out.push(ETok::Doctype {
                name: joptstr(parts.get(1)?, double)?,
                public_id: joptstr(parts.get(2)?, double)?,
                system_id: joptstr(parts.get(3)?, double)?,
                correct: !matches!(parts.get(4), Some(JVal::Bool(false))),
            }),
            "StartTag" => {
                let name = jstr(parts.get(1)?, double)?;
                let mut attrs = vec![];
                if let JVal::Obj(fields) = parts.get(2)? {
                    for (k, v) in fields {
                        attrs.push((k.clone(), jstr(v, double)?));
                    }
                }
                attrs.sort();
                out.push(ETok::StartTag {
                    name,
                    attrs,
                    self_closing: jstr(parts.get(3)?, double)?.as_str() == "self-closing",
                });
            }
            "EndTag" => out.push(ETok::EndTag(jstr(parts.get(1)?, double)?)),
            "Comment" => out.push(ETok::Comment(jstr(parts.get(1)?, double)?)),
            "Character" => out.push(ETok::Character(jstr(parts.get(1)?, double)?)),
            _ => return None,
        }
    }
    // The corpus may list adjacent character tokens separately; the spec
    // makes no distinction, so merge them to match kitae's text-run shape.
    let mut merged: Vec<ETok> = vec![];
    for t in out {
        match (merged.last_mut(), &t) {
            (Some(ETok::Character(prev)), ETok::Character(s)) => prev.push_str(s),
            _ => merged.push(t),
        }
    }
    Some(merged)
}

/// kitae's token stream in the same shape (text runs merged).
fn kitae_tokens(toks: Vec<Token>) -> Vec<ETok> {
    let mut out = vec![];
    for t in toks {
        match t {
            Token::Text(s) => {
                if let Some(ETok::Character(prev)) = out.last_mut() {
                    prev.push_str(&s);
                } else {
                    out.push(ETok::Character(s));
                }
            }
            Token::StartTag {
                name,
                mut attrs,
                self_closing,
            } => {
                attrs.sort();
                out.push(ETok::StartTag {
                    name,
                    attrs,
                    self_closing,
                });
            }
            Token::EndTag { name } => out.push(ETok::EndTag(name)),
            Token::Comment(d) => out.push(ETok::Comment(d)),
            Token::Doctype {
                name,
                public_id,
                system_id,
                force_quirks,
            } => out.push(ETok::Doctype {
                name: (!name.is_empty()).then_some(name),
                public_id,
                system_id,
                correct: !force_quirks,
            }),
        }
    }
    out
}

#[test]
fn wpt_tokenizer_conformance() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/tokenizer");
    let mut files: Vec<_> = fs::read_dir(&dir)
        .expect("tokenizer corpus present")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |x| x == "test"))
        .collect();
    files.sort();

    let diff = std::env::var("KITAE_WPT_DIFF").is_ok();
    let mut total = 0usize;
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut per_file: Vec<(String, usize, usize, usize)> = vec![];

    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == "xmlViolation.test" {
            continue; // fragment-context states; skipped whole (module docs)
        }
        let text = fs::read_to_string(path).unwrap();
        let root = parse_json(&text).expect("valid JSON corpus");
        let JVal::Obj(fields) = &root else {
            panic!("{name}: top level must be an object");
        };
        let Some(JVal::Arr(tests)) = fields.iter().find(|(k, _)| k == "tests").map(|(_, v)| v)
        else {
            panic!("{name}: missing tests array");
        };
        let mut p = 0usize;
        let mut f = 0usize;
        let mut s = 0usize;
        for t in tests {
            let JVal::Obj(fields) = t else {
                continue;
            };
            let get = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v);
            // Non-Data initial states and fragment-context cases are out of
            // reach for the batch entry point (module docs).
            if let Some(JVal::Arr(states)) = get("initialStates") {
                match states.first() {
                    Some(JVal::Str(st)) if st == "Data state" => {}
                    _ => {
                        s += 1;
                        continue;
                    }
                }
            }
            if get("lastStartTag").is_some() {
                s += 1;
                continue;
            }
            let (Some(JVal::Str(input)), Some(output)) = (get("input"), get("output")) else {
                continue;
            };
            let double = matches!(get("doubleEscaped"), Some(JVal::Bool(true)));
            let Some(expected) = expected_tokens(output, double) else {
                continue;
            };
            total += 1;
            let input = if double {
                unescape_unicode(input)
            } else {
                input.clone()
            };
            let actual = kitae_tokens(tokenize(&input));
            if expected == actual {
                p += 1;
            } else {
                f += 1;
                if diff {
                    println!(
                        "[{name}] {:?}\n  in:    {input:?}\n  wpt:   {expected:?}\n  kitae: {actual:?}",
                        get("description").and_then(|v| match v {
                            JVal::Str(d) => Some(d.clone()),
                            _ => None,
                        })
                    );
                }
            }
        }
        passed += p;
        skipped += s;
        per_file.push((name, p, f, s));
    }

    for (name, p, f, s) in &per_file {
        println!("{name}: {p} pass / {f} fail / {s} skip");
    }
    let rate = if total == 0 {
        1.0
    } else {
        passed as f64 / total as f64
    };
    println!(
        "WPT tokenizer: {passed}/{total} pass ({:.1}%), {skipped} skipped",
        rate * 100.0
    );
    assert!(
        rate >= BASELINE_PASS_RATE,
        "tokenizer pass rate {:.1}% regressed below the recorded baseline {:.1}% \
         (see docs/WPT_SCORECARD.md; run with KITAE_WPT_DIFF=1 to inspect divergences)",
        rate * 100.0,
        BASELINE_PASS_RATE * 100.0,
    );
}
