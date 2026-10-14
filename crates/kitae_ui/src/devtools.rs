//! The DevTools panel (RP-C3) — the debugging story that makes the
//! patch-op model *visible*.
//!
//! Three sections, all rendered by one component:
//! - **Component tree** — the mounted scopes with their component names,
//!   scope ids, and hook-slot counts (from [`crate::hooks::scope_tree`]).
//! - **Signals** — live values, read through caller-supplied inspection
//!   closures (the slab is type-erased; the embedder knows the types).
//! - **Patch log** — what the wire actually sent: the recorded
//!   [`PatchOp`] stream with structural (insert/remove/move — the ops
//!   that force real DOM work and relayout) vs property (text/attr/style)
//!   counts.
//!
//! # Wiring
//!
//! ```rust
//! use kitae_ui::prelude::*;
//! use kitae_ui::devtools::{DevTools, PatchLog};
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
//! let log = PatchLog::new();
//! let log_handle = log.clone();
//! let mut app = TestApp::new(move || view! {
//!     div {
//!         Counter()
//!         DevTools(vec![], Some(log_handle.clone()))
//!     }
//! });
//! // The host contract: after each render, feed the patch stream in.
//! log.record(app.ops());
//! assert!(app.text().contains("Count: 0"));
//! // The tree section names every mounted component:
//! assert!(app.text().contains("Counter"));
//! assert!(app.click_text("+1"));
//! log.record(app.ops()); // the change's ops are now on the log
//! let counts = log.counts();
//! assert!(counts.property >= 1, "the Count text change is a property op");
//! assert_eq!(counts.total, counts.structural + counts.property);
//! ```
//!
//! **Mount `DevTools` last** among its siblings (as above): the tree
//! section reflects the scopes entered so far this build, so a tool
//! rendered after its siblings sees all of them.

use crate::component;
use crate::hooks::{scope_tree, ScopeNode};
use crate::patch::PatchOp;
use crate::signals::debug_cells;
use crate::view;
use crate::vnode::{VNode, View};

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

/// How many patch ops the log keeps. Older ops fall off the front; the
/// structural/property counters keep counting everything ever recorded.
const CAP: usize = 256;

/// A bounded, cloneable log of patch ops, fed by the runtime host
/// (`log.record(app.ops())` after each render) and displayed by
/// [`DevTools`]. Clone handles freely — every clone sees the same entries.
#[derive(Clone, Default)]
pub struct PatchLog {
    entries: Rc<RefCell<VecDeque<PatchOp>>>,
    structural: Rc<RefCell<usize>>,
    property: Rc<RefCell<usize>>,
    total: Rc<RefCell<usize>>,
}

/// Structural-vs-property split of a [`PatchLog`] — the splice/relayout
/// proxy: structural ops mutate the tree shape (and force engine
/// splice/relayout work); property ops mutate existing nodes in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpCounts {
    /// InsertNode / RemoveNode / MoveNode.
    pub structural: usize,
    /// SetText / SetAttr / RemoveAttr / SetStyle / SetFocus / ScrollIntoView.
    pub property: usize,
    /// Everything recorded.
    pub total: usize,
}

impl PatchLog {
    pub fn new() -> PatchLog {
        PatchLog::default()
    }

    /// Record one render's patch stream. Call after every init/diff.
    pub fn record(&self, ops: &[PatchOp]) {
        let mut entries = self.entries.borrow_mut();
        for op in ops {
            match op {
                PatchOp::InsertNode { .. }
                | PatchOp::RemoveNode { .. }
                | PatchOp::MoveNode { .. } => {
                    *self.structural.borrow_mut() += 1;
                }
                PatchOp::SetText { .. }
                | PatchOp::SetAttr { .. }
                | PatchOp::RemoveAttr { .. }
                | PatchOp::SetStyle { .. }
                | PatchOp::SetFocus { .. }
                | PatchOp::ScrollIntoView { .. } => {
                    *self.property.borrow_mut() += 1;
                }
            }
            *self.total.borrow_mut() += 1;
            entries.push_back(op.clone());
        }
        while entries.len() > CAP {
            entries.pop_front();
        }
    }

    /// Drop every recorded op. Counters keep their totals.
    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
    }

    /// The retained ops, oldest → newest (at most [`CAP`]).
    pub fn entries(&self) -> Vec<PatchOp> {
        self.entries.borrow().iter().cloned().collect()
    }

    pub fn counts(&self) -> OpCounts {
        OpCounts {
            structural: *self.structural.borrow(),
            property: *self.property.borrow(),
            total: *self.total.borrow(),
        }
    }

    /// Retained entries (≤ [`CAP`]).
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

