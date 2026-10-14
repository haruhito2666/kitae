//! Property tests for the diff engine: for randomly generated trees and
//! randomly generated mutations, applying `Mount::diff` output to a MiniTree
//! must equal rendering `init(new)` directly — the patch stream is a minimal
//! witness of the transition, not an approximation.

use kitae_ui::diff::Mount;
use kitae_ui::patch::apply::MiniTree;
use kitae_ui::patch::{NodeData, NodeRef, PatchOp};
use kitae_ui::prelude::*;
use kitae_ui::vnode::VElement;

// ----------------------------------------------------------- rng (std-only) --

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

// ------------------------------------------------------- random VNode trees --

const TAGS: [&str; 6] = ["div", "p", "span", "section", "ul", "li"];
const ATTR_NAMES: [&str; 4] = ["class", "id", "href", "title"];
const ATTR_VALUES: [&str; 4] = ["a", "b", "hero", "x-1"];
const TEXTS: [&str; 5] = ["alpha", "beta", "gamma", "hello", ""];

fn gen_tree(rng: &mut Rng, depth: u32) -> VNode {
    if depth == 0 || rng.below(4) == 0 {
        return VNode::text(TEXTS[rng.below(TEXTS.len() as u64) as usize]);
    }
    let mut el = VElement {
        tag: TAGS[rng.below(TAGS.len() as u64) as usize].to_string(),
        attrs: Vec::new(),
        styles: Vec::new(),
        bindings: Vec::new(),
        events: Vec::new(),
        key: None,
        node_ref: None,
        children: Vec::new(),
    };
    let n_attrs = rng.below(3);
    let mut used: Vec<usize> = Vec::new();
    for _ in 0..n_attrs {
        // Attribute names are unique per element (HTML semantics).
        let mut ni = rng.below(ATTR_NAMES.len() as u64) as usize;
        while used.contains(&ni) {
            ni = (ni + 1) % ATTR_NAMES.len();
        }
        used.push(ni);
        el.attrs.push((
            ATTR_NAMES[ni].to_string(),
            ATTR_VALUES[rng.below(ATTR_VALUES.len() as u64) as usize].to_string(),
        ));
    }
    if rng.below(4) == 0 {
        el.styles.push((
            "color".to_string(),
            TEXTS[rng.below(TEXTS.len() as u64) as usize].to_string(),
        ));
    }
    // Deterministic children count from the rng stream.
    let kids = rng.below(4);
    for _ in 0..kids {
        el.children.push(gen_tree(rng, depth - 1));
    }
    VNode::Element(std::rc::Rc::new(el))
}

/// Mutate a tree in place; returns true if something changed.
fn mutate(node: &mut VNode, rng: &mut Rng) -> bool {
    match node {
        VNode::Text(t) => {
            if rng.below(2) == 0 {
                *t = std::rc::Rc::new(TEXTS[rng.below(TEXTS.len() as u64) as usize].to_string());
                true
            } else {
                false
            }
        }
        VNode::Element(el) => {
            // Rc shared with shadows — clone-in to edit (events can't Clone,
            // so the mutation rebuilds only structural fields).
            let mut e = VElement {
                tag: el.tag.clone(),
                attrs: el.attrs.clone(),
                styles: el.styles.clone(),
                bindings: Vec::new(),
                events: Vec::new(),
                key: el.key.clone(),
                node_ref: el.node_ref,
                children: el.children.clone(),
            };
            let mut changed = false;
            match rng.below(4) {
                0 => {
                    let i = rng.below(ATTR_NAMES.len() as u64) as usize;
                    let v = ATTR_VALUES[rng.below(ATTR_VALUES.len() as u64) as usize].to_string();
                    if let Some(slot) = e.attrs.iter_mut().find(|(k, _)| *k == ATTR_NAMES[i]) {
                        if slot.1 != v {
                            slot.1 = v;
                            changed = true;
                        }
                    } else {
                        e.attrs.push((ATTR_NAMES[i].to_string(), v));
                        changed = true;
                    }
                }
                1 if !e.attrs.is_empty() => {
                    let i = rng.below(e.attrs.len() as u64) as usize;
                    e.attrs.remove(i);
                    changed = true;
                }
                2 if !e.children.is_empty() => {
                    let i = rng.below(e.children.len() as u64) as usize;
                    if mutate(&mut e.children[i], rng) {
                        changed = true;
                    }
                }
                3 if e.children.len() < 6 => match rng.below(3) {
                    0 => {
                        let i = rng.below((e.children.len() + 1) as u64) as usize;
                        e.children.insert(i, gen_tree(rng, 2));
                        changed = true;
                    }
                    1 if !e.children.is_empty() => {
                        let i = rng.below(e.children.len() as u64) as usize;
                        e.children.remove(i);
                        changed = true;
                    }
                    _ if e.children.len() >= 2 => {
                        let a = rng.below(e.children.len() as u64) as usize;
                        let b = rng.below(e.children.len() as u64) as usize;
                        e.children.swap(a, b);
                        changed = true;
                    }
                    _ => {}
                },
                _ => {}
            }
            if changed {
                *el = std::rc::Rc::new(e);
            }
            changed
        }
    }
}

