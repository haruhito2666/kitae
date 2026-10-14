//! Markdown → HTML for QTSQ text payloads.
//!
//! Decoded QTSQ text documents are overwhelmingly Markdown (project notes,
//! plans, transcripts); dumping them into `<pre>` shows the syntax itself
//! (`#`, `**`, `|`) in clipped monospace lines. This module converts the
//! common Markdown core to the HTML the engine already styles — headings,
//! lists, tables, blockquotes, fenced code, paragraphs, and the inline
//! bold/italic/code/link/image markers. Deliberately a subset: no reference
//! links, no setext headings, no HTML passthrough (payload text is escaped
//! before inline scanning, so a document can never inject markup).
//!
//! Text is decided per payload: [`looks_like_markdown`] gates the rich view
//! (plain prose and JSON keep the monospace view), never the file — QTSQ
//! files carry no extension.

/// Escape `& < > " '` so the result is safe both as text content and inside
/// a double-quoted attribute (the inline scanner runs over escaped text).
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Convert a Markdown document to HTML (block constructs + inline spans).
pub fn to_html(src: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = String::with_capacity(src.len() * 2);
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim_start();

        if let Some(fence) = fenced(line) {
            i = emit_code_block(&lines, i, fence, &mut out);
            continue;
        }
        if let Some(level) = atx_heading(line) {
            out.push_str(&format!("<h{level}>"));
            out.push_str(&inline(&escape(&atx_content(line, level))));
            out.push_str(&format!("</h{level}>\n"));
            i += 1;
            continue;
        }
        // A table is triggered by its header line: pipes + delimiter row next.
        if line.contains('|') && i + 1 < lines.len() && is_delimiter_row(lines[i + 1]) {
            i = emit_table(&lines, i, &mut out);
            continue;
        }
        if is_hr(line) {
            out.push_str("<hr>\n");
            i += 1;
            continue;
        }
        if line.starts_with('>') {
            let mut inner = String::new();
            while i < lines.len() {
                let l = lines[i].trim_start();
                match l.strip_prefix('>') {
                    Some(rest) => {
                        inner.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                        inner.push('\n');
                        i += 1;
                    }
                    None => break,
                }
            }
            out.push_str("<blockquote>");
            out.push_str(&to_html(&inner));
            out.push_str("</blockquote>\n");
            continue;
        }
        if list_marker(line).is_some() {
            i = emit_list(&lines, i, &mut out);
            continue;
        }
        if line.is_empty() {
            i += 1;
            continue;
        }
        // Paragraph: consecutive prose lines until a blank line or the start
        // of another block; soft newlines join with a space.
        let mut para: Vec<&str> = Vec::new();
        while i < lines.len() {
            let l = lines[i];
            let t = l.trim_start();
            let block_start = t.is_empty()
                || fenced(t).is_some()
                || atx_heading(t).is_some()
                || is_hr(t)
                || t.starts_with('>')
                || list_marker(t).is_some()
                || (t.contains('|') && i + 1 < lines.len() && is_delimiter_row(lines[i + 1]));
            if block_start {
                break;
            }
            para.push(t.trim_end());
            i += 1;
        }
        out.push_str("<p>");
        for (n, l) in para.iter().enumerate() {
            if n > 0 {
                out.push(' ');
            }
            out.push_str(&inline(&escape(l)));
        }
        out.push_str("</p>\n");
    }
    out
}

/// Heuristic gate for the QTSQ viewer: does this decoded text read like a
/// Markdown document? Counts structural markers — plain prose keeps the
/// faithful monospace view.
pub fn looks_like_markdown(src: &str) -> bool {
    let mut score = 0usize;
    let mut headings = 0usize;
    for line in src.lines().take(200) {
        let t = line.trim_start();
        if atx_heading(t).is_some() {
            headings += 1;
            score += 2;
        } else if fenced(t).is_some() || is_delimiter_row(t) {
            score += 2;
        } else if list_marker(t).is_some() || t.starts_with('>') {
            score += 1;
        }
        if line.contains("**") {
            // A `**strong**` marker is as conclusive as a fence line.
            score += 2;
        }
        if score >= 6 {
            return true;
        }
    }
    // A single ATX heading is enough to identify the document as Markdown;
    // plain prose and JSON do not begin a line with `# `.
    score >= 6 || headings >= 1
}

