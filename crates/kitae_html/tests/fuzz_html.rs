//! I4 fuzzing program — HTML parser, deterministic mutation fuzzing.
//!
//! Real browsers fuzz their parsers continuously; this is the engine's
//! standing program. Each test drives a fixed-seed xorshift PRNG through a
//! seed corpus and mutation operators (byte flips, splices, hostile-character
//! insertion, truncation) and asserts the invariants that must hold for
//! *every* input, not just the ones we thought of:
//!
//! 1. `parse` never panics, and the produced arena is internally consistent
//!    (every child's parent pointer is correct, ids are in range, the tree
//!    is acyclic by construction of the walk).
//! 2. `parse_fragment` never panics for any context element.
//! 3. `parse_streaming` of a randomly chunked input produces the same DOM
//!    as `parse` of the concatenation (the documented O4 contract).
//!
//! The PRNG is seeded per test, so a failure reproduces exactly. CI runs the
//! default iteration count; a deeper soak is one environment variable away:
//!
//! ```sh
//! KITAE_FUZZ_ITERS=20000 cargo test -p kitae_html --test fuzz_html
//! ```
//!
//! The next step beyond this program is coverage-guided fuzzing
//! (`cargo-fuzz` with `libfuzzer-sys`); see docs/FUZZING.md for the plan.