// ----------------------------------------------------------------- helpers --

/// Apply init(then)+diff(now) on one MiniTree and render — the diff stream is
/// a *delta*, so equivalence is measured against a tree that already holds
/// `then`'s content.
fn patched_render(then: &View, now: &View) -> String {
    let mut mount = Mount::new(NodeRef(0));
    let init_ops = mount.init(then);
    let patch = mount.diff(now);
    let mut t = MiniTree::new();
    for op in init_ops.iter().chain(patch.iter()) {
        t.apply(op);
    }
    t.render(t.root)
}
/// Render a VNode forest the same way init would, onto a fresh MiniTree.
fn reference_render(nodes: &[VNode]) -> String {
    let mut ops = Vec::new();
    fn emit(nodes: &[VNode], parent: u64, ops: &mut Vec<PatchOp>, next: &mut u64) {
        for (i, n) in nodes.iter().enumerate() {
            match n {
                VNode::Text(t) => {
                    let id = *next;
                    *next += 1;
                    ops.push(PatchOp::InsertNode {
                        parent: NodeRef(parent),
                        index: i,
                        id: NodeRef(id),
                        node: NodeData::text(t.to_string()),
                    });
                }
                VNode::Element(el) => {
                    if el.tag.is_empty() {
                        // Fragment: splice children in place.
                        let mut sub = Vec::new();
                        emit(&el.children, parent, &mut sub, next);
                        for op in sub {
                            ops.push(op);
                        }
                        continue;
                    }
                    let id = *next;
                    *next += 1;
                    ops.push(PatchOp::InsertNode {
                        parent: NodeRef(parent),
                        index: i,
                        id: NodeRef(id),
                        node: kitae_ui::diff::node_data(el),
                    });
                    emit(&el.children, id, ops, next);
                }
            }
        }
    }
    let mut next = 1u64;
    emit(nodes, 0, &mut ops, &mut next);
    let mut t = MiniTree::new();
    for op in &ops {
        t.apply(op);
    }
    t.render(t.root)
}

// ------------------------------------------------------------------- tests --

#[test]
fn no_change_yields_no_ops() {
    for seed in 1..40u64 {
        let mut rng = Rng::new(seed);
        let tree = gen_tree(&mut rng, 3);
        let view = View::from_nodes(vec![tree.clone()]);
        let mut mount = Mount::new(NodeRef(0));
        let init = mount.init(&view);
        assert!(!init.is_empty());
        let patch = mount.diff(&View::from_nodes(vec![tree]));
        assert!(
            patch.is_empty(),
            "seed {seed}: identical tree produced {} ops: {patch:?}",
            patch.len()
        );
    }
}

#[test]
fn single_attr_change_yields_one_set_attr() {
    let view = View::from_nodes(vec![kitae_ui::__rt::elem("div")
        .attr("class", "a")
        .children(vec![])
        .build()]);
    let mut mount = Mount::new(NodeRef(0));
    mount.init(&view);

    let next = View::from_nodes(vec![kitae_ui::__rt::elem("div")
        .attr("class", "b")
        .children(vec![])
        .build()]);
    let patch = mount.diff(&next);
    assert_eq!(patch.len(), 1, "{patch:?}");
    assert!(
        matches!(&patch[0], PatchOp::SetAttr { name, value, .. } if name == "class" && value == "b")
    );
}

#[test]
fn text_change_yields_one_set_text() {
    let view = View::from_nodes(vec![VNode::text("before")]);
    let mut mount = Mount::new(NodeRef(0));
    mount.init(&view);
    let patch = mount.diff(&View::from_nodes(vec![VNode::text("after")]));
    assert_eq!(patch.len(), 1, "{patch:?}");
    assert!(matches!(&patch[0], PatchOp::SetText { text, .. } if text == "after"));
}

