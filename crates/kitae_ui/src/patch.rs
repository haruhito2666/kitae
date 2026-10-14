//! The v1 patch-op surface.
//!
//! A `PatchOp` is the *only* way a kitae_ui tree changes the DOM. The op set
//! is deliberately tiny and auditable: there is no script execution, no
//! arbitrary code, no escape hatch — a runtime (local engine or `kitae_live`
//! client) applies these ops to the real tree, nothing else.

use std::fmt;

/// One DOM mutation. Wire format v1.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchOp {
    /// Insert `node` as a child of `parent` at `index`. `id` is the NodeRef
    /// the node will answer to from now on (runtimes build their
    /// ref→real-node maps from this).
    InsertNode {
        parent: NodeRef,
        index: usize,
        id: NodeRef,
        node: NodeData,
    },
    /// Remove the child at `index` from `parent`.
    RemoveNode { parent: NodeRef, index: usize },
    /// Move an existing child of `parent` from `from` to `to`.
    MoveNode {
        parent: NodeRef,
        from: usize,
        to: usize,
    },
    /// Replace the text content of a text node.
    SetText { node: NodeRef, text: String },
    /// Set one attribute (empty value still means present).
    SetAttr {
        node: NodeRef,
        name: String,
        value: String,
    },
    /// Remove an attribute.
    RemoveAttr { node: NodeRef, name: String },
    /// Set one inline style declaration.
    SetStyle {
        node: NodeRef,
        name: String,
        value: String,
    },
    /// Move keyboard focus to a node.
    SetFocus { node: NodeRef },
    /// Scroll a node into view.
    ScrollIntoView { node: NodeRef },
}

/// Identifies a node on either side of the wire. Real runtimes resolve this
/// to their own node ids; the id is assigned by the diff producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeRef(pub u64);

/// The serializable shape of a node being inserted.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeData {
    /// Element tag (`""` for a text node).
    pub tag: String,
    /// Text content for text nodes; ignored for elements.
    pub text: Option<String>,
    pub attrs: Vec<(String, String)>,
    pub styles: Vec<(String, String)>,
    /// Index into the runtime's handler table — how `on:*` survives the wire
    /// without shipping code.
    pub handler: Option<u64>,
    /// A [`NodeHandle`](crate::vnode::NodeHandle) id — the runtime records
    /// handle → real node at insert time so the host can focus, scroll to,
    /// or measure the element.
    pub node_ref: Option<u64>,
}

impl NodeData {
    pub fn text(text: impl Into<String>) -> NodeData {
        NodeData {
            tag: String::new(),
            text: Some(text.into()),
            attrs: Vec::new(),
            styles: Vec::new(),
            handler: None,
            node_ref: None,
        }
    }

    pub fn element(tag: impl Into<String>) -> NodeData {
        NodeData {
            tag: tag.into(),
            text: None,
            attrs: Vec::new(),
            styles: Vec::new(),
            handler: None,
            node_ref: None,
        }
    }
}

impl fmt::Display for PatchOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatchOp::InsertNode {
                parent,
                index,
                id,
                node,
            } => write!(
                f,
                "insert {node:?} as {} into {} at {index}",
                id.0, parent.0
            ),
            PatchOp::RemoveNode { parent, index } => {
                write!(f, "remove child {index} of {}", parent.0)
            }
            PatchOp::MoveNode { parent, from, to } => {
                write!(f, "move child {from} -> {to} of {}", parent.0)
            }
            PatchOp::SetText { node, text } => write!(f, "text {} = {text:?}", node.0),
            PatchOp::SetAttr { node, name, value } => {
                write!(f, "attr {} {name} = {value:?}", node.0)
            }
            PatchOp::RemoveAttr { node, name } => write!(f, "attr -{} {name}", node.0),
            PatchOp::SetStyle { node, name, value } => {
                write!(f, "style {} {name} = {value:?}", node.0)
            }
            PatchOp::SetFocus { node } => write!(f, "focus {}", node.0),
            PatchOp::ScrollIntoView { node } => write!(f, "scroll {}", node.0),
        }
    }
}

/// Apply a patch stream to a minimal in-memory tree — the shared kernel used
/// by test harnesses and (via kitae_dom adapters) the engine. Kept here so
/// the semantics have exactly one definition.
pub mod apply {
    use super::{NodeData, NodeRef, PatchOp};
    use std::collections::HashMap;

    /// A minimal DOM-ish tree the ops apply to.
    #[derive(Debug, Clone, PartialEq)]
    pub struct MiniNode {
        pub tag: String,
        pub text: String,
        pub attrs: Vec<(String, String)>,
        pub styles: Vec<(String, String)>,
        pub children: Vec<u64>,
    }

