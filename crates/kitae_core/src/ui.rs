//! The local UI runtime — Phase 1 of docs/UI_LIBRARY_PLAN.md.
//!
//! kitae_ui produces [`PatchOp`] streams; this module applies them onto the
//! engine's real DOM (`kitae_dom`) so the ordinary style → layout → display
//! list pipeline renders a component tree. No script is involved anywhere:
//! the embedder holds the components in-process, signals re-render on the
//! calling thread, and the diff becomes DOM mutations.
//!
//! Responsibilities kept **out** of here (they belong to `Browser`, which
//! owns the style engine and the display list):
//! - restyle/re-layout policy (all-`SetText` streams skip restyle; anything
//!   touching structure or attributes re-cascades),
//! - incremental splicing of the new display list,
//! - event hit-testing (the runtime only exposes the handler table).
//!
//! `SetFocus` is recorded in [`LocalApp::pending_focus`] for the engine to
//! consume; `ScrollIntoView` is accepted but currently a no-op (Phase 4).

use std::collections::HashMap;
use std::rc::Rc;

use kitae_dom::{Dom, NodeId};
use kitae_ui::diff::Mount;
use kitae_ui::patch::{NodeData, NodeRef, PatchOp};
use kitae_ui::vnode::{Handler, View};

/// An embedder-held kitae_ui application mounted onto the engine DOM.
///
/// The engine DOM *is* the app's DOM: after [`LocalApp::mount_dom`], the
/// `Browser` swaps this `Dom` in as the page tree and every downstream
/// consumer (style engine, layout, accessibility, find-in-page) works
/// unchanged. Subsequent frames call [`LocalApp::update`] to get a patch
/// stream and [`LocalApp::apply`] to land it on the tree.
pub struct LocalApp {
    render: Rc<dyn Fn() -> View>,
    mount: Mount,
    /// kitae_ui ref → engine DOM node (ref 0 = the `<body>`).
    ref2node: HashMap<u64, NodeId>,
    node2ref: HashMap<NodeId, u64>,
    /// [`NodeHandle`](kitae_ui::vnode::NodeHandle) id → engine DOM node —
    /// imperative access (focus, scroll-into-view, measured rects).
    handles: HashMap<u64, NodeId>,
    /// Set by `SetFocus` ops; the engine consumes and clears it.
    pub pending_focus: Option<NodeId>,
}

impl LocalApp {
    /// Wrap a render closure. Signals captured by it drive re-renders. The
    /// closure is wrapped so every build re-enters the hook scopes —
    /// `use_signal`/`use_ref` inside `#[component]` bodies stay stable.
    pub fn new(render: Rc<dyn Fn() -> View>) -> LocalApp {
        LocalApp {
            render: kitae_ui::hooks::wrap_render(render),
            mount: Mount::new(NodeRef(0)),
            ref2node: HashMap::new(),
            node2ref: HashMap::new(),
            handles: HashMap::new(),
            pending_focus: None,
        }
    }

    /// Mount onto a fresh engine DOM shaped like a parsed document
    /// (`html` > `body`), with the view's children under `<body>`.
    /// Ref 0 maps to `<body>`, matching `Mount::new(NodeRef(0))`.
    pub fn mount_dom(&mut self) -> (Dom, NodeId) {
        let view = (self.render)();
        let ops = self.mount.init(&view);
        let mut dom = Dom::new();
        let html = dom.create_element("html", vec![]);
        let body = dom.create_element("body", vec![]);
        dom.append(html, body);
        self.ref2node.clear();
        self.node2ref.clear();
        self.handles.clear();
        self.ref2node.insert(0, body);
        self.node2ref.insert(body, 0);
        self.apply(&mut dom, ops);
        (dom, body)
    }

    /// Adopt an ALREADY-BUILT DOM's `<body>` as ref 0 — the Live client's
    /// hydration path: the static page is already parsed and on screen;
    /// incoming InsertNode ops (parent 0) grow the real document.
    pub fn adopt_body(&mut self, body: NodeId) {
        self.ref2node.clear();
        self.node2ref.clear();
        self.handles.clear();
        self.ref2node.insert(0, body);
        self.node2ref.insert(body, 0);
    }

