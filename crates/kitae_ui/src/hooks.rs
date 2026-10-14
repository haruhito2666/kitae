//! Component scopes — the machinery that makes hooks position-stable.
//!
//! React's rule of hooks works because re-renders re-run the same component
//! tree in the same order: hook call N in a component always addresses slot
//! N. kitae_ui uses the same model. Scopes are identity by *position path*:
//! a component invocation is `parent scope + sibling index`, and a scope's
//! slot list is indexed by hook call order. State survives re-renders
//! because the same tree shape yields the same path every build.
//!
//! Enter/exit is injected by the `#[component]` macro. The render closures
//! registered through kitae_ui's own types (`LocalApp::new`, `Session::new`,
//! `Route::new`) are wrapped so each build bumps a build id, which resets
//! the sibling cursors — the thing that makes positional identity stable.
//!
//! Rules of hooks apply exactly as in React: don't call hooks conditionally,
//! don't call them outside a `#[component]` (outside one, `use_signal` etc.
//! fall back to the legacy allocate-fresh behavior — the hoisted-signal
//! pattern the start page uses).

use crate::vnode::View;
use std::any::{Any, TypeId};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Root scope: the render closure itself.
pub(crate) const ROOT: u64 = 0;

pub(crate) struct Scope {
    /// Hook slots, indexed by call order: `Signal<T>` boxed, or the value a
    /// hook wants to persist (e.g. `Rc<RefCell<T>>`).
    slots: Vec<Rc<dyn Any>>,
    /// (type id, value) provided by this scope; `use_context` walks up.
    provided: Vec<(TypeId, Rc<dyn Any>)>,
    /// Child scope ids by sibling order.
    children: Vec<u64>,
    /// How many children entered during the CURRENT build.
    child_cursor: usize,
    /// How many hooks ran during the CURRENT build (slots persist; this
    /// cursor resets — that's the whole rule-of-hooks trick).
    hook_cursor: usize,
    last_build: u64,
    /// Display name — the component fn name passed by the `#[component]`
    /// macro. Refreshed on every entry: scopes are positional and reused,
    /// so a re-mounted slot may host a different component.
    name: Option<String>,
}

struct Hooks {
    build_id: u64,
    next_id: u64,
    stack: Vec<u64>,
    scopes: HashMap<u64, Scope>,
}

impl Hooks {
    fn new() -> Hooks {
        let mut scopes = HashMap::new();
        scopes.insert(
            ROOT,
            Scope {
                slots: Vec::new(),
                provided: Vec::new(),
                children: Vec::new(),
                child_cursor: 0,
                hook_cursor: 0,
                last_build: 0,
                name: None,
            },
        );
        Hooks {
            build_id: 0,
            next_id: 1,
            stack: Vec::new(),
            scopes,
        }
    }
}

thread_local! {
    static HOOKS: RefCell<Hooks> = RefCell::new(Hooks::new());
}

/// Begin a build: bump the build id and reset the root's sibling cursor.
pub(crate) fn begin_build() {
    HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        h.build_id += 1;
        h.stack.clear();
        h.scopes.get_mut(&ROOT).expect("root scope").child_cursor = 0;
    });
}

thread_local! {
    /// A9 render-purity detection: depth while a render closure is
    /// building. Signal writes inside that window trip the strictness
    /// warning ([`crate::signals`]'s `check_render_write`).
    static RENDER_DEPTH: Cell<usize> = Cell::new(0);
}

/// True while a render closure is building — the window where signal
/// writes are the impure-render bug.
pub fn is_rendering() -> bool {
    RENDER_DEPTH.with(|d| d.get() > 0)
}

/// Marks the current thread as inside a render until the guard drops.
pub(crate) struct RenderGuard;

