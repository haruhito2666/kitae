//! WPT tree-construction conformance harness (I5).
//!
//! Runs every `#data`/`#document` case in the vendored html5lib
//! tree-construction corpus (`tests/data/tree-construction/*.dat`) through
//! `kitae_html::parse` and diffs kitae's DOM against the expected pipe dump.
//! `docs/WPT_SCORECARD.md` records the measured pass rate; the baseline
//! assertion below guards against regressions only — this harness measures
//! conformance, it does not claim it.
//!
//! Documented normalizations (deliberate kitae divergences, applied to the
//! expected side so the score measures *unintended* divergence):
//!   * DOCTYPE and comment nodes are dropped — kitae's DOM has no such
//!     node kinds (scorecard gap).
//!   * An empty `<head>` element is stripped — kitae omits it (the same
//!     convention as the A1 battery).
//!   * Text outside raw-text elements is whitespace-collapsed, and a
//!     leading run is dropped on the first child; whitespace-only nodes
//!     are dropped — kitae's content model. Text inside script/style/
//!     pre/textarea/xmp/iframe/noembed/noframes is verbatim (with the
//!     spec's swallowed leading newline for pre/listing/textarea).
//!   * Attribute lines are sorted — attribute order is not semantic.
//! `#script-on` variants are skipped (kitae has no scripting);
//! `#document-fragment` cases run through `kitae_html::parse_fragment`
//! against the case's context element. Tokenizer/tree-builder error codes
//! are never compared (kitae has no error channel — scorecard gap).
//!
//! Set `KITAE_WPT_DIFF=1` to print the first divergences per file.

use kitae_dom::{Dom, NodeId, NodeKind};
use std::fs;
use std::path::Path;

/// Recorded in docs/WPT_SCORECARD.md. Fails when the measured rate drops
/// below it. Measured 2026-09-25: 1072/1784 (60.1%) after the documented
/// normalizations; the remaining failures are the scorecard's known gaps
/// (in-template modes, adoption-agency scope nuance, rawtext end-tag-open
/// buffering, plaintext integration contexts, foreign-content depth). The
/// constant sits below the exact ratio (0.60089) so the comparison is robust.
const BASELINE_PASS_RATE: f64 = 0.600;

/// A parsed .dat case: one `#data` plus every (script-flag, dump) pair.
struct Case {
    data: String,
    docs: Vec<(bool, Vec<String>)>,
    fragment: Option<String>,
}

/// One parsed `#document` dump, normalized to kitae's DOM model.
#[derive(Debug, PartialEq, Clone)]
enum Exp {
    Doctype(String),
    Comment(String),
    Text(String),
    Elem {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Vec<Exp>,
    },
}

/// Depth marker for multiline-text continuation lines (no `| ` prefix in
/// the .dat format).
const CONTINUATION: usize = usize::MAX;

/// Elements whose text kitae keeps verbatim (the DOM-level raw list in
/// kitae_html's tree builder).
const RAW_TEXT_TAGS: [&str; 8] = [
    "script", "style", "pre", "textarea", "xmp", "iframe", "noembed", "noframes",
];

fn data_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/tree-construction")
}

/// Split a .dat file into cases.
fn parse_dat(text: &str) -> Vec<Case> {
    let mut cases: Vec<Case> = vec![];
    let mut section = "";
    let mut frag: Option<String> = None;
    let mut data = String::new();
    let mut docs: Vec<(bool, Vec<String>)> = vec![];
    let mut script_on = false;
    let mut cur_doc: Vec<String> = vec![];
    let mut frag_line_pending = false;

    macro_rules! flush_case {
        () => {
            if !docs.is_empty() {
                cases.push(Case {
                    data: std::mem::take(&mut data),
                    docs: std::mem::take(&mut docs),
                    fragment: frag.take(),
                });
            }
        };
    }

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('#') {
            let (marker, _) = match rest.split_once(char::is_whitespace) {
                Some((m, r)) => (m, Some(r)),
                None => (rest, None),
            };
            match marker {
                "data" => {
                    flush_case!();
                    section = "data";
                    data.clear();
                    docs.clear();
                    script_on = false;
                }
                "errors" | "new-errors" => section = "errors",
                "document" => {
                    section = "document";
                    cur_doc.clear();
                    docs.push((script_on, std::mem::take(&mut cur_doc)));
                }
                "document-fragment" => {
                    section = "fragment";
                    frag = Some(String::new());
                    frag_line_pending = true;
                }
                "script-on" => script_on = true,
                "script-off" => script_on = false,
                _ => section = "other",
            }
            continue;
        }
        match section {
            "data" => {
                data.push_str(line);
                data.push('\n');
            }
            "document" => {
                if let Some((_, doc)) = docs.last_mut() {
                    doc.push(line.to_string());
                }
            }
            "fragment" => {
                if frag_line_pending {
                    // First line after #fragment is the context element;
                    // further lines would be attributes (not in this corpus).
                    *frag.as_mut().unwrap() = line.trim().to_string();
                    frag_line_pending = false;
                }
            }
            _ => {}
        }
    }
    flush_case!();
    // One trailing newline belongs to the section separator, not the input.
    for c in &mut cases {
        if c.data.ends_with('\n') {
            c.data.pop();
        }
    }
    cases
}

