//! Virtual-tree diff → v1 [`PatchOp`] stream.
//!
//! A [`Mount`] owns the live node refs the runtime created. `init` renders a
//! first [`View`]; `diff` transforms the previous view into the next one.
//! Element children reconcile by key when both sides are keyed lists
//! (preserving per-node identity across reorders); everything else matches
//! positionally. Structural ops address *current* child indexes and are
//! valid only applied in order.

use crate::patch::{NodeData, NodeRef, PatchOp};
use crate::vnode::{Handler, VElement, VNode, View};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// The live tree of one mounted component: the runtime applies the op stream
/// these methods produce, then keeps the [`Mount`] for the next diff.
pub struct Mount {
    root: NodeRef,
    top: Vec<Shadow>,
    next: u64,
    /// ref → (event, handler) for every live element; runtimes dispatch
    /// incoming events through this table.
    handlers: HashMap<u64, Vec<(String, Handler)>>,
}

enum Shadow {
    Text {
        r: NodeRef,
        text: String,
    },
    Elem {
        r: NodeRef,
        ve: Rc<VElement>,
        kids: Vec<Shadow>,
    },
}

impl Mount {
    pub fn new(root: NodeRef) -> Mount {
        Mount {
            root,
            top: Vec::new(),
            next: 1,
            handlers: HashMap::new(),
        }
    }

    /// Refs → handler table for live event dispatch.
    pub fn handlers(&self) -> &HashMap<u64, Vec<(String, Handler)>> {
        &self.handlers
    }

    /// First render under an empty mount.
    pub fn init(&mut self, view: &View) -> Vec<PatchOp> {
        let mut ops = Vec::new();
        let root = self.root;
        for (i, n) in view.nodes.iter().enumerate() {
            let sh = self.insert(root, i, n, &mut ops);
            self.top.push(sh);
        }
        ops
    }

    /// Diff against the next view; returns the ops that transform the live
    /// tree into `view`.
    pub fn diff(&mut self, view: &View) -> Vec<PatchOp> {
        let mut ops = Vec::new();
        let root = self.root;
        let mut top = std::mem::take(&mut self.top);
        diff_list(self, root, &mut top, &view.nodes, &mut ops);
        self.top = top;
        ops
    }

    fn alloc(&mut self) -> NodeRef {
        let r = NodeRef(self.next);
        self.next += 1;
        r
    }

    fn insert(
        &mut self,
        parent: NodeRef,
        index: usize,
        n: &VNode,
        ops: &mut Vec<PatchOp>,
    ) -> Shadow {
        match n {
            VNode::Text(t) => {
                let r = self.alloc();
                ops.push(PatchOp::InsertNode {
                    parent,
                    index,
                    id: r,
                    node: NodeData::text(t.to_string()),
                });
                Shadow::Text {
                    r,
                    text: t.to_string(),
                }
            }
            VNode::Element(el) => {
                let r = self.alloc();
                if !el.events.is_empty() {
                    self.handlers.insert(r.0, el.events.clone());
                }
                let mut data = node_data(el);
                // The wire contract: a handler-bearing element names its own
                // ref so clients know where to aim Event frames without
                // shipping code.
                if !el.events.is_empty() {
                    data.handler = Some(r.0);
                }
                // Node handles ride to the runtime the same way.
                data.node_ref = el.node_ref;
                ops.push(PatchOp::InsertNode {
                    parent,
                    index,
                    id: r,
                    node: data,
                });
                let mut kids = Vec::with_capacity(el.children.len());
                for (ci, c) in el.children.iter().enumerate() {
                    kids.push(self.insert(r, ci, c, ops));
                }
                Shadow::Elem {
                    r,
                    ve: el.clone(),
                    kids,
                }
            }
        }
    }
}

/// Serializable payload for an element (attributes + styles; handlers live in
/// the runtime table, never on the wire).
pub fn node_data(el: &VElement) -> NodeData {
    NodeData {
        tag: el.tag.clone(),
        text: None,
        attrs: el.attrs.clone(),
        styles: el.styles.clone(),
        handler: None,
        node_ref: el.node_ref,
    }
}