impl Drop for RenderGuard {
    fn drop(&mut self) {
        RENDER_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

pub(crate) fn render_guard() -> RenderGuard {
    RENDER_DEPTH.with(|d| d.set(d.get() + 1));
    RenderGuard
}

/// Wrap an authored render closure so every invocation is a fresh build.
/// Also opens the render window: signal writes during the build are
/// recorded as strictness warnings (A9).
pub fn wrap_render(f: Rc<dyn Fn() -> View>) -> Rc<dyn Fn() -> View> {
    Rc::new(move || {
        let _g = render_guard();
        begin_build();
        f()
    })
}

/// Strict-mode render wrapper (A9): runs the closure twice per build and
/// warns when the two passes produce different HTML — the signature of an
/// impure render (state read during render changed by a write during the
/// same render). Returns the first pass's view. Warnings always land in
/// [`crate::kit::take_warnings`]; stderr echo follows `KITAE_STRICT=1`.
///
/// Opt a render closure into this instead of [`wrap_render`] when it hosts
/// components under development; a clean component pays one extra build.
pub fn wrap_render_strict(f: Rc<dyn Fn() -> View>) -> Rc<dyn Fn() -> View> {
    Rc::new(move || {
        let _g = render_guard();
        begin_build();
        let v1 = f();
        begin_build();
        let v2 = f();
        if crate::render::render_html(&v1.nodes) != crate::render::render_html(&v2.nodes) {
            crate::kit::warn(
                "impure render: two renders of the same closure produced different HTML — \
                 a component reads and writes state during render",
            );
        }
        v1
    })
}

/// Enter a component scope (the `#[component]` macro injects this, passing
/// the component fn name for the DevTools scope tree). Returns a guard that
/// pops the scope even when the body panics — error boundaries rely on the
/// stack surviving a caught unwind.
pub fn enter_scope(name: Option<&'static str>) -> ScopeGuard {
    HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        let parent_id = h.stack.last().copied().unwrap_or(ROOT);
        let build_id = h.build_id;
        // Pre-allocate the id before borrowing `scopes` (unused allocations
        // just leave a gap — ids are opaque).
        let fresh = h.next_id;
        h.next_id += 1;
        let id = {
            let parent = h.scopes.get_mut(&parent_id).expect("parent scope");
            if parent.last_build != build_id {
                // First re-entry this build: reset the sibling cursor.
                parent.child_cursor = 0;
                parent.last_build = build_id;
            }
            let index = parent.child_cursor;
            parent.child_cursor += 1;
            match parent.children.get(index).copied() {
                Some(id) => id,
                None => {
                    parent.children.push(fresh);
                    let scope = Scope {
                        slots: Vec::new(),
                        provided: Vec::new(),
                        children: Vec::new(),
                        child_cursor: 0,
                        hook_cursor: 0,
                        last_build: h.build_id,
                        name: name.map(str::to_string),
                    };
                    h.scopes.insert(fresh, scope);
                    fresh
                }
            }
        };
        // Record the PRE-push depth: the guard pops this scope (and anything
        // deeper, on unwind) when the body ends — otherwise a sibling
        // component called after this one would enter as its child.
        let depth = h.stack.len();
        h.stack.push(id);
        // Reset THIS scope's cursors on re-entry (slots persist; the hook
        // call-order cursor and the child sibling cursor both restart each
        // build — rule of hooks). Resetting child_cursor here, at entry, is
        // what makes depth-2+ scopes stable: the lazy parent-side guard
        // below can never fire for a parent that was already entered this
        // build, so without it grandchildren got a fresh scope per render
        // and silently dropped their internal signal state.
        let scope = h.scopes.get_mut(&id).expect("just entered");
        if scope.last_build != build_id {
            scope.hook_cursor = 0;
            scope.child_cursor = 0;
            scope.last_build = build_id;
        }
        scope.name = name.map(str::to_string);
        ScopeGuard { depth }
    })
}

/// Pops the component scope on drop — panic-safe by construction.
pub struct ScopeGuard {
    depth: usize,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        HOOKS.with(|h| {
            h.borrow_mut().stack.truncate(self.depth);
        });
    }
}

/// Leave the component scope.
pub fn exit_scope() {
    HOOKS.with(|h| {
        h.borrow_mut().stack.pop();
    });
}