    /// A tree of `MiniNode`s keyed by id.
    #[derive(Debug, Clone, Default)]
    pub struct MiniTree {
        pub nodes: HashMap<u64, MiniNode>,
        pub root: u64,
        pub next_id: u64,
    }

    impl MiniTree {
        pub fn new() -> MiniTree {
            let mut t = MiniTree {
                nodes: HashMap::new(),
                root: 0,
                next_id: 1,
            };
            t.nodes.insert(
                0,
                MiniNode {
                    tag: "root".into(),
                    text: String::new(),
                    attrs: Vec::new(),
                    styles: Vec::new(),
                    children: Vec::new(),
                },
            );
            t
        }

        fn alloc(&mut self, id: &NodeRef, data: &NodeData) -> u64 {
            let nid = id.0;
            self.next_id = self.next_id.max(nid + 1);
            self.nodes.insert(
                nid,
                MiniNode {
                    tag: data.tag.clone(),
                    text: data.text.clone().unwrap_or_default(),
                    attrs: data.attrs.clone(),
                    styles: data.styles.clone(),
                    children: Vec::new(),
                },
            );
            nid
        }

        /// Apply one op; panics on structural misuse (test harness).
        pub fn apply(&mut self, op: &PatchOp) {
            match op {
                PatchOp::InsertNode {
                    parent,
                    index,
                    id,
                    node,
                } => {
                    let pid = parent.0;
                    let nid = self.alloc(id, node);
                    let kids = &mut self.nodes.get_mut(&pid).unwrap().children;
                    let i = (*index).min(kids.len());
                    kids.insert(i, nid);
                }
                PatchOp::RemoveNode { parent, index } => {
                    let kids = &mut self.nodes.get_mut(&parent.0).unwrap().children;
                    kids.remove(*index);
                }
                PatchOp::MoveNode { parent, from, to } => {
                    let kids = &mut self.nodes.get_mut(&parent.0).unwrap().children;
                    let id = kids.remove(*from);
                    let to = (*to).min(kids.len());
                    kids.insert(to, id);
                }
                PatchOp::SetText { node, text } => {
                    self.nodes.get_mut(&node.0).unwrap().text = text.clone();
                }
                PatchOp::SetAttr { node, name, value } => {
                    set_attr(self.nodes.get_mut(&node.0).unwrap(), name, value.clone());
                }
                PatchOp::RemoveAttr { node, name } => {
                    let n = self.nodes.get_mut(&node.0).unwrap();
                    n.attrs.retain(|(k, _)| k != name);
                }
                PatchOp::SetStyle { node, name, value } => {
                    let n = self.nodes.get_mut(&node.0).unwrap();
                    if let Some(slot) = n.styles.iter_mut().find(|(k, _)| k == name) {
                        slot.1 = value.clone();
                    } else {
                        n.styles.push((name.clone(), value.clone()));
                    }
                }
                PatchOp::SetFocus { .. } | PatchOp::ScrollIntoView { .. } => {}
            }
        }

        /// Render to a canonical string for comparisons.
        pub fn render(&self, id: u64) -> String {
            let n = &self.nodes[&id];
            if n.tag.is_empty() {
                return n.text.clone();
            }
            let mut s = format!("<{}", n.tag);
            for (k, v) in &n.attrs {
                s.push_str(&format!(" {k}={v:?}"));
            }
            if !n.styles.is_empty() {
                s.push_str(&format!(
                    " style={:?}",
                    n.styles
                        .iter()
                        .map(|(k, v)| format!("{k}: {v}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
            }
            if n.children.is_empty() && n.text.is_empty() {
                s.push_str("/>");
                return s;
            }
            s.push('>');
            s.push_str(&n.text);
            for &c in &n.children {
                s.push_str(&self.render(c));
            }
            s.push_str(&format!("</{}>", n.tag));
            s
        }
    }

    fn set_attr(n: &mut MiniNode, name: &str, value: String) {
        if let Some(slot) = n.attrs.iter_mut().find(|(k, _)| k == name) {
            slot.1 = value;
        } else {
            n.attrs.push((name.to_string(), value));
        }
    }

    /// Convenience: apply a stream, return the root rendering.
    pub fn render_after(ops: &[PatchOp]) -> String {
        let mut t = MiniTree::new();
        for op in ops {
            t.apply(op);
        }
        t.render(t.root)
    }

    /// Convenience helper for tests: build `InsertNode` chains.
    pub fn insert(parent: u64, index: usize, tag: &str) -> PatchOp {
        PatchOp::InsertNode {
            parent: NodeRef(parent),
            index,
            id: NodeRef(parent * 1000 + index as u64 + 1),
            node: NodeData::element(tag),
        }
    }
}