    /// Drop every ref mapping EXCEPT the body (ref 0) — the Live client's
    /// hydration swap: static content had no refs, the rebuilt tree takes
    /// fresh refs from the incoming ops. Node handles die with the old
    /// tree (the incoming ops re-mark the live ones).
    pub fn reset_refs(&mut self, _dom: &Dom) {
        let body = self.ref2node.get(&0).copied();
        self.ref2node.clear();
        self.node2ref.clear();
        self.handles.clear();
        if let Some(body) = body {
            self.ref2node.insert(0, body);
            self.node2ref.insert(body, 0);
        }
    }

    /// The engine node a [`NodeHandle`](kitae_ui::vnode::NodeHandle) was
    /// mounted at — `None` when the handle is unknown or its element was
    /// removed.
    pub fn node_for_handle(&self, handle: u64) -> Option<NodeId> {
        self.handles.get(&handle).copied()
    }

    /// Re-render and diff against the mounted shadow. The returned ops go
    /// to [`LocalApp::apply`]; the caller decides restyle/re-layout policy
    /// by scanning them.
    pub fn update(&mut self) -> Vec<PatchOp> {
        let view = (self.render)();
        self.mount.diff(&view)
    }

    /// The live handler table (ref → event name → handler), maintained by
    /// the diff itself. The engine routes hit-tested DOM nodes through
    /// [`LocalApp::ref_for`] into this.
    pub fn handlers(&self) -> &HashMap<u64, Vec<(String, Handler)>> {
        self.mount.handlers()
    }

    pub fn node_for(&self, r: NodeRef) -> Option<NodeId> {
        self.ref2node.get(&r.0).copied()
    }

    pub fn ref_for(&self, n: NodeId) -> Option<u64> {
        self.node2ref.get(&n).copied()
    }

    /// Number of live refs (test/introspection aid).
    pub fn ref2node_len(&self) -> usize {
        self.ref2node.len()
    }

    /// True when the patch stream only rewrites text (restyle skippable).
    pub fn is_text_only(ops: &[PatchOp]) -> bool {
        ops.iter().all(|op| matches!(op, PatchOp::SetText { .. }))
    }

    /// Land a patch stream on the engine DOM, keeping the ref maps and the
    /// child ordering exactly in sync with the shadow. Structural ops only
    /// touch `parent`'s child list — detached subtrees stay in the arena,
    /// which is fine because layout walks from the root.
    pub fn apply(&mut self, dom: &mut Dom, ops: Vec<PatchOp>) {
        for op in ops {
            match op {
                PatchOp::InsertNode {
                    parent,
                    index,
                    id,
                    node,
                } => {
                    let Some(pn) = self.ref2node.get(&parent.0).copied() else {
                        continue;
                    };
                    let child = Self::create_node(dom, &node);
                    if let Some(h) = node.node_ref {
                        self.handles.insert(h, child);
                    }
                    let kids = dom.children(pn).to_vec();
                    if index < kids.len() {
                        dom.insert_before(pn, kids[index], child);
                    } else {
                        dom.append(pn, child);
                    }
                    self.ref2node.insert(id.0, child);
                    self.node2ref.insert(child, id.0);
                }
                PatchOp::RemoveNode { parent, index } => {
                    let Some(pn) = self.ref2node.get(&parent.0).copied() else {
                        continue;
                    };
                    let kids = dom.children(pn).to_vec();
                    let Some(&child) = kids.get(index) else {
                        continue;
                    };
                    // Drop ref mappings for the whole removed subtree.
                    let mut doomed = Vec::new();
                    dom.walk(child, &mut |n| doomed.push(n));
                    for n in doomed {
                        if let Some(r) = self.node2ref.remove(&n) {
                            self.ref2node.remove(&r);
                        }
                        self.handles.retain(|_, &mut v| v != n);
                    }
                    dom.detach(pn, child);
                }
                PatchOp::MoveNode { parent, from, to } => {
                    let Some(pn) = self.ref2node.get(&parent.0).copied() else {
                        continue;
                    };
                    let mut kids = dom.children(pn).to_vec();
                    if from >= kids.len() {
                        continue;
                    }
                    let child = kids.remove(from);
                    dom.detach(pn, child);
                    kids = dom.children(pn).to_vec();
                    if to < kids.len() {
                        dom.insert_before(pn, kids[to], child);
                    } else {
                        dom.append(pn, child);
                    }
                }
                PatchOp::SetText { node, text } => {
                    if let Some(n) = self.ref2node.get(&node.0) {
                        dom.set_text(*n, text);
                    }
                }
                PatchOp::SetAttr { node, name, value } => {
                    if let Some(n) = self.ref2node.get(&node.0) {
                        dom.set_attr(*n, &name, &value);
                    }
                }
                PatchOp::RemoveAttr { node, name } => {
                    if let Some(n) = self.ref2node.get(&node.0) {
                        dom.remove_attr(*n, &name);
                    }
                }
                PatchOp::SetStyle { node, name, value } => {
                    let Some(n) = self.ref2node.get(&node.0) else {
                        continue;
                    };
                    let mut decls = parse_style_attr(
                        dom.element(*n)
                            .and_then(|el| el.attr("style"))
                            .unwrap_or(""),
                    );
                    if value.is_empty() {
                        decls.retain(|(k, _)| *k != name);
                    } else {
                        match decls.iter_mut().find(|(k, _)| *k == name) {
                            Some(entry) => entry.1 = value,
                            None => decls.push((name, value)),
                        }
                    }
                    dom.set_attr(*n, "style", &serialize_style_attr(&decls));
                }
                PatchOp::SetFocus { node } => {
                    self.pending_focus = self.ref2node.get(&node.0).copied();
                }
                PatchOp::ScrollIntoView { .. } => {
                    // Phase 4: route into the engine's scroll machinery.
                }
            }
        }
    }

