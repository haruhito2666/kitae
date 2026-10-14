//! The Document Object Model: an arena-owned element tree plus text nodes.
//!
//! Nodes live in a `Vec` and reference each other by `NodeId` (a newtype over
//! u32) — no `Rc<RefCell>`, no tree-unsafe aliasing. Attribute lookups are
//! case-insensitive because HTML lowercases tag and attribute names on parse.

use std::sync::atomic::{AtomicU64, Ordering};

/// I2: global revision counter backing [`Dom::touch`] — strictly increasing
/// across every DOM in the process, so a revision never repeats for a node
/// even as arenas are dropped and rebuilt.
static NEXT_REV: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Element,
    Text,
}

#[derive(Debug, Clone)]
pub struct ElementData {
    pub tag: String, // lowercased
    pub attributes: Vec<(String, String)>,
}

impl ElementData {
    /// Attribute lookup is ASCII case-insensitive on both sides: HTML attr
    /// names are case-insensitive, and SVG stores camelCase (viewBox) that
    /// callers may spell either way.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn id(&self) -> Option<&str> {
        self.attr("id")
    }

    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.attr("class").unwrap_or("").split_whitespace()
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub element: Option<ElementData>,
    pub text: String,
    /// I2: last revision this node (or anything in its subtree / ancestry
    /// chain) was touched at — 0 means never mutated since creation.
    pub rev: u64,
}

impl Node {
    fn element(data: ElementData) -> Node {
        Node {
            kind: NodeKind::Element,
            parent: None,
            children: Vec::new(),
            element: Some(data),
            text: String::new(),
            rev: 0,
        }
    }

    fn text_node(text: String) -> Node {
        Node {
            kind: NodeKind::Text,
            parent: None,
            children: Vec::new(),
            element: None,
            text,
            rev: 0,
        }
    }

    pub fn tag(&self) -> &str {
        match &self.element {
            Some(e) => e.tag.as_str(),
            None => "",
        }
    }
}

#[derive(Debug, Default)]
pub struct Dom {
    nodes: Vec<Node>,
    /// Text run per node, coalesced during parse; kept as insertion index.
    text_nodes: usize,
}

impl Dom {
    pub fn new() -> Dom {
        Dom {
            nodes: Vec::new(),
            text_nodes: 0,
        }
    }

