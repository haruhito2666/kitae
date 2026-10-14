//! `kitae_ui::testing` — render a component to a queryable tree, fire
//! events, assert on output (RP-C4).
//!
//! The harness mirrors what a runtime does — [`crate::diff::Mount`] init +
//! diff, then dispatching authored handlers — with no DOM and no engine:
//!
//! ```
//! use kitae_ui::prelude::*;
//! use kitae_ui::testing::TestApp;
//!
//! #[component]
//! fn Counter() -> View {
//!     let count = use_signal(|| 0);
//!     view! {
//!         div {
//!             p { "Count: {count}" }
//!             button(on:click = move |_| count.update(|c| *c += 1)) { "+1" }
//!         }
//!     }
//! }
//!
//! let mut app = TestApp::new(|| Counter());
//! assert!(app.text().contains("Count: 0"));
//! assert!(app.click_text("+1"));
//! assert!(app.text().contains("Count: 1"));
//! // The kitae-specific superpower: assert on the *patch stream* too.
//! assert!(app.ops().iter().any(|op| matches!(
//!     op, kitae_ui::patch::PatchOp::SetText { .. }
//! )));
//! ```
//!
//! # Queries
//!
//! [`TestApp::query`] / [`TestApp::query_text`] / [`TestApp::query_class`]
//! walk the rendered tree ([`Node`]) in document order; every method takes
//! `&self` and returns owned nodes, so assertions never fight the borrow
//! checker. Fragments (`tag == ""`) are flattened away, exactly as the
//! renderer flattens them.
//!
//! # Events
//!
//! [`TestApp::click_text`] / [`TestApp::click_tag`] / [`TestApp::fire`]
//! invoke the authored handler for an element — the same `Rc` clone the
//! live runtime dispatches through — then re-render and record the diff,
//! so `app.ops()` always reflects the last dispatched event. An `input`
//! event also writes the payload back into every `value: signal` two-way
//! binding on the element, implementing the documented runtime contract
//! (see `VElement::bindings`).
//!
//! # Property harness
//!
//! [`TestApp::fuzz`] fires pseudo-random authored events (seeded, hence
//! deterministic) for N rounds and fails the run if any handler or render
//! panics. The property under test: *for any event sequence, the tree
//! stays renderable and the patch stream stays well-formed.* Warnings
//! collected by [`crate::kit::warn`] surface in the report.
//!
//! Hook scopes are process-global; when several tests mount the same
//! component function, call [`reset`] between them for fresh hook slots.

use crate::diff::Mount;
use crate::patch::{NodeRef, PatchOp};
use crate::signals::Signal;
use crate::vnode::{Event, Handler, VNode, View};
use std::rc::Rc;

// ------------------------------------------------------------- the tree ---

/// One node of the rendered tree: an element, or a text node (`tag == ""`).
#[derive(Clone)]
pub struct Node {
    tag: String,
    attrs: Vec<(String, String)>,
    styles: Vec<(String, String)>,
    key: Option<String>,
    events: Vec<(String, Handler)>,
    bindings: Vec<(String, Signal<String>)>,
    own_text: String,
    children: Vec<Node>,
}

impl Node {
    /// Element tag; `""` for text nodes.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Reconciliation key, if the element carried one.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// Attribute value by name.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Inline style declaration by name.
    pub fn style(&self, name: &str) -> Option<&str> {
        self.styles
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// This node's own text (text nodes only; elements return `""`).
    pub fn own_text(&self) -> &str {
        &self.own_text
    }

    /// Flattened subtree text, like `textContent`.
    pub fn text(&self) -> String {
        let mut s = self.own_text.clone();
        for c in &self.children {
            s.push_str(&c.text());
        }
        s
    }

    /// Child nodes (fragments already flattened).
    pub fn children(&self) -> &[Node] {
        &self.children
    }

    /// This node and every descendant, document order.
    pub fn walk(&self) -> Vec<&Node> {
        let mut out = vec![self];
        for c in &self.children {
            out.extend(c.walk());
        }
        out
    }

    /// Whether this node carries a handler for `event` — lets tests assert
    /// handler wiring (e.g. the Modal panel's bubble-swallowing no-op, or a
    /// disabled Button having no click handler at all).
    pub fn has_event(&self, name: &str) -> bool {
        self.events.iter().any(|(e, _)| e == name)
    }
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("tag", &self.tag)
            .field("key", &self.key)
            .field("text", &self.own_text)
            .field("children", &self.children.len())
            .finish()
    }
}

