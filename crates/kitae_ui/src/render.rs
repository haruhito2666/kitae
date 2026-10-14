//! Rendering virtual trees to HTML, plus runtime helpers `view!` expands to.

use crate::vnode::{Handler, KeyVal, VNode};
use std::fmt::Write;
use std::rc::Rc;

// ---------------------------------------------------- `view!` __rt helpers ---

/// Static string attribute value.
pub fn attr_str(s: &str) -> String {
    s.to_string()
}

/// Dynamic attribute value: anything `Display` (signals included).
pub fn attr_val<V: std::fmt::Display + ?Sized>(v: &V) -> String {
    v.to_string()
}

/// Attribute from a bare identifier expression: renders now and, when the
/// identifier was a `Signal<String>`, records a two-way binding.
pub fn attr_ref<A: crate::vnode::AttrRef + ?Sized>(v: &A) -> crate::vnode::AttrRendered {
    v.render_attr()
}

/// Box an event handler expression.
pub fn box_handler<F: Fn(crate::vnode::Event) + 'static>(f: F) -> Handler {
    Rc::new(f)
}

/// `key:` value from anything `Display`.
pub fn key_val<K: std::fmt::Display + ?Sized>(k: &K) -> KeyVal {
    KeyVal(k.to_string())
}

// --------------------------------------------------------------- html out ---

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render a virtual tree to HTML (the Init frame / insert payloads).
pub fn render_html(nodes: &[VNode]) -> String {
    let mut s = String::new();
    for n in nodes {
        render_node(n, &mut s);
    }
    s
}

fn void(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn render_node(n: &VNode, s: &mut String) {
    match n {
        VNode::Text(t) => {
            let _ = write!(s, "{}", escape(t));
        }
        VNode::Element(el) => {
            if el.tag.is_empty() {
                // Fragment: renders as its children only.
                for c in &el.children {
                    render_node(c, s);
                }
                return;
            }
            let _ = write!(s, "<{}", el.tag);
            for (k, v) in &el.attrs {
                let _ = write!(s, " {}=\"{}\"", k, escape(v));
            }
            let style = el.style_attr();
            if !style.is_empty() {
                let _ = write!(s, " style=\"{}\"", escape(&style));
            }
            if void(&el.tag) {
                // A void element cannot have content — never emit a closing
                // tag (invalid HTML). Anything the tree still carries under
                // it is dropped, matching how a parser discards markup
                // inside a void tag.
                let _ = write!(s, ">");
                return;
            }
            let _ = write!(s, ">");
            for c in &el.children {
                render_node(c, s);
            }
            let _ = write!(s, "</{}>", el.tag);
        }
    }
}
