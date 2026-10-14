//! A1 regression battery: 30 misnesting / table-voodoo / adoption-agency
//! cases diffed line-by-line against html5lib 1.1 (the reference tree
//! builder). Each `expected` string is html5lib's own S-expression dump
//! (generated via /tmp/h5oracle/oracle.py), with the empty `(head)` stripped
//! — kitae deliberately omits an empty head element. `tree_shape` below emits
//! the identical format from kitae_html's parse. If a change to the tree
//! builder diverges from the spec here, this test fails with the exact case.

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
                                out.push_str(&format!("\"{t}\""));
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
                    out.push_str(&format!("\"{t}\""));
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

fn dump(html: &str) -> String {
    let (dom, root) = kitae_html::parse(html);
    tree_shape(&dom, root.expect("document has a root"))
}

/// (input, html5lib 1.1 expected dump) — the full battery.
const BATTERY: &[(&str, &str)] = &[
    // 0: formatting survives an implied-end p
    ("<b>1<p>2</p>3", "(html(body(b\"1\"(p\"2\")\"3\")))"),
    // 1: formatting element inside a cell ends at the cell boundary
    (
        "<table><td><b>1</td>2</table>",
        "(html(body\"2\"(table(tbody(tr(td(b\"1\")))))))",
    ),
    // 2: same, with trailing formatting after the table
    (
        "<table><td><b>1</td>2</table><i>x</i>",
        "(html(body\"2\"(table(tbody(tr(td(b\"1\")))))(i\"x\")))",
    ),
    // 3: adoption agency moves the clone after the p
    ("<b>1<p>2</b>3", "(html(body(b\"1\")(p(b\"2\")\"3\")))"),
    // 4: formatting before a table is fostered out
    (
        "<table><b>1</b>2</table>",
        "(html(body(b\"1\")\"2\"(table)))",
    ),
    // 5: adoption inside a div
    (
        "<div><b>1<p>2</b>3</div>",
        "(html(body(div(b\"1\")(p(b\"2\")\"3\"))))",
    ),
    // 6: formatting spans an explicit p end
    ("<p><b>1</b>2</p>3", "(html(body(p(b\"1\")\"2\")\"3\"))"),
    // 7: reconstruct for a p opened inside formatting
    (
        "<div>1<b>2<p>3</b>4</div>",
        "(html(body(div\"1\"(b\"2\")(p(b\"3\")\"4\"))))",
    ),
    // 8: fostered text merges into the preceding text node
    (
        "<i>1<table><tr><td>2</td>3</table>4</i>",
        "(html(body(i\"13\"(table(tbody(tr(td\"2\"))))\"4\")))",
    ),
    // 9: b wraps the whole fostered table
    (
        "<b><table><td>x</table>2</b>",
        "(html(body(b(table(tbody(tr(td\"x\"))))\"2\")))",
    ),
    // 10: (duplicate of 4)
    (
        "<table><b>1</b>2</table>",
        "(html(body(b\"1\")\"2\"(table)))",
    ),
    // 11: b fostered into the table; text after the table stays plain
    (
        "<b>1<table><td>2</td></table>3",
        "(html(body(b\"1\"(table(tbody(tr(td\"2\"))))\"3\")))",
    ),
    // 12: (duplicate of 1)
    (
        "<table><td><b>1</td>2</table>",
        "(html(body\"2\"(table(tbody(tr(td(b\"1\")))))))",
    ),
    // 13: fostered b is cleared off the stack before the implied tbody
    (
        "<table><b><tr><td>2</td></tr></b></table>",
        "(html(body(b)(table(tbody(tr(td\"2\"))))))",
    ),
    // 14: empty formatting elements stay empty (no spurious reconstruct)
    (
        "<i><table><i></i>x</table></i>",
        "(html(body(i(i)\"x\"(table))))",
    ),
    // 15: same with b
    (
        "<b><table><b></b>x</table>x</b>",
        "(html(body(b(b)\"x\"(table)\"x\")))",
    ),
    // 16: caption keeps its formatting; the row is a sibling
    (
        "<table><caption><b>x</b></caption><tr><td>y</td></tr></table>",
        "(html(body(table(caption(b\"x\"))(tbody(tr(td\"y\"))))))",
    ),
    // 17: fostered b with text, then tr clears the stack
    (
        "<table><b>x<tr><td>y</td></tr></table>",
        "(html(body(b\"x\")(table(tbody(tr(td\"y\"))))))",
    ),
    // 18: two cells, formatting ends at the first cell boundary
    (
        "<table><tr><td><b>1</td><td>2</td></tr></table>",
        "(html(body(table(tbody(tr(td(b\"1\"))(td\"2\"))))))",
    ),
    // 19: nested table closes the open one; trailing text reconstructs b
    (
        "<table><b>1<table><tr><td>2</td></tr></table>3</table>",
        "(html(body(b\"1\")(table)(table(tbody(tr(td\"2\"))))(b\"3\")))",
    ),
    // 20: adoption agency with a div
    ("<b>1<div>2</b>3", "(html(body(b\"1\")(div(b\"2\")\"3\")))"),
    // 21: <a> adoption
    (
        "<a href=x>1<div>2</a>3",
        "(html(body(a\"1\")(div(a\"2\")\"3\")))",
    ),
    // 22: nobr behaves like a formatting element
    (
        "<nobr>1<div>2</nobr>3",
        "(html(body(nobr\"1\")(div(nobr\"2\")\"3\")))",
    ),
    // 23: button is not a formatting element — no clone after the div
    (
        "<button>1<div>2</button>3",
        "(html(body(button\"1\"(div\"2\"))\"3\"))",
    ),
    // 24: a around a table: fostered, text after the table reconstructs a
    (
        "<a>1<table><td>2</td></table>3</a>",
        "(html(body(a\"1\"(table(tbody(tr(td\"2\"))))\"3\")))",
    ),
    // 25: nested formatting, adoption on the outer one
    (
        "<b><i>1<p>2</b>3",
        "(html(body(b(i\"1\"))(i(p(b\"2\")\"3\"))))",
    ),
    // 26: three nested b's, adoption unwinds them in order
    (
        "<b>1<b>2<b>3<div>4</b>5",
        "(html(body(b\"1\"(b\"2\"(b\"3\")(div(b\"4\")\"5\")))))",
    ),
    // 27: font with attributes: reconstruction keeps identity per attrs
    (
        "<font color=red>1<font color=blue>2<p>3</font>4</font>5",
        "(html(body(font\"1\"(font\"2\"))(p(font(font\"3\")\"4\")\"5\")))",
    ),
    // 28: b inside tbody is fostered out; tr clears the stack
    (
        "<table><tbody><b>x</b><tr><td>y</td></tr></tbody></table>",
        "(html(body(b\"x\")(table(tbody(tr(td\"y\"))))))",
    ),
    // 29: closeP does NOT reconstruct — no stray empty b clones
    (
        "<p><b>1<div><p>2</b>3</div>",
        "(html(body(p(b\"1\"))(div(p(b\"2\")\"3\"))))",
    ),
];

#[test]
fn html5lib_battery() {
    let mut failures = Vec::new();
    for (i, (input, expected)) in BATTERY.iter().enumerate() {
        let got = dump(input);
        if got != *expected {
            failures.push(format!(
                "case {i}: {input}\n  expected: {expected}\n  actual:   {got}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} battery case(s) diverge from html5lib:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