const PANEL_STYLE: &str = "font-family: ui-monospace, Menlo, monospace; font-size: 12px; \
line-height: 1.5; background: #0f172a; color: #e2e8f0; padding: 10px 12px; \
border-radius: 8px; max-width: 420px;";
const TITLE_STYLE: &str = "color: #94a3b8; font-size: 10px; letter-spacing: 0.1em; \
text-transform: uppercase; margin-top: 10px;";
const MUTED_STYLE: &str = "color: #94a3b8;";

/// One tree row per scope, indented by depth.
fn scope_rows(nodes: &[ScopeNode], depth: usize, out: &mut Vec<VNode>) {
    for node in nodes {
        let label = match &node.name {
            Some(name) => format!("{name} · {} slots · #{}", node.slots, node.id),
            None => format!("(anonymous) · {} slots · #{}", node.slots, node.id),
        };
        let pad = 6 + depth * 14;
        out.extend(
            view! {
                div(class: "kit-devtools-scope", style: format!("padding-left: {pad}px;")) {
                    span(class: "kit-devtools-scope-name") { "{label}" }
                }
            }
            .nodes,
        );
        scope_rows(&node.children, depth + 1, out);
    }
}

/// The DevTools panel. Mount it LAST among its siblings so the tree section
/// sees every sibling's scopes for the current build.
///
/// `inspected` names live values to watch; each closure runs once per
/// render, so pass cheap reads (`Rc::new(move || n.get().to_string())`).
/// `log` attaches a [`PatchLog`] the host has been feeding; without one the
/// patch section explains what to pass.
#[component]
pub fn DevTools(inspected: Vec<(String, Rc<dyn Fn() -> String>)>, log: Option<PatchLog>) -> View {
    let tree = scope_tree();
    let slab_cells = debug_cells().len();

    let mut tree_rows: Vec<VNode> = Vec::new();
    scope_rows(&tree, 0, &mut tree_rows);
    let tree_view = View::from_nodes(tree_rows);

    let mut signal_rows: Vec<VNode> = Vec::new();
    for (name, read) in &inspected {
        let value = read();
        signal_rows.extend(
            view! {
                div(class: "kit-devtools-signal", style: "padding-left: 6px;") {
                    span(class: "kit-devtools-signal-name") { "{name}" }
                    span(class: "kit-devtools-signal-value") { " = {value}" }
                }
            }
            .nodes,
        );
    }
    let signals_view = if signal_rows.is_empty() {
        view! { div(class: "kit-devtools-empty", style: MUTED_STYLE) { "no signals inspected" } }
    } else {
        View::from_nodes(signal_rows)
    };

    let (counts_line, entries) = match &log {
        Some(l) => {
            let c = l.counts();
            (
                format!(
                    "{total} ops · {structural} structural (splice/relayout) · {property} property",
                    total = c.total,
                    structural = c.structural,
                    property = c.property,
                ),
                l.entries()
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>(),
            )
        }
        None => (
            "no PatchLog attached — feed one with log.record(ops)".to_string(),
            Vec::new(),
        ),
    };

    let mut op_rows: Vec<VNode> = Vec::new();
    for (i, op) in entries.iter().enumerate() {
        let n = i + 1;
        op_rows.extend(
            view! {
                div(class: "kit-devtools-op", style: "padding-left: 6px; white-space: pre-wrap;") {
                    "{n}. {op}"
                }
            }
            .nodes,
        );
    }
    let ops_view = if op_rows.is_empty() && log.is_some() {
        view! { div(class: "kit-devtools-empty", style: MUTED_STYLE) { "no ops recorded yet" } }
    } else {
        View::from_nodes(op_rows)
    };

    let clear = log.clone();
    let clear_btn = if log.is_some() {
        view! {
            button(class: "kit-devtools-clear", style: "margin-top: 6px; font: inherit; \
            background: #1e293b; color: #e2e8f0; border: 1px solid #334155; border-radius: 6px; \
            padding: 2px 10px; cursor: pointer;",
                on:click = move |_| {
                    if let Some(l) = &clear {
                        l.clear();
                    }
                }) { "Clear log" }
        }
    } else {
        View::default()
    };

    view! {
        aside(class: "kit-devtools", style: PANEL_STYLE) {
            div(class: "kit-devtools-head", style: "font-weight: 600; color: #93c5fd;") { "kitae devtools" }
            div(class: "kit-devtools-sub", style: MUTED_STYLE) { "signal slab: {slab_cells} cells" }
            div(class: "kit-devtools-title", style: TITLE_STYLE) { "component tree" }
            {tree_view}
            div(class: "kit-devtools-title", style: TITLE_STYLE) { "signals" }
            {signals_view}
            div(class: "kit-devtools-title", style: TITLE_STYLE) { "patch log" }
            div(class: "kit-devtools-counts", style: MUTED_STYLE) { "{counts_line}" }
            {ops_view}
            {clear_btn}
        }
    }
}
