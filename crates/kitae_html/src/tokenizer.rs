//! HTML tokenizer: characters → tokens. Hand-rolled per the WHATWG tokenizer
//! states (simplified), with named/numeric entity decoding and full support
//! for quoted/unquoted attributes, comments, doctype, and raw-text elements.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// `<!DOCTYPE …>`. `force_quirks` mirrors the spec flag: `true` when the
    /// token must switch the document into quirks mode (missing name,
    /// missing/abrupt identifiers). The tree builder ignores it today, but
    /// the WPT tokenizer harness compares it.
    Doctype {
        name: String,
        public_id: Option<String>,
        system_id: Option<String>,
        force_quirks: bool,
    },
    StartTag {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag {
        name: String,
    },
    Text(String),
    /// `<!-- … -->` (and bogus `<!…>` comments): the data between the
    /// delimiters, verbatim.
    Comment(String),
}

/// Tokenize a complete document in one shot (batch mode). Public harness
/// entry point for the WPT tokenizer conformance suite.
pub fn tokenize(html: &str) -> Vec<Token> {
    let mut t = Tokenizer::new(html);
    let mut out = Vec::new();
    while let Some(tok) = t.next_token() {
        out.push(tok);
    }
    out
}

pub struct Tokenizer {
    input: Vec<char>,
    pos: usize,
    /// Tokens queued ahead (raw-text content is emitted together with its
    /// start tag, then the end tag tokenizes normally).
    pending: std::collections::VecDeque<Token>,
    /// False while more input may still arrive via [`feed`]: truncation
    /// points rewind and wait instead of emitting partial tokens.
    eof: bool,
    /// Input-stream preprocessing: a CR awaiting its successor (CRLF
    /// collapses to one LF; a lone CR becomes LF). Held across feed()
    /// boundaries so streaming normalizes exactly like the batch path.
    pending_cr: bool,
    plaintext: bool,
}

/// HTML ASCII whitespace: tab, LF, FF, CR, space. Rust's
/// `char::is_whitespace` is too broad (VT, NEL, Unicode spaces are name
/// characters to the HTML tokenizer).
fn is_html_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{c}' | '\r' | ' ')
}

/// Numeric reference values 0x80–0x9F map through the Windows-1252 table
/// (the spec's numericref substitution table), not to those control codes.
const CP1252: [u32; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, //
    0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x008D, 0x017D, 0x008F, //
    0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, //
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
];

/// The spec's numeric character reference end state: NUL, surrogates and
/// out-of-range values all become U+FFFD; 0x80–0x9F remap via cp1252.
fn numeric_char(cp: u32) -> char {
    match cp {
        0 => '\u{FFFD}',
        0x80..=0x9F => char::from_u32(CP1252[(cp - 0x80) as usize]).unwrap_or('\u{FFFD}'),
        0xD800..=0xDFFF => '\u{FFFD}',
        _ => char::from_u32(cp).unwrap_or('\u{FFFD}'),
    }
}

/// Byte index in `raw` where a trailing partial entity starts (`&` with no
/// `;` after it and only name characters since), or `None` when the run
/// ends cleanly. Used to hold text across chunk boundaries while streaming.
fn partial_entity_tail(raw: &str) -> Option<usize> {
    let amp = raw.rfind('&')?;
    let tail = &raw[amp + 1..];
    if tail.contains(';') {
        return None;
    }
    if tail.is_empty() || tail.chars().all(|c| c.is_ascii_alphanumeric() || c == '#') {
        Some(amp)
    } else {
        None
    }
}

/// Elements whose content is raw text: no markup and no entity decoding.
fn is_raw_text(tag: &str) -> bool {
    matches!(
        tag,
        "script" | "style" | "xmp" | "iframe" | "noembed" | "noframes"
    )
}

/// Outcome of attempting one tag: ready, junk (skip), or input ended
/// mid-token while streaming (rewind; the caller waits for more).
enum TagOutcome {
    Ready(Token),
    Junk,
    NeedMore,
}

/// Escapable raw text (RCDATA): raw until the end tag, but entities decode.
fn is_rcdata(tag: &str) -> bool {
    matches!(tag, "title" | "textarea")
}

impl Tokenizer {
    pub fn new(input: &str) -> Tokenizer {
        let mut t = Tokenizer {
            input: Vec::new(),
            pos: 0,
            pending: std::collections::VecDeque::new(),
            eof: true,
            pending_cr: false,
            plaintext: false,
        };
        t.push_normalized(input);
        // A trailing lone CR is LF by the time the stream ends.
        if t.pending_cr {
            t.input.push('\n');
            t.pending_cr = false;
        }
        t
    }

    /// Empty tokenizer awaiting [`feed`]: nothing is emitted until input (or
    /// [`finish`]) arrives.
    pub fn new_streaming() -> Tokenizer {
        Tokenizer {
            input: Vec::new(),
            pos: 0,
            pending: std::collections::VecDeque::new(),
            eof: false,
            pending_cr: false,
            plaintext: false,
        }
    }