/// Flatten a view into query nodes: fragments dissolve into their parent,
/// text becomes a `tag == ""` node.
fn build_tree(nodes: &[VNode]) -> Vec<Node> {
    let mut out = Vec::new();
    for n in nodes {
        match n {
            VNode::Text(t) => out.push(Node {
                tag: String::new(),
                attrs: Vec::new(),
                styles: Vec::new(),
                key: None,
                events: Vec::new(),
                bindings: Vec::new(),
                own_text: (**t).clone(),
                children: Vec::new(),
            }),
            VNode::Element(e) if e.tag.is_empty() => out.extend(build_tree(&e.children)),
            VNode::Element(e) => out.push(Node {
                tag: e.tag.clone(),
                attrs: e.attrs.clone(),
                styles: e.styles.clone(),
                key: e.key.clone(),
                events: e.events.clone(),
                bindings: e.bindings.clone(),
                own_text: String::new(),
                children: build_tree(&e.children),
            }),
        }
    }
    out
}

// ---------------------------------------------------------------- the app ---

/// A mounted component under test: render closure + live [`Mount`] + the
/// queryable snapshot of the last render.
pub struct TestApp {
    render: Rc<dyn Fn() -> View>,
    mount: Mount,
    view: View,
    tree: Vec<Node>,
    last_ops: Vec<PatchOp>,
}

impl TestApp {
    /// Mount `f` (a component call or any `Fn() -> View`): render once and
    /// apply the init ops to an empty mount.
    pub fn new(f: impl Fn() -> View + 'static) -> TestApp {
        let render = crate::hooks::wrap_render(Rc::new(f));
        let mut mount = Mount::new(NodeRef(0));
        let view = render();
        let last_ops = mount.init(&view);
        let tree = build_tree(&view.nodes);
        TestApp {
            render,
            mount,
            view,
            tree,
            last_ops,
        }
    }

    /// Re-render and diff — the [`TestApp::refresh`] a runtime would run
    /// after a dispatched event. Drains landed resource fetches first (the
    /// runtime's poll loop does this every tick), so a test that renders
    /// after a fetch completed sees its data. Returns the patch stream.
    pub fn refresh(&mut self) -> Vec<PatchOp> {
        let _landed = crate::resource::drain();
        let view = (self.render)();
        let ops = self.mount.diff(&view);
        self.tree = build_tree(&view.nodes);
        self.last_ops = ops.clone();
        self.view = view;
        ops
    }

    /// The patch stream from the last render/init (empty when nothing
    /// changed) — assert on *what the wire would carry*.
    pub fn ops(&self) -> &[PatchOp] {
        &self.last_ops
    }

    /// HTML of the current render (the engine's Init payload shape).
    pub fn html(&self) -> String {
        crate::render::render_html(&self.view.nodes)
    }

    /// Flattened text of the current render.
    pub fn text(&self) -> String {
        self.tree.iter().map(|n| n.text()).collect()
    }

    /// Roots of the queryable tree.
    pub fn root(&self) -> &[Node] {
        &self.tree
    }

    /// All elements with this tag, document order.
    pub fn query(&self, tag: &str) -> Vec<Node> {
        self.query_where(|n| n.tag == tag)
    }

    /// All elements whose flattened text contains `needle`, document order.
    pub fn query_text(&self, needle: &str) -> Vec<Node> {
        self.query_where(|n| n.text().contains(needle))
    }