/// Whole-payload JSON detection: `{…}` or `[…]` after trimming. JSON reads
/// best in the wrapping monospace view, never as Markdown.
pub fn looks_like_json(src: &str) -> bool {
    let t = src.trim();
    let b = t.as_bytes();
    b.len() >= 2 && matches!((b[0], b[b.len() - 1]), (b'{', b'}') | (b'[', b']'))
}

// --- block helpers ---------------------------------------------------------

/// Fenced code opener: returns the fence (` ``` ` or `~~~`).
fn fenced(line: &str) -> Option<&str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// ATX heading level (1–6): `#`s followed by a space or end of line.
fn atx_heading(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    (rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')).then_some(hashes)
}

fn atx_content(line: &str, level: usize) -> String {
    let mut content = line[level..].trim().to_string();
    if content.ends_with('#') {
        let without = content.trim_end_matches('#');
        if without.is_empty() || without.chars().last().is_some_and(char::is_whitespace) {
            content = without.trim_end().to_string();
        }
    }
    content
}

/// Thematic break: `---`, `***`, `___` (with optional inner spaces).
fn is_hr(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3
        && (t.chars().all(|c| c == '-' || c == ' ')
            || t.chars().all(|c| c == '*' || c == ' ')
            || t.chars().all(|c| c == '_' || c == ' '))
}

/// A list item marker at the start of a (de-indented) line:
/// `- `, `* `, `+ `, or `12. `/`12) `. Returns (marker_len, ordered).
fn list_marker(line: &str) -> Option<(usize, bool)> {
    let b = line.as_bytes();
    match b.first()? {
        b'-' | b'*' | b'+' => {
            (b.len() >= 2 && (b[1] == b' ' || b[1] == b'\t')).then_some((2, false))
        }
        c if c.is_ascii_digit() => {
            let digits = b.iter().take_while(|b| b.is_ascii_digit()).count();
            let rest = &b[digits..];
            (rest.len() >= 2
                && (rest[0] == b'.' || rest[0] == b')')
                && (rest[1] == b' ' || rest[1] == b'\t'))
                .then_some((digits + 2, true))
        }
        _ => None,
    }
}

/// Table delimiter row: `| --- | :---: |` style — every cell is dashes with
/// optional alignment colons.
fn is_delimiter_row(line: &str) -> bool {
    let t = line.trim();
    if !t.contains('|') || !t.contains('-') {
        return false;
    }
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').all(|cell| {
        let c = cell.trim();
        let c = c.strip_prefix(':').unwrap_or(c);
        let c = c.strip_suffix(':').unwrap_or(c);
        !c.is_empty() && c.chars().all(|ch| ch == '-')
    })
}

fn emit_code_block(lines: &[&str], start: usize, fence: &str, out: &mut String) -> usize {
    let info = lines[start].trim_start()[fence.len()..].trim();
    let mut code = String::new();
    let mut i = start + 1;
    while i < lines.len() && !lines[i].trim_start().starts_with(fence) {
        code.push_str(lines[i]);
        code.push('\n');
        i += 1;
    }
    if i < lines.len() {
        i += 1; // skip the closing fence when present
    }
    out.push_str("<pre><code");
    if !info.is_empty() {
        out.push_str(&format!(
            " class=\"language-{}\"",
            escape(&info.to_lowercase().replace([' ', '\t'], "-"))
        ));
    }
    out.push('>');
    out.push_str(&escape(&code));
    out.push_str("</code></pre>\n");
    i
}

fn emit_table(lines: &[&str], start: usize, out: &mut String) -> usize {
    let cells = |line: &str| -> Vec<String> {
        let t = line.trim();
        let t = t.strip_prefix('|').unwrap_or(t);
        let t = t.strip_suffix('|').unwrap_or(t);
        t.split('|').map(|c| inline(&escape(c.trim()))).collect()
    };
    out.push_str("<table><thead><tr>");
    for c in cells(lines[start]) {
        out.push_str(&format!("<th>{c}</th>"));
    }
    out.push_str("</tr></thead><tbody>");
    let mut i = start + 2; // header + delimiter
    while i < lines.len() {
        let t = lines[i].trim_start();
        let ends_table = t.is_empty()
            || !t.contains('|')
            || fenced(t).is_some()
            || t.starts_with('>')
            || atx_heading(t).is_some();
        if ends_table {
            break;
        }
        out.push_str("<tr>");
        for c in cells(t) {
            out.push_str(&format!("<td>{c}</td>"));
        }
        out.push_str("</tr>");
        i += 1;
    }
    out.push_str("</tbody></table>\n");
    i
}

