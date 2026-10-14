//! Virtual node tree produced by `view!`.

use crate::signals::Signal;
use std::fmt::Display;
use std::rc::Rc;

/// A handler bound through `on:event = ...`. Takes `Event` by value — a
/// `&Event` parameter makes closure inference fight the higher-ranked
/// lifetime and `move |_| ..` handlers fail to check.
pub type Handler = Rc<dyn Fn(Event)>;

/// A UI event, routed back into the tree by a runtime.
#[derive(Debug, Clone)]
pub struct Event {
    /// `click`, `submit`, `change`, …
    pub name: String,
    /// The engine/OS value payload, if any (input text, checkbox state).
    pub value: Option<String>,
}

/// One node of the virtual tree.
#[derive(Clone)]
pub enum VNode {
    Element(Rc<VElement>),
    Text(Rc<String>),
}

impl std::fmt::Debug for VNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VNode::Element(e) => f
                .debug_struct("Element")
                .field("tag", &e.tag)
                .field("key", &e.key)
                .field("children", &e.children.len())
                .finish(),
            VNode::Text(t) => f.debug_tuple("Text").field(t).finish(),
        }
    }
}

/// An element of the virtual tree.
pub struct VElement {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    /// `style` declarations, kept separate so the diff can emit per-property
    /// `SetStyle` ops.
    pub styles: Vec<(String, String)>,
    /// Attributes whose value came from a `Signal<String>` (two-way binding:
    /// the runtime writes DOM input events back into these).
    pub bindings: Vec<(String, Signal<String>)>,
    /// (event name, handler)
    pub events: Vec<(String, Handler)>,
    /// Reconciliation key for list children.
    pub key: Option<String>,
    /// A [`NodeHandle`] id marking this element for imperative access
    /// (focus, scroll-into-view, measured rects). The runtime records
    /// handle → real node when the element is inserted.
    pub node_ref: Option<u64>,
    pub children: Vec<VNode>,
}

impl VElement {
    /// The `style` attribute string as it would appear in HTML.
    pub fn style_attr(&self) -> String {
        let mut s = String::new();
        for (n, v) in &self.styles {
            if !s.is_empty() {
                s.push_str("; ");
            }
            s.push_str(n);
            s.push_str(": ");
            s.push_str(v);
        }
        s
    }
}

/// The result of a component: zero or more nodes.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub nodes: Vec<VNode>,
}

impl View {
    pub fn from_nodes(nodes: Vec<VNode>) -> View {
        View { nodes }
    }
}

impl From<VNode> for View {
    fn from(n: VNode) -> View {
        View { nodes: vec![n] }
    }
}

impl From<String> for View {
    fn from(s: String) -> View {
        View {
            nodes: vec![VNode::Text(Rc::new(s))],
        }
    }
}

impl From<&str> for View {
    fn from(s: &str) -> View {
        View::from(s.to_string())
    }
}

// ------------------------------------------------------------- builders ----

/// Builder produced by `view!` for each element; chained attribute calls then
/// a final `children(vec![..]).build()`.
pub struct ElemBuilder {
    el: VElement,
}

impl ElemBuilder {
    pub fn new(tag: &'static str) -> ElemBuilder {
        ElemBuilder {
            el: VElement {
                tag: tag.to_string(),
                attrs: Vec::new(),
                styles: Vec::new(),
                bindings: Vec::new(),
                events: Vec::new(),
                key: None,
                node_ref: None,
                children: Vec::new(),
            },
        }
    }

    /// Mark this element with a [`NodeHandle`](crate::vnode::NodeHandle) —
    /// the runtime records handle → node so the host can focus it, scroll
    /// it into view, or read its measured rect.
    pub fn node_ref(mut self, handle: NodeHandle) -> Self {
        self.el.node_ref = Some(handle.id());
        self
    }

    /// Static or dynamic attribute. `style` is parsed into declarations so the
    /// diff can update individual properties. Later writes to the same name
    /// replace earlier ones (HTML semantics) — no duplicate attr entries.
    pub fn attr(mut self, name: &str, value: impl Display) -> Self {
        let v = value.to_string();
        if name == "style" {
            self.el.styles = parse_declarations(&v);
        } else {
            set_attr_slot(&mut self.el.attrs, name, v);
        }
        self
    }

