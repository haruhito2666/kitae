//! kitae_ui — reactive UI core for Kitae.
//!
//! React made web pages interactive with JavaScript; kitae_ui makes them
//! interactive with Rust. The browser speaks it natively, so nothing is
//! shipped, compiled to, or executed as script.
//!
//! One reactive core, two runtimes:
//! - **local** — the embedder holds the tree in-process (this crate);
//! - **live** — a server holds the tree and streams patch ops over a socket
//!   ([`crate::patch`], consumed by `kitae_live`).
//!
//! This crate has no dependency on the engine. The engine (kitae_core)
//! consumes the [`PatchOp`](patch::PatchOp) stream this crate produces.
//!
//! # A static page — HTML-shaped Rust, no script
//!
//! ```
//! use kitae_ui::prelude::*;
//!
//! #[component]
//! fn Home() -> View {
//!     view! {
//!         html {
//!             head { title { "Hello Kitae" } }
//!             body {
//!                 h1(class: "hero") { "Hello, Kitae!" }
//!                 p { "This page has no JavaScript. The server speaks Rust." }
//!                 a(href: "/about") { "About" }
//!             }
//!         }
//!     }
//! }
//!
//! let page = Home();
//! let html = kitae_ui::render::render_html(&page.nodes);
//! assert!(html.contains("Hello, Kitae!"));
//! assert!(html.contains(r#"href="/about""#));
//! ```
//!
//! # Interactivity — state, handlers, conditional UI
//!
//! ```
//! use kitae_ui::prelude::*;
//!
//! #[component]
//! fn Counter() -> View {
//!     let count = use_signal(|| 0);
//!     view! {
//!         section(class: "card") {
//!             h2 { "Count: {count}" }
//!             button(on:click = move |_| count.update(|c| *c += 1)) { "+1" }
//!             if count.get() > 10 {
//!                 p(class: "warn") { "That's a lot of clicking." }
//!             }
//!         }
//!     }
//! }
//!
//! let page = Counter();
//! assert!(kitae_ui::render::render_html(&page.nodes).contains("Count: 0"));
//! ```
//!
//! # The reactive loop — a state change becomes a minimal patch stream
//!
//! ```
//! use kitae_ui::prelude::*;
//!
//! let count = use_signal(|| 0);
//! let render = move || view! {
//!     section(class: "card") {
//!         h2 { "Count: {count}" }
//!     }
//! };
//!
//! let mut mount = Mount::new(NodeRef(0));
//! let init_ops = mount.init(&render());
//! assert_eq!(init_ops.len(), 3); // <section>, its <h2>, then the text node
//!
//! count.set(1);
//! let patch = mount.diff(&render());
//! assert_eq!(patch.len(), 1); // exactly one SetText
//! assert!(matches!(
//!     &patch[0],
//!     PatchOp::SetText { text, .. } if text == "Count: 1"
//! ));
//! ```
//!
//! # Lists — keyed reconciliation
//!
//! ```
//! use kitae_ui::prelude::*;
//!
//! #[derive(Clone)]
//! struct Todo { text: String, done: bool }
//!
//! #[component]
//! fn Todos() -> View {
//!     let todos = use_signal(Vec::new);
//!     let draft = use_signal(String::new);
//!
//!     let add = move |_| {
//!         let text = draft.get().trim().to_string();
//!         if !text.is_empty() {
//!             todos.update(|t| t.push(Todo { text, done: false }));
//!             draft.set(String::new());
//!         }
//!     };
//!
//!     view! {
//!         form(on:submit = add) {
//!             // One-way value + an input handler: `value: draft` binds the
//!             // attribute to the signal AND registers a two-way binding the
//!             // runtime writes DOM input events back into (Signal<String>
//!             // props only). A plain string attr is display-only — pair it
//!             // with on:input to keep `draft` in sync.
//!             input(value: draft, placeholder: "What needs doing?")
//!             button(type: "submit") { "Add" }
//!         }
//!         ul {
//!             for todos.iter().enumerate() -> (i, todo) {
//!                 li(key: i) {
//!                     checkbox(checked: todo.done, on:change = move |_| todos.update(|t| t[i].done = !t[i].done))
//!                     span { "{todo.text}" }
//!                 }
//!             }
//!         }
//!     }
//! }
//!
//! let page = Todos();
//! let html = kitae_ui::render::render_html(&page.nodes);
//! assert!(html.contains("Add"));
//! assert!(html.contains(r#"placeholder="What needs doing?""#));
//! ```
//!
//! Two-way text input: `value: draft` (a `Signal<String>` prop) already
//! registers a runtime binding that writes DOM input events back into the
//! signal. For a display-only attribute or a non-Signal value, sync by
//! hand instead — the attribute does not write back on its own:
//!
//! ```rust,ignore
//! input(value: draft.get(), on:input = move |e| draft.set(e.value.unwrap_or_default()))
//! ```