fn diff_list(
    m: &mut Mount,
    parent: NodeRef,
    old: &mut Vec<Shadow>,
    new: &[VNode],
    ops: &mut Vec<PatchOp>,
) {
    // A9 (strict): a list that mixes keyed and unkeyed children loses the
    // reconciliation guarantees keys exist to provide — warn once per diff.
    let keyed = new
        .iter()
        .filter(|n| matches!(n, VNode::Element(e) if e.key.is_some()))
        .count();
    if keyed > 0 && keyed < new.len() {
        crate::kit::warn("list mixes keyed and unkeyed children — give every element a key");
    }
    let old_keyed = old.iter().all(shadow_is_keyed);
    let new_keyed = new
        .iter()
        .all(|n| matches!(n, VNode::Element(e) if e.key.is_some()));
    if old_keyed && new_keyed {
        // Duplicate keys crash reconcile (the plan maps each key once, so a
        // repeated key walks off the end). React's behavior is the model:
        // warn and degrade to positional reconciliation for this list.
        if has_duplicate_keys(old, new) {
            crate::kit::warn(
                "duplicate keys in a keyed list — falling back to positional reconcile \
                 for this diff (give every element a unique key)",
            );
        } else {
            keyed_reconcile(m, parent, old, new, ops);
            return;
        }
    }

    // Positional.
    let common = old.len().min(new.len());
    for i in 0..common {
        diff_node(m, parent, i, &mut old[i], &new[i], ops);
    }
    for i in (common..old.len()).rev() {
        prune_handlers(m, &old[i]);
        ops.push(PatchOp::RemoveNode { parent, index: i });
        old.remove(i);
    }
    for n in new[common..].iter() {
        let sh = m.insert(parent, old.len(), n, ops);
        old.push(sh);
    }
}

fn keyed_reconcile(
    m: &mut Mount,
    parent: NodeRef,
    old: &mut Vec<Shadow>,
    new: &[VNode],
    ops: &mut Vec<PatchOp>,
) {
    #[derive(Clone, Copy)]
    enum Step {
        Keep,
        Fresh,
    }
    let mut old_keys: HashMap<&str, ()> = HashMap::new();
    for s in old.iter() {
        if let Some(k) = shadow_key(s) {
            old_keys.insert(k, ());
        }
    }
    let plan: Vec<Step> = new
        .iter()
        .map(|n| match n {
            VNode::Element(e) if e.key.as_deref().map_or(false, |k| old_keys.contains_key(k)) => {
                Step::Keep
            }
            _ => Step::Fresh,
        })
        .collect();

    // Remove unmatched old children first (descending index keeps live
    // indexes valid).
    let mut cur = std::mem::take(old);
    for i in (0..cur.len()).rev() {
        let keep = match &cur[i] {
            Shadow::Elem { ve, .. } => ve.key.as_deref().map_or(false, |k| {
                new.iter()
                    .any(|n| matches!(n, VNode::Element(e) if e.key.as_deref() == Some(k)))
            }),
            _ => false,
        };
        if !keep {
            prune_handlers(m, &cur[i]);
            ops.push(PatchOp::RemoveNode { parent, index: i });
            cur.remove(i);
        }
    }

    // Walk the new order left to right, moving keeps into place and
    // inserting fresh subtrees at their final index.
    for (j, n) in new.iter().enumerate() {
        match plan[j] {
            Step::Fresh => {
                let sh = m.insert(parent, j, n, ops);
                cur.insert(j, sh);
            }
            Step::Keep => {
                let k = vnode_key(n).expect("keyed plan");
                let p = cur
                    .iter()
                    .position(|s| shadow_key(s) == Some(k))
                    .expect("kept key must be live");
                if p != j {
                    ops.push(PatchOp::MoveNode {
                        parent,
                        from: p,
                        to: j,
                    });
                    let sh = cur.remove(p);
                    cur.insert(j, sh);
                }
            }
        }
    }

    // Content diffs on the survivors (fresh subtrees are already correct).
    for (j, n) in new.iter().enumerate() {
        if matches!(plan[j], Step::Keep) {
            diff_node(m, parent, j, &mut cur[j], n, ops);
        }
    }
    *old = cur;
}