    /// Attribute whose value expression was a bare identifier — rendered now,
    /// and if it was a `Signal<String>` the runtime gets a binding handle.
    pub fn attr_source(mut self, name: &str, rendered: AttrRendered) -> Self {
        let AttrRendered { text, signal } = rendered;
        if name == "style" {
            self.el.styles = parse_declarations(&text);
        } else {
            set_attr_slot(&mut self.el.attrs, name, text);
        }
        if let Some(sig) = signal {
            self.el.bindings.push((name.to_string(), sig));
        }
        self
    }

    pub fn on(mut self, event: &str, handler: Handler) -> Self {
        self.el.events.push((event.to_string(), handler));
        self
    }

    pub fn key(mut self, key: KeyVal) -> Self {
        self.el.key = Some(key.0);
        self
    }

    pub fn children(mut self, children: Vec<VNode>) -> Self {
        self.el.children = children;
        self
    }

    pub fn build(self) -> VNode {
        VNode::Element(Rc::new(self.el))
    }
}

/// Insert or replace an attribute by name — attributes are a set.
fn set_attr_slot(attrs: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some(slot) = attrs.iter_mut().find(|(k, _)| k == name) {
        slot.1 = value;
    } else {
        attrs.push((name.to_string(), value));
    }
}

/// `key:` values; constructed by [`crate::__rt::key_val`].
pub struct KeyVal(pub String);

/// The rendered form of an attribute value expression.
pub struct AttrRendered {
    pub text: String,
    pub signal: Option<Signal<String>>,
}

/// Types usable as attribute values. Bare-identifier attribute expressions
/// go through this so `Signal<String>` values double as two-way bindings.
pub trait AttrRef {
    fn render_attr(&self) -> AttrRendered;
}

macro_rules! attr_ref_primitives {
    ($($t:ty),* $(,)?) => {
        $(
            impl AttrRef for $t {
                fn render_attr(&self) -> AttrRendered {
                    AttrRendered { text: self.to_string(), signal: None }
                }
            }
        )*
    };
}

attr_ref_primitives!(
    String, &str, bool, i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize, f32, f64
);

impl<T: Display + Clone + 'static> AttrRef for Signal<T> {
    fn render_attr(&self) -> AttrRendered {
        let signal = {
            let any: &dyn std::any::Any = self;
            any.downcast_ref::<Signal<String>>().cloned()
        };
        AttrRendered {
            text: self.get().to_string(),
            signal,
        }
    }
}

/// Parse `a: b; c: d` into declaration pairs (best effort, mirrors CSS
/// declaration syntax without a full parser).
/// Split a declaration block into (property, value) pairs. The `;` split is
/// quote- and paren-aware: a value like `url(data:image/png;base64,AAAA)`
/// or `content: "a;b"` survives whole instead of being chopped at its
/// embedded separator. (Colons are NOT tracked — a colon inside a quoted
/// value still belongs to the value, and `split_once(':')` on the
/// declaration keeps the first, which is the property separator.)
pub fn parse_declarations(s: &str) -> Vec<(String, String)> {
    match split_declarations(s, true) {
        Some(decls) => collect_decls(&decls),
        // Malformed block — an unclosed quote or `(` swallowed the rest
        // (nothing split after it). Re-split blindly so a stray `(` can't
        // eat the whole stylesheet.
        None => collect_decls(&split_declarations(s, false).unwrap_or_default()),
    }
}

fn collect_decls(decls: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for decl in decls {
        let decl = decl.trim();
        if decl.is_empty() {
            continue;
        }
        if let Some((name, value)) = decl.split_once(':') {
            out.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    out
}

/// Cut the block into `;`-separated declarations. With `strict`, `;` inside
/// quotes or parentheses (url(...), rgb(...), calc(...)) does not split.
/// Returns None when the block ends still inside a quote or paren — the
/// strict scan could not see the tail as separate declarations.
fn split_declarations(s: &str, strict: bool) -> Option<Vec<&str>> {
    let mut decls: Vec<&str> = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut start = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = s[i..].chars().next().unwrap();
        let len = c.len_utf8();
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' if strict => quote = Some(c),
                '(' if strict => depth += 1,
                ')' if strict && depth > 0 => depth -= 1,
                ';' if depth == 0 => {
                    decls.push(&s[start..i]);
                    start = i + len;
                }
                _ => {}
            },
        }
        i += len;
    }
    if quote.is_some() || depth > 0 {
        return None;
    }
    decls.push(&s[start..]);
    Some(decls)
}