    /// All elements carrying this class token (space-separated match).
    pub fn query_class(&self, class: &str) -> Vec<Node> {
        self.query_where(|n| {
            n.attr("class")
                .map_or(false, |c| c.split_whitespace().any(|t| t == class))
        })
    }

    /// All nodes matching an arbitrary predicate, document order.
    pub fn query_where(&self, pred: impl Fn(&Node) -> bool) -> Vec<Node> {
        let mut out = Vec::new();
        for n in &self.tree {
            for w in n.walk() {
                if pred(w) {
                    out.push(w.clone());
                }
            }
        }
        out
    }

    /// Element at an index path from the roots (`node_at(&[0, 2, 1])`).
    pub fn node_at(&self, path: &[usize]) -> Option<&Node> {
        let mut cur = self.tree.get(*path.first()?)?;
        for i in &path[1..] {
            cur = cur.children.get(*i)?;
        }
        Some(cur)
    }

    /// How many elements carry this tag.
    pub fn count(&self, tag: &str) -> usize {
        self.query(tag).len()
    }

    /// Whether any element carries this tag.
    pub fn exists(&self, tag: &str) -> bool {
        self.count(tag) > 0
    }

    // ------------------------------------------------------------- events ---

    /// Fire `event` on `node` (a node from a query) with an optional value
    /// payload, then re-render. An `input` event with a value also writes
    /// into every `value:` two-way binding on the element. Returns false
    /// when the node carries neither a matching handler nor such a binding.
    pub fn fire(&mut self, node: &Node, event: &str, value: Option<&str>) -> bool {
        let mut acted = false;
        if event == "input" {
            if let Some(v) = value {
                for (name, sig) in &node.bindings {
                    if name == "value" {
                        sig.set(v.to_string());
                        acted = true;
                    }
                }
            }
        }
        if let Some((_, h)) = node.events.iter().find(|(n, _)| n == event) {
            let h = h.clone();
            h(Event {
                name: event.to_string(),
                value: value.map(str::to_string),
            });
            acted = true;
        }
        if acted {
            self.refresh();
        }
        acted
    }

    /// Click the first element with tag `tag` bearing a `click` handler.
    pub fn click_tag(&mut self, tag: &str) -> bool {
        let hit = self
            .query_where(|n| n.tag == tag && n.has_event("click"))
            .into_iter()
            .next();
        match hit {
            Some(n) => self.fire(&n, "click", None),
            None => false,
        }
    }

    /// Click the first element whose flattened text contains `text` and
    /// which bears a `click` handler (document order — the interactive
    /// ancestor of the label wins, e.g. the `<button>` around a `<span>`).
    pub fn click_text(&mut self, text: &str) -> bool {
        let hit = self
            .query_where(|n| n.text().contains(text) && n.has_event("click"))
            .into_iter()
            .next();
        match hit {
            Some(n) => self.fire(&n, "click", None),
            None => false,
        }
    }

    /// Type into the first element with tag `tag` that has an `input`
    /// handler or a `value:` binding: fires `input` with the payload (the
    /// binding write-back included) and re-renders.
    pub fn set_value(&mut self, tag: &str, value: &str) -> bool {
        let hit = self
            .query_where(|n| {
                n.tag == tag
                    && (n.has_event("input") || n.bindings.iter().any(|(nm, _)| nm == "value"))
            })
            .into_iter()
            .next();
        match hit {
            Some(n) => self.fire(&n, "input", Some(value)),
            None => false,
        }
    }

    // ------------------------------------------------------ property fuzz ---