    pub fn create_element(&mut self, tag: &str, attributes: Vec<(String, String)>) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node::element(ElementData {
            tag: tag.to_ascii_lowercase(),
            attributes,
        }));
        id
    }

    /// Create an element keeping the tag verbatim — the tree builder passes
    /// canonical camelCase for foreign-content elements (foreignObject,
    /// clipPath); HTML insertions use [`create_element`], which lowercases.
    pub fn create_element_verbatim(
        &mut self,
        tag: &str,
        attributes: Vec<(String, String)>,
    ) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node::element(ElementData {
            tag: tag.to_string(),
            attributes,
        }));
        id
    }

    pub fn create_text(&mut self, text: String) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node::text_node(text));
        self.text_nodes += 1;
        id
    }

    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        self.nodes[parent.0 as usize].children.push(child);
        self.nodes[child.0 as usize].parent = Some(parent);
        self.touch(child);
    }

    /// Insert `child` into `parent`'s child list immediately before `before`
    /// (the tree builder's foster-parenting primitive). `before` must be a
    /// direct child of `parent`.
    pub fn insert_before(&mut self, parent: NodeId, before: NodeId, child: NodeId) {
        debug_assert_eq!(self.node(before).parent, Some(parent));
        let kids = &mut self.nodes[parent.0 as usize].children;
        let pos = kids.iter().position(|&c| c == before).unwrap_or(kids.len());
        kids.insert(pos, child);
        self.nodes[child.0 as usize].parent = Some(parent);
        self.touch(child);
    }

    /// Remove `child` from `parent`'s child list (the subtree stays alive in
    /// the arena, which is fine — layout only walks from the root).
    pub fn detach(&mut self, parent: NodeId, child: NodeId) {
        self.nodes[parent.0 as usize]
            .children
            .retain(|&c| c != child);
        self.touch(child);
    }

    /// I2: the revision `id` was last touched at (0 = never mutated).
    pub fn rev(&self, id: NodeId) -> u64 {
        self.nodes[id.0 as usize].rev
    }

    /// I2: mark `id`, its whole subtree and its ancestor chain as changed
    /// under one fresh revision. Ancestors are included so a cached fragment
    /// for an enclosing block invalidates when any transitive child changes;
    /// the subtree is included so a fragment stored for an inner block can't
    /// outlive a mutation inside it.
    pub fn touch(&mut self, id: NodeId) {
        let r = NEXT_REV.fetch_add(1, Ordering::Relaxed);
        self.mark_subtree(id, r);
        let mut p = self.nodes[id.0 as usize].parent;
        while let Some(pid) = p {
            self.nodes[pid.0 as usize].rev = r;
            p = self.nodes[pid.0 as usize].parent;
        }
    }

    /// I2: fresh revision across the whole tree — every cached fragment is
    /// invalid. For state that restyles without a DOM mutation.
    pub fn bump_all(&mut self) {
        let r = NEXT_REV.fetch_add(1, Ordering::Relaxed);
        for n in &mut self.nodes {
            n.rev = r;
        }
    }

    fn mark_subtree(&mut self, id: NodeId, r: u64) {
        self.nodes[id.0 as usize].rev = r;
        let kids = self.nodes[id.0 as usize].children.clone();
        for k in kids {
            self.mark_subtree(k, r);
        }
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    /// Replace a text node's content (no-op for elements). Returns false when
    /// `id` is not a text node. Callers mark the node's ancestors dirty for
    /// incremental relayout.
    pub fn set_text(&mut self, id: NodeId, text: String) -> bool {
        let n = &mut self.nodes[id.0 as usize];
        if n.kind != NodeKind::Text {
            return false;
        }
        n.text = text;
        self.touch(id);
        true
    }

    /// Set (or add) an attribute on an element. Returns false for text nodes.
    /// The name is stored lowercased; existing slots match case-insensitively
    /// so a camelCase SVG attribute is updated in place, not duplicated.
    pub fn set_attr(&mut self, id: NodeId, name: &str, value: &str) -> bool {
        let n = &mut self.nodes[id.0 as usize];
        let Some(el) = n.element.as_mut() else {
            return false;
        };
        let name = name.to_ascii_lowercase();
        let changed = if let Some(slot) = el
            .attributes
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(&name))
        {
            let changed = slot.1 != value;
            slot.1 = value.to_string();
            changed
        } else {
            el.attributes.push((name, value.to_string()));
            true
        };
        if changed {
            self.touch(id);
        }
        true
    }

    /// Remove an attribute from an element. Returns true when removed.
    pub fn remove_attr(&mut self, id: NodeId, name: &str) -> bool {
        let n = &mut self.nodes[id.0 as usize];
        let Some(el) = n.element.as_mut() else {
            return false;
        };
        let name = name.to_ascii_lowercase();
        let before = el.attributes.len();
        el.attributes.retain(|(k, _)| *k != name);
        if el.attributes.len() != before {
            self.touch(id);
            return true;
        }
        false
    }

    pub fn element(&self, id: NodeId) -> Option<&ElementData> {
        let n = self.node(id);
        if n.kind == NodeKind::Element {
            n.element.as_ref()
        } else {
            None
        }
    }

    pub fn tag(&self, id: NodeId) -> &str {
        self.node(id).tag()
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.node(id).children
    }

    /// Serialize the subtree rooted at `id` back to HTML/XML source (R6).
    /// Used for inline `<svg>`: the subtree is re-parsed by the SVG renderer,
    /// so childless elements close XML-style (`<path/>`) and text escapes
    /// `& < >`. The root `<svg>` gains `xmlns` when the source omitted it.
    pub fn outer_html(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.write_outer(id, &mut out);
        out
    }

    fn write_outer(&self, id: NodeId, out: &mut String) {
        let n = self.node(id);
        match n.kind {
            NodeKind::Text => {
                for c in n.text.chars() {
                    match c {
                        '&' => out.push_str("&amp;"),
                        '<' => out.push_str("&lt;"),
                        '>' => out.push_str("&gt;"),
                        c => out.push(c),
                    }
                }
            }
            NodeKind::Element => {
                let el = n.element.as_ref().expect("element kind carries data");
                out.push('<');
                out.push_str(&el.tag);
                let mut has_xmlns = false;
                for (k, v) in &el.attributes {
                    if k == "xmlns" {
                        has_xmlns = true;
                    }
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    for c in v.chars() {
                        match c {
                            '&' => out.push_str("&amp;"),
                            '"' => out.push_str("&quot;"),
                            '<' => out.push_str("&lt;"),
                            c => out.push(c),
                        }
                    }
                    out.push('"');
                }
                if el.tag == "svg" && !has_xmlns {
                    out.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
                }
                if n.children.is_empty() {
                    out.push_str("/>");
                    return;
                }
                out.push('>');
                for &child in &n.children {
                    self.write_outer(child, out);
                }
                out.push_str("</");
                out.push_str(&el.tag);
                out.push('>');
            }
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Depth-first pre-order walk of the subtree rooted at `root`.
    pub fn walk<F: FnMut(NodeId)>(&self, root: NodeId, f: &mut F) {
        f(root);
        for &child in &self.node(root).children {
            self.walk(child, f);
        }
    }

    /// First element in document order whose `id` attribute matches.
    pub fn get_element_by_id(&self, id: &str) -> Option<NodeId> {
        if let Some(root) = self.root() {
            let mut found = None;
            self.walk(root, &mut |n| {
                if found.is_none() {
                    if let Some(el) = self.element(n) {
                        if el.id() == Some(id) {
                            found = Some(n);
                        }
                    }
                }
            });
            found
        } else {
            None
        }
    }

    /// The document root element (`<html>`) if the tree has one.
    pub fn root(&self) -> Option<NodeId> {
        // Root = the node no other node claims as a child. With a single
        // linear scan we find the unique parentless element.
        self.nodes
            .iter()
            .position(|n| n.parent.is_none() && n.kind == NodeKind::Element)
            .map(|i| NodeId(i as u32))
    }

    /// All `href` values of `<a>` elements in the tree, with their node ids
    /// (used for click hit-testing).
    pub fn links(&self, root: NodeId) -> Vec<(NodeId, String)> {
        let mut out = Vec::new();
        self.walk(root, &mut |n| {
            if let Some(el) = self.element(n) {
                if el.tag == "a" {
                    if let Some(href) = el.attr("href") {
                        if !href.is_empty() && !href.starts_with('#') {
                            out.push((n, href.to_string()));
                        }
                    }
                }
            }
        });
        out
    }

    pub fn text_node_count(&self) -> usize {
        self.text_nodes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_walks_tree() {
        let mut dom = Dom::new();
        let html = dom.create_element("html", vec![]);
        let body = dom.create_element("body", vec![]);
        let div = dom.create_element("div", vec![("id".into(), "main".into())]);
        let text = dom.create_text("hello".into());
        dom.append(html, body);
        dom.append(body, div);
        dom.append(div, text);

        assert_eq!(dom.root(), Some(html));
        assert_eq!(dom.get_element_by_id("main"), Some(div));
        assert_eq!(dom.children(body), &[div]);

        let mut seen = Vec::new();
        dom.walk(html, &mut |n| seen.push(n));
        assert_eq!(seen, vec![html, body, div, text]);
    }

    #[test]
    fn detaches_children() {
        let mut dom = Dom::new();
        let a = dom.create_element("div", vec![]);
        let b = dom.create_element("span", vec![]);
        dom.append(a, b);
        dom.detach(a, b);
        assert!(dom.children(a).is_empty());
    }

    #[test]
    fn mutates_text_and_attrs() {
        let mut dom = Dom::new();
        let div = dom.create_element("div", vec![("id".into(), "a".into())]);
        let text = dom.create_text("old".into());
        dom.append(div, text);
        assert!(dom.set_text(text, "new".into()));
        assert_eq!(dom.node(text).text, "new");
        assert!(!dom.set_text(div, "x".into()), "elements reject text");
        assert!(dom.set_attr(div, "CLASS", "big"));
        assert_eq!(dom.element(div).unwrap().attr("class"), Some("big"));
        assert!(!dom.set_attr(text, "x", "y"));
    }

    #[test]
    fn revisions_track_mutations_up_and_down_the_tree() {
        let mut dom = Dom::new();
        let html = dom.create_element("html", vec![]);
        let body = dom.create_element("body", vec![]);
        let a = dom.create_element("div", vec![]);
        let b = dom.create_element("div", vec![]);
        let t = dom.create_text("x".into());
        dom.append(html, body);
        dom.append(body, a);
        dom.append(body, b);
        dom.append(a, t);
        // Construction itself touches (append), so every node has a
        // revision by the time the tree is built.
        let base = dom.rev(b);
        // Touching a leaf bumps it and every ancestor — never the siblings.
        dom.touch(t);
        assert!(dom.rev(t) > base);
        assert_eq!(dom.rev(a), dom.rev(t), "parent bumped");
        assert_eq!(dom.rev(html), dom.rev(t), "root bumped");
        assert_eq!(dom.rev(b), base, "sibling untouched");

        // The mutators touch on change…
        let before = dom.rev(body);
        dom.set_text(t, "y".into());
        assert!(dom.rev(body) > before);
        let (b_before, body_before) = (dom.rev(b), dom.rev(body));
        dom.set_attr(b, "class", "c");
        assert!(dom.rev(b) > b_before);
        assert!(dom.rev(body) > body_before, "ancestor chain too");
        // …but a same-value attribute write is a no-op for revisions.
        let stable = dom.rev(b);
        assert!(dom.set_attr(b, "class", "c"));
        assert_eq!(dom.rev(b), stable);
        // A missed removal doesn't touch either.
        assert!(!dom.remove_attr(b, "nope"));
        assert_eq!(dom.rev(b), stable);
        // Detach invalidates the old parent chain.
        let body_rev = dom.rev(body);
        dom.detach(body, b);
        assert!(dom.rev(body) > body_rev);

        // bump_all converges everything onto one fresh revision.
        dom.bump_all();
        let r = dom.rev(html);
        assert_eq!(dom.rev(b), r);
        assert_eq!(dom.rev(t), r);
        assert!(r > stable);
    }
}