fn diff_node(
    m: &mut Mount,
    parent: NodeRef,
    index: usize,
    sh: &mut Shadow,
    n: &VNode,
    ops: &mut Vec<PatchOp>,
) {
    match (sh, n) {
        (Shadow::Text { r, text }, VNode::Text(t)) => {
            if &**t != text {
                ops.push(PatchOp::SetText {
                    node: *r,
                    text: t.to_string(),
                });
                *text = t.to_string();
            }
        }
        (Shadow::Elem { r, ve, kids }, VNode::Element(ne)) if ve.tag == ne.tag => {
            diff_attrs(m, r, ve, ne, ops);
            let mut kk = std::mem::take(kids);
            diff_list(m, *r, &mut kk, &ne.children, ops);
            *kids = kk;
            // The shadow must track the latest element, or the next diff
            // compares against stale attrs and drops needed removals.
            *ve = ne.clone();
        }
        (sh, other) => {
            // Type or tag changed: replace in place.
            prune_handlers(m, sh);
            ops.push(PatchOp::RemoveNode { parent, index });
            let fresh = m.insert(parent, index, other, ops);
            *sh = fresh;
        }
    }
}

fn diff_attrs(
    m: &mut Mount,
    r: &NodeRef,
    old: &Rc<VElement>,
    new: &Rc<VElement>,
    ops: &mut Vec<PatchOp>,
) {
    for (k, v) in &new.attrs {
        let changed = match old.attrs.iter().find(|(ok, _)| ok == k) {
            Some((_, ov)) => ov != v,
            None => true,
        };
        if changed {
            ops.push(PatchOp::SetAttr {
                node: *r,
                name: k.clone(),
                value: v.clone(),
            });
        }
    }
    for (k, _) in &old.attrs {
        if !new.attrs.iter().any(|(nk, _)| nk == k) {
            ops.push(PatchOp::RemoveAttr {
                node: *r,
                name: k.clone(),
            });
        }
    }
    for (k, v) in &new.styles {
        let changed = match old.styles.iter().find(|(ok, _)| ok == k) {
            Some((_, ov)) => ov != v,
            None => true,
        };
        if changed {
            ops.push(PatchOp::SetStyle {
                node: *r,
                name: k.clone(),
                value: v.clone(),
            });
        }
    }
    for (k, _) in &old.styles {
        if !new.styles.iter().any(|(nk, _)| nk == k) {
            ops.push(PatchOp::SetStyle {
                node: *r,
                name: k.clone(),
                value: String::new(),
            });
        }
    }
    // Handlers: runtime-table bookkeeping, not DOM mutation — no op emitted.
    if !new.events.is_empty() || !old.events.is_empty() {
        if new.events.is_empty() {
            m.handlers.remove(&r.0);
        } else {
            m.handlers.insert(r.0, new.events.clone());
        }
    }
}

fn vnode_key(n: &VNode) -> Option<&str> {
    match n {
        VNode::Element(e) => e.key.as_deref(),
        _ => None,
    }
}

/// True when either side of the keyed diff repeats a key. Duplicates make
/// `keyed_reconcile`'s plan ambiguous (each key maps once), so callers
/// degrade to positional reconciliation instead.
fn has_duplicate_keys(old: &[Shadow], new: &[VNode]) -> bool {
    fn dup_in<'a, I: Iterator<Item = &'a str>>(mut keys: I) -> bool {
        let mut seen: HashSet<&str> = HashSet::new();
        keys.any(|k| !seen.insert(k))
    }
    dup_in(old.iter().filter_map(shadow_key)) || dup_in(new.iter().filter_map(vnode_key))
}

/// Drop the handler-table entries of a removed shadow subtree. Without this,
/// every dynamic list (tabs, todos, search results) leaks its closures for
/// the whole session — and a reused node id would dispatch stale handlers.
fn prune_handlers(m: &mut Mount, sh: &Shadow) {
    match sh {
        Shadow::Text { .. } => {}
        Shadow::Elem { r, kids, .. } => {
            m.handlers.remove(&r.0);
            for k in kids {
                prune_handlers(m, k);
            }
        }
    }
}

fn shadow_is_keyed(s: &Shadow) -> bool {
    matches!(s, Shadow::Elem { ve, .. } if ve.key.is_some())
}

fn shadow_key(s: &Shadow) -> Option<&str> {
    match s {
        Shadow::Elem { ve, .. } => ve.key.as_deref(),
        _ => None,
    }
}