/// One node of the DevTools scope snapshot.
#[derive(Clone, Debug)]
pub struct ScopeNode {
    /// Opaque scope id.
    pub id: u64,
    /// The component fn name (from the `#[component]` macro), when known.
    pub name: Option<String>,
    /// Hook slots held by this scope (signals, refs, memos).
    pub slots: usize,
    /// Child scopes in sibling order.
    pub children: Vec<ScopeNode>,
}

/// Snapshot the mounted component tree as it stood at the last completed
/// build. Top-level entries are the root render closure's children; call
/// this from a tool rendered LAST among its siblings (the DevTools panel)
/// so every sibling's scopes have been entered for the current build.
///
/// Scopes persist for the thread's lifetime — a component that stops
/// rendering leaves its scope behind, so the snapshot may carry retired
/// scopes rather than pruning them.
pub fn scope_tree() -> Vec<ScopeNode> {
    fn snap(scopes: &HashMap<u64, Scope>, id: u64) -> ScopeNode {
        let s = scopes.get(&id).expect("scope tree: missing scope");
        ScopeNode {
            id,
            name: s.name.clone(),
            slots: s.slots.len(),
            children: s.children.iter().map(|&c| snap(scopes, c)).collect(),
        }
    }
    HOOKS.with(|h| {
        let h = h.borrow();
        let root = h.scopes.get(&ROOT).expect("root scope");
        root.children.iter().map(|&c| snap(&h.scopes, c)).collect()
    })
}

/// The innermost component's display name — for strictness warnings ("in
/// Counter"). `None` outside any component (the root render closure).
pub fn current_scope_name() -> Option<String> {
    HOOKS.with(|h| {
        let h = h.borrow();
        let id = *h.stack.last()?;
        h.scopes.get(&id)?.name.clone()
    })
}

/// True when inside a component body — hooks slot only then.
pub(crate) fn in_scope() -> bool {
    HOOKS.with(|h| !h.borrow().stack.is_empty())
}

/// Run `make` once per slot position: first build creates and stores, later
/// builds reuse the stored handle (cloned out — it is `Clone`, and hooks
/// return cheap handles).
/// Store a hook slot at `idx` — for a position that was EMPTY at reserve
/// time. A `make` that ran more same-scope hooks pushes past the reserved
/// index and its first value lands exactly at `idx` (push appends at the
/// end, which was `idx`), so INSERTING — not replacing — shifts it back up
/// to where the next build's hook cursor will look for it.
fn store_slot(scope: &mut Scope, idx: usize, value: Rc<dyn Any>) {
    if idx < scope.slots.len() {
        scope.slots.insert(idx, value);
    } else {
        scope.slots.push(value);
    }
}

/// Swap a hook slot IN PLACE — for a position known occupied at reserve
/// time (stale memo deps, a skip slot whose props changed, a type
/// mismatch). Inserting here would shift every later hook by one and break
/// rule-of-hooks alignment for the whole scope.
fn replace_slot(scope: &mut Scope, idx: usize, value: Rc<dyn Any>) {
    if idx < scope.slots.len() {
        scope.slots[idx] = value;
    } else {
        scope.slots.push(value);
    }
}

/// Warn once per (scope, position) for a hook slot whose stored type no
/// longer matches the hook now calling at that position — the classic
/// rules-of-hooks violation (a conditional swapped the component type or
/// hook order at the same position). Degrades to a slot reset, like React's
/// dev-mode remount, instead of a hard panic. `owner` is captured by the
/// caller while it already holds the hooks borrow — this must NOT re-borrow
/// HOOKS (it runs under `borrow_mut`).
fn warn_slot_mismatch(id: u64, idx: usize, owner: Option<String>) {
    use std::collections::HashSet;
    thread_local! {
        static WARNED: RefCell<HashSet<(u64, usize)>> = RefCell::new(HashSet::new());
    }
    let fresh = WARNED.with(|w| w.borrow_mut().insert((id, idx)));
    if fresh {
        let owner = owner.unwrap_or_else(|| format!("scope #{id}"));
        crate::kit::warn(format!(
            "hook slot #{idx} in {owner} changed type between builds — hooks were \
             called in a different order or conditionally; resetting that slot's \
             state (the rules of hooks still apply: state in the slot is lost)"
        ));
    }
}