#[test]
fn keyed_reorder_yields_only_moves_and_updates() {
    // Three keyed items swapped; structure must survive via MoveNode, never
    // Remove+Insert of the same key.
    fn item(key: &str) -> VNode {
        kitae_ui::__rt::elem("li")
            .key(kitae_ui::__rt::key_val(key))
            .children(vec![VNode::text(key)])
            .build()
    }
    let old = View::from_nodes(vec![kitae_ui::__rt::elem("ul")
        .children(vec![item("a"), item("b"), item("c")])
        .build()]);
    let new = View::from_nodes(vec![kitae_ui::__rt::elem("ul")
        .children(vec![item("c"), item("a"), item("b")])
        .build()]);
    let mut mount = Mount::new(NodeRef(0));
    mount.init(&old);
    let patch = mount.diff(&new);
    for op in &patch {
        assert!(
            matches!(op, PatchOp::MoveNode { .. } | PatchOp::SetText { .. }),
            "keyed reorder should not remove/insert: {patch:?}"
        );
    }
}

#[test]
fn duplicate_keys_degrade_to_positional_not_panic() {
    fn item(key: &str) -> VNode {
        kitae_ui::__rt::elem("li")
            .key(kitae_ui::__rt::key_val(key))
            .children(vec![VNode::text(key)])
            .build()
    }
    // The crash case from the audit: new = [a, a], old = [a]. The keyed plan
    // maps the key once and walks off the end; now it warns and reconciles
    // positionally instead of panicking.
    let old = View::from_nodes(vec![kitae_ui::__rt::elem("ul")
        .children(vec![item("a")])
        .build()]);
    let new = View::from_nodes(vec![kitae_ui::__rt::elem("ul")
        .children(vec![item("a"), item("a")])
        .build()]);
    let mut mount = Mount::new(NodeRef(0));
    mount.init(&old);
    let _ = mount.diff(&new); // must not panic
    let new2 = View::from_nodes(vec![kitae_ui::__rt::elem("ul")
        .children(vec![item("a")])
        .build()]);
    let _ = mount.diff(&new2); // and the list can go back
}

#[test]
fn removed_subtrees_drop_their_handlers() {
    fn item(key: &str) -> VNode {
        kitae_ui::__rt::elem("li")
            .key(kitae_ui::__rt::key_val(key))
            .on(
                "click",
                std::rc::Rc::new(move |_: kitae_ui::vnode::Event| {}),
            )
            .children(vec![VNode::text(key)])
            .build()
    }
    let old = View::from_nodes(vec![kitae_ui::__rt::elem("ul")
        .children(vec![item("a"), item("b"), item("c")])
        .build()]);
    let new = View::from_nodes(vec![kitae_ui::__rt::elem("ul")
        .children(vec![item("a")])
        .build()]);
    let mut mount = Mount::new(NodeRef(0));
    mount.init(&old);
    let init_handlers = mount.handlers().len();
    assert_eq!(init_handlers, 3, "each li carries one click handler");
    mount.diff(&new);
    let left: Vec<_> = mount.handlers().keys().copied().collect();
    assert_eq!(
        left.len(),
        1,
        "only the surviving li keeps its handler, got {left:?}"
    );
}

#[test]
fn apply_diff_equals_fresh_init_random() {
    for seed in 1..120u64 {
        let mut rng = Rng::new(seed);
        let old = gen_tree(&mut rng, 3);
        let mut new = old.clone();
        // Apply a few mutations; skip seeds where nothing changed.
        let mut changed = false;
        for _ in 0..3 {
            changed |= mutate(&mut new, &mut rng);
        }
        if !changed {
            continue;
        }
        let old_view = View::from_nodes(vec![old]);
        let new_view = View::from_nodes(vec![new]);

        let patched = patched_render(&old_view, &new_view);
        let fresh = reference_render(&new_view.nodes);
        assert_eq!(
            patched, fresh,
            "seed {seed}: apply(diff(old,new)) != init(new)"
        );
    }
}

#[test]
fn chained_diffs_stay_equivalent() {
    // Apply several successive diffs on the same mount; each must equal the
    // corresponding fresh render.
    // One mount + one tree across all steps: node ids keep growing and every
    // diff must still land exactly on the fresh render of its target.
    let mut rng = Rng::new(777);
    let mut current = gen_tree(&mut rng, 3);
    let mut mount = Mount::new(NodeRef(0));
    let init_ops = mount.init(&View::from_nodes(vec![current.clone()]));
    let mut tree = MiniTree::new();
    for op in &init_ops {
        tree.apply(op);
    }
    for step in 0..30 {
        let mut next = current.clone();
        let changed = mutate(&mut next, &mut rng);
        if changed {
            let patch = mount.diff(&View::from_nodes(vec![next.clone()]));
            for op in &patch {
                tree.apply(op);
            }
            let fresh = reference_render(&[next.clone()]);
            assert_eq!(
                tree.render(tree.root),
                fresh,
                "step {step}: chained diff diverged from fresh render"
            );
            current = next;
        }
    }
}