fn ordered_list_start(line: &str) -> u32 {
    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or(1).max(1)
}

fn open_list(ordered: bool, start: u32, out: &mut String) {
    if ordered && start != 1 {
        out.push_str(&format!("<ol start=\"{start}\">"));
    } else if ordered {
        out.push_str("<ol>");
    } else {
        out.push_str("<ul>");
    }
}

fn emit_list(lines: &[&str], start: usize, out: &mut String) -> usize {
    // (item indent, ordered, `<li>` still open)
    let mut stack: Vec<(usize, bool, bool)> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let raw = lines[i];
        let t = raw.trim_start();
        let indent = raw.len() - t.len();
        if t.is_empty() {
            break;
        }
        let Some((marker, ordered)) = list_marker(t) else {
            break;
        };
        // Close frames deeper than this item; then open or reuse a frame.
        while let Some((ind, _, li_open)) = stack.last().copied() {
            if indent >= ind {
                break;
            }
            if li_open {
                out.push_str("</li>");
            }
            out.push_str(if stack.last().unwrap().1 {
                "</ol>"
            } else {
                "</ul>"
            });
            stack.pop();
        }
        match stack.last().copied() {
            None => {
                open_list(ordered, ordered_list_start(t), out);
                stack.push((indent, ordered, false));
            }
            Some((ind, ord, _)) if indent > ind => {
                // Nested list, inside the parent's open <li>.
                open_list(ordered, ordered_list_start(t), out);
                stack.push((indent, ordered, false));
            }
            Some((_, ord, li_open)) => {
                if ord != ordered {
                    if li_open {
                        out.push_str("</li>");
                    }
                    out.push_str(if ord { "</ol>" } else { "</ul>" });
                    stack.pop();
                    open_list(ordered, ordered_list_start(t), out);
                    stack.push((indent, ordered, false));
                } else if li_open {
                    out.push_str("</li>");
                    stack.last_mut().unwrap().2 = false;
                }
            }
        }
        let content = t[marker..].trim();
        out.push_str("<li>");
        out.push_str(&inline(&escape(content)));
        stack.last_mut().unwrap().2 = true;
        i += 1;
        // Wrapped continuation lines of this item: deeper-indented prose
        // without a marker joins the `<li>` with a space.
        while i < lines.len() {
            let c = lines[i];
            let ct = c.trim_start();
            if ct.is_empty() || list_marker(ct).is_some() || c.len() - ct.len() <= indent {
                break;
            }
            out.push(' ');
            out.push_str(&inline(&escape(ct.trim())));
            i += 1;
        }
    }
    while let Some((_, ord, li_open)) = stack.pop() {
        if li_open {
            out.push_str("</li>");
        }
        out.push_str(if ord { "</ol>" } else { "</ul>" });
    }
    out.push('\n');
    i
}

// --- inline spans ----------------------------------------------------------