    /// Materialize one `NodeData` as an engine DOM node. Component styles
    /// land as the standard inline `style` attribute so the style engine's
    /// existing inline-style path styles them; `view!` emits styles
    /// separately from attributes, so the two merge.
    fn create_node(dom: &mut Dom, data: &NodeData) -> NodeId {
        if data.tag.is_empty() {
            return dom.create_text(data.text.clone().unwrap_or_default());
        }
        let mut attrs = data.attrs.clone();
        if !data.styles.is_empty() {
            let style = serialize_style_attr(&data.styles);
            match attrs.iter_mut().find(|(k, _)| k == "style") {
                Some(entry) => entry.1.push_str(&style),
                None => attrs.push(("style".into(), style)),
            }
        }
        dom.create_element(&data.tag, attrs)
    }
}

/// Parse an inline `style` attribute into ordered declarations.
fn parse_style_attr(s: &str) -> Vec<(String, String)> {
    s.split(';')
        .filter_map(|d| d.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// Serialize declarations back to an inline `style` attribute.
fn serialize_style_attr(decls: &[(String, String)]) -> String {
    let mut s = String::new();
    for (k, v) in decls {
        if !s.is_empty() {
            s.push_str("; ");
        }
        s.push_str(k);
        s.push_str(": ");
        s.push_str(v);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use kitae_ui::prelude::*;

    /// A minimal two-signal app: click bumps a counter that renders as
    /// text; a toggle swaps the card's background color via SetStyle.
    fn counter_view(count: Signal<u32>) -> View {
        view! {
            div(class: "card", style: "background: #ffffff") {
                h2 { "Count: {count}" }
                button(on:click = move |_| count.update(|c| *c += 1)) { "+1" }
            }
        }
    }

    fn mounted_app() -> (LocalApp, Dom, NodeId) {
        let count = use_signal(|| 0u32);
        let render = move || counter_view(count.clone());
        let mut app = LocalApp::new(Rc::new(render));
        let (dom, body) = app.mount_dom();
        (app, dom, body)
    }

    #[test]
    fn mount_builds_document_shaped_dom() {
        let (app, dom, body) = mounted_app();
        assert_eq!(dom.tag(body), "body");
        // body > div.card > (h2 > text, button > text)
        let card = dom.children(body)[0];
        assert_eq!(dom.tag(card), "div");
        let card_el = dom.element(card).unwrap();
        assert_eq!(card_el.attr("class"), Some("card"));
        assert_eq!(card_el.attr("style"), Some("background: #ffffff"));
        assert_eq!(dom.tag(dom.children(card)[0]), "h2");
        assert_eq!(dom.tag(dom.children(card)[1]), "button");
        // Ref maps: 0 = body, 1 = div, 2 = h2, 3 = text, 4 = button, 5 = text
        assert_eq!(app.ref_for(body), Some(0));
        assert_eq!(app.node_for(NodeRef(4)), Some(dom.children(card)[1]));
    }

    #[test]
    fn click_through_handler_table_mutates_dom() {
        let (mut app, mut dom, body) = mounted_app();
        // Hit the button's ref and dispatch its click handler.
        let card = dom.children(body)[0];
        let button = dom.children(card)[1];
        let btn_ref = app.ref_for(button).unwrap();
        let handler = &app.handlers()[&btn_ref][0];
        assert_eq!(handler.0, "click");
        (handler.1)(Event {
            name: "click".into(),
            value: None,
        });
        // Re-render: exactly one SetText with the new count.
        let ops = app.update();
        assert_eq!(ops.len(), 1);
        assert!(matches!(
            &ops[0],
            PatchOp::SetText { text, .. } if text == "Count: 1"
        ));
        assert!(LocalApp::is_text_only(&ops));
        app.apply(&mut dom, ops);
        let h2 = dom.children(card)[0];
        let text = dom.children(h2)[0];
        assert_eq!(dom.node(text).text, "Count: 1");
    }

    #[test]
    fn style_patches_merge_into_style_attr() {
        let count = use_signal(|| 0u32);
        let flip = use_signal(|| false);
        let render = move || {
            let color = if flip.get() { "#101014" } else { "#ffffff" };
            view! {
                div(style: ("padding: 4px; background: ".to_string() + color)) { "{count}" }
            }
        };
        let mut app = LocalApp::new(Rc::new(render));
        let (mut dom, body) = app.mount_dom();
        let div = dom.children(body)[0];
        assert_eq!(
            dom.element(div).unwrap().attr("style"),
            Some("padding: 4px; background: #ffffff")
        );
        flip.set(true);
        let ops = app.update();
        // Only the background declaration is patched; padding survives.
        assert!(matches!(
            &ops[0],
            PatchOp::SetStyle { name, value, .. } if name == "background" && value == "#101014"
        ));
        app.apply(&mut dom, ops);
        assert_eq!(
            dom.element(div).unwrap().attr("style"),
            Some("padding: 4px; background: #101014")
        );
    }

    #[test]
    fn insert_remove_and_maps_stay_in_sync() {
        let items = use_signal(Vec::new);
        let render = move || {
            view! {
                ul {
                    for items.iter().enumerate() -> (i, it) {
                        li(key: i) { "{it}" }
                    }
                }
            }
        };
        let mut app = LocalApp::new(Rc::new(render));
        let (mut dom, body) = app.mount_dom();
        let ul = dom.children(body)[0];
        assert!(dom.children(ul).is_empty());

        items.update(|v| v.push("alpha".to_string()));
        let ops = app.update();
        app.apply(&mut dom, ops);
        assert_eq!(dom.children(ul).len(), 1);

        // Prepend: keyed reconcile moves the survivor and inserts ahead.
        items.update(|v| v.insert(0, "zero".to_string()));
        let ops = app.update();
        app.apply(&mut dom, ops);
        let kids = dom.children(ul);
        assert_eq!(kids.len(), 2);
        let first_text = dom.children(kids[0])[0];
        assert_eq!(dom.node(first_text).text, "zero");

        // Pop the last: subtree refs drop out of the maps.
        items.update(|v| {
            v.pop();
        });
        let ops = app.update();
        let before_len = app.ref2node_len();
        app.apply(&mut dom, ops);
        assert_eq!(dom.children(ul).len(), 1);
        assert_eq!(app.ref2node_len(), before_len - 2); // li + its text
    }
}