// ---------------------------------------------------------- __rt helpers ---

/// Fragment element: an empty tag flattens to its children in every consumer
/// (renderer, diff, patch applier).
pub fn frag(nodes: Vec<VNode>) -> VNode {
    match nodes.as_slice() {
        [one] => one.clone(),
        _ => VNode::Element(Rc::new(VElement {
            tag: String::new(),
            attrs: Vec::new(),
            styles: Vec::new(),
            bindings: Vec::new(),
            events: Vec::new(),
            key: None,
            node_ref: None,
            children: nodes,
        })),
    }
}

/// The implicit `else` of an `if` with no else branch.
pub fn none() -> VNode {
    // Represented as an empty fragment element: renders nothing, diffs to
    // nothing.
    frag(Vec::new())
}

/// Missing text constructor on VNode.
impl VNode {
    pub fn text(s: impl Into<String>) -> VNode {
        VNode::Text(Rc::new(s.into()))
    }
}

/// Turn anything `Into<View>` (a nested component call, a block expression)
/// into the flat child list the parent builder expects.
pub fn view_nodes<V: Into<View>>(v: V) -> Vec<VNode> {
    v.into().nodes
}

/// The children a component receives from its call site's braces:
/// `Card() { p { "body" } }` passes the braces as `Children`.
///
/// Children are **lazy**: the braces build only when the component's own
/// view interpolates `{children}` — which is what makes context work (a
/// provider's `provide_context` runs before its children are built, so
/// nested components find it) and keeps child subtrees out of the tree
/// entirely when a component decides not to render them.
///
/// A component declares it as its LAST parameter — `children: Children` —
/// and splices it with `{children}` (`From<Children> for View` drives the
/// ordinary interpolation path).
pub struct Children(Rc<dyn Fn() -> View>);

impl Children {
    /// Construct from a deferred builder (`__rt::make_children`).
    pub fn new(f: Rc<dyn Fn() -> View>) -> Children {
        Children(f)
    }

    /// Build the children's view (runs the deferred construction inside
    /// the component's scope).
    pub fn render(&self) -> View {
        (self.0)()
    }
}

impl From<Children> for View {
    fn from(c: Children) -> View {
        c.render()
    }
}

/// The named slots a component receives from its call site:
/// `Card() { slot(name: "header") { h1 { … } } }`. A component declares
/// `slots: Slots` as its LAST parameter and splices a slot wherever it
/// belongs with [`Slots::render`] (`{slots.render("header")}`). Slots are
/// lazy like [`Children`] — a slot's view builds only where it is rendered
/// — and a name the call site never passed renders empty.
#[derive(Clone, Default)]
pub struct Slots {
    entries: Rc<Vec<(String, Children)>>,
}

impl Slots {
    /// Construct from (name, builder) pairs (`__rt`, i.e. the `view!`
    /// macro's slot-directive expansion).
    pub fn new(entries: Vec<(String, Children)>) -> Slots {
        Slots {
            entries: Rc::new(entries),
        }
    }

    /// The names that were passed at the call site.
    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Whether the call site passed a slot with this name.
    pub fn has(&self, name: &str) -> bool {
        self.entries.iter().any(|(n, _)| n == name)
    }

    /// Build a slot's view — empty when the call site didn't pass it.
    pub fn render(&self, name: &str) -> View {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.render())
            .unwrap_or_default()
    }
}

/// A stable identity for one mounted element — React's ref, Kitae-shaped.
///
/// Create with [`crate::signals::use_node_ref`] (inside a component it is
/// slot-stable; outside one it is created fresh, the hoisted pattern) and
/// mark the element with `node_ref: handle`. The runtime records
/// handle → real node at insert time; the host then focuses it, scrolls it
/// into view, or reads its measured rect (`Browser::focus_node`,
/// `scroll_node_into_view`, `node_rect`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeHandle {
    pub(crate) id: u64,
}

impl NodeHandle {
    pub fn new(id: u64) -> NodeHandle {
        NodeHandle { id }
    }

    /// The runtime-facing identity.
    pub fn id(&self) -> u64 {
        self.id
    }
}

thread_local! {
    static NEXT_HANDLE: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

pub(crate) fn alloc_node_handle() -> NodeHandle {
    NEXT_HANDLE.with(|n| {
        let id = n.get();
        n.set(id + 1);
        NodeHandle { id }
    })
}