/// The iteration budget per test. Override with `KITAE_FUZZ_ITERS`.
fn iters() -> usize {
    std::env::var("KITAE_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

/// xorshift64*: deterministic, dependency-free, good enough to explore the
/// neighborhood of a seed corpus.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Characters that stress a tree builder: tag delimiters, attribute quotes,
/// comment markers, entity openers, nulls, replacement chars, bidi controls,
/// emoji, and plain markup letters.
const HOSTILE_CHARS: [char; 24] = [
    '<',
    '>',
    '"',
    '\'',
    '/',
    '&',
    '=',
    '!',
    '-',
    '\t',
    '\n',
    '\u{0}',
    '\u{9}',
    '\u{c}',
    '\u{feff}',
    '\u{fffd}',
    '\u{200f}',
    '\u{202e}',
    '\u{1f600}',
    '<',
    'a',
    'p',
    '"',
    ';',
];

/// Seed corpus drawn from the shapes the tree builder has scope rules about:
/// formatting elements, tables, raw text, templates, foreign content.
const SEEDS: [&str; 14] = [
    "<html><head><title>t</title></head><body><p>hello</p></body></html>",
    "<p><b><i>x</p>y</i></b>",
    "<table><tr><td>cell<td>cell2</table>",
    "<table><b>x<tr><td>y</table>",
    "<select><option>a<option>b</select>",
    "<ul><li>a<li>b</ul>",
    "<svg><circle/><foreignObject><p>fo</p></foreignObject></svg>",
    "<math><mi>x</mi></math>",
    "<template><p>inert</p></template>",
    "<a href='x'>link<b>bold</a>more</b>",
    "<plaintext>rest of document",
    "<textarea><div>raw</div></textarea>",
    "<form><input><form><input></form></form>",
    "<body onload=x><img src=y onerror=z><script>var a = '</p>';</script>",
];

/// Mutate `s`: one random edit — flip a byte, insert a hostile char, delete
/// a chunk, duplicate a chunk, or splice in a slice of another seed.
fn mutate(rng: &mut XorShift, s: &str, donor: &str) -> String {
    let bytes: Vec<u8> = s.bytes().collect();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 8);
    match rng.below(5) {
        0 => {
            // Flip one byte in place (when non-empty).
            out = bytes;
            if !out.is_empty() {
                let at = rng.below(out.len());
                out[at] = (rng.next() & 0xff) as u8;
            }
        }
        1 => {
            // Insert a hostile char at a char boundary.
            let at = rng.below(s.chars().count() + 1);
            let c = HOSTILE_CHARS[rng.below(HOSTILE_CHARS.len())];
            for (i, ch) in s.chars().enumerate() {
                if i == at {
                    out.extend(c.to_string().bytes());
                }
                out.extend(ch.to_string().bytes());
            }
            if at == s.chars().count() {
                out.extend(c.to_string().bytes());
            }
        }
        2 => {
            // Delete a chunk.
            let start = rng.below(bytes.len() + 1);
            let len = rng.below(8);
            out.extend_from_slice(&bytes[..start.min(bytes.len())]);
            out.extend_from_slice(&bytes[(start + len).min(bytes.len())..]);
        }
        3 => {
            // Duplicate a chunk.
            let start = rng.below(bytes.len() + 1);
            let len = rng.below(8);
            let end = (start + len).min(bytes.len());
            out.extend_from_slice(&bytes);
            out.extend_from_slice(&bytes[start..end]);
        }
        _ => {
            // Splice: first half of s + second half of donor.
            let cut = rng.below(bytes.len() + 1);
            let dbytes = donor.as_bytes();
            let dcut = rng.below(dbytes.len() + 1);
            out.extend_from_slice(&bytes[..cut]);
            out.extend_from_slice(&dbytes[dcut..]);
        }
    }
    // Byte edits can split UTF-8 sequences; the parser takes `&str`, so the
    // lossy round-trip is part of the fuzz (replacement chars are legal input).
    String::from_utf8_lossy(&out).into_owned()
}

/// Invariant 1: parse never panics and the arena is parent-consistent.
/// Walks from the root through children and checks each child's parent
/// pointer and id range; unattached arena nodes are legal leftovers
/// (foster-parented spares) and are not visited.
fn assert_consistent(dom: &kitae_dom::Dom, root: Option<kitae_dom::NodeId>) {
    let Some(root) = root else { return };
    assert!(
        (root.0 as usize) < dom.len(),
        "root id out of range: {} vs {}",
        root.0,
        dom.len()
    );
    assert!(dom.node(root).parent.is_none(), "root has a parent");
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = stack.pop() {
        assert!(seen.insert(id), "node {} reachable twice: cycle", id.0);
        assert!(
            (id.0 as usize) < dom.len(),
            "node id out of range: {}",
            id.0
        );
        for &c in &dom.node(id).children {
            assert!((c.0 as usize) < dom.len(), "child id out of range: {}", c.0);
            assert_eq!(
                dom.node(c).parent,
                Some(id),
                "child {} of {} has wrong parent {:?}",
                c.0,
                id.0,
                dom.node(c).parent
            );
            stack.push(c);
        }
    }
}

#[test]
fn parse_never_panics_and_keeps_the_arena_consistent() {
    let mut rng = XorShift::new(0x48544D4C); // "HTML"
    for round in 0..iters() {
        let donor = SEEDS[rng.below(SEEDS.len())];
        let mut s = SEEDS[rng.below(SEEDS.len())].to_string();
        // 1–4 edits per input.
        for _ in 0..1 + rng.below(4) {
            s = mutate(&mut rng, &s, donor);
        }
        let (dom, root) = kitae_html::parse(&s);
        assert_consistent(&dom, root);
        if let Some(r) = root {
            assert_eq!(dom.tag(r), "html", "round {round}: root is <html>");
        }
    }
}

#[test]
fn parse_fragment_never_panics_for_any_context() {
    let mut rng = XorShift::new(0x46524147); // "FRAG"
    let contexts = [
        "div", "table", "tbody", "title", "select", "template", "p", "span",
    ];
    for _ in 0..iters() {
        let donor = SEEDS[rng.below(SEEDS.len())];
        let mut s = SEEDS[rng.below(SEEDS.len())].to_string();
        for _ in 0..1 + rng.below(3) {
            s = mutate(&mut rng, &s, donor);
        }
        let ctx = contexts[rng.below(contexts.len())];
        let (dom, root) = kitae_html::parse_fragment(&s, ctx);
        assert_eq!(dom.node(root).parent, None, "fragment root is parentless");
        for &c in &dom.node(root).children {
            assert_eq!(dom.node(c).parent, Some(root), "fragment child parent");
        }
    }
}

#[test]
fn streaming_parse_matches_whole_parse_for_random_chunks() {
    let mut rng = XorShift::new(0x5354524D); // "STRM"
    for _ in 0..iters() {
        let donor = SEEDS[rng.below(SEEDS.len())];
        let mut s = SEEDS[rng.below(SEEDS.len())].to_string();
        for _ in 0..1 + rng.below(3) {
            s = mutate(&mut rng, &s, donor);
        }
        // Chunk at char boundaries (streaming feeds &str chunks).
        let chars: Vec<char> = s.chars().collect();
        let mut chunks: Vec<String> = Vec::new();
        let mut rest = &chars[..];
        while !rest.is_empty() {
            let take = 1 + rng.below(rest.len());
            chunks.push(rest[..take].iter().collect());
            rest = &rest[take..];
        }
        let chunk_refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let (dom_stream, root_stream) = kitae_html::parse_streaming(&chunk_refs);
        let (dom_whole, root_whole) = kitae_html::parse(&s);
        assert_eq!(root_stream.is_some(), root_whole.is_some());
        if let (Some(a), Some(b)) = (root_stream, root_whole) {
            assert_eq!(
                tree_shape(&dom_stream, a),
                tree_shape(&dom_whole, b),
                "streaming DOM diverged from whole-parse for input {s:?}"
            );
        }
    }
}

#[test]
fn tokenizer_streaming_matches_batch_for_random_chunks() {
    let mut rng = XorShift::new(0x544F4B53);
    for _ in 0..iters() {
        let donor = SEEDS[rng.below(SEEDS.len())];
        let mut s = SEEDS[rng.below(SEEDS.len())].to_string();
        for _ in 0..1 + rng.below(3) {
            s = mutate(&mut rng, &s, donor);
        }
        let chars: Vec<char> = s.chars().collect();
        let mut chunks: Vec<String> = Vec::new();
        let mut rest = &chars[..];
        while !rest.is_empty() {
            let take = 1 + rng.below(rest.len());
            chunks.push(rest[..take].iter().collect());
            rest = &rest[take..];
        }
        let mut tokenizer = kitae_html::tokenizer::Tokenizer::new_streaming();
        let mut streaming = Vec::new();
        for chunk in &chunks {
            tokenizer.feed(chunk);
            while let Some(token) = tokenizer.next_token() {
                streaming.push(token);
            }
        }
        tokenizer.finish();
        while let Some(token) = tokenizer.next_token() {
            streaming.push(token);
        }
        let batch = kitae_html::tokenizer::tokenize(&s);
        assert_eq!(
            coalesce_text_tokens(streaming),
            coalesce_text_tokens(batch),
            "tokenizer streaming diverged for input {s:?}"
        );
    }
}

fn coalesce_text_tokens(
    mut tokens: Vec<kitae_html::tokenizer::Token>,
) -> Vec<kitae_html::tokenizer::Token> {
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens.drain(..) {
        match token {
            kitae_html::tokenizer::Token::Text(next) => {
                if let Some(kitae_html::tokenizer::Token::Text(previous)) = out.last_mut() {
                    previous.push_str(&next);
                } else {
                    out.push(kitae_html::tokenizer::Token::Text(next));
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Compact structural fingerprint: element tags, attribute names/values and
/// text content in document order, wrapped in parens.
fn tree_shape(dom: &kitae_dom::Dom, root: kitae_dom::NodeId) -> String {
    let mut out = String::new();
    fn walk(dom: &kitae_dom::Dom, id: kitae_dom::NodeId, out: &mut String) {
        let node = dom.node(id);
        out.push('(');
        match node.kind {
            kitae_dom::NodeKind::Element => {
                let el = node.element.as_ref().expect("element kind carries data");
                out.push_str(&el.tag);
                for (k, v) in &el.attributes {
                    out.push(' ');
                    out.push_str(k);
                    out.push('=');
                    out.push_str(v);
                }
                for &c in &node.children {
                    walk(dom, c, out);
                }
            }
            kitae_dom::NodeKind::Text => {
                out.push('#');
                out.push_str(&node.text);
            }
        }
        out.push(')');
    }
    walk(dom, root, &mut out);
    out
}
