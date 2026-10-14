//! Oracle harness: prints kitae_html's parse of each line of a file as an
//! S-expression tree — `(tag "text" (child "text") "tail")` — in exactly the
//! format the html5lib comparison script (/tmp/h5oracle/oracle.py) emits, so
//! the two can be diffed line by line. Text runs collapse whitespace; a text
//! node that is whitespace-only disappears. Not part of the public API.
//!
//! Usage: cargo run -p kitae_html --example oracle <cases.txt>

use std::process::ExitCode;

use kitae_dom::{Dom, NodeId, NodeKind};

fn tree_shape(dom: &Dom, root: NodeId) -> String {
    fn rec(dom: &Dom, n: NodeId, out: &mut String) {
        let node = dom.node(n);
        match node.kind {
            NodeKind::Element => {
                out.push('(');
                out.push_str(node.tag());
                for &c in dom.children(n) {
                    match dom.node(c).kind {
                        NodeKind::Text => {
                            let t = node_text(dom, c);
                            if !t.is_empty() {
                                out.push_str(&format!(" \"{t}\""));
                            }
                        }
                        NodeKind::Element => rec(dom, c, out),
                        _ => {}
                    }
                }
                out.push(')');
            }
            NodeKind::Text => {
                let t = node_text(dom, n);
                if !t.is_empty() {
                    out.push_str(&format!(" \"{t}\""));
                }
            }
            _ => {}
        }
    }
    fn node_text(dom: &Dom, n: NodeId) -> String {
        dom.node(n)
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
    let mut out = String::new();
    rec(dom, root, &mut out);
    out
}

fn main() -> ExitCode {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: oracle <cases.txt>");
            return ExitCode::FAILURE;
        }
    };
    let cases = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    for (i, line) in cases.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let (dom, root) = kitae_html::parse(line);
        match root {
            Some(root) => println!("{i}\t{}", tree_shape(&dom, root)),
            None => println!("{i}\t<no root>"),
        }
    }
    ExitCode::SUCCESS
}