// Lets the `#[component]`/`view!` macros' emitted `kitae_ui::…` paths
// resolve inside this crate too (kit.rs uses its own macros).
extern crate self as kitae_ui;

pub mod charts;
pub mod components;
pub mod devtools;
pub mod diff;
pub mod effects;
pub mod hooks;
pub mod kit;
pub mod patch;
pub mod render;
pub mod resource;
pub mod signals;
pub mod testing;
pub mod vnode;

pub use crate::vnode::{VNode, View};
pub use kitae_ui_macros::{component, view};

/// Runtime helpers the `view!` macro expands to. Not public API; do not use.
#[doc(hidden)]
pub mod __rt {
    pub use crate::render::{attr_ref, attr_str, attr_val, box_handler, key_val};
    pub use crate::vnode::{frag, none, view_nodes, ElemBuilder, VNode, View};

    /// Wrap a deferred children builder as the `Children` argument of a
    /// component call that was written with braces: `Card() { … }`. The
    /// closure runs when the component interpolates `{children}`.
    pub fn make_children(f: impl Fn() -> View + 'static) -> crate::vnode::Children {
        crate::vnode::Children::new(std::rc::Rc::new(f))
    }

    /// Element constructor used by `view!`.
    pub fn elem(tag: &'static str) -> ElemBuilder {
        ElemBuilder::new(tag)
    }

    /// Text constructor used by `view!`.
    pub fn text(s: impl Into<String>) -> VNode {
        VNode::text(s)
    }

    /// Top-level `view!` result: a `View` from the flat child list.
    pub fn view_from(nodes: Vec<VNode>) -> View {
        View::from_nodes(nodes)
    }

    /// `for .. -> (..) { .. }` expansion: flattened loop output.
    pub fn for_nodes(nodes: Vec<VNode>) -> Vec<VNode> {
        nodes
    }
}

/// The kitae_ui prelude — import this and you have the whole API.
///
/// ```rust,ignore
/// use kitae_ui::prelude::*;
/// ```
pub mod prelude {
    pub use crate::charts::{AreaChart, BarChart, LineChart, Sparkline};
    pub use crate::components::{
        push_toast, theme_class, Button, DatePicker, Drawer, Modal, Table, Tabs, ThemeProvider,
        ToastHost, ToastMsg, Tooltip, VirtualList,
    };
    pub use crate::devtools::{DevTools, OpCounts, PatchLog};
    pub use crate::diff::Mount;
    pub use crate::effects::use_effect;
    pub use crate::hooks::{
        provide_context, scope_tree, try_use_context, use_context, wrap_render, ScopeNode,
    };
    pub use crate::kit::{take_warnings as take_strict_warnings, use_form, use_memo_view};
    pub use crate::kit::{ErrorBoundary, Form, Link, Router};
    pub use crate::patch::{NodeData, NodeRef, PatchOp};
    pub use crate::resource::{
        clear_cached, clear_cached_all, drain, use_cached_resource, use_resource,
        use_resource_with_deps, Resource,
    };
    pub use crate::signals::{use_node_ref, use_ref, use_signal, Signal};
    pub use crate::testing::TestApp;
    pub use crate::vnode::{Children, Event, VNode, View};
    pub use kitae_ui_macros::{component, view};
}