    /// Append a response chunk. Tokens spanning the boundary wait for the
    /// rest (or [`finish`]).
    pub fn feed(&mut self, chunk: &str) {
        self.push_normalized(chunk);
        self.eof = false;
    }

    /// No more input is coming: drain everything, truncated or not (the
    /// batch behavior `new` has always had).
    pub fn finish(&mut self) {
        if self.pending_cr {
            self.input.push('\n');
            self.pending_cr = false;
        }
        self.eof = true;
    }

    pub(crate) fn enter_plaintext(&mut self) {
        self.plaintext = true;
    }

    pub(crate) fn cancel_plaintext(&mut self) {
        self.plaintext = false;
    }

    /// Spec input-stream preprocessing: CRLF collapses to LF and a lone CR
    /// becomes LF, applied once per character and across chunk boundaries.
    fn push_normalized(&mut self, chunk: &str) {
        for c in chunk.chars() {
            if self.pending_cr {
                self.pending_cr = false;
                if c != '\n' {
                    self.input.push('\n');
                }
            }
            if c == '\r' {
                self.pending_cr = true;
            } else {
                self.input.push(c);
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.input.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.input.get(self.pos + off).copied()
    }

    fn starts_with(&self, s: &str) -> bool {
        s.chars()
            .enumerate()
            .all(|(i, c)| self.peek_at(i) == Some(c))
    }

    fn starts_with_ci(&self, s: &str) -> bool {
        let lower = s.to_ascii_lowercase();
        lower
            .chars()
            .enumerate()
            .all(|(i, c)| self.peek_at(i).map(|x| x.to_ascii_lowercase()) == Some(c))
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn take_while<F: Fn(char) -> bool>(&mut self, f: F) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if f(c) {
                out.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        out
    }

    /// Decode entities and character references. Data-state NULs stay raw;
    /// other tokenizer states replace them with U+FFFD.
    fn decode_entities(&self, raw: &str, replace_nul: bool) -> String {
        let has_nul = replace_nul && raw.contains('\0');
        if !raw.contains('&') {
            return if has_nul {
                raw.replace('\0', "\u{FFFD}")
            } else {
                raw.to_string()
            };
        }
        let chars: Vec<char> = raw.chars().collect();
        let mut out = String::with_capacity(raw.len());
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '&' {
                // Longest named match first (lookahead covers the longest
                // WHATWG name — see the table-size test in lib.rs).
                let rest: String = chars[i + 1..].iter().take(32).collect::<String>();
                let mut matched = false;
                for (name, value) in crate::entities::NAMED_ENTITIES {
                    if rest.starts_with(name) {
                        out.push_str(value);
                        i += 1 + name.len();
                        matched = true;
                        break;
                    }
                }
                if matched {
                    continue;
                }
                // Numeric references: digits consume without bound
                // (zero-prefixed values can run hundreds of digits), the
                // `;` is optional, and the value goes through the spec's
                // numeric-reference mapping.
                let numeric = if rest.starts_with("#x") || rest.starts_with("#X") {
                    let digits = chars[i + 3..]
                        .iter()
                        .take_while(|c| c.is_ascii_hexdigit())
                        .count();
                    if digits == 0 {
                        None
                    } else {
                        let text: String = chars[i + 3..i + 3 + digits].iter().collect();
                        Some((
                            u32::from_str_radix(&text, 16).unwrap_or(0x110000),
                            digits,
                            3,
                        ))
                    }
                } else if rest.starts_with('#') {
                    let digits = chars[i + 2..]
                        .iter()
                        .take_while(|c| c.is_ascii_digit())
                        .count();
                    if digits == 0 {
                        None
                    } else {
                        let text: String = chars[i + 2..i + 2 + digits].iter().collect();
                        Some((text.parse::<u32>().unwrap_or(0x110000), digits, 2))
                    }
                } else {
                    None
                };
                if let Some((cp, digits, head)) = numeric {
                    out.push(numeric_char(cp));
                    // consumed: '&' + '#'['x'] + digits + (';')? — `head`
                    // already counts the '&'.
                    i += head + digits + usize::from(chars.get(i + head + digits) == Some(&';'));
                    continue;
                }
                out.push('&');
                i += 1;
            } else if chars[i] == '\0' {
                out.push(if replace_nul { '\u{FFFD}' } else { '\0' });
                i += 1;
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        out
    }

    pub fn next_token(&mut self) -> Option<Token> {
        // Queued tokens (raw-text content) survive even when the input
        // already ran to EOF (e.g. an unclosed <style>).
        if let Some(tok) = self.pending.pop_front() {
            return Some(tok);
        }
        if self.plaintext {
            if self.pos >= self.input.len() {
                return None;
            }
            let text: String = self.input[self.pos..]
                .iter()
                .map(|&c| if c == '\0' { '\u{FFFD}' } else { c })
                .collect();
            self.pos = self.input.len();
            return Some(Token::Text(text));
        }
        if self.pos >= self.input.len() {
            return None;
        }
        let token_start = self.pos;

        if self.peek() == Some('<') {
            // Comment
            if self.starts_with("<!--") {
                self.pos += 4;
                // The spec's comment states with a pending buffer: dashes
                // (and the bang) hold back until more data folds them in;
                // `>` and EOF drop them. Mirrors the spec exactly — e.g.
                // `<!--->` closes empty (comment-start-dash state), while
                // `<!-- ->` keeps " ->" (comment-end-dash folds back), and
                // each extra `-` in comment-end appends one dash to the
                // data (`<!----->` is "-").
                let mut data = String::new();
                let mut pending = String::new();
                #[derive(PartialEq)]
                enum CSt {
                    Start,
                    Data,
                    DashStart,
                    DashEnd,
                    End,
                    EndBang,
                }
                let mut st = CSt::Start;
                loop {
                    let Some(c) = self.bump() else {
                        if !self.eof {
                            // Unterminated so far: the close may be in
                            // the next chunk.
                            self.pos = token_start;
                            return None;
                        }
                        break; // EOF inside a comment: data runs to the end.
                    };
                    let c = if c == '\0' { '\u{FFFD}' } else { c };
                    match st {
                        CSt::Start => match c {
                            '>' => break,
                            '-' => {
                                pending.push('-');
                                st = CSt::DashStart;
                            }
                            _ => {
                                data.push(c);
                                st = CSt::Data;
                            }
                        },
                        CSt::DashStart => match c {
                            '>' => break,
                            '-' => {
                                pending.push('-');
                                st = CSt::End;
                            }
                            _ => {
                                data.push_str(&pending);
                                pending.clear();
                                data.push(c);
                                st = CSt::Data;
                            }
                        },
                        CSt::Data => match c {
                            '-' => {
                                pending.push('-');
                                st = CSt::DashEnd;
                            }
                            _ => data.push(c),
                        },
                        CSt::DashEnd => match c {
                            '-' => {
                                pending.push('-');
                                st = CSt::End;
                            }
                            _ => {
                                data.push_str(&pending);
                                pending.clear();
                                data.push(c);
                                st = CSt::Data;
                            }
                        },
                        CSt::End => match c {
                            '>' => break,
                            '!' => {
                                pending.push('!');
                                st = CSt::EndBang;
                            }
                            // Spec comment-end state: each extra dash lands
                            // in the data while staying in the state.
                            '-' => data.push('-'),
                            _ => {
                                data.push_str(&pending);
                                pending.clear();
                                data.push(c);
                                st = CSt::Data;
                            }
                        },
                        CSt::EndBang => match c {
                            '>' => break,
                            '-' => {
                                data.push_str(&pending);
                                pending.clear();
                                pending.push('-');
                                st = CSt::DashEnd;
                            }
                            _ => {
                                data.push_str(&pending);
                                pending.clear();
                                data.push(c);
                                st = CSt::Data;
                            }
                        },
                    }
                }
                return Some(Token::Comment(data));
            }
            // Bogus comment also covers `<?…>`: the spec's question-mark
            // branch drops only the `<`, so the `?` is reconsumed as the
            // first data character (`<?xml …>` → comment "?xml …").
            if self.starts_with("<?") {
                self.pos += 1;
                let mut data = String::new();
                let mut closed = false;
                while let Some(c) = self.bump() {
                    if c == '>' {
                        closed = true;
                        break;
                    }
                    data.push(if c == '\0' { '\u{FFFD}' } else { c });
                }
                if !closed && !self.eof {
                    self.pos = token_start;
                    return None;
                }
                return Some(Token::Comment(data));
            }
            // Doctype vs bogus comment: `<!DOCTYPE` (case-insensitive) starts
            // a doctype — the spec's doctype state consumes the rest of the
            // tag as the name, so `<!DOCTYPEhtml>` is a doctype named "html",
            // not a bogus comment. Any other `<!…>` is a bogus comment with
            // the raw contents as its data (this includes `<![CDATA[…]]>`,
            // which HTML treats as bogus).
            if self.starts_with("<!") {
                if self.starts_with_ci("<!doctype") {
                    self.pos += 9;
                    match self.tokenize_doctype(token_start) {
                        Some(tok) => return Some(tok),
                        None => return None, // rewound: waits for more input
                    }
                }
                self.pos += 2; // bogus comment: data runs from here to '>'
                let mut data = String::new();
                let mut closed = false;
                while let Some(c) = self.bump() {
                    if c == '>' {
                        closed = true;
                        break;
                    }
                    data.push(if c == '\0' { '\u{FFFD}' } else { c });
                }
                if !closed && !self.eof {
                    self.pos = token_start;
                    return None;
                }
                return Some(Token::Comment(data));
            }
            // A real tag opener: '<' followed by a letter or '/'. A malformed
            // opener (`< b`, `<3`) falls through to the text run below.
            // A trailing '<' with nothing after it yet waits for more input.
            if self.peek_at(1).is_none() && !self.eof {
                return None;
            }
            let next = self.peek_at(1).unwrap_or(' ');
            if next.is_ascii_alphabetic() || next == '/' {
                match self.tokenize_tag() {
                    TagOutcome::Ready(tok) => return Some(tok),
                    TagOutcome::NeedMore => return None,
                    TagOutcome::Junk => return self.next_token(),
                }
            }
        }

        // Text run. A malformed '<' (e.g. "a < b") is literal text and the
        // run continues past it until real markup appears.
        let mut raw = String::new();
        loop {
            raw.push_str(&self.take_while(|c| c != '<'));
            if self.peek() != Some('<') {
                break;
            }
            // A trailing '<' with nothing after it yet may open markup in
            // the next chunk — leave it for then.
            if self.peek_at(1).is_none() && !self.eof {
                break;
            }
            let next = self.peek_at(1).unwrap_or(' ');
            let opens_markup = self.starts_with("<!--")
                || self.starts_with("<!")
                || next.is_ascii_alphabetic()
                || next == '/';
            if opens_markup {
                break;
            }
            raw.push('<');
            self.pos += 1;
        }
        // A run ending mid-entity ("&am" + "p;" across chunks) must wait for
        // the rest: rewind past the partial entity and emit the head (or
        // nothing when the head is empty).
        if !self.eof {
            if let Some(cut) = partial_entity_tail(&raw) {
                let cut_chars: usize = raw[cut..].chars().count();
                raw.truncate(cut);
                self.pos -= cut_chars;
            }
        }
        if raw.is_empty() {
            // Empty for one of two reasons: a partial entity was rewound
            // above, or input truly ended. While streaming, waiting is
            // always safe (the next feed or finish() resumes); at EOF keep
            // the legacy skip-and-retry.
            if !self.eof {
                return None;
            }
            if self.pos >= self.input.len() {
                return None;
            }
            self.pos += 1; // safety: never stall
            return self.next_token();
        }
        Some(Token::Text(self.decode_entities(&raw, false)))
    }

    /// After the `<!doctype` opener (already consumed): parse the name, the
    /// optional PUBLIC/SYSTEM identifiers, and the quirks flag per the
    /// WHATWG DOCTYPE states. Parse errors are not modelled (the tokenizer
    /// has no error channel) but the force-quirks outcomes they produce are.
    /// Truncation while streaming rewinds to `start` and returns `None`.
    fn tokenize_doctype(&mut self, start: usize) -> Option<Token> {
        let wait = |tk: &mut Tokenizer| {
            tk.pos = start;
            None::<Token>
        };
        let tok = |name: String, pid: Option<String>, sid: Option<String>, quirks: bool| {
            Some(Token::Doctype {
                name,
                public_id: pid,
                system_id: sid,
                force_quirks: quirks,
            })
        };
        // Bogus doctype tail: everything up to `>` (or EOF) is ignored.
        let finish = |tk: &mut Tokenizer,
                      name: String,
                      pid: Option<String>,
                      sid: Option<String>,
                      quirks: bool| {
            while let Some(c) = tk.bump() {
                if c == '>' {
                    break;
                }
            }
            tok(name, pid, sid, quirks)
        };

        self.take_while(is_html_ws);
        if self.truncated() {
            return wait(self);
        }
        let mut name = self
            .take_while(|c| !is_html_ws(c) && c != '>')
            .to_ascii_lowercase();
        if self.truncated() {
            return wait(self);
        }
        if name.contains('\0') {
            name = name.replace('\0', "\u{FFFD}");
        }
        // Missing name, or EOF anywhere inside the doctype: force-quirks.
        if name.is_empty() || (self.pos >= self.input.len() && self.eof) {
            if self.peek() == Some('>') {
                self.pos += 1;
            }
            return tok(name, None, None, true);
        }

        self.take_while(is_html_ws);
        if self.truncated() {
            return wait(self);
        }
        if self.peek() == Some('>') {
            self.pos += 1;
            return tok(name, None, None, false);
        }
        if self.pos >= self.input.len() && self.eof {
            return tok(name, None, None, true);
        }

        // PUBLIC/SYSTEM keywords: the spec matches them case-insensitively
        // right after the whitespace, then reconsumes whatever follows (a
        // quote directly against the keyword still opens the identifier).
        let public = self.starts_with_ci("public");
        let system = self.starts_with_ci("system");
        if !public && !system {
            // Junk after the name sets force-quirks up front (browsers put
            // `<!DOCTYPE html junk>` in quirks mode); the tail is bogus.
            return finish(self, name, None, None, true);
        }
        self.pos += 6;
        self.take_while(is_html_ws);
        if self.truncated() {
            return wait(self);
        }
        // Anything but a quote here is a missing-identifier error: `>`
        // (or junk with a bogus tail) emits force-quirks.
        let Some(q @ ('"' | '\'')) = self.peek() else {
            if self.peek() == Some('>') {
                self.pos += 1;
            }
            return finish(self, name, None, None, true);
        };
        self.pos += 1;
        let (id, closed, abrupt) = self.read_doctype_id(q);
        if abrupt {
            // Abrupt-closing `>` inside the identifier: the `>` is consumed
            // and the doctype emits with force-quirks on.
            return if public {
                tok(name, Some(id), None, true)
            } else {
                tok(name, None, Some(id), true)
            };
        }
        if !closed {
            if !self.eof {
                return wait(self);
            }
            // Unterminated identifier at EOF: force-quirks.
            return if public {
                tok(name, Some(id), None, true)
            } else {
                tok(name, None, Some(id), true)
            };
        }
        let (pid, sid) = if public {
            (Some(id), None)
        } else {
            (None, Some(id))
        };
        let mut sid = sid;

        if !public {
            // After a system id: whitespace then `>` ends it; junk is a
            // bogus tail (no force-quirks per the spec's after-system state).
            self.take_while(is_html_ws);
            if self.truncated() {
                return wait(self);
            }
            match self.peek() {
                Some('>') => {
                    self.pos += 1;
                    tok(name, None, sid, false)
                }
                None if self.eof => tok(name, None, sid, true),
                None => wait(self),
                Some(_) => finish(self, name, None, sid, false),
            }
        } else {
            // After a public id the spec looks for a system id: a quote may
            // follow directly (the missing-whitespace error reconsumes into
            // the before-system state), whitespace routes there too, and `>`
            // right after the public id emits without force-quirks.
            match self.peek() {
                Some('>') => {
                    self.pos += 1;
                    tok(name, pid, None, false)
                }
                None if self.eof => tok(name, pid, None, true),
                None => wait(self),
                Some(q2 @ ('"' | '\'')) => {
                    self.pos += 1;
                    let (id2, closed2, abrupt2) = self.read_doctype_id(q2);
                    if abrupt2 {
                        return tok(name, pid, Some(id2), true);
                    }
                    if !closed2 {
                        if !self.eof {
                            return wait(self);
                        }
                        return tok(name, pid, Some(id2), true);
                    }
                    self.take_while(is_html_ws);
                    if self.truncated() {
                        return wait(self);
                    }
                    match self.peek() {
                        Some('>') => {
                            self.pos += 1;
                            tok(name, pid, Some(id2), false)
                        }
                        None => tok(name, pid, Some(id2), true),
                        Some(_) => finish(self, name, pid, Some(id2), false),
                    }
                }
                Some(c) if is_html_ws(c) => {
                    self.take_while(is_html_ws);
                    if self.truncated() {
                        return wait(self);
                    }
                    match self.peek() {
                        Some('>') => {
                            self.pos += 1;
                            tok(name, pid, None, true)
                        }
                        None => tok(name, pid, None, true),
                        Some(q2 @ ('"' | '\'')) => {
                            self.pos += 1;
                            let (id2, closed2, abrupt2) = self.read_doctype_id(q2);
                            if abrupt2 {
                                return tok(name, pid, Some(id2), true);
                            }
                            if !closed2 {
                                if !self.eof {
                                    return wait(self);
                                }
                                return tok(name, pid, Some(id2), true);
                            }
                            self.take_while(is_html_ws);
                            if self.truncated() {
                                return wait(self);
                            }
                            match self.peek() {
                                Some('>') => {
                                    self.pos += 1;
                                    tok(name, pid, Some(id2), false)
                                }
                                None => tok(name, pid, Some(id2), true),
                                Some(_) => finish(self, name, pid, Some(id2), false),
                            }
                        }
                        Some(_) => finish(self, name, pid, None, true),
                    }
                }
                Some(_) => finish(self, name, pid, None, true),
            }
        }
    }

    /// Reads a quoted DOCTYPE identifier (opening quote already consumed).
    /// Returns the text, whether the closing quote was seen, and whether a
    /// `>` closed the identifier abruptly instead (that `>` is consumed;
    /// per the spec it emits the token with force-quirks on).
    fn read_doctype_id(&mut self, q: char) -> (String, bool, bool) {
        let mut id = String::new();
        while let Some(c) = self.bump() {
            if c == q {
                return (id, true, false);
            }
            if c == '>' {
                return (id, false, true);
            }
            id.push(if c == '\0' { '\u{FFFD}' } else { c });
        }
        (id, false, false)
    }

    /// Tokenize a `<...>` tag (the '<' is at self.pos). Junk returns `Junk`;
    /// truncation mid-tag while streaming rewinds to the '<' (`NeedMore`).
    fn tokenize_tag(&mut self) -> TagOutcome {
        let start = self.pos;
        // Wait for more input instead of emitting half a token.
        let wait = |tk: &mut Tokenizer| {
            tk.pos = start;
            TagOutcome::NeedMore
        };
        self.pos += 1; // consume '<'
        let is_end = if self.peek() == Some('/') {
            self.pos += 1;
            true
        } else {
            false
        };

        // End-tag open: only ASCII letters begin a tag name; anything else
        // is a bogus comment that reconsumes the character (`</1x>` is the
        // comment "1x"). `</>` emits nothing, and a bare `</` at EOF is text.
        if is_end && !self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            if self.truncated() {
                return wait(self);
            }
            match self.peek() {
                Some('>') => {
                    self.pos += 1;
                    return TagOutcome::Junk; // consumed and dropped
                }
                None => return TagOutcome::Ready(Token::Text("</".into())),
                Some(c0) => {
                    let mut data = String::new();
                    data.push(if c0 == '\0' { '\u{FFFD}' } else { c0 });
                    self.pos += 1;
                    while let Some(c) = self.bump() {
                        if c == '>' {
                            return TagOutcome::Ready(Token::Comment(data));
                        }
                        data.push(if c == '\0' { '\u{FFFD}' } else { c });
                    }
                    if !self.eof {
                        // The close may still arrive; wait at the '<'.
                        self.pos = start;
                        return TagOutcome::NeedMore;
                    }
                    return TagOutcome::Ready(Token::Comment(data));
                }
            }
        }

        // Spec tag-name state: everything but whitespace, `/` and `>` is a
        // name character (`<div.class>` names the tag "div.class"); NUL
        // becomes U+FFFD.
        let name = self
            .take_while(|c| !is_html_ws(c) && c != '/' && c != '>')
            .to_ascii_lowercase();
        let name = if name.contains('\0') {
            name.replace('\0', "\u{FFFD}")
        } else {
            name
        };
        if name.is_empty() {
            if self.truncated() {
                return wait(self);
            }
            // Start-tag junk — skip a char; caller retries. Unreachable in
            // practice: the text run and the end-tag-open check above route
            // every non-letter here first.
            self.pos += 1;
            return TagOutcome::Junk;
        }

        if is_end {
            // Skip to '>' (end tags have no attributes in practice).
            let mut closed = false;
            while let Some(c) = self.bump() {
                if c == '>' {
                    closed = true;
                    break;
                }
            }
            if !closed && !self.eof {
                return wait(self);
            }
            // EOF in tag: the spec drops the token instead of emitting it.
            if !closed {
                return TagOutcome::Junk;
            }
            return TagOutcome::Ready(Token::EndTag { name });
        }

        // Attributes
        let mut attrs: Vec<(String, String)> = Vec::new();
        let mut self_closing = false;
        let mut closed = false;
        loop {
            self.take_while(is_html_ws);
            if self.truncated() {
                return wait(self);
            }
            match self.peek() {
                None => break,
                Some('>') => {
                    self.pos += 1;
                    closed = true;
                    break;
                }
                Some('/') => {
                    self.pos += 1;
                    if self.peek() == Some('>') {
                        self.pos += 1;
                        self_closing = true;
                        closed = true;
                        break;
                    }
                    if self.truncated() {
                        return wait(self);
                    }
                }
                Some(_) => {
                    let attr_name = self
                        .take_while(|c| !is_html_ws(c) && c != '=' && c != '>' && c != '/')
                        .to_ascii_lowercase();
                    let attr_name = if attr_name.contains('\0') {
                        attr_name.replace('\0', "\u{FFFD}")
                    } else {
                        attr_name
                    };
                    if attr_name.is_empty() {
                        self.pos += 1; // skip stray char
                        continue;
                    }
                    if self.truncated() {
                        // Name may continue in the next chunk.
                        return wait(self);
                    }
                    self.take_while(is_html_ws);
                    let value = if self.peek() == Some('=') {
                        self.pos += 1;
                        self.take_while(is_html_ws);
                        match self.peek() {
                            Some(q @ ('"' | '\'')) => {
                                self.pos += 1;
                                let mut v = String::new();
                                let mut closed = false;
                                while let Some(c) = self.peek() {
                                    if c == q {
                                        self.pos += 1;
                                        closed = true;
                                        break;
                                    }
                                    v.push(if c == '\0' { '\u{FFFD}' } else { c });
                                    self.pos += 1;
                                }
                                if !closed && !self.eof {
                                    return wait(self);
                                }
                                self.decode_entities(&v, true)
                            }
                            _ => {
                                let v = self.take_while(|c| !is_html_ws(c) && c != '>');
                                if self.truncated() {
                                    return wait(self);
                                }
                                self.decode_entities(&v, true)
                            }
                        }
                    } else {
                        String::new() // boolean attribute
                    };
                    attrs.push((attr_name, value));
                }
            }
        }
        // EOF in tag: the spec drops the token instead of emitting it.
        if !closed {
            return TagOutcome::Junk;
        }

        if name == "plaintext" {
            self.plaintext = true;
        }

        // Raw text / RCDATA elements: emit their content as the next
        // token (queued ahead), then let the end tag tokenize normally.
        // A missing close tag while streaming waits (the content is still
        // arriving); at EOF the content model is unchanged from batch.
        if !self_closing && (is_raw_text(&name) || is_rcdata(&name)) {
            let close = format!("</{}", name);
            let mut content = String::new();
            while self.pos < self.input.len() && !self.starts_with_ci(&close) {
                let c = self.input[self.pos];
                content.push(if c == '\0' { '\u{FFFD}' } else { c });
                self.pos += 1;
            }
            if !self.starts_with_ci(&close) && !self.eof {
                self.pos = start;
                // The queued half-content (if any) belongs to a token that
                // never emitted — drop it; the retry re-collects everything.
                self.pending.clear();
                return TagOutcome::NeedMore;
            }
            let content = if is_rcdata(&name) {
                self.decode_entities(&content, true)
            } else {
                content
            };
            if !content.is_empty() {
                self.pending.push_back(Token::Text(content));
            }
        }

        TagOutcome::Ready(Token::StartTag {
            name,
            attrs,
            self_closing,
        })
    }

    /// True when input is exhausted but more may still arrive.
    fn truncated(&self) -> bool {
        self.pos >= self.input.len() && !self.eof
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(html: &str) -> Vec<Token> {
        let mut t = Tokenizer::new(html);
        let mut out = Vec::new();
        while let Some(tok) = t.next_token() {
            out.push(tok);
        }
        out
    }

    #[test]
    fn tokenizes_tags_attrs_text() {
        let toks = tokens(r#"<div class="a b" hidden id=x>hi &amp; bye</div>"#);
        assert_eq!(
            toks,
            vec![
                Token::StartTag {
                    name: "div".into(),
                    attrs: vec![
                        ("class".into(), "a b".into()),
                        ("hidden".into(), "".into()),
                        ("id".into(), "x".into()),
                    ],
                    self_closing: false,
                },
                Token::Text("hi & bye".into()),
                Token::EndTag { name: "div".into() },
            ]
        );
    }

    #[test]
    fn decodes_numeric_entities() {
        let toks = tokens("<p>a&#65;&#x42;&mdash;b</p>");
        match &toks[1] {
            Token::Text(s) => assert_eq!(s, "aAB—b"),
            other => panic!("expected text, got {:?}", other),
        }
    }

    #[test]
    fn skips_comments_and_doctype() {
        let toks = tokens("<!DOCTYPE html><!-- hidden --><p>x</p>");
        assert!(
            matches!(&toks[0], Token::Doctype { name, public_id: None, system_id: None, force_quirks: false } if name == "html")
        );
        assert!(matches!(&toks[1], Token::Comment(d) if d == " hidden "));
        // Bogus `<!…>` is a comment; PUBLIC/SYSTEM ids parse; missing ids
        // force quirks.
        assert!(matches!(&tokens("<!foo bar>")[0], Token::Comment(d) if d == "foo bar"));
        assert!(matches!(
            &tokens("<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"http://www.w3.org/TR/html4/strict.dtd\">")[0],
            Token::Doctype { public_id: Some(p), system_id: Some(s), force_quirks: false, .. } if p.contains("W3C") && s.ends_with("strict.dtd")
        ));
        assert!(matches!(
            &tokens("<!DOCTYPE html PUBLIC \"x\">")[0],
            // `>` right after the public id emits WITHOUT force-quirks
            // (spec after-public-identifier state; corpus-verified).
            Token::Doctype {
                public_id: Some(_),
                system_id: None,
                force_quirks: false,
                ..
            }
        ));
    }

    #[test]
    fn comment_edge_cases() {
        // Pendings drop on the close and on EOF.
        let t = |s: &str| match tokens(s).first() {
            Some(Token::Comment(d)) => d.clone(),
            other => panic!("expected comment, got {:?}", other),
        };
        assert_eq!(t("<!-->"), "");
        assert_eq!(t("<!--->"), "");
        assert_eq!(t("<!---->"), "");
        assert_eq!(t("<!----->"), "-");
        assert_eq!(t("<!--a--b-->"), "a--b");
        assert_eq!(t("<!--a--->"), "a-");
        assert_eq!(t("<!-- ->"), " ->");
        assert_eq!(t("<!---x"), "-x");
        assert_eq!(t("<!-- -"), " ");
        assert_eq!(t("<!-- --"), " ");
        assert_eq!(t("<!---"), "");
        assert_eq!(t("<!-- <!--"), " <!");
        assert_eq!(t("<!--<!-->"), "<!");
        assert_eq!(t("<!---- -"), "-- ");
        assert_eq!(t("<!---- >"), "-- >");
        assert_eq!(t("<!----!\0-->"), "--!\u{FFFD}");
        // `<?` reconsumes the question mark as comment data.
        assert_eq!(t("<?xml version=\"1.0\">"), "?xml version=\"1.0\"");
    }

    #[test]
    fn entity_numeric_edge_cases() {
        let t = |s: &str| match tokens(s).first() {
            Some(Token::Text(d)) => d.clone(),
            other => panic!("expected text, got {:?}", other),
        };
        // cp1252 remap for 0x80–0x9F; NUL/overflow → U+FFFD; long digit
        // runs consume without bound.
        assert_eq!(t("&#128;"), "\u{20AC}");
        assert_eq!(t("&#x80;"), "\u{20AC}");
        assert_eq!(t("&#0;"), "\u{FFFD}");
        assert_eq!(t("&#x110000;"), "\u{FFFD}");
        assert_eq!(t("&#0000000000000000000000000000000000000000065;"), "A");
        assert_eq!(t("&#x1F600;"), "\u{1F600}");
    }

    #[test]
    fn eof_in_tag_drops_token() {
        assert!(tokens("<div").is_empty());
        assert!(tokens("<div class=x").is_empty());
        assert!(tokens("</div").is_empty());
        assert!(tokens("<div/").is_empty());
    }

    #[test]
    fn spec_tag_name_characters() {
        // `id= a` yields value "a" (whitespace after `=` is skipped).
        let toks = tokens("<div.class id= a b=x>");
        match &toks[0] {
            Token::StartTag { name, attrs, .. } => {
                assert_eq!(name, "div.class");
                assert_eq!(
                    attrs,
                    &vec![("id".into(), "a".into()), ("b".into(), "x".into())]
                );
            }
            other => panic!("expected start tag, got {:?}", other),
        }
        // Empty value before the next attribute.
        let toks = tokens("<div.class id>");
        match &toks[0] {
            Token::StartTag { name, attrs, .. } => {
                assert_eq!(name, "div.class");
                assert_eq!(attrs, &vec![("id".into(), String::new())]);
            }
            other => panic!("expected start tag, got {:?}", other),
        }
    }

    #[test]
    fn raw_text_elements_emit_content() {
        let toks = tokens("<style>p > b { color: red }</style><p>t</p>");
        assert_eq!(toks.len(), 6);
        assert!(matches!(&toks[0], Token::StartTag { name, .. } if name == "style"));
        assert!(matches!(&toks[1], Token::Text(s) if s.contains("color: red")));
        assert!(matches!(&toks[2], Token::EndTag { name } if name == "style"));
        // Title decodes entities (RCDATA); script does not.
        let toks = tokens("<title>a &amp; b</title><script>if (a < b) x();</script>");
        assert!(matches!(&toks[1], Token::Text(s) if s == "a & b"));
        assert!(matches!(&toks[4], Token::Text(s) if s.contains("a < b")));
    }

    #[test]
    fn data_text_preserves_nul_but_references_replace_it() {
        assert_eq!(tokens("\0<\0"), vec![Token::Text("\0<\0".into())]);
        assert_eq!(tokens("\r\0"), vec![Token::Text("\n\0".into())]);
        assert_eq!(tokens("&#0;"), vec![Token::Text("\u{FFFD}".into())]);
    }

    #[test]
    fn plaintext_consumes_the_remainder_as_text() {
        assert_eq!(
            tokens("<plaintext><b>x</b>&amp;"),
            vec![
                Token::StartTag {
                    name: "plaintext".into(),
                    attrs: vec![],
                    self_closing: false,
                },
                Token::Text("<b>x</b>&amp;".into()),
            ]
        );

        let mut streaming = Tokenizer::new_streaming();
        streaming.feed("<plaintext><b>");
        let mut out = Vec::new();
        while let Some(token) = streaming.next_token() {
            out.push(token);
        }
        streaming.feed("x</plaintext>");
        while let Some(token) = streaming.next_token() {
            out.push(token);
        }
        streaming.finish();
        while let Some(token) = streaming.next_token() {
            out.push(token);
        }
        assert_eq!(out[1], Token::Text("<b>".into()));
        assert_eq!(out[2], Token::Text("x</plaintext>".into()));
    }

    #[test]
    fn malformed_lt_is_text() {
        let toks = tokens("a < b <3 c");
        assert_eq!(toks.len(), 1);
        match &toks[0] {
            Token::Text(s) => assert_eq!(s, "a < b <3 c"),
            other => panic!("expected text, got {:?}", other),
        }
    }
}