pub(crate) fn with_slot<R: 'static + Clone>(make: impl FnOnce() -> R) -> R {
    // Two-phase: reserve the position under the borrow, run `make` OUTSIDE
    // it, then store. A make that calls hooks (or writes a signal whose
    // subscriber re-enters the hooks state) would otherwise re-borrow
    // HOOKS while held — the RefCell double-borrow panic. Reserving also
    // makes nested first-build hooks land in their own positions (insert
    // at the reserved index, not an append).
    enum Step<R> {
        Hit(R),
        Miss(usize),
        /// Occupied by a different type — the slot must be rebuilt AND the
        /// stale value replaced in place at store time.
        Replace(usize),
    }
    let step = HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        let id = *h.stack.last().expect("with_slot outside a component");
        let scope = h.scopes.get_mut(&id).expect("scope");
        let idx = scope.hook_cursor;
        scope.hook_cursor += 1;
        match scope.slots.get(idx) {
            Some(stored) => match stored.downcast_ref::<R>() {
                Some(v) => Step::Hit(v.clone()),
                None => {
                    // Read the owner name while the borrow is held —
                    // warn_slot_mismatch must not re-borrow HOOKS.
                    let owner = scope.name.clone();
                    warn_slot_mismatch(id, idx, owner);
                    Step::Replace(idx)
                }
            },
            None => Step::Miss(idx),
        }
    });
    match step {
        Step::Hit(value) => value,
        Step::Miss(idx) => {
            let value: R = make();
            HOOKS.with(|h| {
                let mut h = h.borrow_mut();
                let id = *h.stack.last().expect("with_slot outside a component");
                let scope = h.scopes.get_mut(&id).expect("scope");
                // Empty at reserve time → insert semantics: a `make` that
                // ran nested same-scope hooks pushed past this index, and
                // inserting shifts them into their next-build positions.
                store_slot(scope, idx, Rc::new(value.clone()));
            });
            value
        }
        Step::Replace(idx) => {
            let value: R = make();
            HOOKS.with(|h| {
                let mut h = h.borrow_mut();
                let id = *h.stack.last().expect("with_slot outside a component");
                let scope = h.scopes.get_mut(&id).expect("scope");
                // Occupied at reserve time → in-place swap, never insert.
                replace_slot(scope, idx, Rc::new(value.clone()));
            });
            value
        }
    }
}

/// Memo slot: (deps version, cached value). Same hook position = same R.
pub(crate) fn with_memo_slot<R: 'static + Clone>(deps: u64, make: impl FnOnce() -> R) -> R {
    struct MemoSlot<R> {
        deps: u64,
        value: R,
    }
    enum Step<R> {
        Hit(R),
        Miss(usize),
    }
    // Two-phase like [`with_slot`]: the memo body may call hooks or render
    // components, none of which may run while the hooks state is borrowed.
    let step = HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        let id = *h.stack.last().expect("with_memo_slot outside a component");
        let scope = h.scopes.get_mut(&id).expect("scope");
        let idx = scope.hook_cursor;
        scope.hook_cursor += 1;
        if let Some(stored) = scope.slots.get(idx) {
            if let Some(slot) = stored.downcast_ref::<MemoSlot<R>>() {
                if slot.deps == deps {
                    return Step::Hit(slot.value.clone());
                }
            }
        }
        Step::Miss(idx)
    });
    match step {
        Step::Hit(value) => value,
        Step::Miss(idx) => {
            let value = make();
            HOOKS.with(|h| {
                let mut h = h.borrow_mut();
                let id = *h.stack.last().expect("with_memo_slot outside a component");
                let scope = h.scopes.get_mut(&id).expect("scope");
                // The position is occupied whenever deps changed (a stale
                // MemoSlot sits there) — swap in place, never insert, or
                // every later hook shifts by one.
                replace_slot(
                    scope,
                    idx,
                    Rc::new(MemoSlot {
                        deps,
                        value: value.clone(),
                    }),
                );
            });
            value
        }
    }
}