    /// The property harness: fire `cfg.rounds` pseudo-random authored
    /// events (seeded → deterministic) and fail the run if any handler or
    /// render panics. Invariant under test: *any event sequence leaves the
    /// tree renderable and the patch stream well-formed.*
    pub fn fuzz(&mut self, cfg: FuzzCfg) -> Result<FuzzReport, FuzzFailure> {
        const VALUES: [&str; 7] = ["", "x", "hello world", "<b>&\"'", "0", "42", "日本語"];
        let mut state = cfg.seed | 1;
        let mut report = FuzzReport {
            rounds: 0,
            fired: 0,
            dead_ends: 0,
            ops_emitted: 0,
            warnings: Vec::new(),
        };
        for round in 0..cfg.rounds {
            report.rounds = round + 1;
            let candidates = self.fuzz_candidates();
            if candidates.is_empty() {
                report.dead_ends += 1;
                break;
            }
            let pick = xorshift(&mut state) as usize % candidates.len();
            let (path, event) = candidates[pick].clone();
            let value = match event.as_str() {
                "input" | "change" => Some(VALUES[xorshift(&mut state) as usize % VALUES.len()]),
                _ => None,
            };
            let node = match self.node_at(&path).cloned() {
                Some(n) => n,
                None => {
                    return Err(FuzzFailure {
                        round,
                        event,
                        detail: "node vanished mid-round".into(),
                    })
                }
            };
            let fired = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.fire(&node, &event, value)
            }));
            match fired {
                Ok(true) => {
                    report.fired += 1;
                    report.ops_emitted += self.last_ops.len();
                }
                Ok(false) => {}
                Err(payload) => {
                    return Err(FuzzFailure {
                        round,
                        event,
                        detail: panic_detail(&payload),
                    });
                }
            }
            report.warnings.extend(crate::kit::take_warnings());
        }
        Ok(report)
    }

    /// Every (index path, event name) pair that could act, document order.
    fn fuzz_candidates(&self) -> Vec<(Vec<usize>, String)> {
        fn walk(n: &Node, prefix: &mut Vec<usize>, out: &mut Vec<(Vec<usize>, String)>) {
            for (e, _) in &n.events {
                out.push((prefix.clone(), e.clone()));
            }
            if !n.bindings.is_empty() && !n.events.iter().any(|(e, _)| e == "input") {
                out.push((prefix.clone(), "input".to_string()));
            }
            for (i, c) in n.children.iter().enumerate() {
                prefix.push(i);
                walk(c, prefix, out);
                prefix.pop();
            }
        }
        let mut out = Vec::new();
        let mut prefix = Vec::new();
        for (i, n) in self.tree.iter().enumerate() {
            prefix.push(i);
            walk(n, &mut prefix, &mut out);
            prefix.pop();
        }
        out
    }
}

/// Deterministic xorshift64* — no RNG dependency, reproducible runs.
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Fuzz configuration: seed and round count.
#[derive(Clone, Copy, Debug)]
pub struct FuzzCfg {
    pub seed: u64,
    pub rounds: usize,
}

impl Default for FuzzCfg {
    fn default() -> FuzzCfg {
        FuzzCfg {
            seed: 0xC0FFEE,
            rounds: 64,
        }
    }
}

/// What a fuzz run observed.
#[derive(Clone, Debug, Default)]
pub struct FuzzReport {
    /// Rounds actually attempted (may stop early on `Err` or dead end).
    pub rounds: usize,
    /// Events that acted (handler ran or binding wrote).
    pub fired: usize,
    /// Runs that stopped because the tree offered no events at all.
    pub dead_ends: usize,
    /// Total patch ops the rounds produced — the wire-load shape.
    pub ops_emitted: usize,
    /// Strictness/lint warnings drained after the run.
    pub warnings: Vec<String>,
}

/// A fuzz round found a panicking handler or render.
#[derive(Clone, Debug)]
pub struct FuzzFailure {
    /// Zero-based round that failed.
    pub round: usize,
    /// The event that was being fired.
    pub event: String,
    /// Panic payload, or a structural note.
    pub detail: String,
}

impl std::fmt::Display for FuzzFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fuzz round {}: firing '{}': {}",
            self.round, self.event, self.detail
        )
    }
}

impl std::error::Error for FuzzFailure {}

fn panic_detail(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Drop every hook scope/slot — test isolation between apps that mount the
/// same component function (same positional scope otherwise reuses slots).
/// Signals hoisted outside components are unaffected: they live in the
/// signal slab, not in scopes.
pub fn reset() {
    crate::hooks::__reset_for_tests();
}