/// Split the `| `-prefixed dump into (depth, content) pairs. Physical
/// lines without the `| ` prefix are continuations of a multiline text
/// node (the .dat format writes them bare); they carry the CONTINUATION
/// marker and their raw content.
fn dump_lines(doc: &[String]) -> Vec<(usize, String)> {
    let mut out = vec![];
    for line in doc {
        let body = match line.strip_prefix("| ") {
            Some(b) => b,
            None if line.trim() == "|" => "",
            None => {
                out.push((CONTINUATION, line.clone()));
                continue;
            }
        };
        let depth = body.len() - body.trim_start_matches(' ').len();
        out.push((depth / 2, body.trim_start_matches(' ').to_string()));
    }
    out
}

fn is_attr_line(s: &str) -> bool {
    !s.starts_with('<') && !s.starts_with('"') && s.contains("=\"")
}

fn unescape(s: &str) -> String {
    s.replace("\\\"", "\"")
}

/// Parse a whole dump into a tree (doctype/comment included; the
/// normalizer drops them).
fn parse_nodes(lines: &[(usize, String)], cur: &mut usize, depth: usize) -> Vec<Exp> {
    let mut out = vec![];
    while *cur < lines.len() {
        let (d, content) = &lines[*cur];
        if *d < depth {
            break;
        }
        if content.starts_with("<!DOCTYPE") || content.starts_with("<!doctype") {
            out.push(Exp::Doctype(content.to_string()));
            *cur += 1;
        } else if content.starts_with("<!--") {
            out.push(Exp::Comment(
                content
                    .trim_start_matches("<!--")
                    .trim_end_matches("-->")
                    .to_string(),
            ));
            *cur += 1;
        } else if content.starts_with('<') && content.ends_with('>') {
            // Element open: `<tag>`; attributes follow at depth+1. Foreign
            // elements dump with a namespace label (`svg circle`,
            // `math mn`) — HTML tags never contain spaces; the engine keeps
            // lowercase local names.
            let raw_tag = &content[1..content.len() - 1];
            let tag = match raw_tag.split_once(' ') {
                Some((ns, local)) if ns == "svg" || ns == "math" => local.to_ascii_lowercase(),
                _ => raw_tag.to_string(),
            };
            *cur += 1;
            let mut attrs = vec![];
            while *cur < lines.len() && lines[*cur].0 == depth + 1 && is_attr_line(&lines[*cur].1) {
                let a = &lines[*cur].1;
                if let Some(eq) = a.find("=\"") {
                    let name = a[..eq].to_string();
                    let (value, used) = scan_quoted_value(a, eq + 2);
                    attrs.push((name, value));
                    let _ = used;
                }
                *cur += 1;
            }
            let children = parse_nodes(lines, cur, depth + 1);
            out.push(Exp::Elem {
                tag,
                attrs,
                children,
            });
        } else if content.starts_with('"') {
            // Text, possibly spanning physical lines: the .dat format
            // writes continuation lines bare (no `| ` prefix).
            let mut text = String::new();
            let mut chars: Vec<char> = content.chars().collect();
            let mut i = 1; // skip the opening quote
            loop {
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '"' {
                        text.push('"');
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        break;
                    }
                    text.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    break; // closing quote found
                }
                if *cur + 1 < lines.len() && lines[*cur + 1].0 == CONTINUATION {
                    *cur += 1;
                    text.push('\n'); // real newline in the dump
                    chars = lines[*cur].1.chars().collect();
                    i = 0;
                } else {
                    break; // unterminated; keep what we have
                }
            }
            *cur += 1;
            out.push(Exp::Text(text));
        } else {
            // Attribute-looking or unrecognized line at this depth: skip.
            *cur += 1;
        }
    }
    out
}