/// A6 auto-skip slot: the cached view is reused — and the component body
/// skipped entirely — whenever `props` compares equal to the previous
/// build's props (React.memo semantics, driven by `#[component(skip_eq)]`).
/// The build closure runs OUTSIDE the hooks borrow (it may call hooks),
/// then lands in the slot.
/// Public because the `#[component(skip_eq)]` macro emits a fully-qualified
/// call into generated code.
pub fn with_skip_eq_slot<P: PartialEq + 'static, R: Clone + 'static>(
    props: P,
    build: impl FnOnce() -> R,
) -> R {
    struct SkipSlot<P, R> {
        props: P,
        view: R,
    }
    enum Step<P, R> {
        Hit(R),
        Miss(usize, P),
    }
    let step = HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        let id = *h
            .stack
            .last()
            .expect("with_skip_eq_slot outside a component");
        let scope = h.scopes.get_mut(&id).expect("scope");
        let idx = scope.hook_cursor;
        scope.hook_cursor += 1;
        if let Some(stored) = scope.slots.get(idx) {
            if let Some(slot) = stored.downcast_ref::<SkipSlot<P, R>>() {
                if slot.props == props {
                    return Step::Hit(slot.view.clone());
                }
            }
        }
        Step::Miss(idx, props)
    });
    match step {
        Step::Hit(view) => view,
        Step::Miss(idx, props) => {
            let view = build();
            HOOKS.with(|h| {
                let mut h = h.borrow_mut();
                let id = *h
                    .stack
                    .last()
                    .expect("with_skip_eq_slot outside a component");
                let scope = h.scopes.get_mut(&id).expect("scope");
                // Occupied whenever props changed (a stale SkipSlot sits
                // there) — swap in place, never insert.
                replace_slot(
                    scope,
                    idx,
                    Rc::new(SkipSlot {
                        props,
                        view: view.clone(),
                    }),
                );
            });
            view
        }
    }
}

/// Provide a context value to the current component's subtree. Reactivity
/// comes from providing a `Signal<T>`; consumers read it with `.get()`.
pub fn provide_context<T: 'static>(value: T) {
    HOOKS.with(|h| {
        let mut h = h.borrow_mut();
        let id = *h.stack.last().expect("provide_context outside a component");
        let scope = h.scopes.get_mut(&id).expect("scope");
        let key = TypeId::of::<T>();
        scope.provided.retain(|(k, _)| *k != key);
        scope.provided.push((key, Rc::new(value)));
    });
}

/// Read the nearest provided context value of type `T`, walking up the
/// component chain (and the root). Panics when nothing provides it — fail
/// loudly, like React's missing-provider error.
pub fn use_context<T: 'static + Clone>() -> T {
    try_use_context::<T>().expect("use_context: no provider for this type in scope")
}

/// Non-panicking [`use_context`]: `None` when nothing in scope provides a
/// value of type `T`. The optional-context form — the kit's theming falls
/// back to defaults instead of requiring a provider.
pub fn try_use_context<T: 'static + Clone>() -> Option<T> {
    let found = HOOKS.with(|h| {
        let h = h.borrow();
        let key = TypeId::of::<T>();
        let mut ids: Vec<u64> = h.stack.iter().rev().copied().collect();
        ids.push(ROOT);
        for id in ids {
            if let Some(scope) = h.scopes.get(&id) {
                if let Some((_, v)) = scope.provided.iter().find(|(k, _)| *k == key) {
                    return Some(v.clone());
                }
            }
        }
        None
    });
    let rc = found?;
    let v = rc.downcast::<T>().ok()?;
    Some((*v).clone())
}

/// Reset every scope's slots — test-only (`crate::testing::reset`): a fresh
/// world in the same thread (the slab is thread-local and never cleared,
/// and neither are slots).
pub(crate) fn reset_all() {
    HOOKS.with(|h| *h.borrow_mut() = Hooks::new());
}

/// Test-only scope reset, exposed through `crate::testing::reset`.
#[doc(hidden)]
pub fn __reset_for_tests() {
    reset_all();
}