/// Inline formatting over PRE-ESCAPED text: `` `code` ``, `**strong**`,
/// `*em*`, `_em_` (word-boundary guarded), `![alt](src)`, `[text](url)`.
fn inline(escaped: &str) -> String {
    let chars: Vec<char> = escaped.chars().collect();
    let mut out = String::with_capacity(escaped.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' => {
                let close = (i + 1..chars.len()).find(|&k| chars[k] == '`');
                match close {
                    Some(c) if c > i + 1 => {
                        out.push_str("<code>");
                        out.extend(&chars[i + 1..c]);
                        out.push_str("</code>");
                        i = c + 1;
                    }
                    _ => {
                        out.push('`');
                        i += 1;
                    }
                }
            }
            '!' if i + 1 < chars.len() && chars[i + 1] == '[' => match parse_link(&chars, i + 1) {
                Some((text, url, next)) => {
                    out.push_str(&format!("<img src=\"{url}\" alt=\"{text}\">"));
                    i = next;
                }
                None => {
                    out.push('!');
                    i += 1;
                }
            },
            '[' => match parse_link(&chars, i) {
                Some((text, url, next)) => {
                    out.push_str(&format!("<a href=\"{url}\">"));
                    out.push_str(&inline(&text));
                    out.push_str("</a>");
                    i = next;
                }
                None => {
                    out.push('[');
                    i += 1;
                }
            },
            '*' | '_' => {
                let m = chars[i];
                let double = i + 1 < chars.len() && chars[i + 1] == m;
                // `_` only opens outside a word (intraword underscores in
                // names like snake_case stay literal).
                let opens = m == '*' || i == 0 || !chars[i - 1].is_alphanumeric();
                let consumed = if !opens {
                    0
                } else if double {
                    strong_or_fail(&chars, i, m, &mut out)
                } else {
                    em_or_fail(&chars, i, m, &mut out)
                };
                if consumed == 0 {
                    out.push(m);
                    i += 1;
                } else {
                    // Helpers return the absolute index past the closer.
                    i = consumed;
                }
            }
            _ => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// `**strong**` from `i`; 0 means "not emphasis, emit literally".
fn strong_or_fail(chars: &[char], i: usize, m: char, out: &mut String) -> usize {
    let close = find_run(chars, i + 2, m, 2);
    let ok = close
        .map(|c| !chars[i + 2].is_whitespace() && !chars[c - 1].is_whitespace())
        .unwrap_or(false);
    if ok {
        out.push_str("<strong>");
        out.push_str(&inline(
            &chars[i + 2..close.unwrap()].iter().collect::<String>(),
        ));
        out.push_str("</strong>");
        close.unwrap() + 2
    } else {
        0
    }
}

/// `*em*` / `_em_` from `i`; 0 means "not emphasis, emit literally".
fn em_or_fail(chars: &[char], i: usize, m: char, out: &mut String) -> usize {
    let close = (i + 1..chars.len()).find(|&k| chars[k] == m);
    let ok = close
        .map(|c| c > i + 1 && !chars[i + 1].is_whitespace() && !chars[c - 1].is_whitespace())
        .unwrap_or(false);
    if ok {
        out.push_str("<em>");
        out.push_str(&inline(
            &chars[i + 1..close.unwrap()].iter().collect::<String>(),
        ));
        out.push_str("</em>");
        close.unwrap() + 1
    } else {
        0
    }
}

/// Index of the first run of `n` copies of `m` at or after `from`.
fn find_run(chars: &[char], from: usize, m: char, n: usize) -> Option<usize> {
    let mut k = from;
    while k + n <= chars.len() {
        if (0..n).all(|j| chars[k + j] == m) {
            return Some(k);
        }
        k += 1;
    }
    None
}

/// `[text](url)` (or `![alt](url)` with `open` at the `[`): returns
/// (inner text, url, index after the closing `)`).
fn parse_link(chars: &[char], open: usize) -> Option<(String, String, usize)> {
    let mut close = None;
    let mut depth = 0usize;
    for k in open + 1..chars.len() {
        match chars[k] {
            '[' => depth += 1,
            ']' => {
                if depth == 0 {
                    close = Some(k);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let close = close?;
    if close + 1 >= chars.len() || chars[close + 1] != '(' {
        return None;
    }
    let mut url = String::new();
    let mut k = close + 2;
    while k < chars.len() && chars[k] != ')' {
        url.push(chars[k]);
        k += 1;
    }
    if k >= chars.len() || url.is_empty() {
        return None;
    }
    Some((chars[open + 1..close].iter().collect(), url, k + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_and_paragraphs() {
        let html = to_html("# Title\n\nFirst para\nsecond line.\n\n## Sub");
        assert!(html.contains("<h1>Title</h1>"), "{html}");
        assert!(html.contains("<h2>Sub</h2>"), "{html}");
        assert!(html.contains("<p>First para second line.</p>"), "{html}");
        // ####### (7 hashes) is not a heading.
        assert!(to_html("####### not").contains("#######"));
    }

    #[test]
    fn atx_heading_drops_optional_closing_hashes() {
        assert!(to_html("## Title ##").contains("<h2>Title</h2>"));
        assert!(to_html("### C#").contains("<h3>C#</h3>"));
    }

    #[test]
    fn ordered_lists_preserve_their_start_value() {
        assert!(to_html("7. seven\n8. eight").contains("<ol start=\"7\">"));
    }

    #[test]
    fn fenced_code_escapes_and_carries_language() {
        let html = to_html("```rust\nlet s = \"<b>&</b>\";\n```");
        assert!(
            html.contains("<pre><code class=\"language-rust\">let s = &quot;&lt;b&gt;&amp;&lt;/b&gt;&quot;;\n</code></pre>"),
            "{html}"
        );
        // Unterminated fence still renders the code it has.
        assert!(to_html("```\nx\n").contains("<pre><code>x\n"));
    }

    #[test]
    fn lists_nest_and_mix_ordered() {
        let html = to_html("- a\n- b\n  - b1\n- c\n\n1. one\n2. two");
        assert!(html.contains("<li>a</li>"), "{html}");
        assert!(html.contains("<li>b<ul><li>b1</li></ul></li>"), "{html}");
        assert!(html.contains("<li>c</li>"), "{html}");
        assert!(html.contains("<ol><li>one</li><li>two</li></ol>"), "{html}");
    }

    #[test]
    fn continuation_lines_join_the_item() {
        let html = to_html("- item text\n  wrapped onto\n  two more lines");
        assert!(
            html.contains("<li>item text wrapped onto two more lines</li>"),
            "{html}"
        );
    }

    #[test]
    fn tables_emit_header_and_body() {
        let html = to_html("| a | b |\n|---|:-:|\n| 1 | 2 |\n| x | y |");
        assert!(html.contains("<th>a</th><th>b</th>"), "{html}");
        assert!(html.contains("<td>1</td><td>2</td>"), "{html}");
        assert!(html.contains("<td>x</td><td>y</td>"), "{html}");
    }

    #[test]
    fn blockquotes_recurse() {
        let html = to_html("> quoted *line*\n> > deeper");
        assert!(html.contains("<em>line</em>"), "{html}");
        // Nesting, not adjacency: the outer quote wraps its paragraph and
        // the inner quote (paragraphs render as <p> inside quotes).
        assert_eq!(html.matches("<blockquote>").count(), 2, "{html}");
        assert!(
            html.find("<em>line</em>").unwrap() < html.rfind("<blockquote>").unwrap(),
            "{html}"
        );
    }

    #[test]
    fn hr_variants() {
        for src in ["---", "***", "___", "- - -"] {
            assert!(to_html(src).contains("<hr>"), "{src}");
        }
        // Not an hr: a list-ish dash or prose.
        assert!(!to_html("- item").contains("<hr>"));
        assert!(!to_html("hello - world").contains("<hr>"));
    }

    #[test]
    fn inline_spans() {
        assert_eq!(
            inline(&escape("**bold** and *it*")),
            "<strong>bold</strong> and <em>it</em>"
        );
        assert_eq!(inline(&escape("`code` stay")), "<code>code</code> stay");
        assert_eq!(
            inline(&escape("[k](https://x.y/a?b=1)")),
            "<a href=\"https://x.y/a?b=1\">k</a>"
        );
        assert_eq!(
            inline(&escape("![alt pic](img/x.png)")),
            "<img src=\"img/x.png\" alt=\"alt pic\">"
        );
        // Multiplication prose is not emphasis.
        assert_eq!(inline(&escape("2 * 3 * 4")), "2 * 3 * 4");
        // Intraword underscores stay literal.
        assert_eq!(inline(&escape("some_var_name_here")), "some_var_name_here");
        // Unmatched markers stay literal.
        assert_eq!(inline(&escape("a * b")), "a * b");
    }

    #[test]
    fn payload_text_never_injects_markup() {
        let html = to_html("<script>alert(1)</script>\n\n<b>bold</b>");
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("<b>bold"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn detector_thresholds() {
        assert!(looks_like_markdown("# Plan\n"));
        assert!(looks_like_markdown("# Plan\n\n## Steps\n\n- one\n- two\n"));
        assert!(looks_like_markdown("# A\n# B\n")); // two headings suffice
        assert!(looks_like_markdown(
            "Intro\n\n```go\ncode\n```\n\n**done**\n"
        ));
        assert!(!looks_like_markdown(
            "Dear team,\n\nPlease review the attached file before Friday.\nThanks!"
        ));
        assert!(!looks_like_markdown("just a sentence with a - dash"));
        assert!(looks_like_json("{\"a\": 1}"));
        assert!(looks_like_json("  [1, 2, 3]\n"));
        assert!(!looks_like_json("{\"a\": 1"));
        assert!(!looks_like_json("plain text (with braces { somewhere)"));
    }

    #[test]
    fn corpus_style_document_renders_rich() {
        let doc = "# DTCN\n\nA **dictionary** model.\n\n## Design\n\n| Part | Role |\n|---|---|\n| dict | words |\n\n```text\nraw = tokens(dict)\n```\n\n> Note: *approximate*.\n\n1. encode\n2. compress\n";
        let html = to_html(doc);
        for tag in [
            "<h1>",
            "<h2>",
            "<strong>",
            "<table>",
            "<pre><code",
            "<blockquote>",
            "<em>",
            "<ol>",
        ] {
            assert!(html.contains(tag), "missing {tag} in {html}");
        }
    }
}