/// Unescape a quoted attribute value starting at `from` (just after `="`).
fn scan_quoted_value(s: &str, from: usize) -> (String, usize) {
    let rest = &s[from..];
    let end = rest.rfind('"').unwrap_or(rest.len());
    (unescape(&rest[..end]), end)
}

/// Apply kitae's content model to the expected tree (see module docs).
fn normalize(nodes: Vec<Exp>, parent_tag: Option<&str>) -> Vec<Exp> {
    let mut out = vec![];
    for node in nodes {
        match node {
            Exp::Doctype(_) | Exp::Comment(_) => {}
            Exp::Text(t) => {
                let raw_parent = parent_tag.map_or(false, |p| RAW_TEXT_TAGS.contains(&p));
                let t = if raw_parent {
                    // Verbatim: the WPT dump reflects the reference DOM,
                    // where the spec's swallowed leading newline for
                    // pre/listing/textarea is already applied — exactly
                    // what kitae's tree builder does too.
                    t
                } else {
                    let mut s = String::new();
                    let mut last_ws = false;
                    for c in t.chars() {
                        if c.is_ascii_whitespace() || c == '\u{b}' {
                            if !last_ws {
                                s.push(' ');
                            }
                            last_ws = true;
                        } else {
                            s.push(c);
                            last_ws = false;
                        }
                    }
                    // kitae trims a leading run when the node is the
                    // parent's first child; dropped doctype/comment nodes
                    // don't count — hence out.is_empty(), not an index.
                    if out.is_empty() {
                        s = s.trim_start().to_string();
                    }
                    s
                };
                if !t.is_empty() {
                    out.push(Exp::Text(t));
                }
            }
            Exp::Elem {
                tag,
                mut attrs,
                children,
            } => {
                attrs.sort();
                let kids = normalize(children, Some(&tag));
                if tag == "head" && kids.is_empty() {
                    continue;
                }
                out.push(Exp::Elem {
                    tag,
                    attrs,
                    children: kids,
                });
            }
        }
    }
    out
}

/// Kitae's DOM in the same normalized shape.
fn kitae_tree(dom: &Dom, n: NodeId, out: &mut Vec<Exp>) {
    for &c in dom.children(n) {
        let node = dom.node(c);
        match node.kind {
            NodeKind::Text => out.push(Exp::Text(node.text.clone())),
            NodeKind::Element => {
                let (tag, attrs) = node
                    .element
                    .as_ref()
                    .map(|e| (e.tag.clone(), e.attributes.clone()))
                    .unwrap_or_default();
                let mut children = vec![];
                kitae_tree(dom, c, &mut children);
                let mut attrs = attrs;
                attrs.sort();
                out.push(Exp::Elem {
                    tag,
                    attrs,
                    children,
                });
            }
        }
    }
}

/// Strip an empty `<head>` from kitae's actual tree (the same convention
/// the expected side applies — see normalize; kitae keeps the element but
/// it carries no semantic content).
fn strip_empty_head(nodes: Vec<Exp>) -> Vec<Exp> {
    nodes
        .into_iter()
        .filter_map(|n| match n {
            Exp::Elem {
                tag,
                attrs,
                children,
            } => {
                let kids = strip_empty_head(children);
                if tag == "head" && kids.is_empty() {
                    None
                } else {
                    Some(Exp::Elem {
                        tag,
                        attrs,
                        children: kids,
                    })
                }
            }
            other => Some(other),
        })
        .collect()
}

/// First-line difference between two trees, for KITAE_WPT_DIFF.
fn first_diff(a: &[Exp], b: &[Exp]) -> Option<String> {
    fn shape(e: &Exp, out: &mut String) {
        match e {
            Exp::Text(t) => out.push_str(&format!("\"{t}\"")),
            Exp::Doctype(d) => out.push_str(&format!("<!{d}>")),
            Exp::Comment(c) => out.push_str(&format!("<!--{c}-->")),
            Exp::Elem {
                tag,
                attrs,
                children,
            } => {
                out.push_str(&format!("({tag}"));
                for (k, v) in attrs {
                    out.push_str(&format!(" {k}={v:?}"));
                }
                for c in children {
                    out.push(' ');
                    shape(c, out);
                }
                out.push(')');
            }
        }
    }
    let mut sa = String::new();
    let mut sb = String::new();
    for e in a {
        shape(e, &mut sa);
    }
    for e in b {
        shape(e, &mut sb);
    }
    if sa == sb {
        return None;
    }
    // Common prefix, then show a window around the split.
    let ba = sa.as_bytes();
    let bb = sb.as_bytes();
    let mut i = 0;
    while i < ba.len() && i < bb.len() && ba[i] == bb[i] {
        i += 1;
    }
    let start = i.saturating_sub(40);
    Some(format!(
        "wpt:   …{}\n  kitae: …{}",
        &sa[start..(start + 160).min(sa.len())],
        &sb[start..(start + 160).min(sb.len())],
    ))
}

#[test]
fn wpt_tree_construction_conformance() {
    let dir = data_dir();
    let mut files: Vec<_> = fs::read_dir(&dir)
        .expect("tree-construction corpus present")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |x| x == "dat"))
        .collect();
    files.sort();

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut per_file: Vec<(String, usize, usize, usize)> = vec![];
    let diff = std::env::var("KITAE_WPT_DIFF").is_ok();

    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let mut p = 0usize;
        let mut f = 0usize;
        let mut s = 0usize;
        for case in parse_dat(&text) {
            let frag_ctx = case.fragment.clone();
            for (script_on, doc) in &case.docs {
                if *script_on {
                    s += 1; // kitae has no scripting
                    continue;
                }
                total += 1;
                let lines = dump_lines(doc);
                let mut cur = 0usize;
                let mut expected = parse_nodes(&lines, &mut cur, 0);
                let is_frag = frag_ctx.is_some();
                expected = if is_frag {
                    // Fragment dumps list the context element's children;
                    // kitae's content model applies with the context tag.
                    normalize(expected, frag_ctx.as_deref())
                } else {
                    normalize(expected, None)
                };
                // Fragment: parse_fragment returns the context element as
                // root — compare its children (no wrapper in the dump).
                // Document: parse returns the <html> element itself as
                // root; the WPT dump includes it, so emit it as wrapper.
                let (dom, root_id) = if is_frag {
                    kitae_html::parse_fragment(&case.data, frag_ctx.as_deref().unwrap_or("div"))
                } else {
                    let (d, r) = kitae_html::parse(&case.data);
                    match r {
                        Some(r) => (d, r),
                        None => {
                            f += 1;
                            continue;
                        }
                    }
                };
                let mut children = vec![];
                kitae_tree(&dom, root_id, &mut children);
                let actual = if is_frag {
                    strip_empty_head(children)
                } else {
                    let node = dom.node(root_id);
                    let (tag, attrs) = node
                        .element
                        .as_ref()
                        .map(|e| (e.tag.clone(), e.attributes.clone()))
                        .unwrap_or_default();
                    let mut attrs = attrs;
                    attrs.sort();
                    strip_empty_head(vec![Exp::Elem {
                        tag,
                        attrs,
                        children,
                    }])
                };
                if expected == actual {
                    p += 1;
                } else {
                    f += 1;
                    if diff {
                        if let Some(d) = first_diff(&expected, &actual) {
                            println!("[{name}] {:?}\n  {}", case.data, d);
                        }
                    }
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
        "WPT tree-construction: {passed}/{total} pass ({:.1}%), {skipped} skipped",
        rate * 100.0
    );
    assert!(
        rate >= BASELINE_PASS_RATE,
        "tree-construction pass rate {:.1}% regressed below the recorded baseline {:.1}% \
         (see docs/WPT_SCORECARD.md; run with KITAE_WPT_DIFF=1 to inspect divergences)",
        rate * 100.0,
        BASELINE_PASS_RATE * 100.0,
    );
}
